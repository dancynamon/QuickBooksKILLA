//! QBO sales orders, mirrored on their own path. DECISIONS.md D42.
//!
//! A QBO sales order is not a v3 Accounting entity. It lives in QBO
//! Advanced's commerce module ("Commerce / Orders"): the v3 query endpoint
//! answers `SELECT * FROM SalesOrder` with `QueryValidationError: Metadata
//! not found for Entity: SalesOrder`, CDC never mentions one, and the
//! invoice QBO makes from one carries no `LinkedTxn` back to it. The QBO web
//! app reads them from the commerce GraphQL service with the browser
//! session, which this crate's OAuth client cannot use.
//!
//! So sales orders reach the replica as a **feed**: one JSON document holding
//! every sales order the realm has, in the commerce model's own field names
//! ([`SalesOrderFeed`], format `qbo-sales-orders/1`), read from a file or an
//! `http(s)` URL. Whatever produces it (today: a read of the QBO page's own
//! sales-order store through the qbo-headless browser session) only has to
//! copy fields across; every rule about what "open" means lives here, where
//! it is tested.
//!
//! The replica keeps the raw order and a projection: header, lines, how much
//! of each line is still to be invoiced, and whether the order is open. It
//! sits in its own tables (`sales_orders`, `sales_order_lines`,
//! `sales_order_snapshots`), not in `entities`/`documents`: the id space is
//! the commerce module's (`149…` ten-digit ids), not v3's, and nothing in
//! the CDC, reconciliation or outbox machinery may ever try to read or write
//! one through the v3 API.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::domain::RealmId;

/// The `feed` tag a [`SalesOrderFeed`] must carry.
pub const FEED_FORMAT: &str = "qbo-sales-orders/1";

/// How long a feed URL gets to answer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum SalesOrderError {
    #[error("sales-order feed: {0}")]
    Feed(String),
    #[error("sales-order feed {source_name}: {message}")]
    Fetch {
        source_name: String,
        message: String,
    },
    #[error("sales order {id}: {message}")]
    Order { id: String, message: String },
    #[error("money: {0}")]
    Money(#[from] MoneyError),
}

/// One feed: every sales order of one realm at `captured_at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SalesOrderFeed {
    /// Must be [`FEED_FORMAT`].
    pub feed: String,
    /// The realm the orders belong to. Checked against the realm synced.
    pub realm_id: Option<String>,
    pub captured_at: DateTime<Utc>,
    /// True when `sales_orders` is every order the realm has. Only a complete
    /// feed may mark an order the replica holds, and the feed lacks, deleted.
    #[serde(default)]
    pub complete: bool,
    pub sales_orders: Vec<FeedSalesOrder>,
}

/// One sales order, in the commerce model's field names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedSalesOrder {
    /// The commerce `detailId` (ten digits, `149…`), never the SO number.
    pub id: String,
    /// The SO number a person sees (`1174`).
    pub order_number: Option<String>,
    /// The order date: `2026-10-13` or the commerce model's
    /// `2026-10-13T07:00:00Z` (midnight Pacific); only the date is kept.
    pub issued_at: String,
    /// The v3 Customer id.
    pub customer_id: Option<String>,
    #[serde(default)]
    pub customer_name: Option<String>,
    #[serde(default, rename = "customerPONumber")]
    pub customer_po_number: Option<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    #[serde(default)]
    pub ship_date: Option<String>,
    /// `OPEN`, `CLOSED`, … QBO leaves an invoiced order `OPEN`.
    pub order_status: Option<String>,
    /// The grid's "Invoice status": `NOT_INVOICED`, `PARTIALLY_INVOICED`,
    /// `INVOICED`, `PAID`, `VOIDED`.
    pub payment_status: Option<String>,
    #[serde(default)]
    pub fulfillment_status: Option<String>,
    pub total_amount: Option<Value>,
    #[serde(default)]
    pub total_discount: Option<Value>,
    #[serde(default)]
    pub total_shipping: Option<Value>,
    #[serde(default)]
    pub memo: Option<String>,
    /// The commerce model's edit counter; stored as the sync token.
    #[serde(default)]
    pub version: Option<Value>,
    #[serde(default)]
    pub lines: Vec<FeedLine>,
    /// Every invoice made from this order (v3 Invoice ids).
    #[serde(default)]
    pub invoice_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedLine {
    #[serde(default)]
    pub id: Option<String>,
    /// The v3 Item id; absent on a description-only line.
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub quantity: Option<Value>,
    #[serde(default)]
    pub rate: Option<Value>,
    #[serde(default)]
    pub amount: Option<Value>,
    #[serde(default)]
    pub taxable: Option<bool>,
    /// How much of `quantity` invoices have taken. `null` means none.
    #[serde(default)]
    pub quantity_consumed_in_invoices: Option<Value>,
    /// The invoices that took from this line (v3 Invoice ids).
    #[serde(default)]
    pub invoice_ids: Vec<String>,
}

