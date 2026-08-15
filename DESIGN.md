# DESIGN.md — ledger-core shared crate

Scope: the money type, domain enums, QBO entity models, rounding policy, class
dimension, and sync-forward types shared by `ledger` (double-entry GL,
long-term QBO replacement) and `qbo-local` (fast local mirror + outbox,
near-term). Both consume this crate as a path dependency in the
`ledger-workspace` monorepo — this answers qbo-local prompt §14 Q5: monorepo,
not a git dependency, because the two projects evolve in lockstep during M0-M3
and a git dependency adds a version-pinning tax with no payoff at this stage.

No I/O in this crate. No SQLite. No Tauri. Pure types and pure functions only.

---

## 1. Money type

```rust
/// Signed integer amount in minor units (cents). Never a float, anywhere.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Money(i64);

impl Money {
    pub const ZERO: Money = Money(0);

    pub fn from_minor(minor: i64) -> Self { Money(minor) }
    pub fn minor(self) -> i64 { self.0 }

    pub fn checked_add(self, rhs: Money) -> Option<Money> {
        self.0.checked_add(rhs.0).map(Money)
    }
    pub fn checked_sub(self, rhs: Money) -> Option<Money> {
        self.0.checked_sub(rhs.0).map(Money)
    }
    pub fn is_zero(self) -> bool { self.0 == 0 }
}
```

- Backed by `i64` minor units. At cent granularity `i64` covers ~±92 quadrillion
  dollars — no overflow risk for either business. All arithmetic goes through
  `checked_add`/`checked_sub`; a checked-arithmetic failure is a panic in debug
  and a hard error in release, never a silent wrap.
- `rust_decimal::Decimal` is used for anything that is *not* a posted ledger
  amount: unit rates, quantities, tax percentages, board-foot costs. These get
  converted to `Money` exactly once, at the point a line is extended, via the
  rounding policy below. `Decimal` never touches `journal_lines`.
- Rejected alternative: `f64` everywhere with rounding at display time. Rejected
  because float drift in an accumulator (e.g. summing 500 invoice lines) is
  exactly the "silently corrupts a balance" failure mode §2.2 of the ledger
  prompt calls catastrophic.

## 2. Rounding policy

Named, not scattered:

```rust
pub enum RoundingPolicy {
    /// Line extension (qty * rate -> Money): round half-up to the cent.
    LineExtension,
    /// Tax calculation (taxable subtotal * rate -> Money): banker's rounding
    /// (round-half-to-even), matching how most US sales-tax engines and QBO
    /// itself round to avoid systematic upward bias across many small lines.
    TaxCalculation,
}

pub fn round_money(amount: Decimal, policy: RoundingPolicy) -> Money {
    let rounded = match policy {
        RoundingPolicy::LineExtension => amount.round_dp_with_strategy(
            2, RoundingStrategy::MidpointAwayFromZero),
        RoundingPolicy::TaxCalculation => amount.round_dp_with_strategy(
            2, RoundingStrategy::MidpointNearestEven),
    };
    Money::from_minor((rounded * Decimal::ONE_HUNDRED).to_i64().unwrap())
}
```

This is the *only* function in the codebase permitted to convert `Decimal` to
`Money`. Enforced by convention now; a lint/CI grep for raw `Decimal -> Money`
casts outside this module is a cheap M0 addition.

## 3. Domain enums

Shared between both projects so a document type or account type means the same
thing whether it originates in the QBO mirror or the native ledger.

```rust
pub enum AccountType {
    Asset, Liability, Equity, Income, Cogs, Expense,
}

pub enum AccountSubtype {
    // Asset
    Bank, AccountsReceivable, OtherCurrentAsset, FixedAsset, Inventory,
    // Liability
    AccountsPayable, CreditCard, OtherCurrentLiability, LongTermLiability,
    // Equity
    RetainedEarnings, OwnerEquity,
    // Income / COGS / Expense
    SalesOfProductIncome, ServiceFeeIncome, CostOfGoodsSold,
    OperatingExpense, PayrollExpense, ShippingExpense,
    // extend as QBO's detail-type taxonomy demands; keep 1:1 mappable to
    // QBO AccountSubType strings for clean export/import.
}

pub enum NormalBalance { Debit, Credit }

pub enum DocumentType {
    Estimate, SalesOrder, Invoice, Payment, Deposit, CreditMemo,
    PurchaseOrder, Bill, BillPayment, VendorCredit, RefundReceipt,
    JournalEntry, SalesReceipt,
}

pub enum ItemType {
    Inventory, NonInventory, Service, Assembly, Discount,
}

pub enum TaxTreatment {
    Taxable, ExemptResale, ExemptOutOfState, ExemptNonprofit, ExemptOther,
}

pub enum InventoryCostingMethod {
    WeightedAverage,
    Fifo, // implemented behind a trait per ledger prompt §4; not built yet
}
```

