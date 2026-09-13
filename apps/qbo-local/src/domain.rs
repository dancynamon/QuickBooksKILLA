//! Realm scoping and entity types. `DESIGN.md` §2.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("realm id must be a non-empty string of digits, got {0:?}")]
    InvalidRealmId(String),
    #[error("unknown entity type {0:?}")]
    UnknownEntityType(String),
}

/// A QuickBooks company (realm) identifier.
///
/// Aquamentor and WaterLine CNC are separate realms that share nothing — separate
/// masters, separate chart of accounts, separate rate-limit budgets. There is no
/// cross-realm join anywhere in this system.
///
/// The inner value is private and construction is validating, so a realm-scoped
/// query cannot be handed a bare string that was never a realm id. That is the
/// first of the three scoping guarantees in `DESIGN.md` §2.1; the other two —
/// the private connection and the realm-first repository signatures — live in
/// [`crate::store`].
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RealmId(String);

impl RealmId {
    /// QBO realm ids are numeric strings — Aquamentor's is a 16-digit value.
    /// Rejecting anything else catches a display name or a file path being
    /// threaded into a scoping parameter by mistake.
    pub fn parse(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DomainError::InvalidRealmId(raw));
        }
        Ok(RealmId(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RealmId {
    type Error = DomainError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        RealmId::parse(value)
    }
}

impl From<RealmId> for String {
    fn from(value: RealmId) -> String {
        value.0
    }
}

impl std::fmt::Display for RealmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A QBO Class — the per-product-line dimension (foam, signs, chairs, CNC work).
///
/// A newtype rather than a bare `String`. The earlier design draft contradicted
/// itself, declaring `Option<String>` on the document structs and `Option<ClassId>`
/// in the prose; this is the resolution.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClassId(String);

impl ClassId {
    pub fn new(id: impl Into<String>) -> Self {
        ClassId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Every entity type mirrored from QBO.
///
/// Wider than [`DocumentType`] on purpose. The earlier draft typed the outbox's
/// entity as `DocumentType`, which cannot represent Customer, Vendor or Item —
/// making the outbox's own canonical dependency case (create a customer, then
/// invoice them) unrepresentable in the type that was supposed to carry it.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum EntityType {
    // Masters
    Account,
    Class,
    Customer,
    Vendor,
    Item,
    TaxCode,
    TaxRate,
    Term,
    Department,
    CompanyInfo,
    Preferences,
    Attachable,
    // Documents
    Estimate,
    Invoice,
    SalesReceipt,
    CreditMemo,
    RefundReceipt,
    Payment,
    PurchaseOrder,
    Bill,
    BillPayment,
    VendorCredit,
    Purchase,
    Deposit,
    JournalEntry,
}

/// The subset of [`EntityType`] that represents a financial document.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum DocumentType {
    Estimate,
    Invoice,
    SalesReceipt,
    CreditMemo,
    RefundReceipt,
    Payment,
    PurchaseOrder,
    Bill,
    BillPayment,
    VendorCredit,
    Purchase,
    Deposit,
    JournalEntry,
}

/// Which sync tier an entity belongs to. `DESIGN.md` §3.1 — history depth is
/// never staged, but breadth is, so a working sync arrives sooner.
///
/// The declaration order is load-bearing: `Ord` follows it, and the sync driver
/// orders a run by tier so masters are mirrored before the documents that
/// display them. Reordering these variants reorders sync.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SyncTier {
    /// Masters. Everything else references these, so they sync first.
    Masters,
    /// Documents.
    Documents,
    /// Peripheral; deferred until after M0.
    Peripheral,
}

