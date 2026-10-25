//! Finding things. `DESIGN.md` §3.3.
//!
//! The brief this was built to is specific: typing a number should land on the
//! transaction, immediately. So this is not one ranked full-text query — it is
//! an ordered set of attempts, exact identifiers first, text last. A document
//! number is an answer; a fuzzy match on a memo containing the same digits is a
//! guess, and guesses never outrank answers here.
//!
//! This module sits under `store` rather than beside it so that the connection
//! stays private to the store's module tree (§2.1).

use std::str::FromStr;

use ledger_core::Money;
use rusqlite::params;
use rust_decimal::Decimal;

use super::{document_row, serialize_money_opt, DocumentRow, Store, StoreError, DOCUMENT_SELECT};
use crate::domain::{ContactType, RealmId};

/// Why a row matched. The declaration order **is** the ranking — `derive(Ord)`
/// on a fieldless enum orders by variant position, so adding a reason in the
/// wrong place silently reorders results. Add at the end unless you mean to.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize)]
pub enum MatchReason {
    /// The query is this document's number, exactly.
    DocumentNumber,
    /// The query is the customer PO number carried on this document.
    PurchaseOrderNumber,
    /// The query is this item's SKU, exactly.
    Sku,
    /// The query reads as an amount and this document's total or balance is it.
    Amount,
    /// The query is the start of this document's number.
    DocumentNumberPrefix,
    /// Full-text: names, memos, line descriptions.
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind")]
pub enum Hit {
    Document(Box<DocumentRow>),
    Contact(ContactHit),
    Item(ItemHit),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SearchHit {
    pub reason: MatchReason,
    pub hit: Hit,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ContactHit {
    pub contact_type: ContactType,
    pub qbo_id: String,
    pub display_name: String,
    pub company_name: Option<String>,
    #[serde(serialize_with = "serialize_money_opt")]
    pub balance: Option<Money>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ItemHit {
    pub qbo_id: String,
    pub name: String,
    pub sku: Option<String>,
    pub item_type: Option<String>,
    pub unit_price: Option<String>,
}

impl SearchHit {
    /// Identity for de-duplication: the same document reached by two routes is
    /// one result, keeping the better reason.
    fn key(&self) -> (u8, String, String) {
        match &self.hit {
            Hit::Document(document) => (0, String::new(), document.qbo_id.clone()),
            Hit::Contact(contact) => (
                1,
                contact.contact_type.as_str().to_string(),
                contact.qbo_id.clone(),
            ),
            Hit::Item(item) => (2, String::new(), item.qbo_id.clone()),
        }
    }
}

impl Store {
    /// Search a realm. Results come back best-reason-first, de-duplicated,
    /// capped at `limit`.
    ///
    /// Each stage runs only while there is room left, so the common case — the
    /// user typed a document number — touches one index and stops.
    pub fn search(
        &self,
        realm: &RealmId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut found = Collector::new(limit);

        self.documents_where(
            realm,
            "d.doc_number = ?2 COLLATE NOCASE",
            query,
            MatchReason::DocumentNumber,
            &mut found,
        )?;

        if !found.is_full() {
            self.documents_where(
                realm,
                "d.po_number = ?2 COLLATE NOCASE",
                query,
                MatchReason::PurchaseOrderNumber,
                &mut found,
            )?;
        }

        if !found.is_full() {
            self.items_by_sku(realm, query, &mut found)?;
        }

        if !found.is_full() {
            if let Some(amount) = as_amount(query) {
                self.documents_by_amount(realm, amount, &mut found)?;
            }
        }

        if !found.is_full() {
            self.documents_where(
                realm,
                "d.doc_number LIKE ?2 || '%' COLLATE NOCASE",
                query,
                MatchReason::DocumentNumberPrefix,
                &mut found,
            )?;
        }

        if !found.is_full() {
            if let Some(expression) = fts_expression(query) {
                self.contacts_matching(realm, &expression, &mut found)?;
                if !found.is_full() {
                    self.items_matching(realm, &expression, &mut found)?;
                }
                if !found.is_full() {
                    self.documents_matching(realm, &expression, &mut found)?;
                }
                if !found.is_full() {
                    self.documents_by_line_text(realm, &expression, &mut found)?;
                }
            }
        }

        Ok(found.into_sorted())
    }

    fn documents_where(
        &self,
        realm: &RealmId,
        predicate: &str,
        query: &str,
        reason: MatchReason,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.is_deleted = 0 AND {predicate}
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), query, found.room() as i64],
            document_row,
        )?;
        for row in rows {
            found.push(reason, Hit::Document(Box::new(row?)));
        }
        Ok(())
    }

