//! Authorize.net settlement import. `LEDGER-DESIGN.md` §1 (the Settlement
//! and Deposit rows), §10 (settlement matching); `DECISIONS.md` D15
//! (customer payments move to Authorize.net at cutover) and D17 (bank and
//! card feeds are the statements the bank already generates, no
//! aggregator — Authorize.net's own Merchant Interface export plays the same
//! role here that a bank CSV plays in [`crate::bank`]).
//!
//! [`csv`] reads the Merchant Interface's "Settled Batch" / "Transaction
//! Detail" export into [`csv::TransactionRow`]s; [`group_batches`] turns
//! those into one [`SettlementBatch`] per `Batch ID`; [`settlement_documents`]
//! turns each batch into the [`crate::types::LedgerDocument`]s
//! [`crate::post::post`] already knows how to post — one `Settlement`
//! document per batch, one `Payment` per settled sale (applied to the
//! invoice it names, when one resolves), one `RefundReceipt` per settled
//! refund.
//!
//! **Why the fee is `Option`, filled in later, never by this module.** The
//! Merchant Interface's transaction export has no per-batch fee column:
//! Authorize.net bills its gateway fee monthly (a separate expense bill,
//! `docs/AUTHNET.md`), and the processor's actual bank payout is net of a
//! per-transaction discount fee this export never states either. So
//! [`SettlementBatch::fee`] starts `None` and [`settlement_documents`] posts
//! the batch's net *before* fees — `Dr` the bank, `Cr` 1160 clearing, both at
//! `net_before_fees` — which is deliberately optimistic (it assumes the
//! whole batch reached the bank) but is also exactly right about the one
//! thing it can be sure of: 1160 nets to zero the moment this entry posts,
//! against the `Payment`/`RefundReceipt` documents' own debits and credits to
//! 1160 for the same batch. Only when the real bank statement shows a
//! smaller deposit does the discount fee become knowable, and only
//! [`crate::bank`]'s fee-aware settlement match (§10) ever supplies one —
//! by correcting the *bank* leg this module guessed at, not 1160, which was
//! already right. See `crate::bank::Ledger::try_settlement_fee_match`'s doc
//! comment for the arithmetic.
//!
//! **Why a sale's `Payment` debits 1160, not 1150.** `LEDGER-DESIGN.md` §1's
//! own note on the Authorize.net settlement row is explicit: "the customer
//! payment already debited 1160 when it settled." A settled sale in this
//! export *is* that payment, so [`AuthnetConfig::clearing`] is what
//! `deposit_to` names, not [`AuthnetConfig::undeposited`] — using 1150 there
//! instead would leave 1160 permanently unbalanced by every batch's gross,
//! which is precisely the "nets to zero" property `chart.rs` documents for
//! it. `undeposited` is kept on the config regardless, matching
//! `PostingConfig`'s own shape, for a caller wiring the posting context this
//! module's documents go through (`ledger authnet import`, `main.rs`).

pub mod csv;

pub use csv::{parse_transactions, ParseError, TransactionRow, TransactionStatus};

use std::collections::HashMap;

use chrono::NaiveDate;

use ledger_core::{Money, MoneyError};

use crate::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, LedgerDocument,
    LineKind,
};

/// Whether a batch transaction moved money in (a settled sale) or out (a
/// settled refund). Drives which document [`settlement_documents`] builds
/// for it — a `Payment` or a `RefundReceipt`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TxnKind {
    Sale,
    Refund,
}

/// One settled transaction inside a batch, everything [`settlement_documents`]
/// needs to build its `Payment`/`RefundReceipt`. `amount` is always
/// non-negative (`csv::parse_transactions`'s own normalisation); `kind` is
/// the sign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthnetTxn {
    pub txn_id: String,
    pub invoice_number: Option<String>,
    pub customer_id: Option<String>,
    pub amount: Money,
    pub kind: TxnKind,
}