`AccountType` fixes `NormalBalance` (Asset/Expense/Cogs = Debit,
Liability/Equity/Income = Credit) via a pure function, not a stored column —
one less place for the DB and the type to disagree.

## 4. QBO entity models

One struct per mirrored entity (qbo-local prompt §5's list), each carrying the
full raw payload alongside parsed fields used for search/sort/filter/posting:

```rust
pub struct QboMeta {
    pub qbo_id: String,
    pub sync_token: String,
    pub last_updated_utc: DateTime<Utc>,
    pub realm_id: String,
    pub raw_json: serde_json::Value,
}

pub struct QboCustomer {
    pub meta: QboMeta,
    pub display_name: String,
    pub company_name: Option<String>,
    pub email: Option<String>,
    pub balance: Money,
    pub tax_treatment: TaxTreatment,
    pub is_active: bool,
}

pub struct QboItem {
    pub meta: QboMeta,
    pub name: String,
    pub sku: Option<String>,
    pub item_type: ItemType,
    pub unit_price: Option<Decimal>,
    pub income_account_ref: Option<String>,
    pub expense_account_ref: Option<String>,
    pub taxable: bool,
}

pub struct QboDocumentLine {
    pub line_no: i32,
    pub item_ref: Option<String>,
    pub description: Option<String>,
    pub qty: Option<Decimal>,
    pub unit_price: Option<Decimal>,
    pub amount: Money,
    pub class_id: Option<String>,   // see §5
    pub tax_treatment: TaxTreatment,
}

pub struct QboDocument {
    pub meta: QboMeta,
    pub doc_type: DocumentType,
    pub doc_number: Option<String>,
    pub txn_date: NaiveDate,
    pub customer_or_vendor_ref: Option<String>,
    pub class_id: Option<String>,   // header-level default
    pub lines: Vec<QboDocumentLine>,
    pub total: Money,
    pub balance: Option<Money>,
}
```

`QboDocument` covers Estimate/SalesOrder/Invoice/CreditMemo/RefundReceipt/
PurchaseOrder/Bill/JournalEntry/SalesReceipt uniformly; `doc_type` disambiguates.
Payment/BillPayment/Deposit have their own shape (applied-to lines against a
document id + amount, not qty/rate lines) — `QboPaymentApplication` is a
follow-on struct, not sketched here, deferred until M2 needs it.

`Account`, `Vendor`, `TaxCode`, `TaxRate`, `Term`, `Class`, `Department`,
`Attachable`, `CompanyInfo`, `Preferences` follow the same `QboMeta`-plus-parsed
-fields shape; omitted here for length, same pattern.

## 5. Class dimension

`Class` is a real type, not a raw-JSON afterthought, per both prompts:

```rust
pub struct ClassId(pub String); // QBO Class.Id when mirrored; local UUID for
                                 // native ledger classes not yet synced to QBO
```

Every `QboDocumentLine` and `QboDocument` carries `class_id: Option<ClassId>`.
The native ledger's `journal_lines.class_id` (ledger prompt §4) uses the same
type. This is the one column that must never be `NULL`-by-omission when a
document has one — both projects' write paths set it from the item's default
class if the line doesn't override.

## 6. Sync-forward types

Two different mechanisms for two different projects, both typed here so the
shape is shared even though the machinery lives in each app:

```rust
/// ledger project (§6 of the ledger prompt): every mutation is a command.
pub struct CommandId(pub Uuid); // UUIDv7
pub struct Command {
    pub id: CommandId,
    pub actor_id: String,
    pub hlc_timestamp: HlcTimestamp,
    pub payload: CommandPayload, // enum, one variant per mutation type
}

/// qbo-local project (§6 of the qbo-local prompt): every mutation is an
/// outbox record with an explicit state machine.
pub struct OutboxId(pub Uuid); // UUIDv7
pub enum OutboxState { Pending, InFlight, Applied, Conflicted, Rejected }
pub struct OutboxRecord {
    pub id: OutboxId,
    pub realm_id: String,
    pub entity_type: DocumentType, // reuses the enum from §3
    pub operation: OutboxOperation, // Create | Update | Delete-not-in-v1
    pub payload_json: serde_json::Value,
    pub local_entity_id: String,
    pub base_sync_token: Option<String>,
    pub state: OutboxState,
    pub attempts: u32,
    pub last_error: Option<String>,
}
```

`Command` and `OutboxRecord` are structurally similar (id, actor/realm, payload,
ordering key) by design — if `ledger` ever needs to sync with a second device,
the outbox state machine `qbo-local` builds first is the proven model to copy,
not a fresh design. Neither is implemented yet; both are typed now so neither
project's schema forecloses the other's future.

---

## 7. Open questions — ledger prompt §13a Q1-3, with recommendation

1. **Fiscal year start / prior-year lock.** Recommend: fiscal year = calendar
   year for both entities (confirm with Dan — no evidence either runs a
   non-calendar FY). Locked periods *reject* posting outright, not warn — a
   warning that can be clicked through is not a control. A locked period can
   be reopened only by an explicit unlock action that itself posts an audit
   entry. Needs Dan's confirmation on FY start; not guessed further here.

2. **Starting class taxonomy**, proposed for Dan to correct:
   - Aquamentor: `Foam Products`, `Signs`, `Lifeguard Chairs`, `Dropship/Resale`
   - WaterLine CNC: `CNC Cutting`, `UV Printing`
   Flat list, no parent/child nesting for v1 — `classes.parent_id` exists in
   the schema for later but a flat list is enough to answer "what's my margin
   on foam" without forcing a hierarchy decision before any real data exists.

3. **Cash-basis reconciliation precision against QBO.** Recommend: "materially
   equal" (reconciles to the penny on totals, documented known-divergence list
   for edge cases like partial-payment date attribution) rather than
   bit-for-bit identical. QBO's own cash-basis engine has undocumented edge
   cases around partial payments and credit memo application order; chasing
   exact parity there is unbounded effort for a basis my CPA uses as a
   secondary view, not the book of record. Flag any real divergence found
   during the M4 parallel run rather than pre-solving hypothetical ones now.