    fn documents_by_amount(
        &self,
        realm: &RealmId,
        amount: Money,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.is_deleted = 0
               AND (d.total_minor = ?2 OR d.balance_minor = ?2)
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), amount.minor(), found.room() as i64],
            document_row,
        )?;
        for row in rows {
            found.push(MatchReason::Amount, Hit::Document(Box::new(row?)));
        }
        Ok(())
    }

    fn items_by_sku(
        &self,
        realm: &RealmId,
        query: &str,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, name, sku, item_type, unit_price
             FROM items
             WHERE realm_id = ?1 AND is_deleted = 0 AND sku = ?2 COLLATE NOCASE
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![realm.as_str(), query, found.room() as i64],
            item_hit,
        )?;
        for row in rows {
            found.push(MatchReason::Sku, Hit::Item(row?));
        }
        Ok(())
    }

    fn contacts_matching(
        &self,
        realm: &RealmId,
        expression: &str,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT c.contact_type, c.qbo_id, c.display_name, c.company_name, c.balance_minor
             FROM contacts_fts f
             JOIN contacts c ON c.rowid = f.rowid
             WHERE contacts_fts MATCH ?2 AND c.realm_id = ?1 AND c.is_deleted = 0
             ORDER BY bm25(contacts_fts)
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![realm.as_str(), expression, found.room() as i64],
            |row| {
                let contact_type: String = row.get(0)?;
                Ok(ContactHit {
                    // Only the projector writes this column, and it writes a
                    // typed value. Anything else means the file was edited
                    // outside the app; say so rather than defaulting to one
                    // side of the book and quietly filing a vendor as a customer.
                    contact_type: ContactType::parse(&contact_type).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(crate::domain::DomainError::UnknownEntityType(contact_type)),
                        )
                    })?,
                    qbo_id: row.get(1)?,
                    display_name: row.get(2)?,
                    company_name: row.get(3)?,
                    balance: row.get::<_, Option<i64>>(4)?.map(Money::from_minor),
                })
            },
        )?;
        for row in rows {
            found.push(MatchReason::Text, Hit::Contact(row?));
        }
        Ok(())
    }

    fn items_matching(
        &self,
        realm: &RealmId,
        expression: &str,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT i.qbo_id, i.name, i.sku, i.item_type, i.unit_price
             FROM items_fts f
             JOIN items i ON i.rowid = f.rowid
             WHERE items_fts MATCH ?2 AND i.realm_id = ?1 AND i.is_deleted = 0
             ORDER BY bm25(items_fts)
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![realm.as_str(), expression, found.room() as i64],
            item_hit,
        )?;
        for row in rows {
            found.push(MatchReason::Text, Hit::Item(row?));
        }
        Ok(())
    }

    fn documents_matching(
        &self,
        realm: &RealmId,
        expression: &str,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             JOIN documents_fts f ON f.rowid = d.rowid
             WHERE documents_fts MATCH ?2 AND d.realm_id = ?1 AND d.is_deleted = 0
             ORDER BY bm25(documents_fts)
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), expression, found.room() as i64],
            document_row,
        )?;
        for row in rows {
            found.push(MatchReason::Text, Hit::Document(Box::new(row?)));
        }
        Ok(())
    }

    /// Line descriptions are where "that blue foam job" actually lives, so a
    /// text match on a line returns the document it belongs to.
    fn documents_by_line_text(
        &self,
        realm: &RealmId,
        expression: &str,
        found: &mut Collector,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.is_deleted = 0 AND d.qbo_id IN (
                 SELECT l.doc_qbo_id
                 FROM lines_fts f
                 JOIN document_lines l ON l.rowid = f.rowid
                 WHERE lines_fts MATCH ?2 AND l.realm_id = ?1
             )
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), expression, found.room() as i64],
            document_row,
        )?;
        for row in rows {
            found.push(MatchReason::Text, Hit::Document(Box::new(row?)));
        }
        Ok(())
    }
}