/// A projected sales order, as the replica stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct SalesOrder {
    pub qbo_id: String,
    pub doc_number: Option<String>,
    pub txn_date: NaiveDate,
    pub customer_id: Option<String>,
    pub customer_name: Option<String>,
    pub po_number: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub ship_date: Option<NaiveDate>,
    pub order_status: Option<String>,
    pub invoice_status: Option<String>,
    pub fulfillment_status: Option<String>,
    pub total: Money,
    /// What is still to be invoiced, before tax: the sum of the lines'
    /// `open_amount`.
    pub open_amount: Money,
    pub is_open: bool,
    pub version: Option<String>,
    pub memo: Option<String>,
    pub invoice_ids: Vec<String>,
    pub lines: Vec<SalesOrderLine>,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SalesOrderLine {
    pub line_no: i64,
    pub line_id: Option<String>,
    pub item_id: Option<String>,
    pub description: Option<String>,
    pub qty: Option<Decimal>,
    pub unit_price: Option<Decimal>,
    pub amount: Money,
    pub is_taxable: bool,
    pub qty_invoiced: Decimal,
    /// `qty - qty_invoiced`, never below zero. `None` when the line has no
    /// quantity.
    pub qty_open: Option<Decimal>,
    /// The part of `amount` still to be invoiced.
    pub open_amount: Money,
    pub invoice_ids: Vec<String>,
}

/// Statuses that close an order whatever its lines say.
fn closed_status(status: Option<&str>) -> bool {
    matches!(
        status.map(str::to_ascii_uppercase).as_deref(),
        Some("CLOSED" | "CANCELLED" | "CANCELED" | "VOIDED" | "DELETED")
    )
}

/// Invoice statuses that say every line went onto an invoice.
fn fully_invoiced_status(status: Option<&str>) -> bool {
    matches!(
        status.map(str::to_ascii_uppercase).as_deref(),
        Some("INVOICED" | "PAID")
    )
}

fn decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        Value::Number(n) => Decimal::from_str(&n.to_string())
            .or_else(|_| Decimal::from_scientific(&n.to_string()))
            .ok(),
        Value::String(s) => {
            let cleaned: String = s.chars().filter(|c| !matches!(c, '$' | ',')).collect();
            let cleaned = cleaned.trim();
            if cleaned.is_empty() {
                None
            } else {
                Decimal::from_str(cleaned).ok()
            }
        }
        _ => None,
    }
}

fn money(value: Option<&Value>) -> Result<Option<Money>, MoneyError> {
    decimal(value)
        .map(|d| round_money(d, RoundingPolicy::MirroredAmount))
        .transpose()
}

fn date(raw: Option<&str>) -> Option<NaiveDate> {
    let raw = raw?.trim();
    NaiveDate::parse_from_str(raw.get(..10)?, "%Y-%m-%d").ok()
}

