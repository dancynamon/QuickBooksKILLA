//! A small, referentially closed slice of the replica, for fixtures.
//! `HANDOFF.md` §2.6, `DECISIONS.md` D26 and D31.
//!
//! The whole book is ~67,000 records; a fixture should be a few hundred. A
//! slice starts from a handful of contacts (N customers and M vendors, picked
//! deterministically, or named) and their most recent documents, then closes
//! over references until nothing in it points outside it:
//!
//! - **Outward**: every `…Ref` (customer, vendor, item, account, class, term,
//!   tax code, tax rate, parent) and every `LinkedTxn` of every record in the
//!   slice is pulled in, recursively. Masters never pull documents.
//! - **Incoming applications**: a document in the slice pulls in every
//!   document that links *to* it (the payments applied to an invoice, the
//!   bill payments applied to a bill), so balances can be checked against
//!   what was applied. Payments are per contact, so this stays within the
//!   seed contacts in practice.
//! - **Deposits are hubs**: one deposit batches many customers' payments, so
//!   following them would pull in the whole book. A deposit joins the slice
//!   only when every document it links is already in; a link from an
//!   in-slice payment to a deposit that is not in is removed from the sliced
//!   copy and counted in [`SliceReport::pruned_links`].
//!
//! A reference whose target is not in the replica at all (US `TAX`/`NON`
//! pseudo tax codes, a `ReimburseCharge`) cannot be closed over and is left
//! as it is; [`dangling`] counts only references the replica could have
//! satisfied. The slicer refuses to finish if that count is not zero.
//!
//! Reads only. What it produces is a *recording* of real data — it goes to
//! `.local/` and through `tools/scrub-fixtures.py` before anything is
//! committed.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::client::{EntityPayload, MockQbo};
use crate::domain::{EntityType, RealmId};
use crate::driver::{SyncDriver, SyncOptions};
use crate::fixture::RecordingQbo;
use crate::store::{MirroredEntity, Store, StoreError};

/// The scrubber reads this file from the recording's directory and treats
/// every line as a name to scrub (`tools/scrub-fixtures.py`, DENY_NAMES_FILE).
pub const DENY_NAMES_FILE: &str = "deny-names.txt";

/// One record: its type and QBO id.
pub type Key = (EntityType, String);

/// What to start a slice from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceOptions {
    /// How many customers to pick (deterministically, by `pick_seed`), from
    /// those with at least one document.
    pub customers: usize,
    /// How many vendors to pick, likewise.
    pub vendors: usize,
    /// Customers to include by id, on top of `customers`.
    pub customer_ids: Vec<String>,
    /// Vendors to include by id, on top of `vendors`.
    pub vendor_ids: Vec<String>,
    /// Only seed documents dated on or after this (closure may still pull
    /// older ones in — an old invoice a new payment applies to).
    pub since: Option<NaiveDate>,
    /// The most recent seed documents to take per contact.
    pub max_docs_per_contact: usize,
    /// Refuse a slice bigger than this, rather than write a fixture nobody
    /// can review.
    pub max_records: usize,
    /// Which contacts get picked. Not a secret: the picked names are scrubbed.
    pub pick_seed: String,
}