/// One `Batch ID`'s worth of settled activity. `gross` is the sum of settled
/// sales, `refunds` the sum of settled refunds, and `net_before_fees =
/// gross - refunds` — the amount [`settlement_documents`] books as the
/// batch's movement out of 1160 clearing, since that is the one figure this
/// export actually states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementBatch {
    pub batch_id: String,
    pub settled_on: NaiveDate,
    pub gross: Money,
    pub refunds: Money,
    pub net_before_fees: Money,
    pub transactions: Vec<AuthnetTxn>,
    /// The processor's discount fee actually taken out of the payout. Always
    /// `None` straight out of [`group_batches`] — see the module doc — and
    /// filled in only by `crate::bank`'s fee-aware settlement match, which
    /// never mutates a `SettlementBatch` in place; it posts a separate
    /// correcting entry instead. This field exists so a caller who *does*
    /// already know a batch's fee (a manual reconciliation, a test) has
    /// somewhere to put it before calling [`settlement_documents`].
    pub fee: Option<Money>,
}

/// Groups parsed rows into one [`SettlementBatch`] per `Batch ID`, in the
/// order each batch id first appears. `Voided` and `Declined` rows carry no
/// settled money and are dropped here (they still parsed, in
/// [`csv::parse_transactions`] — dropping them is this function's job, not
/// the parser's, so a malformed voided row still fails loudly on read).
pub fn group_batches(rows: &[TransactionRow]) -> Result<Vec<SettlementBatch>, MoneyError> {
    let mut order: Vec<String> = Vec::new();
    let mut batches: HashMap<String, SettlementBatch> = HashMap::new();

    for row in rows {
        let kind = match row.status {
            TransactionStatus::SettledSuccessfully => TxnKind::Sale,
            TransactionStatus::RefundSettledSuccessfully => TxnKind::Refund,
            TransactionStatus::Voided | TransactionStatus::Declined => continue,
        };

        let batch = batches.entry(row.batch_id.clone()).or_insert_with(|| {
            order.push(row.batch_id.clone());
            SettlementBatch {
                batch_id: row.batch_id.clone(),
                settled_on: row.settlement_date,
                gross: Money::ZERO,
                refunds: Money::ZERO,
                net_before_fees: Money::ZERO,
                transactions: Vec::new(),
                fee: None,
            }
        });

        match kind {
            TxnKind::Sale => batch.gross = batch.gross.checked_add(row.amount)?,
            TxnKind::Refund => batch.refunds = batch.refunds.checked_add(row.amount)?,
        }
        batch.transactions.push(AuthnetTxn {
            txn_id: row.transaction_id.clone(),
            invoice_number: row.invoice_number.clone(),
            customer_id: row.customer_id.clone(),
            amount: row.amount,
            kind,
        });
    }

    let mut result = Vec::with_capacity(order.len());
    for batch_id in order {
        let mut batch = batches.remove(&batch_id).expect("just inserted above");
        batch.net_before_fees = batch.gross.checked_sub(batch.refunds)?;
        result.push(batch);
    }
    Ok(result)
}

/// The accounts [`settlement_documents`] posts against. `clearing` (1160)
/// and `fees` (6700) are the two `Account` lines `crate::post::post_settlement`
/// already expects on a `Settlement` document; `undeposited` (1150) is kept
/// for symmetry with `crate::types::PostingConfig` and for a caller building
/// the posting context these documents go through — see the module doc for
/// why it is *not* what a sale's `Payment` deposits into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthnetConfig {
    pub clearing: AccountId,
    pub fees: AccountId,
    pub undeposited: AccountId,
    /// The class an Authorize.net-sourced `RefundReceipt` posts its 4950
    /// leg under. The settlement export carries no item or class
    /// information at all, so this is a single, explicit fallback rather
    /// than a per-line guess (§3's default chain has nothing else to offer
    /// here, since these documents have no item and no header class to fall
    /// back through).
    pub refund_class: ClassId,
    /// The class a `Settlement` document's 6700 leg carries, for the rare
    /// caller who already knows a batch's fee before calling
    /// [`settlement_documents`] (§1's table calls this leg "overhead", but
    /// §3's class rule is enforced uniformly — see `crate::post::post_settlement`'s
    /// doc comment). The ordinary flow never sets [`SettlementBatch::fee`]
    /// here at all (the module doc explains why), so this field is inert
    /// for a straight CSV import; `crate::bank`'s fee-aware settlement match
    /// has its own class for the fee it discovers, `MatchRules::fee_class`.
    pub fee_class: ClassId,
}