## 8. Open questions — qbo-local prompt §14 Q1-4, with recommendation

1. **CDC coverage.** Not yet verified against current Intuit docs (open item,
   do before M0 write path). Recommend building the fallback full-sweep for
   every entity in §5's list from day one regardless of CDC coverage, so
   "CDC doesn't cover entity X" is a non-event instead of a gap discovered in
   production.

2. **Offline longer than CDC lookback window.** Recommend: on reconnect, check
   `last_cdc_cursor` age; if older than (lookback window − safety margin,
   e.g. 25 of 30 days), skip CDC and run the full-sweep reconciliation (§7 of
   the qbo-local prompt) instead of attempting a CDC call that will silently
   miss changes. Report wall-clock time actually taken, per the prompt's own
   instruction not to assert performance.

3. **SyncToken conflict: last-writer-wins vs. always surface.** Recommend:
   **always surface to me.** This system exists because QBO's book of record
   status is real — a silent last-writer-wins on a stale token can quietly
   discard a change made in the QBO web UI (e.g. by John, or by Dan on his
   phone). Surfacing every conflict costs a glance at the failure inbox (§6.4);
   silently resolving one costs a dropped edit nobody notices until it matters.

4. **QBO writes that cannot be made idempotent with RequestId.** Not yet
   verified against current docs — flag as an M0 research task before the
   write path (M2) is built, not a design decision to make blind.

---

## 9. What this doc does not cover

Full SQLite schema (`ledger-db`), the posting-rules table, the Tauri command
surface, and the outbox worker implementation are each their own design pass
inside the respective app once this shared-crate design is approved. This doc
is scoped to what both projects import.