impl Default for SliceOptions {
    fn default() -> Self {
        SliceOptions {
            customers: 8,
            vendors: 4,
            customer_ids: Vec::new(),
            vendor_ids: Vec::new(),
            since: None,
            max_docs_per_contact: 12,
            max_records: 1500,
            pick_seed: "slice".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SliceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("no {0} with id {1} in the replica")]
    UnknownContact(&'static str, String),
    #[error("the slice closed over {found} records, more than --max-records {max}; pick fewer contacts or a later --since")]
    TooBig { found: usize, max: usize },
    #[error("the slice is not closed: {0} reference(s) point outside it, first {1}")]
    NotClosed(usize, String),
    #[error("the slice is empty: no contact picked has a document")]
    Empty,
    #[error("{0} already exists and is not empty; record a slice into a fresh directory")]
    DirNotEmpty(String),
    #[error("recording the slice failed: {0}")]
    Record(String),
    #[error("writing {0}: {1}")]
    Io(String, std::io::Error),
}

/// A reference found in a record: the types it could name (an `EntityRef`
/// with no `type` could be a customer or a vendor) and the id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub types: Vec<EntityType>,
    pub id: String,
    /// A `LinkedTxn` entry rather than a `…Ref`.
    pub linked: bool,
}

/// Everything the closure needs to know about one record, without its JSON.
#[derive(Clone, Debug)]
pub struct Node {
    pub refs: Vec<Reference>,
    /// The contact a document belongs to: `CustomerRef`, `VendorRef`, or a
    /// top-level `EntityRef`; for a journal entry, every line's entity.
    pub contacts: Vec<Reference>,
    pub txn_date: Option<String>,
}

const NAME_ENTITY_TYPES: [EntityType; 2] = [EntityType::Customer, EntityType::Vendor];
const NAME_FIELDS: [&str; 7] = [
    "DisplayName",
    "CompanyName",
    "GivenName",
    "MiddleName",
    "FamilyName",
    "FullyQualifiedName",
    "PrintOnCheckName",
];

fn ref_types(key: &str, owner: EntityType, sibling_type: Option<&str>) -> Option<Vec<EntityType>> {
    use EntityType as E;
    let types = match key {
        "CustomerRef" => vec![E::Customer],
        "VendorRef" | "PrefVendorRef" => vec![E::Vendor],
        "ItemRef" | "GroupItemRef" => vec![E::Item],
        "ClassRef" => vec![E::Class],
        "SalesTermRef" | "TermRef" => vec![E::Term],
        "TaxRateRef" => vec![E::TaxRate],
        "ParentRef" => vec![owner],
        "EntityRef" => match sibling_type {
            Some("Customer") => vec![E::Customer],
            Some("Vendor") => vec![E::Vendor],
            Some(_) => return None, // Employee, not mirrored in M0
            None => vec![E::Customer, E::Vendor],
        },
        k if k.ends_with("AccountRef") => vec![E::Account],
        k if k.ends_with("TaxCodeRef") => vec![E::TaxCode],
        _ => return None,
    };
    Some(types)
}

/// `LinkedTxn.TxnType` -> the entity type it names.
fn txn_type(raw: &str) -> Option<EntityType> {
    let name = match raw {
        "BillPaymentCheck" | "BillPaymentCreditCard" => "BillPayment",
        "Check" | "Expense" | "CreditCardCredit" | "CashPurchase" => "Purchase",
        other => other,
    };
    EntityType::parse(name).ok()
}

fn collect_refs(node: &Value, owner: EntityType, out: &mut Vec<Reference>) {
    match node {
        Value::Object(map) => {
            let parent_type = map.get("Type").and_then(Value::as_str);
            for (key, value) in map {
                if key == "LinkedTxn" {
                    for entry in value.as_array().into_iter().flatten() {
                        let target = entry
                            .get("TxnType")
                            .and_then(Value::as_str)
                            .and_then(txn_type);
                        let id = entry.get("TxnId").and_then(Value::as_str);
                        if let (Some(target), Some(id)) = (target, id) {
                            out.push(Reference {
                                types: vec![target],
                                id: id.to_string(),
                                linked: true,
                            });
                        }
                    }
                    continue;
                }
                if key.ends_with("Ref") {
                    if let Some(id) = value.get("value").and_then(Value::as_str) {
                        let sibling = value.get("type").and_then(Value::as_str).or(parent_type);
                        if let Some(types) = ref_types(key, owner, sibling) {
                            out.push(Reference {
                                types,
                                id: id.to_string(),
                                linked: false,
                            });
                        }
                        continue;
                    }
                }
                collect_refs(value, owner, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, owner, out)),
        _ => {}
    }
}

/// The closure's view of one record.
pub fn summarize(entity_type: EntityType, raw: &Value) -> Node {
    let mut refs = Vec::new();
    collect_refs(raw, entity_type, &mut refs);
    let mut contacts = Vec::new();
    if entity_type.as_document().is_some() {
        for key in ["CustomerRef", "VendorRef", "EntityRef"] {
            if let Some(id) = raw
                .get(key)
                .and_then(|r| r.get("value"))
                .and_then(Value::as_str)
            {
                let sibling = raw
                    .get(key)
                    .and_then(|r| r.get("type"))
                    .and_then(Value::as_str);
                if let Some(types) = ref_types(key, entity_type, sibling) {
                    contacts.push(Reference {
                        types,
                        id: id.to_string(),
                        linked: false,
                    });
                }
            }
        }
        if entity_type == EntityType::JournalEntry {
            for line in raw
                .get("Line")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let entity = line
                    .get("JournalEntryLineDetail")
                    .and_then(|d| d.get("Entity"));
                let id = entity
                    .and_then(|e| e.get("EntityRef"))
                    .and_then(|r| r.get("value"))
                    .and_then(Value::as_str);
                let kind = entity.and_then(|e| e.get("Type")).and_then(Value::as_str);
                if let (Some(id), Some(types)) = (id, ref_types("EntityRef", entity_type, kind)) {
                    contacts.push(Reference {
                        types,
                        id: id.to_string(),
                        linked: false,
                    });
                }
            }
        }
    }
    let txn_date = raw
        .get("TxnDate")
        .and_then(Value::as_str)
        .map(str::to_string);
    Node {
        refs,
        contacts,
        txn_date,
    }
}

/// The key a reference resolves to in `known`, if any.
pub fn resolve(reference: &Reference, known: &HashSet<Key>) -> Option<Key> {
    reference
        .types
        .iter()
        .map(|t| (*t, reference.id.clone()))
        .find(|key| known.contains(key))
}

fn sort_keys(keys: &mut [Key]) {
    keys.sort_by(|a, b| (a.0.as_str(), &a.1).cmp(&(b.0.as_str(), &b.1)));
}

/// FNV-1a over `seed:id` — a fixed algorithm, so the same seed picks the
/// same contacts on every build.
fn pick_order(seed: &str, id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes().chain(*b":").chain(id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The chosen keys, and what had to be cut to keep them closed.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub keys: Vec<Key>,
    pub seed_contacts: Vec<Key>,
}

/// Choose a closed slice from summaries of every live record.
pub fn select(nodes: &HashMap<Key, Node>, options: &SliceOptions) -> Result<Selection, SliceError> {
    let known: HashSet<Key> = nodes.keys().cloned().collect();

    // contact -> its documents, newest first.
    let mut docs_of: HashMap<Key, Vec<(String, Key)>> = HashMap::new();
    let mut incoming: HashMap<Key, Vec<Key>> = HashMap::new();
    let mut ordered: Vec<Key> = nodes.keys().cloned().collect();
    sort_keys(&mut ordered);
    for key in &ordered {
        let node = &nodes[key];
        if key.0 != EntityType::Deposit {
            for contact in &node.contacts {
                if let Some(contact) = resolve(contact, &known) {
                    let date = node.txn_date.clone().unwrap_or_default();
                    docs_of
                        .entry(contact)
                        .or_default()
                        .push((date, key.clone()));
                }
            }
        }
        for reference in node.refs.iter().filter(|r| r.linked) {
            if let Some(target) = resolve(reference, &known) {
                incoming.entry(target).or_default().push(key.clone());
            }
        }
    }
    for docs in docs_of.values_mut() {
        docs.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| (a.1 .0.as_str(), &a.1 .1).cmp(&(b.1 .0.as_str(), &b.1 .1)))
        });
        docs.dedup();
    }

    let mut seeds: Vec<Key> = Vec::new();
    for (entity_type, label, ids, count) in [
        (
            EntityType::Customer,
            "customer",
            &options.customer_ids,
            options.customers,
        ),
        (
            EntityType::Vendor,
            "vendor",
            &options.vendor_ids,
            options.vendors,
        ),
    ] {
        for id in ids {
            let key = (entity_type, id.clone());
            if !known.contains(&key) {
                return Err(SliceError::UnknownContact(label, id.clone()));
            }
            seeds.push(key);
        }
        let mut candidates: Vec<&Key> = docs_of
            .keys()
            .filter(|k| k.0 == entity_type && !seeds.contains(k))
            .collect();
        candidates.sort_by_key(|k| (pick_order(&options.pick_seed, &k.1), k.1.clone()));
        seeds.extend(candidates.into_iter().take(count).cloned());
    }

    let since = options.since.map(|d| d.format("%Y-%m-%d").to_string());
    let mut queue: VecDeque<Key> = VecDeque::new();
    let mut included: HashSet<Key> = HashSet::new();
    let push = |key: Key, included: &mut HashSet<Key>, queue: &mut VecDeque<Key>| {
        if included.insert(key.clone()) {
            queue.push_back(key);
        }
    };
    for seed in &seeds {
        push(seed.clone(), &mut included, &mut queue);
        let docs = docs_of.get(seed).map(Vec::as_slice).unwrap_or_default();
        let recent = docs
            .iter()
            .filter(|(date, _)| since.as_ref().is_none_or(|s| date >= s))
            .take(options.max_docs_per_contact);
        for (_, doc) in recent {
            push(doc.clone(), &mut included, &mut queue);
        }
    }
    if !included.iter().any(|k| k.0.as_document().is_some()) {
        return Err(SliceError::Empty);
    }

    let mut deposits: Vec<Key> = ordered
        .iter()
        .filter(|k| k.0 == EntityType::Deposit)
        .cloned()
        .collect();
    loop {
        while let Some(key) = queue.pop_front() {
            let node = &nodes[&key];
            for reference in &node.refs {
                let Some(target) = resolve(reference, &known) else {
                    continue;
                };
                // Deposits are hubs: never followed into (see module docs).
                if target.0 == EntityType::Deposit && key.0 != EntityType::Deposit {
                    continue;
                }
                push(target, &mut included, &mut queue);
            }
            if key.0.as_document().is_some() {
                for source in incoming.get(&key).into_iter().flatten() {
                    if source.0 != EntityType::Deposit {
                        push(source.clone(), &mut included, &mut queue);
                    }
                }
            }
            if included.len() > options.max_records {
                return Err(SliceError::TooBig {
                    found: included.len(),
                    max: options.max_records,
                });
            }
        }
        // A deposit joins only when everything it links is already in.
        let mut added = false;
        deposits.retain(|deposit| {
            let links: Vec<Key> = nodes[deposit]
                .refs
                .iter()
                .filter(|r| r.linked)
                .filter_map(|r| resolve(r, &known))
                .collect();
            if !links.is_empty() && links.iter().all(|l| included.contains(l)) {
                push(deposit.clone(), &mut included, &mut queue);
                added = true;
                false
            } else {
                true
            }
        });
        if !added {
            break;
        }
    }

    let mut keys: Vec<Key> = included.into_iter().collect();
    sort_keys(&mut keys);
    Ok(Selection {
        keys,
        seed_contacts: seeds,
    })
}