impl EntityType {
    /// Every entity type, in the order a full sync should walk them: masters
    /// before documents, because documents reference masters.
    pub const ALL: &'static [EntityType] = &[
        // Tier 1 — masters
        EntityType::CompanyInfo,
        EntityType::Account,
        EntityType::Class,
        EntityType::Customer,
        EntityType::Vendor,
        EntityType::Item,
        EntityType::TaxCode,
        EntityType::TaxRate,
        EntityType::Term,
        // Tier 2 — documents
        EntityType::Estimate,
        EntityType::Invoice,
        EntityType::SalesReceipt,
        EntityType::CreditMemo,
        EntityType::RefundReceipt,
        EntityType::Payment,
        EntityType::PurchaseOrder,
        EntityType::Bill,
        EntityType::BillPayment,
        EntityType::VendorCredit,
        EntityType::Purchase,
        EntityType::Deposit,
        EntityType::JournalEntry,
        // Tier 3 — peripheral
        EntityType::Department,
        EntityType::Preferences,
        EntityType::Attachable,
    ];

    pub const fn tier(self) -> SyncTier {
        use EntityType::*;
        match self {
            CompanyInfo | Account | Class | Customer | Vendor | Item | TaxCode | TaxRate | Term => {
                SyncTier::Masters
            }
            Department | Preferences | Attachable => SyncTier::Peripheral,
            _ => SyncTier::Documents,
        }
    }

    /// Entity types synced in M0 — everything but the peripheral tier.
    pub fn m0_scope() -> impl Iterator<Item = EntityType> {
        EntityType::ALL
            .iter()
            .copied()
            .filter(|e| e.tier() != SyncTier::Peripheral)
    }

    /// `RequestId` reportedly does not provide idempotency for Customer or Item
    /// (`DESIGN.md` §0, §6.5). Those two need a query-before-create guard instead
    /// of relying on the retry being deduplicated by Intuit.
    ///
    /// Returns true when a create of this type must check for an existing record
    /// before issuing. Deliberately conservative: being wrong in this direction
    /// costs one extra query, being wrong in the other costs a duplicate master
    /// record in the book of record.
    pub const fn requires_query_before_create(self) -> bool {
        matches!(self, EntityType::Customer | EntityType::Item)
    }

    /// The QBO API name, also the value stored in the `entity_type` column.
    pub const fn as_str(self) -> &'static str {
        use EntityType::*;
        match self {
            Account => "Account",
            Class => "Class",
            Customer => "Customer",
            Vendor => "Vendor",
            Item => "Item",
            TaxCode => "TaxCode",
            TaxRate => "TaxRate",
            Term => "Term",
            Department => "Department",
            CompanyInfo => "CompanyInfo",
            Preferences => "Preferences",
            Attachable => "Attachable",
            Estimate => "Estimate",
            Invoice => "Invoice",
            SalesReceipt => "SalesReceipt",
            CreditMemo => "CreditMemo",
            RefundReceipt => "RefundReceipt",
            Payment => "Payment",
            PurchaseOrder => "PurchaseOrder",
            Bill => "Bill",
            BillPayment => "BillPayment",
            VendorCredit => "VendorCredit",
            Purchase => "Purchase",
            Deposit => "Deposit",
            JournalEntry => "JournalEntry",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        EntityType::ALL
            .iter()
            .copied()
            .find(|e| e.as_str() == raw)
            .ok_or_else(|| DomainError::UnknownEntityType(raw.to_string()))
    }

    pub const fn as_document(self) -> Option<DocumentType> {
        use EntityType as E;
        Some(match self {
            E::Estimate => DocumentType::Estimate,
            E::Invoice => DocumentType::Invoice,
            E::SalesReceipt => DocumentType::SalesReceipt,
            E::CreditMemo => DocumentType::CreditMemo,
            E::RefundReceipt => DocumentType::RefundReceipt,
            E::Payment => DocumentType::Payment,
            E::PurchaseOrder => DocumentType::PurchaseOrder,
            E::Bill => DocumentType::Bill,
            E::BillPayment => DocumentType::BillPayment,
            E::VendorCredit => DocumentType::VendorCredit,
            E::Purchase => DocumentType::Purchase,
            E::Deposit => DocumentType::Deposit,
            E::JournalEntry => DocumentType::JournalEntry,
            _ => return None,
        })
    }
}

impl DocumentType {
    pub const fn as_entity(self) -> EntityType {
        use DocumentType as D;
        match self {
            D::Estimate => EntityType::Estimate,
            D::Invoice => EntityType::Invoice,
            D::SalesReceipt => EntityType::SalesReceipt,
            D::CreditMemo => EntityType::CreditMemo,
            D::RefundReceipt => EntityType::RefundReceipt,
            D::Payment => EntityType::Payment,
            D::PurchaseOrder => EntityType::PurchaseOrder,
            D::Bill => EntityType::Bill,
            D::BillPayment => EntityType::BillPayment,
            D::VendorCredit => EntityType::VendorCredit,
            D::Purchase => EntityType::Purchase,
            D::Deposit => EntityType::Deposit,
            D::JournalEntry => EntityType::JournalEntry,
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.as_entity().as_str()
    }
}

/// Which side of the book a contact sits on.
///
/// Customers and vendors have separate id spaces in QBO, so an id alone does
/// not identify a contact — this travels with it everywhere.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum ContactType {
    Customer,
    Vendor,
}