fn value_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn non_empty(raw: &Option<String>) -> Option<String> {
    raw.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// What `amount` is still to be invoiced when `open` of `qty` remains.
fn open_part(
    amount: Money,
    qty: Decimal,
    open: Decimal,
    rate: Option<Decimal>,
) -> Result<Money, MoneyError> {
    if open >= qty {
        return Ok(amount);
    }
    if open <= Decimal::ZERO {
        return Ok(Money::ZERO);
    }
    match rate {
        Some(rate) => round_money(rate * open, RoundingPolicy::MirroredAmount),
        None => round_money(
            Decimal::new(amount.minor(), 2) * open / qty,
            RoundingPolicy::MirroredAmount,
        ),
    }
}

impl SalesOrder {
    /// Project one feed order. Pure.
    pub fn from_feed(order: &FeedSalesOrder) -> Result<SalesOrder, SalesOrderError> {
        let fail = |message: String| SalesOrderError::Order {
            id: order.id.clone(),
            message,
        };
        if order.id.trim().is_empty() {
            return Err(SalesOrderError::Feed(
                "a sales order has an empty id".into(),
            ));
        }
        let txn_date = date(Some(&order.issued_at))
            .ok_or_else(|| fail(format!("issuedAt {:?} is not a date", order.issued_at)))?;

        let all_invoiced = fully_invoiced_status(order.payment_status.as_deref());
        let any_consumption = order
            .lines
            .iter()
            .any(|line| decimal(line.quantity_consumed_in_invoices.as_ref()).is_some());

        let mut lines = Vec::with_capacity(order.lines.len());
        for (index, line) in order.lines.iter().enumerate() {
            let qty = decimal(line.quantity.as_ref());
            let unit_price = decimal(line.rate.as_ref());
            let amount = match money(line.amount.as_ref())? {
                Some(amount) => amount,
                None => match (qty, unit_price) {
                    (Some(q), Some(r)) => round_money(q * r, RoundingPolicy::MirroredAmount)?,
                    _ => Money::ZERO,
                },
            };
            // QBO reports invoiced quantities per line. An order whose status
            // says invoiced but whose lines say nothing (an older export) is
            // taken at its status: every line consumed.
            let qty_invoiced = match decimal(line.quantity_consumed_in_invoices.as_ref()) {
                Some(consumed) => consumed,
                None if all_invoiced && !any_consumption => qty.unwrap_or(Decimal::ZERO),
                None => Decimal::ZERO,
            };
            let (qty_open, open_amount) = match qty {
                Some(q) => {
                    let open = (q - qty_invoiced).max(Decimal::ZERO);
                    (Some(open), open_part(amount, q, open, unit_price)?)
                }
                None if all_invoiced && !any_consumption => (None, Money::ZERO),
                None => (None, amount),
            };
            lines.push(SalesOrderLine {
                line_no: index as i64 + 1,
                line_id: non_empty(&line.id),
                item_id: non_empty(&line.item_id),
                description: line.description.clone().filter(|d| !d.is_empty()),
                qty,
                unit_price,
                amount,
                is_taxable: line.taxable.unwrap_or(false),
                qty_invoiced,
                qty_open,
                open_amount,
                invoice_ids: line.invoice_ids.clone(),
            });
        }

        let line_total = Money::checked_sum(lines.iter().map(|l| l.amount))?;
        let total = money(order.total_amount.as_ref())?.unwrap_or(line_total);
        let open_amount = Money::checked_sum(lines.iter().map(|l| l.open_amount))?;
        let is_open = !closed_status(order.order_status.as_deref()) && open_amount.minor() > 0;
        let mut invoice_ids = order.invoice_ids.clone();
        for line in &lines {
            for id in &line.invoice_ids {
                if !invoice_ids.contains(id) {
                    invoice_ids.push(id.clone());
                }
            }
        }
        Ok(SalesOrder {
            qbo_id: order.id.trim().to_string(),
            doc_number: non_empty(&order.order_number),
            txn_date,
            customer_id: non_empty(&order.customer_id),
            customer_name: non_empty(&order.customer_name),
            po_number: non_empty(&order.customer_po_number),
            due_date: date(order.due_date.as_deref()),
            ship_date: date(order.ship_date.as_deref()),
            order_status: non_empty(&order.order_status),
            invoice_status: non_empty(&order.payment_status),
            fulfillment_status: non_empty(&order.fulfillment_status),
            total,
            open_amount,
            is_open,
            version: value_text(order.version.as_ref()),
            memo: non_empty(&order.memo),
            invoice_ids,
            lines,
            is_deleted: false,
        })
    }

    /// The lines still to be invoiced as of `as_of`, given the date of each
    /// invoice the replica knows (`invoice_dates`, v3 Invoice id to
    /// `TxnDate`). A line's invoiced quantity counts only when an invoice
    /// that took from it is dated on or before `as_of`: when every invoice
    /// on the line is dated after it, the line was still open on that date
    /// and its whole quantity is returned. An invoice the replica does not
    /// know is taken as dated on or before (it was invoiced; we cannot say
    /// when). Returns `(line, open qty, open amount)` for every line with
    /// something open; an empty result means the order was closed then.
    pub fn open_lines_as_of(
        &self,
        as_of: NaiveDate,
        invoice_dates: &HashMap<String, NaiveDate>,
    ) -> Result<Vec<(SalesOrderLine, Option<Decimal>, Money)>, SalesOrderError> {
        if self.is_deleted || self.txn_date > as_of || closed_status(self.order_status.as_deref()) {
            return Ok(Vec::new());
        }
        let later = |id: &String| invoice_dates.get(id).is_some_and(|d| *d > as_of);
        let mut out = Vec::new();
        for line in &self.lines {
            let invoiced_later = !line.invoice_ids.is_empty() && line.invoice_ids.iter().all(later);
            let (qty_open, open_amount) = if invoiced_later {
                (line.qty, line.amount)
            } else {
                (line.qty_open, line.open_amount)
            };
            let open_line = match qty_open {
                Some(q) => q > Decimal::ZERO && open_amount.minor() != 0,
                None => open_amount.minor() != 0,
            };
            if open_line {
                out.push((line.clone(), qty_open, open_amount));
            }
        }
        Ok(out)
    }
}

/// Parse a feed and check it is one this crate reads, for `realm`.
pub fn parse_feed(raw: &str, realm: &RealmId) -> Result<SalesOrderFeed, SalesOrderError> {
    let feed: SalesOrderFeed = serde_json::from_str(raw)
        .map_err(|err| SalesOrderError::Feed(format!("not a sales-order feed: {err}")))?;
    if feed.feed != FEED_FORMAT {
        return Err(SalesOrderError::Feed(format!(
            "format {:?} is not {FEED_FORMAT:?}",
            feed.feed
        )));
    }
    if let Some(feed_realm) = &feed.realm_id {
        if feed_realm != realm.as_str() {
            return Err(SalesOrderError::Feed(format!(
                "feed is for realm {feed_realm}, not {realm}"
            )));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for order in &feed.sales_orders {
        if !seen.insert(order.id.trim()) {
            return Err(SalesOrderError::Feed(format!(
                "sales order {} appears twice",
                order.id
            )));
        }
    }
    Ok(feed)
}

/// Read a feed from `source`: an `http://` or `https://` URL (GET, JSON
/// body) or a file path.
pub fn fetch_feed(source: &str, realm: &RealmId) -> Result<SalesOrderFeed, SalesOrderError> {
    let fetch_err = |message: String| SalesOrderError::Fetch {
        source_name: source.to_string(),
        message,
    };
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        let agent = ureq::AgentBuilder::new().timeout(FETCH_TIMEOUT).build();
        match agent.get(source).set("Accept", "application/json").call() {
            Ok(response) => response
                .into_string()
                .map_err(|err| fetch_err(err.to_string()))?,
            Err(ureq::Error::Status(code, _)) => {
                return Err(fetch_err(format!("HTTP {code}")));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(fetch_err(transport.to_string()));
            }
        }
    } else {
        std::fs::read_to_string(source).map_err(|err| fetch_err(err.to_string()))?
    };
    parse_feed(&raw, realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn order(lines: Value, payment_status: &str) -> FeedSalesOrder {
        serde_json::from_value(json!({
            "id": "1490000001",
            "orderNumber": "1001",
            "issuedAt": "2026-10-13T07:00:00Z",
            "customerId": "58",
            "orderStatus": "OPEN",
            "paymentStatus": payment_status,
            "totalAmount": "300.00",
            "version": 4,
            "lines": lines,
        }))
        .unwrap()
    }

    #[test]
    fn an_uninvoiced_order_is_open_for_its_whole_subtotal() {
        let so = SalesOrder::from_feed(&order(
            json!([
                { "id": "a", "itemId": "10", "quantity": 2, "rate": "100", "amount": "200.00",
                  "quantityConsumedInInvoices": null },
                { "id": "b", "itemId": "11", "quantity": 1, "rate": 100, "amount": 100 },
                { "id": "c", "description": "note" }
            ]),
            "NOT_INVOICED",
        ))
        .unwrap();
        assert!(so.is_open);
        assert_eq!(so.txn_date, NaiveDate::from_ymd_opt(2026, 10, 13).unwrap());
        assert_eq!(so.open_amount, Money::from_minor(30_000));
        assert_eq!(so.version.as_deref(), Some("4"));
        assert_eq!(so.lines[2].open_amount, Money::ZERO);
    }

    #[test]
    fn invoiced_quantities_close_lines_and_a_fully_invoiced_order() {
        let so = SalesOrder::from_feed(&order(
            json!([
                { "itemId": "10", "quantity": 2, "rate": "100", "amount": "200",
                  "quantityConsumedInInvoices": 1, "invoiceIds": ["900"] },
                { "itemId": "11", "quantity": 1, "rate": "100", "amount": "100",
                  "quantityConsumedInInvoices": 1, "invoiceIds": ["900"] }
            ]),
            "PARTIALLY_INVOICED",
        ))
        .unwrap();
        assert!(so.is_open);
        assert_eq!(so.open_amount, Money::from_minor(10_000));
        assert_eq!(so.lines[0].qty_open, Some(Decimal::ONE));
        assert_eq!(so.invoice_ids, vec!["900".to_string()]);

        let done = SalesOrder::from_feed(&order(
            json!([{ "itemId": "10", "quantity": 2, "rate": "100", "amount": "200",
                     "quantityConsumedInInvoices": 2 }]),
            "INVOICED",
        ))
        .unwrap();
        assert!(!done.is_open);
    }

    #[test]
    fn an_invoiced_status_with_no_line_detail_counts_as_consumed() {
        let so = SalesOrder::from_feed(&order(
            json!([{ "itemId": "10", "quantity": 2, "rate": "100", "amount": "200" }]),
            "PAID",
        ))
        .unwrap();
        assert!(!so.is_open);
    }

    #[test]
    fn a_closed_status_closes_the_order() {
        let mut feed = order(
            json!([{ "itemId": "10", "quantity": 2, "rate": "100", "amount": "200" }]),
            "NOT_INVOICED",
        );
        feed.order_status = Some("CLOSED".into());
        assert!(!SalesOrder::from_feed(&feed).unwrap().is_open);
    }

    #[test]
    fn an_invoice_dated_after_the_as_of_date_leaves_the_line_open_then() {
        let so = SalesOrder::from_feed(&order(
            json!([
                { "itemId": "10", "quantity": 2, "rate": "100", "amount": "200",
                  "quantityConsumedInInvoices": 2, "invoiceIds": ["900"] },
                { "itemId": "11", "quantity": 1, "rate": "100", "amount": "100",
                  "quantityConsumedInInvoices": 1, "invoiceIds": ["800"] }
            ]),
            "INVOICED",
        ))
        .unwrap();
        assert!(!so.is_open);
        let as_of = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let dates = HashMap::from([
            (
                "900".to_string(),
                NaiveDate::from_ymd_opt(2027, 1, 5).unwrap(),
            ),
            (
                "800".to_string(),
                NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            ),
        ]);
        let open = so.open_lines_as_of(as_of, &dates).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0.item_id.as_deref(), Some("10"));
        assert_eq!(open[0].1, Some(Decimal::from(2)));
        assert_eq!(open[0].2, Money::from_minor(20_000));
        // Dated after the as-of date: not open on it.
        let earlier = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert!(so.open_lines_as_of(earlier, &dates).unwrap().is_empty());
    }

    #[test]
    fn a_feed_for_another_realm_or_format_is_refused() {
        let realm = RealmId::parse("123").unwrap();
        let ok = json!({ "feed": FEED_FORMAT, "realmId": "123",
                         "capturedAt": "2026-10-17T12:00:00Z", "salesOrders": [] });
        assert!(parse_feed(&ok.to_string(), &realm).is_ok());
        let other = json!({ "feed": FEED_FORMAT, "realmId": "456",
                            "capturedAt": "2026-10-17T12:00:00Z", "salesOrders": [] });
        assert!(parse_feed(&other.to_string(), &realm).is_err());
        let format = json!({ "feed": "something/2",
                             "capturedAt": "2026-10-17T12:00:00Z", "salesOrders": [] });
        assert!(parse_feed(&format.to_string(), &realm).is_err());
    }
}