/// Remove every `LinkedTxn` entry naming a record the replica has but the
/// slice does not (by construction, only deposits). Returns how many went.
pub fn prune_links(raw: &mut Value, known: &HashSet<Key>, included: &HashSet<Key>) -> usize {
    let mut pruned = 0;
    match raw {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "LinkedTxn" {
                    if let Value::Array(entries) = value {
                        let before = entries.len();
                        entries.retain(|entry| {
                            let target = entry
                                .get("TxnType")
                                .and_then(Value::as_str)
                                .and_then(txn_type);
                            let id = entry.get("TxnId").and_then(Value::as_str);
                            match (target, id) {
                                (Some(t), Some(id)) => {
                                    let key = (t, id.to_string());
                                    !known.contains(&key) || included.contains(&key)
                                }
                                _ => true,
                            }
                        });
                        pruned += before - entries.len();
                    }
                } else {
                    pruned += prune_links(value, known, included);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                pruned += prune_links(item, known, included);
            }
        }
        _ => {}
    }
    pruned
}

/// Every reference in `entities` that the replica (`known`) could satisfy but
/// the slice does not hold: `(from, to)`. Empty for a closed slice.
pub fn dangling(entities: &[MirroredEntity], known: &HashSet<Key>) -> Vec<(Key, Key)> {
    let held: HashSet<Key> = entities
        .iter()
        .map(|e| (e.entity_type, e.qbo_id.clone()))
        .collect();
    let mut out = Vec::new();
    for entity in entities {
        for reference in summarize(entity.entity_type, &entity.raw_json).refs {
            if let Some(target) = resolve(&reference, known) {
                if !held.contains(&target) {
                    out.push(((entity.entity_type, entity.qbo_id.clone()), target));
                }
            }
        }
    }
    out
}