fn item_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemHit> {
    Ok(ItemHit {
        qbo_id: row.get(0)?,
        name: row.get(1)?,
        sku: row.get(2)?,
        item_type: row.get(3)?,
        unit_price: row.get(4)?,
    })
}

/// Accumulates hits, keeps the best reason per row, and knows when to stop.
struct Collector {
    limit: usize,
    hits: Vec<SearchHit>,
}

impl Collector {
    fn new(limit: usize) -> Self {
        Collector {
            limit,
            hits: Vec::new(),
        }
    }

    fn push(&mut self, reason: MatchReason, hit: Hit) {
        let candidate = SearchHit { reason, hit };
        let key = candidate.key();
        if let Some(existing) = self.hits.iter_mut().find(|held| held.key() == key) {
            // Stages run best-first, so an existing entry already holds the
            // better reason. Keeping it is the point of de-duplicating at all.
            if candidate.reason < existing.reason {
                existing.reason = candidate.reason;
            }
            return;
        }
        if self.hits.len() < self.limit {
            self.hits.push(candidate);
        }
    }

    fn is_full(&self) -> bool {
        self.hits.len() >= self.limit
    }

    fn room(&self) -> usize {
        self.limit.saturating_sub(self.hits.len())
    }

    fn into_sorted(mut self) -> Vec<SearchHit> {
        self.hits.sort_by_key(|hit| hit.reason);
        self.hits
    }
}

/// Read a query as a money amount, or decline to.
///
/// A bare run of digits is deliberately **not** an amount: `21234` is far more
/// likely to be an invoice number than $21,234.00, and treating it as both
/// fills the list with noise. Something in the query has to say "money" — a
/// currency symbol, a thousands separator, or a decimal point.
fn as_amount(query: &str) -> Option<Money> {
    if !query.contains(['$', ',', '.']) {
        return None;
    }
    let cleaned: String = query
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let decimal = Decimal::from_str(&cleaned).ok()?;
    if decimal.scale() > 2 {
        return None;
    }
    ledger_core::round_money(decimal, ledger_core::RoundingPolicy::MirroredAmount).ok()
}

/// Build an FTS5 MATCH expression from arbitrary user input.
///
/// The query is never interpolated as FTS syntax. Each alphanumeric run is
/// quoted, which makes operators, quotes and stray punctuation inert; the last
/// token gets `*` so that a half-typed word still matches. Returns `None` when
/// nothing survives, so an all-punctuation query runs no query at all rather
/// than a malformed one.
fn fts_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect();

    let (last, rest) = tokens.split_last()?;
    let mut expression = rest.join(" ");
    if !expression.is_empty() {
        expression.push(' ');
    }
    expression.push_str(last);
    expression.push('*');
    Some(expression)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_a_reference_not_an_amount() {
        assert_eq!(as_amount("21234"), None);
    }

    #[test]
    fn something_has_to_say_money() {
        assert_eq!(as_amount("1234.56"), Some(Money::from_minor(123_456)));
        assert_eq!(as_amount("$1234"), Some(Money::from_minor(123_400)));
        assert_eq!(as_amount("1,234"), Some(Money::from_minor(123_400)));
    }

    #[test]
    fn sub_cent_precision_is_not_an_amount_we_will_guess_at() {
        assert_eq!(as_amount("1.005"), None);
    }

    #[test]
    fn fts_syntax_in_the_query_is_inert() {
        assert_eq!(
            fts_expression("blue harbor").as_deref(),
            Some("\"blue\" \"harbor\"*")
        );
        // `OR`, `NEAR`, quotes and a trailing `*` are all just text here.
        assert_eq!(
            fts_expression("foam OR \"tube\"*").as_deref(),
            Some("\"foam\" \"OR\" \"tube\"*")
        );
    }

    #[test]
    fn a_query_with_nothing_to_match_produces_no_expression() {
        assert_eq!(fts_expression("   ---   "), None);
        assert_eq!(fts_expression(""), None);
    }
}