impl ContactType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ContactType::Customer => "Customer",
            ContactType::Vendor => "Vendor",
        }
    }

    /// Employees and other `EntityRef` targets are deliberately not contacts:
    /// they are neither a customer nor a vendor, and saying so returns `None`
    /// rather than picking the nearer of two wrong answers.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "Customer" => Some(ContactType::Customer),
            "Vendor" => Some(ContactType::Vendor),
            _ => None,
        }
    }

    pub const fn as_entity(self) -> EntityType {
        match self {
            ContactType::Customer => EntityType::Customer,
            ContactType::Vendor => EntityType::Vendor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_id_accepts_a_realm_shaped_string() {
        // A placeholder of the right shape. Real realm ids do not go in the
        // repository — HANDOFF.md §2.6; read yours from the QBO deep link.
        let realm = RealmId::parse("1234567890123456").unwrap();
        assert_eq!(realm.as_str(), "1234567890123456");
    }

    #[test]
    fn realm_id_rejects_things_that_are_not_realm_ids() {
        for bad in [
            "",
            "Aquamentor",
            "1234567890123456 ",
            "/tmp/replica.db",
            "-1",
        ] {
            assert!(RealmId::parse(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn realm_id_round_trips_through_serde() {
        let realm = RealmId::parse("1234567890123456").unwrap();
        let json = serde_json::to_string(&realm).unwrap();
        assert_eq!(json, "\"1234567890123456\"");
        assert_eq!(serde_json::from_str::<RealmId>(&json).unwrap(), realm);
    }

    #[test]
    fn realm_id_deserialisation_validates() {
        // A realm id arriving from a config file gets the same check as one
        // constructed in code.
        assert!(serde_json::from_str::<RealmId>("\"not-a-realm\"").is_err());
    }

    #[test]
    fn entity_type_names_round_trip() {
        for &entity in EntityType::ALL {
            assert_eq!(EntityType::parse(entity.as_str()).unwrap(), entity);
        }
    }

    #[test]
    fn entity_type_names_are_unique() {
        let mut names: Vec<&str> = EntityType::ALL.iter().map(|e| e.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate entity type name");
    }

    #[test]
    fn every_document_type_round_trips_through_entity_type() {
        // The property that makes the widened enum safe: no document type is
        // lost when it passes through the outbox as an EntityType.
        let documents = [
            DocumentType::Estimate,
            DocumentType::Invoice,
            DocumentType::SalesReceipt,
            DocumentType::CreditMemo,
            DocumentType::RefundReceipt,
            DocumentType::Payment,
            DocumentType::PurchaseOrder,
            DocumentType::Bill,
            DocumentType::BillPayment,
            DocumentType::VendorCredit,
            DocumentType::Purchase,
            DocumentType::Deposit,
            DocumentType::JournalEntry,
        ];
        for doc in documents {
            assert_eq!(doc.as_entity().as_document(), Some(doc));
        }
    }

    #[test]
    fn masters_are_not_documents() {
        for entity in [
            EntityType::Customer,
            EntityType::Vendor,
            EntityType::Item,
            EntityType::Account,
            EntityType::Class,
        ] {
            assert_eq!(entity.as_document(), None);
            assert_eq!(entity.tier(), SyncTier::Masters);
        }
    }

    #[test]
    fn masters_sync_before_documents() {
        // Documents reference masters, so a full sync must walk them in this
        // order or every document lands with dangling references.
        let tiers: Vec<SyncTier> = EntityType::ALL.iter().map(|e| e.tier()).collect();
        let last_master = tiers.iter().rposition(|t| *t == SyncTier::Masters).unwrap();
        let first_document = tiers
            .iter()
            .position(|t| *t == SyncTier::Documents)
            .unwrap();
        assert!(last_master < first_document);
    }

    #[test]
    fn m0_scope_excludes_only_peripheral_entities() {
        let scope: Vec<EntityType> = EntityType::m0_scope().collect();
        assert_eq!(scope.len(), EntityType::ALL.len() - 3);
        assert!(!scope.contains(&EntityType::Attachable));
        assert!(!scope.contains(&EntityType::Preferences));
        assert!(!scope.contains(&EntityType::Department));
        assert!(scope.contains(&EntityType::Invoice));
        assert!(scope.contains(&EntityType::Customer));
    }

    #[test]
    fn customer_and_item_need_a_query_before_create_guard() {
        // The RequestId idempotency gap, DESIGN.md §6.5.
        assert!(EntityType::Customer.requires_query_before_create());
        assert!(EntityType::Item.requires_query_before_create());
        assert!(!EntityType::Invoice.requires_query_before_create());
        assert!(!EntityType::Bill.requires_query_before_create());
    }
}