/// What [`slice_realm`] produced.
#[derive(Debug, Default)]
pub struct SliceReport {
    pub entities: Vec<MirroredEntity>,
    pub seed_contacts: Vec<Key>,
    pub pruned_links: usize,
    /// References to targets the replica does not hold at all (not closable).
    pub unresolvable_refs: usize,
    /// Every customer and vendor name in the whole book, for the scrubber's
    /// deny list: a document in the slice may mention a contact outside it.
    pub deny_names: BTreeSet<String>,
    pub replica_records: usize,
}

impl SliceReport {
    pub fn count(&self, entity_type: EntityType) -> usize {
        self.entities
            .iter()
            .filter(|e| e.entity_type == entity_type)
            .count()
    }
}

/// Slice a realm of the replica. Reads only.
pub fn slice_realm(
    store: &Store,
    realm: &RealmId,
    options: &SliceOptions,
) -> Result<SliceReport, SliceError> {
    let mut nodes: HashMap<Key, Node> = HashMap::new();
    let mut deny_names = BTreeSet::new();
    store.for_each_entity(realm, |entity| {
        if NAME_ENTITY_TYPES.contains(&entity.entity_type) {
            for field in NAME_FIELDS {
                if let Some(name) = entity.raw_json.get(field).and_then(Value::as_str) {
                    if !name.trim().is_empty() {
                        deny_names.insert(name.trim().to_string());
                    }
                }
            }
        }
        // Deleted rows and CompanyInfo (the realm's own record) never join.
        if !entity.is_deleted && entity.entity_type != EntityType::CompanyInfo {
            let node = summarize(entity.entity_type, &entity.raw_json);
            nodes.insert((entity.entity_type, entity.qbo_id), node);
        }
        Ok(())
    })?;

    let selection = select(&nodes, options)?;
    let known: HashSet<Key> = nodes.keys().cloned().collect();
    let included: HashSet<Key> = selection.keys.iter().cloned().collect();

    let mut report = SliceReport {
        seed_contacts: selection.seed_contacts,
        deny_names,
        replica_records: nodes.len(),
        ..SliceReport::default()
    };
    for key in &selection.keys {
        let Some(mut entity) = store.get_entity(realm, key.0, &key.1)? else {
            continue;
        };
        report.pruned_links += prune_links(&mut entity.raw_json, &known, &included);
        report.unresolvable_refs += summarize(entity.entity_type, &entity.raw_json)
            .refs
            .iter()
            .filter(|r| resolve(r, &known).is_none())
            .count();
        report.entities.push(entity);
    }

    let open = dangling(&report.entities, &known);
    if let Some((from, to)) = open.first() {
        return Err(SliceError::NotClosed(
            open.len(),
            format!(
                "{} {} -> {} {}",
                from.0.as_str(),
                from.1,
                to.0.as_str(),
                to.1
            ),
        ));
    }
    Ok(report)
}