fn account_line(number: &AccountId, amount: Money, class: Option<ClassId>) -> DocLine {
    DocLine {
        line_no: 1,
        kind: LineKind::Account,
        amount,
        class,
        item_id: None,
        account: Some(number.clone()),
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

fn customer_ref(customer_id: &Option<String>) -> Option<ContactRef> {
    customer_id.clone().map(|id| ContactRef {
        kind: ContactKind::Customer,
        id,
    })
}

fn settlement_document(batch: &SettlementBatch, config: &AuthnetConfig) -> LedgerDocument {
    let mut lines = vec![account_line(&config.clearing, batch.net_before_fees, None)];
    if let Some(fee) = batch.fee {
        if !fee.is_zero() {
            lines.push(account_line(
                &config.fees,
                fee,
                Some(config.fee_class.clone()),
            ));
        }
    }
    LedgerDocument {
        document_id: format!("authnet:{}", batch.batch_id),
        kind: DocKind::Settlement,
        number: None,
        txn_date: batch.settled_on,
        due_date: None,
        contact: None,
        header_class: None,
        lines,
        tax: None,
        // `None` defaults to `PostingConfig::default_bank_account` in
        // `crate::post::post_settlement` (1100 Checking).
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: Some(batch.batch_id.clone()),
        memo: Some(format!("Authorize.net batch {}", batch.batch_id)),
    }
}

/// A settled sale's `Payment`: applies to the invoice `resolve_invoice`
/// finds for `txn.invoice_number`, else the whole amount is `unapplied`
/// (2300, per `crate::post::post_payment`) — the memo always names the
/// invoice number that was looked up (or its absence), so a caller can
/// render a warning for the unapplied case without this function returning
/// anything richer than a document.
fn payment_document(
    batch: &SettlementBatch,
    txn: &AuthnetTxn,
    config: &AuthnetConfig,
    resolve_invoice: &dyn Fn(&str) -> Option<(String, DocKind)>,
) -> LedgerDocument {
    let resolved = txn.invoice_number.as_deref().and_then(resolve_invoice);
    let (applications, unapplied, memo) = match (&resolved, &txn.invoice_number) {
        (Some((document_id, kind)), Some(number)) => (
            vec![Application {
                target_document_id: document_id.clone(),
                target_kind: *kind,
                amount: txn.amount,
            }],
            Money::ZERO,
            format!(
                "Authorize.net sale {} for invoice {number} (batch {})",
                txn.txn_id, batch.batch_id
            ),
        ),
        (None, Some(number)) => (
            Vec::new(),
            txn.amount,
            format!(
                "Authorize.net sale {} for invoice {number}: no matching invoice found, parked unapplied (batch {})",
                txn.txn_id, batch.batch_id
            ),
        ),
        (_, None) => (
            Vec::new(),
            txn.amount,
            format!(
                "Authorize.net sale {}: no invoice number on the transaction, parked unapplied (batch {})",
                txn.txn_id, batch.batch_id
            ),
        ),
    };

    LedgerDocument {
        document_id: format!("authnet:txn:{}", txn.txn_id),
        kind: DocKind::Payment,
        number: None,
        txn_date: batch.settled_on,
        due_date: None,
        contact: customer_ref(&txn.customer_id),
        header_class: None,
        lines: Vec::new(),
        tax: None,
        // §1's Authorize.net settlement row: "the customer payment already
        // debited 1160 when it settled" — see the module doc.
        deposit_to: Some(config.clearing.clone()),
        pay_from: None,
        applications,
        unapplied,
        is_voided: false,
        source_ref: Some(txn.txn_id.clone()),
        memo: Some(memo),
    }
}

/// A settled refund's `RefundReceipt`: Dr 4950 under [`AuthnetConfig::refund_class`],
/// Cr 1160 (`pay_from`) — the mirror of [`payment_document`]'s Dr 1160, so
/// the pair nets to zero inside 1160 exactly as a sale and its payment do.
fn refund_document(
    batch: &SettlementBatch,
    txn: &AuthnetTxn,
    config: &AuthnetConfig,
) -> LedgerDocument {
    let line = DocLine {
        line_no: 1,
        kind: LineKind::Item,
        amount: txn.amount,
        class: Some(config.refund_class.clone()),
        item_id: None,
        account: None,
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: Some(format!("Authorize.net refund {}", txn.txn_id)),
        posting: None,
        entity: None,
    };
    LedgerDocument {
        document_id: format!("authnet:txn:{}", txn.txn_id),
        kind: DocKind::RefundReceipt,
        number: None,
        txn_date: batch.settled_on,
        due_date: None,
        contact: customer_ref(&txn.customer_id),
        header_class: None,
        lines: vec![line],
        tax: None,
        deposit_to: None,
        pay_from: Some(config.clearing.clone()),
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: Some(txn.txn_id.clone()),
        memo: Some(format!(
            "Authorize.net refund {} (batch {})",
            txn.txn_id, batch.batch_id
        )),
    }
}

/// Turns settled batches into the documents `crate::post::post` already
/// knows how to post: one `Settlement` per batch (§1's Authorize.net
/// settlement row, in the `Account`-line shape `post_settlement` expects),
/// one `Payment` per settled sale, one `RefundReceipt` per settled refund.
/// `resolve_invoice` looks an invoice number up against whatever store the
/// caller has (a live `Ledger`, or a fixed map in a test) and returns the
/// `(document_id, DocKind)` `crate::post::post_payment` needs to build an
/// `Application` — `None` when no invoice matches, in which case the sale's
/// whole amount goes `unapplied` to 2300.
///
/// `document_id`s (`authnet:<batch_id>` for the settlement,
/// `authnet:txn:<txn_id>` for each sale/refund) are the re-import dedupe
/// key: a caller that skips saving a document whose id already has a saved
/// version (`crate::store::Ledger::document_version_count`) gets exactly-once
/// posting for free, since a settled Authorize.net transaction is immutable
/// once it appears in this export.
pub fn settlement_documents(
    batches: &[SettlementBatch],
    config: &AuthnetConfig,
    resolve_invoice: &dyn Fn(&str) -> Option<(String, DocKind)>,
) -> Vec<LedgerDocument> {
    let mut documents = Vec::new();
    for batch in batches {
        documents.push(settlement_document(batch, config));
        for txn in &batch.transactions {
            match txn.kind {
                TxnKind::Sale => {
                    documents.push(payment_document(batch, txn, config, resolve_invoice))
                }
                TxnKind::Refund => documents.push(refund_document(batch, txn, config)),
            }
        }
    }
    documents
}

#[cfg(test)]
mod tests {
    use super::*;
    use csv::parse_transactions;

    fn row(
        txn_id: &str,
        status: TransactionStatus,
        batch_id: &str,
        amount_minor: i64,
        invoice: Option<&str>,
        customer: Option<&str>,
    ) -> TransactionRow {
        TransactionRow {
            transaction_id: txn_id.to_string(),
            status,
            submit_date: None,
            settlement_date: NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(),
            batch_id: batch_id.to_string(),
            amount: Money::from_minor(amount_minor),
            invoice_number: invoice.map(str::to_string),
            customer_id: customer.map(str::to_string),
            card_type: None,
            transaction_type: None,
        }
    }

    #[test]
    fn group_batches_sums_sales_and_refunds_and_drops_voids() {
        let rows = vec![
            row(
                "t1",
                TransactionStatus::SettledSuccessfully,
                "b1",
                10_000,
                Some("1001"),
                Some("cust-a"),
            ),
            row(
                "t2",
                TransactionStatus::SettledSuccessfully,
                "b1",
                15_000,
                Some("1002"),
                Some("cust-b"),
            ),
            row(
                "t3",
                TransactionStatus::RefundSettledSuccessfully,
                "b1",
                4_000,
                Some("1001"),
                Some("cust-a"),
            ),
            row(
                "t4",
                TransactionStatus::Voided,
                "b1",
                2_500,
                None,
                Some("cust-c"),
            ),
        ];
        let batches = group_batches(&rows).expect("groups");
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.batch_id, "b1");
        assert_eq!(batch.gross, Money::from_minor(25_000));
        assert_eq!(batch.refunds, Money::from_minor(4_000));
        assert_eq!(batch.net_before_fees, Money::from_minor(21_000));
        assert_eq!(batch.fee, None);
        // The voided row contributed no transaction.
        assert_eq!(batch.transactions.len(), 3);
    }

    #[test]
    fn settlement_documents_shapes_match_post_settlements_expectations() {
        let rows = vec![row(
            "t1",
            TransactionStatus::SettledSuccessfully,
            "b1",
            10_000,
            Some("1001"),
            Some("cust-a"),
        )];
        let batches = group_batches(&rows).expect("groups");
        let config = AuthnetConfig {
            clearing: AccountId("1160".into()),
            fees: AccountId("6700".into()),
            undeposited: AccountId("1150".into()),
            refund_class: ClassId("foam".into()),
            fee_class: ClassId("foam".into()),
        };
        let resolve = |_: &str| None;
        let documents = settlement_documents(&batches, &config, &resolve);
        assert_eq!(documents.len(), 2);

        let settlement = &documents[0];
        assert_eq!(settlement.kind, DocKind::Settlement);
        assert_eq!(settlement.document_id, "authnet:b1");
        assert_eq!(settlement.source_ref.as_deref(), Some("b1"));
        assert_eq!(settlement.lines.len(), 1);
        assert_eq!(settlement.lines[0].account, Some(AccountId("1160".into())));
        assert_eq!(settlement.lines[0].amount, Money::from_minor(10_000));

        let payment = &documents[1];
        assert_eq!(payment.kind, DocKind::Payment);
        assert_eq!(payment.deposit_to, Some(AccountId("1160".into())));
        assert_eq!(payment.unapplied, Money::from_minor(10_000));
        assert!(payment.applications.is_empty());
    }

    #[test]
    fn unresolved_and_resolved_sales_apply_or_park_unapplied() {
        let rows = vec![
            row(
                "t1",
                TransactionStatus::SettledSuccessfully,
                "b1",
                10_000,
                Some("1001"),
                Some("cust-a"),
            ),
            row(
                "t2",
                TransactionStatus::SettledSuccessfully,
                "b1",
                5_000,
                Some("9999"),
                Some("cust-b"),
            ),
        ];
        let batches = group_batches(&rows).expect("groups");
        let config = AuthnetConfig {
            clearing: AccountId("1160".into()),
            fees: AccountId("6700".into()),
            undeposited: AccountId("1150".into()),
            refund_class: ClassId("foam".into()),
            fee_class: ClassId("foam".into()),
        };
        let resolve = |number: &str| -> Option<(String, DocKind)> {
            (number == "1001").then(|| ("inv-1".to_string(), DocKind::Invoice))
        };
        let documents = settlement_documents(&batches, &config, &resolve);
        // settlement + 2 payments
        assert_eq!(documents.len(), 3);
        let resolved_payment = &documents[1];
        assert_eq!(resolved_payment.applications.len(), 1);
        assert_eq!(resolved_payment.applications[0].target_document_id, "inv-1");
        assert_eq!(resolved_payment.unapplied, Money::ZERO);

        let unresolved_payment = &documents[2];
        assert!(unresolved_payment.applications.is_empty());
        assert_eq!(unresolved_payment.unapplied, Money::from_minor(5_000));
    }

    #[test]
    fn a_refund_document_mirrors_the_payments_clearing_leg() {
        let rows = vec![row(
            "t1",
            TransactionStatus::RefundSettledSuccessfully,
            "b1",
            4_000,
            Some("1001"),
            Some("cust-a"),
        )];
        let batches = group_batches(&rows).expect("groups");
        let config = AuthnetConfig {
            clearing: AccountId("1160".into()),
            fees: AccountId("6700".into()),
            undeposited: AccountId("1150".into()),
            refund_class: ClassId("foam".into()),
            fee_class: ClassId("foam".into()),
        };
        let resolve = |_: &str| None;
        let documents = settlement_documents(&batches, &config, &resolve);
        let refund = &documents[1];
        assert_eq!(refund.kind, DocKind::RefundReceipt);
        assert_eq!(refund.pay_from, Some(AccountId("1160".into())));
        assert_eq!(refund.lines[0].class, Some(ClassId("foam".into())));
        assert_eq!(refund.lines[0].amount, Money::from_minor(4_000));
    }

    #[test]
    fn parsing_then_grouping_the_csv_sample_round_trips() {
        let text = "\
Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount,Invoice Number,Customer ID\n\
50001,Settled Successfully,9/12/2026,b900,60.00,2001,cust-x\n\
50002,Voided,9/12/2026,b900,60.00,,cust-y\n";
        let rows = parse_transactions(text).expect("parses");
        let batches = group_batches(&rows).expect("groups");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].transactions.len(), 1);
        assert_eq!(batches[0].gross, Money::from_minor(6_000));
    }
}