/// Record a slice into `dir` exactly the way `qbo-local record` records a
/// live sync — a [`MockQbo`] holding only the slice, swept through a
/// [`RecordingQbo`] — so `qbo-local replay` and the scrubber read it like
/// any other recording. Also writes [`DENY_NAMES_FILE`]. Returns the number
/// of manifest entries.
pub fn record_slice(
    report: &SliceReport,
    realm: &RealmId,
    dir: &Path,
    at: DateTime<Utc>,
) -> Result<usize, SliceError> {
    if dir.exists()
        && fs::read_dir(dir)
            .map_err(|e| SliceError::Io(dir.display().to_string(), e))?
            .next()
            .is_some()
    {
        return Err(SliceError::DirNotEmpty(dir.display().to_string()));
    }
    let mut mock = MockQbo::new(at);
    for entity in &report.entities {
        mock.seed(
            realm,
            EntityPayload {
                entity_type: entity.entity_type,
                qbo_id: entity.qbo_id.clone(),
                sync_token: entity.sync_token.clone(),
                last_updated_utc: entity.last_updated_utc,
                is_deleted: false,
                raw_json: entity.raw_json.clone(),
            },
        );
    }
    let scratch = Store::open_in_memory()?;
    scratch.register_realm(realm, "slice", at)?;
    let mut driver = SyncDriver::new(RecordingQbo::new(mock, dir), SyncOptions::default(), at);
    let entity_types: Vec<EntityType> = EntityType::m0_scope().collect();
    driver
        .sync_realm(&scratch, realm, &entity_types, at)
        .map_err(|e| SliceError::Record(e.to_string()))?;

    let deny_path = dir.join(DENY_NAMES_FILE);
    let mut names = String::new();
    for name in &report.deny_names {
        names.push_str(name);
        names.push('\n');
    }
    fs::write(&deny_path, names).map_err(|e| SliceError::Io(deny_path.display().to_string(), e))?;
    Ok(driver.client_mut().manifest().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(t: EntityType, id: &str) -> Key {
        (t, id.to_string())
    }

    /// Two customers, each with an invoice and a payment; one deposit batching
    /// both payments; a vendor with a bill; masters behind them all.
    fn book() -> HashMap<Key, Node> {
        use EntityType as E;
        let rows: Vec<(E, &str, Value)> = vec![
            (E::Account, "1", json!({"Id": "1"})),
            (
                E::Account,
                "2",
                json!({"Id": "2", "ParentRef": {"value": "1"}}),
            ),
            (
                E::Item,
                "10",
                json!({"Id": "10", "IncomeAccountRef": {"value": "2"}}),
            ),
            (E::Term, "3", json!({"Id": "3"})),
            (
                E::Customer,
                "100",
                json!({"Id": "100", "SalesTermRef": {"value": "3"}}),
            ),
            (E::Customer, "200", json!({"Id": "200"})),
            (E::Vendor, "300", json!({"Id": "300"})),
            (
                E::Invoice,
                "1000",
                json!({"Id": "1000", "TxnDate": "2026-01-01", "CustomerRef": {"value": "100"},
                "Line": [{"SalesItemLineDetail": {"ItemRef": {"value": "10"}, "TaxCodeRef": {"value": "TAX"}}}],
                "LinkedTxn": [{"TxnId": "1001", "TxnType": "Payment"}]}),
            ),
            (
                E::Payment,
                "1001",
                json!({"Id": "1001", "TxnDate": "2026-01-05", "CustomerRef": {"value": "100"},
                "Line": [{"LinkedTxn": [{"TxnId": "1000", "TxnType": "Invoice"}]}],
                "LinkedTxn": [{"TxnId": "5000", "TxnType": "Deposit"}]}),
            ),
            (
                E::Invoice,
                "2000",
                json!({"Id": "2000", "TxnDate": "2026-02-01", "CustomerRef": {"value": "200"}}),
            ),
            (
                E::Payment,
                "2001",
                json!({"Id": "2001", "CustomerRef": {"value": "200"},
                "Line": [{"LinkedTxn": [{"TxnId": "2000", "TxnType": "Invoice"}]}]}),
            ),
            (
                E::Deposit,
                "5000",
                json!({"Id": "5000", "DepositToAccountRef": {"value": "1"},
                "Line": [{"LinkedTxn": [{"TxnId": "1001", "TxnType": "Payment"}]},
                         {"LinkedTxn": [{"TxnId": "2001", "TxnType": "Payment"}]}]}),
            ),
            (
                E::Bill,
                "3000",
                json!({"Id": "3000", "VendorRef": {"value": "300"},
                "Line": [{"AccountBasedExpenseLineDetail": {"AccountRef": {"value": "2"}}}]}),
            ),
            (
                E::JournalEntry,
                "4000",
                json!({"Id": "4000", "Line": [{"JournalEntryLineDetail": {
                "PostingType": "Debit", "AccountRef": {"value": "1"},
                "Entity": {"Type": "Vendor", "EntityRef": {"value": "300"}}}}]}),
            ),
        ];
        rows.into_iter()
            .map(|(t, id, raw)| (key(t, id), summarize(t, &raw)))
            .collect()
    }

    fn options(customers: &[&str], vendors: &[&str]) -> SliceOptions {
        SliceOptions {
            customers: 0,
            vendors: 0,
            customer_ids: customers.iter().map(|s| s.to_string()).collect(),
            vendor_ids: vendors.iter().map(|s| s.to_string()).collect(),
            ..SliceOptions::default()
        }
    }

    #[test]
    fn one_customer_pulls_its_documents_and_their_masters_but_not_the_shared_deposit() {
        let selection = select(&book(), &options(&["100"], &[])).unwrap();
        let keys: HashSet<Key> = selection.keys.into_iter().collect();
        use EntityType as E;
        for expected in [
            key(E::Customer, "100"),
            key(E::Term, "3"),
            key(E::Invoice, "1000"),
            key(E::Payment, "1001"),
            key(E::Item, "10"),
            key(E::Account, "2"),
            key(E::Account, "1"),
        ] {
            assert!(keys.contains(&expected), "missing {expected:?}");
        }
        assert!(
            !keys.contains(&key(E::Deposit, "5000")),
            "a deposit linking an outside payment joined"
        );
        assert!(!keys.contains(&key(E::Customer, "200")));
        assert!(!keys.contains(&key(E::Payment, "2001")));
    }

    #[test]
    fn a_deposit_joins_once_every_payment_it_links_is_in() {
        let selection = select(&book(), &options(&["100", "200"], &[])).unwrap();
        assert!(selection.keys.contains(&key(EntityType::Deposit, "5000")));
    }

    #[test]
    fn a_vendor_pulls_its_bill_and_the_journal_entry_that_names_it() {
        let selection = select(&book(), &options(&[], &["300"])).unwrap();
        assert!(selection.keys.contains(&key(EntityType::Bill, "3000")));
        assert!(selection
            .keys
            .contains(&key(EntityType::JournalEntry, "4000")));
        assert!(!selection.keys.iter().any(|k| k.0 == EntityType::Invoice));
    }

    #[test]
    fn an_invoice_alone_still_pulls_the_payment_applied_to_it() {
        // Seeded through the invoice only (one doc per contact, and the
        // payment undated so the invoice is the newest), with the invoice's
        // own link to the payment gone: only the incoming rule can bring the
        // payment in.
        let mut nodes = book();
        nodes
            .get_mut(&key(EntityType::Invoice, "1000"))
            .unwrap()
            .refs
            .retain(|r| !r.linked);
        nodes
            .get_mut(&key(EntityType::Payment, "1001"))
            .unwrap()
            .txn_date = None;
        let selection = select(
            &nodes,
            &SliceOptions {
                max_docs_per_contact: 1,
                ..options(&["100"], &[])
            },
        )
        .unwrap();
        assert!(selection.keys.contains(&key(EntityType::Payment, "1001")));
        assert!(selection.keys.contains(&key(EntityType::Invoice, "1000")));
    }

    #[test]
    fn picking_is_deterministic_and_seed_dependent() {
        let opts = SliceOptions {
            customers: 1,
            vendors: 0,
            ..SliceOptions::default()
        };
        let a = select(&book(), &opts).unwrap();
        let b = select(&book(), &opts).unwrap();
        assert_eq!(a.keys, b.keys);
        let picks: HashSet<String> = (0..16)
            .map(|n| {
                let opts = SliceOptions {
                    pick_seed: format!("s{n}"),
                    ..opts.clone()
                };
                select(&book(), &opts).unwrap().seed_contacts[0].1.clone()
            })
            .collect();
        assert_eq!(
            picks.len(),
            2,
            "both customers should be picked by some seed"
        );
    }

    #[test]
    fn too_big_and_unknown_are_refused() {
        let opts = SliceOptions {
            max_records: 3,
            ..options(&["100"], &[])
        };
        assert!(matches!(
            select(&book(), &opts),
            Err(SliceError::TooBig { .. })
        ));
        assert!(matches!(
            select(&book(), &options(&["999"], &[])),
            Err(SliceError::UnknownContact(..))
        ));
    }

    #[test]
    fn pruning_drops_only_links_to_records_the_slice_left_out() {
        let known: HashSet<Key> = book().keys().cloned().collect();
        let included: HashSet<Key> = [key(EntityType::Invoice, "1000")].into_iter().collect();
        let mut raw = json!({"LinkedTxn": [
            {"TxnId": "5000", "TxnType": "Deposit"},
            {"TxnId": "1000", "TxnType": "Invoice"},
            {"TxnId": "77", "TxnType": "ReimburseCharge"}
        ]});
        assert_eq!(prune_links(&mut raw, &known, &included), 1);
        assert_eq!(raw["LinkedTxn"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn pseudo_tax_codes_are_not_dangling() {
        let known: HashSet<Key> = book().keys().cloned().collect();
        let entity = MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: "9".into(),
            sync_token: "0".into(),
            last_updated_utc: chrono::Utc::now(),
            is_deleted: false,
            raw_json: json!({"Line": [{"SalesItemLineDetail": {"TaxCodeRef": {"value": "TAX"}}}],
                             "CustomerRef": {"value": "100"}}),
        };
        let open = dangling(&[entity], &known);
        assert_eq!(
            open,
            vec![(
                key(EntityType::Invoice, "9"),
                key(EntityType::Customer, "100")
            )]
        );
    }
}
