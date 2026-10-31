//! D33: the item → class worksheet. `docs/CUTOVER.md`, "Item classes (D33)".
//!
//! QBO items carry no default class, so a Q1 2027 replay under the full
//! posting gate refuses nearly every sales line. The fix is Dan's decision,
//! item by item, and this module only helps him make it and then records it:
//!
//! - [`worksheet`] reads the replica and lists every active item with its
//!   last-12-month sales and a `suggested_class` from [`Rules`] (a checked-in
//!   TOML, `apps/ledger/item-class-rules.toml`), naming the rule that fired.
//!   [`render_csv`] writes it, largest sales first, with an empty `class`
//!   column for Dan.
//! - [`parse_reviewed`] and [`apply`] read his reviewed CSV — the `class`
//!   column only, never the suggestion — and write the `item_class_map`
//!   table in the book. Every class must be one of the book's classes; a row
//!   with no class refuses the whole file. Nothing is ever defaulted.
//! - [`item_class_map`] is what the importer reads: `pipeline::import_replica`
//!   (so `ledger import` and `ledger nightly`) and `slim::import_slim` give an
//!   item the class the map names, ahead of any default class QBO carries.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Months, NaiveDate, Utc};
use rusqlite::params;
use serde::Deserialize;

use ledger_core::Money;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::project::{ParsedEntity, Projection};
use qbo_local::store::{Store, StoreError};

use crate::accountant::csv_field;
use crate::chart;
use crate::import::{self, ImportError};
use crate::store::{Ledger, LedgerError};
use crate::types::ClassId;

/// The checked-in rules: `apps/ledger/item-class-rules.toml`.
pub const DEFAULT_RULES: &str = include_str!("../item-class-rules.toml");

/// The worksheet's header. `apply` needs only `qbo_id` and `class`, in any
/// column order, so Dan can sort, hide or add columns in a spreadsheet.
pub const CSV_HEADER: &str = "qbo_id,sku,name,type,category,income_account,\
sales_lines_12m,sales_amount_12m,suggested_class,rule,class";

#[derive(Debug, thiserror::Error)]
pub enum WorksheetError {
    #[error("rules: {0}")]
    Rules(String),
    #[error("replica: {0}")]
    Replica(#[from] StoreError),
    #[error("import: {0}")]
    Import(#[from] ImportError),
    #[error("ledger: {0}")]
    Ledger(#[from] LedgerError),
    /// Every problem in a reviewed CSV, one per line. Nothing was written.
    #[error("nothing applied:\n{}", .0.join("\n"))]
    Refused(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    rule: Vec<Rule>,
}

/// One `[[rule]]`. See the comments at the top of the rules file.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    pub class: String,
    #[serde(default)]
    pub sku_prefix: Vec<String>,
    #[serde(default)]
    pub name_contains: Vec<String>,
    #[serde(default)]
    pub category: Vec<String>,
    #[serde(default)]
    pub income_account: Vec<String>,
}

/// What a rule looks at for one item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemFacts {
    pub qbo_id: String,
    pub sku: Option<String>,
    pub name: String,
    pub item_type: String,
    /// The QBO category path: `FullyQualifiedName` less the item's own name.
    pub category: Option<String>,
    /// The name of the item's QBO income account.
    pub income_account: Option<String>,
}

/// An ordered rule list; the first rule that fires wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules(Vec<Rule>);

impl Rules {
    /// Parse and check a rules file for `company`: every rule named, names
    /// unique, at least one pattern, and a class from the company's §3 list
    /// (never the import-only fallback).
    pub fn parse(text: &str, company: &str) -> Result<Rules, WorksheetError> {
        let file: RulesFile =
            toml::from_str(text).map_err(|err| WorksheetError::Rules(err.to_string()))?;
        let allowed = company_classes(company);
        let mut problems = Vec::new();
        let mut seen = HashMap::new();
        for (index, rule) in file.rule.iter().enumerate() {
            let label = format!("rule {} ({:?})", index + 1, rule.name);
            if rule.name.trim().is_empty() {
                problems.push(format!("{label}: no name"));
            }
            if let Some(first) = seen.insert(rule.name.clone(), index + 1) {
                problems.push(format!("{label}: same name as rule {first}"));
            }
            if !allowed.iter().any(|(id, _)| *id == rule.class) {
                problems.push(format!(
                    "{label}: class {:?} is not one of {}",
                    rule.class,
                    allowed
                        .iter()
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let patterns = [
                &rule.sku_prefix,
                &rule.name_contains,
                &rule.category,
                &rule.income_account,
            ];
            if patterns.iter().all(|list| list.is_empty()) {
                problems.push(format!("{label}: no patterns"));
            }
            if patterns
                .iter()
                .any(|list| list.iter().any(|p| p.is_empty()))
            {
                problems.push(format!("{label}: an empty pattern matches everything"));
            }
        }
        if !problems.is_empty() {
            return Err(WorksheetError::Rules(problems.join("; ")));
        }
        Ok(Rules(file.rule))
    }

    pub fn rules(&self) -> &[Rule] {
        &self.0
    }

    /// The first rule that fires for `item`, if any.
    pub fn suggest(&self, item: &ItemFacts) -> Option<&Rule> {
        let sku = item.sku.as_deref().map(strip_department);
        let name = strip_department(&item.name);
        let full_name = item.name.to_uppercase();
        let category = item.category.as_deref().map(str::to_uppercase);
        let income = item.income_account.as_deref().map(str::to_uppercase);
        self.0.iter().find(|rule| {
            rule.sku_prefix.iter().any(|prefix| {
                let prefix = prefix.to_uppercase();
                sku.as_deref().is_some_and(|sku| sku.starts_with(&prefix))
                    || name.starts_with(&prefix)
            }) || rule
                .name_contains
                .iter()
                .any(|needle| full_name.contains(&needle.to_uppercase()))
                || rule.category.iter().any(|prefix| {
                    category
                        .as_deref()
                        .is_some_and(|path| path.starts_with(&prefix.to_uppercase()))
                })
                || rule
                    .income_account
                    .iter()
                    .any(|account| income.as_deref() == Some(account.to_uppercase().as_str()))
        })
    }
}

/// Upper-cased, with a leading "<digit> " department prefix removed:
/// "3 PM-TRN-2412" → "PM-TRN-2412".
fn strip_department(raw: &str) -> String {
    let upper = raw.trim().to_uppercase();
    let mut chars = upper.chars();
    match (chars.next(), chars.next()) {
        (Some(digit), Some(' ')) if digit.is_ascii_digit() => chars.as_str().trim().to_string(),
        _ => upper,
    }
}

fn company_classes(company: &str) -> &'static [(&'static str, &'static str)] {
    if company.eq_ignore_ascii_case("waterline") {
        chart::WATERLINE_CLASSES
    } else {
        chart::AQUAMENTOR_CLASSES
    }
}

/// One worksheet row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorksheetRow {
    pub item: ItemFacts,
    pub sales_lines: u64,
    pub sales_amount: Money,
    pub suggested_class: Option<String>,
    pub rule: Option<String>,
}

/// The twelve months ending `as_of`: `(as_of − 12 months, as_of]`.
pub fn sales_window(as_of: NaiveDate) -> (NaiveDate, NaiveDate) {
    let start = as_of
        .checked_sub_months(Months::new(12))
        .and_then(|d| d.succ_opt())
        .unwrap_or(as_of);
    (start, as_of)
}

/// Every active, non-category item in `realm`, with its sales over
/// [`sales_window`]`(as_of)` and the rule suggestion, sorted by sales
/// amount (largest first), then sales lines, then QBO id.
pub fn worksheet(
    store: &Store,
    realm: &RealmId,
    rules: &Rules,
    as_of: NaiveDate,
) -> Result<Vec<WorksheetRow>, WorksheetError> {
    let accounts: HashMap<String, String> = import::parsed_accounts(store, realm)?
        .into_iter()
        .map(|account| (account.qbo_id, account.name))
        .collect();
    let (from, to) = sales_window(as_of);
    let sales: HashMap<String, (u64, Money)> = store
        .item_sales(realm, from, to)?
        .into_iter()
        .map(|row| (row.item_id, (row.lines, row.amount)))
        .collect();

    let mut rows = Vec::new();
    for entry in store.entity_index(realm, EntityType::Item)? {
        if entry.is_deleted {
            continue;
        }
        let Some(entity) = store.get_entity(realm, EntityType::Item, &entry.qbo_id)? else {
            continue;
        };
        let Projection::Parsed(parsed) = qbo_local::project::parse(&entity) else {
            continue;
        };
        let ParsedEntity::Item(item) = *parsed else {
            continue;
        };
        let item_type = item.item_type.clone().unwrap_or_default();
        if !item.is_active || item.is_deleted || item_type.eq_ignore_ascii_case("category") {
            continue;
        }
        let category = entity
            .raw_json
            .get("FullyQualifiedName")
            .and_then(|v| v.as_str())
            .and_then(|full| full.rsplit_once(':'))
            .map(|(path, _)| path.to_string());
        let facts = ItemFacts {
            qbo_id: item.qbo_id.clone(),
            sku: item.sku.clone().filter(|s| !s.trim().is_empty()),
            name: item.name.clone(),
            item_type,
            category,
            income_account: item
                .income_account_id
                .as_deref()
                .and_then(|id| accounts.get(id).cloned()),
        };
        let rule = rules.suggest(&facts);
        let (sales_lines, sales_amount) =
            sales.get(&item.qbo_id).copied().unwrap_or((0, Money::ZERO));
        rows.push(WorksheetRow {
            suggested_class: rule.map(|r| r.class.clone()),
            rule: rule.map(|r| r.name.clone()),
            item: facts,
            sales_lines,
            sales_amount,
        });
    }
    rows.sort_by(|a, b| {
        b.sales_amount
            .cmp(&a.sales_amount)
            .then(b.sales_lines.cmp(&a.sales_lines))
            .then(id_order(&a.item.qbo_id).cmp(&id_order(&b.item.qbo_id)))
    });
    Ok(rows)
}

/// QBO ids are numeric strings; order them as numbers where they are.
fn id_order(id: &str) -> (u64, &str) {
    (id.parse().unwrap_or(u64::MAX), id)
}

/// The worksheet as CSV, [`CSV_HEADER`] first, `class` left empty.
pub fn render_csv(rows: &[WorksheetRow]) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for row in rows {
        let fields = [
            csv_field(&row.item.qbo_id),
            csv_field(row.item.sku.as_deref().unwrap_or_default()),
            csv_field(&row.item.name),
            csv_field(&row.item.item_type),
            csv_field(row.item.category.as_deref().unwrap_or_default()),
            csv_field(row.item.income_account.as_deref().unwrap_or_default()),
            row.sales_lines.to_string(),
            Money::display(row.sales_amount),
            csv_field(row.suggested_class.as_deref().unwrap_or_default()),
            csv_field(row.rule.as_deref().unwrap_or_default()),
            String::new(),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// Aggregates only — what is safe to print or paste anywhere.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuggestSummary {
    pub items: usize,
    pub items_sold: usize,
    pub suggested: usize,
    /// Per suggested class: items, and their last-12-month sales.
    pub per_class: BTreeMap<String, (usize, Money)>,
    pub sales_total: Money,
    /// Last-12-month sales on items that got a suggestion.
    pub sales_covered: Money,
}

pub fn summarize(rows: &[WorksheetRow]) -> SuggestSummary {
    let mut summary = SuggestSummary {
        items: rows.len(),
        ..SuggestSummary::default()
    };
    for row in rows {
        summary.sales_total =
            Money::from_minor(summary.sales_total.minor() + row.sales_amount.minor());
        if row.sales_lines > 0 {
            summary.items_sold += 1;
        }
        let key = row.suggested_class.clone().unwrap_or_default();
        let entry = summary.per_class.entry(key).or_insert((0, Money::ZERO));
        entry.0 += 1;
        entry.1 = Money::from_minor(entry.1.minor() + row.sales_amount.minor());
        if row.suggested_class.is_some() {
            summary.suggested += 1;
            summary.sales_covered =
                Money::from_minor(summary.sales_covered.minor() + row.sales_amount.minor());
        }
    }
    summary
}

impl SuggestSummary {
    /// Share of last-12-month sales covered by a suggestion, in percent.
    pub fn covered_percent(&self) -> f64 {
        if self.sales_total.is_zero() {
            return 0.0;
        }
        self.sales_covered.minor() as f64 * 100.0 / self.sales_total.minor() as f64
    }

    pub fn render_text(&self) -> String {
        let mut out = format!(
            "{} active items, {} sold in the last 12 months; {} with a suggested class\n",
            self.items, self.items_sold, self.suggested
        );
        out.push_str(&format!(
            "last-12-month sales {} , {} ({:.1}%) on items with a suggestion\n\n",
            Money::display(self.sales_total),
            Money::display(self.sales_covered),
            self.covered_percent()
        ));
        out.push_str("suggested class    items   12-month sales\n");
        for (class, (items, amount)) in &self.per_class {
            let label = if class.is_empty() { "(none)" } else { class };
            out.push_str(&format!(
                "  {label:<14} {items:>7}   {:>14}\n",
                Money::display(*amount)
            ));
        }
        out
    }
}

/// One data row of a reviewed worksheet. `line` is the 1-based CSV line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewedRow {
    pub line: usize,
    pub qbo_id: String,
    /// What Dan wrote, trimmed and lower-cased; may be empty here — [`apply`]
    /// refuses it.
    pub class: String,
    /// The `name` column when there is one, for error messages.
    pub name: Option<String>,
}

/// Read a reviewed worksheet: a header naming `qbo_id` and `class` (other
/// columns are ignored), then one row per item. Blank lines are skipped.
pub fn parse_reviewed(text: &str) -> Result<Vec<ReviewedRow>, WorksheetError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let records = split_csv(text);
    let mut records = records
        .into_iter()
        .filter(|(_, fields)| fields.iter().any(|f| !f.trim().is_empty()));
    let Some((_, header)) = records.next() else {
        return Err(WorksheetError::Refused(vec!["the CSV is empty".into()]));
    };
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let (Some(id_col), Some(class_col)) = (column("qbo_id"), column("class")) else {
        return Err(WorksheetError::Refused(vec![
            "the header must name a qbo_id column and a class column".into(),
        ]));
    };
    let name_col = column("name");
    Ok(records
        .map(|(line, fields)| {
            let get = |col: usize| {
                fields
                    .get(col)
                    .map(|f| f.trim().to_string())
                    .unwrap_or_default()
            };
            ReviewedRow {
                line,
                qbo_id: get(id_col),
                class: get(class_col).to_lowercase(),
                name: name_col.map(get).filter(|n| !n.is_empty()),
            }
        })
        .collect())
}

/// RFC 4180-ish: quoted fields may hold commas, doubled quotes and line
/// breaks; CRLF or LF. Each record comes with the line it starts on.
fn split_csv(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut line = 1;
    let mut record_line = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut field)),
            '\r' if !in_quotes => {}
            '\n' if !in_quotes => {
                fields.push(std::mem::take(&mut field));
                records.push((record_line, std::mem::take(&mut fields)));
                line += 1;
                record_line = line;
            }
            other => {
                if other == '\n' {
                    line += 1;
                }
                field.push(other);
            }
        }
    }
    if !field.is_empty() || !fields.is_empty() {
        fields.push(field);
        records.push((record_line, fields));
    }
    records
}

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub rows: usize,
    pub added: usize,
    pub changed: usize,
    pub unchanged: usize,
    /// Items per class in the map after the apply.
    pub map_per_class: BTreeMap<String, usize>,
}

impl ApplyReport {
    pub fn render_text(&self, company: &str) -> String {
        let mut out = format!(
            "item classes for {company}: {} row(s) read; {} added, {} changed, {} unchanged\n",
            self.rows, self.added, self.changed, self.unchanged
        );
        out.push_str("map now holds:\n");
        for (class, count) in &self.map_per_class {
            out.push_str(&format!("  {class:<14} {count:>6} item(s)\n"));
        }
        out
    }
}

/// Write `rows` into `company`'s item → class map, all or nothing. Refuses
/// the whole file, listing every problem, when a row has no QBO id or no
/// class, names a class the book does not have (or the import-only
/// fallback), or gives one item two different classes. Items not in the
/// file keep whatever the map already says, so Dan can apply in batches.
pub fn apply(
    ledger: &Ledger,
    company: &str,
    rows: &[ReviewedRow],
    by: &str,
    now: DateTime<Utc>,
) -> Result<ApplyReport, WorksheetError> {
    let classes: HashMap<String, bool> = ledger
        .list_classes(company)?
        .into_iter()
        .map(|class| (class.class_id.0, class.is_active))
        .collect();
    if classes.is_empty() {
        return Err(WorksheetError::Refused(vec![format!(
            "company {company:?} has no classes in this book; run `ledger init` first"
        )]));
    }
    let mut known: Vec<&str> = classes
        .iter()
        .filter(|(id, active)| **active && id.as_str() != chart::IMPORT_FALLBACK_CLASS.0)
        .map(|(id, _)| id.as_str())
        .collect();
    known.sort_unstable();

    let mut problems = Vec::new();
    let mut wanted: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for row in rows {
        let who = match &row.name {
            Some(name) => format!("line {} (item {} {name:?})", row.line, row.qbo_id),
            None => format!("line {} (item {})", row.line, row.qbo_id),
        };
        if row.qbo_id.is_empty() {
            problems.push(format!("line {}: no qbo_id", row.line));
            continue;
        }
        if row.class.is_empty() {
            problems.push(format!(
                "{who}: no class — fill it in or delete the row; apply never defaults one"
            ));
            continue;
        }
        if row.class == chart::IMPORT_FALLBACK_CLASS.0 {
            problems.push(format!(
                "{who}: {:?} is the import-only fallback (D28), not a class an item can have",
                row.class
            ));
            continue;
        }
        match classes.get(&row.class) {
            None => {
                problems.push(format!(
                    "{who}: class {:?} is not in the book (one of: {})",
                    row.class,
                    known.join(", ")
                ));
                continue;
            }
            Some(false) => {
                problems.push(format!("{who}: class {:?} is inactive", row.class));
                continue;
            }
            Some(true) => {}
        }
        if let Some((earlier, earlier_line)) = wanted.get(&row.qbo_id) {
            if *earlier != row.class {
                problems.push(format!(
                    "{who}: class {:?}, but line {earlier_line} gives it {earlier:?}",
                    row.class
                ));
            }
            continue;
        }
        wanted.insert(row.qbo_id.clone(), (row.class.clone(), row.line));
    }
    if !problems.is_empty() {
        return Err(WorksheetError::Refused(problems));
    }

    let before = item_class_map(ledger, company)?;
    let mut report = ApplyReport {
        rows: rows.len(),
        ..ApplyReport::default()
    };
    ledger.atomically(|| -> Result<(), WorksheetError> {
        for (qbo_id, (class, _)) in &wanted {
            match before.get(qbo_id) {
                Some(existing) if existing.0 == *class => {
                    report.unchanged += 1;
                    continue;
                }
                Some(_) => report.changed += 1,
                None => report.added += 1,
            }
            ledger.conn().execute(
                "INSERT INTO item_class_map (company_id, item_ref, class_id, applied_at, applied_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (company_id, item_ref) DO UPDATE SET
                     class_id = excluded.class_id, applied_at = excluded.applied_at,
                     applied_by = excluded.applied_by",
                params![company, qbo_id, class, now.to_rfc3339(), by],
            )
            .map_err(LedgerError::from)?;
        }
        Ok(())
    })?;
    for class in item_class_map(ledger, company)?.into_values() {
        *report.map_per_class.entry(class.0).or_default() += 1;
    }
    Ok(report)
}

/// `company`'s item → class map: QBO item id to class. Empty until
/// [`apply`] has run.
pub fn item_class_map(
    ledger: &Ledger,
    company: &str,
) -> Result<BTreeMap<String, ClassId>, LedgerError> {
    let mut statement = ledger.conn().prepare(
        "SELECT item_ref, class_id FROM item_class_map WHERE company_id = ?1 ORDER BY item_ref",
    )?;
    let rows = statement.query_map(params![company], |row| {
        Ok((row.get::<_, String>(0)?, ClassId(row.get(1)?)))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(
        sku: Option<&str>,
        name: &str,
        category: Option<&str>,
        income: Option<&str>,
    ) -> ItemFacts {
        ItemFacts {
            qbo_id: "1".into(),
            sku: sku.map(String::from),
            name: name.into(),
            item_type: "Inventory".into(),
            category: category.map(String::from),
            income_account: income.map(String::from),
        }
    }

    fn fired(rules: &Rules, item: &ItemFacts) -> Option<String> {
        rules.suggest(item).map(|rule| rule.name.clone())
    }

    #[test]
    fn the_checked_in_rules_parse_for_aquamentor() {
        let rules = Rules::parse(DEFAULT_RULES, "aquamentor").unwrap();
        assert!(!rules.rules().is_empty());
    }

    #[test]
    fn the_checked_in_rules_suggest_the_examples_d33_names() {
        let rules = Rules::parse(DEFAULT_RULES, "aquamentor").unwrap();
        let tube = facts(Some("XRT-50-RD-KF"), "XRT-50-RD-KF", None, Some("Sales"));
        assert_eq!(rules.suggest(&tube).unwrap().class, "foam");
        assert_eq!(fired(&rules, &tube).as_deref(), Some("rescue-tubes"));
        let chair = facts(None, "1 AM LGC 72", None, Some("Sales"));
        assert_eq!(rules.suggest(&chair).unwrap().class, "chair");
        let mat = facts(None, "3 PM-TRN-2412", None, None);
        assert_eq!(fired(&rules, &mat).as_deref(), Some("pool-mats"));
        let cleaner = facts(None, "Dolphin Wave 120 Robotic Pool Cleaner", None, None);
        assert_eq!(rules.suggest(&cleaner).unwrap().class, "drop");
        let shipping = facts(None, "SHIP-U-GND", None, Some("UPS"));
        assert_eq!(rules.suggest(&shipping), None);
    }

    #[test]
    fn the_first_rule_that_fires_wins_and_each_pattern_kind_matches() {
        let rules = Rules::parse(
            r#"
            [[rule]]
            name = "by-account"
            class = "cnc"
            income_account = ["CNC Income"]
            [[rule]]
            name = "by-category"
            class = "drop"
            category = ["vendor x"]
            [[rule]]
            name = "by-name"
            class = "sign"
            name_contains = ["sign"]
            [[rule]]
            name = "by-sku"
            class = "foam"
            sku_prefix = ["rt-"]
            "#,
            "aquamentor",
        )
        .unwrap();
        let both = facts(Some("RT-1"), "Tube sign", None, None);
        assert_eq!(fired(&rules, &both).as_deref(), Some("by-name"));
        let sku = facts(Some("rt-9"), "Tube", None, None);
        assert_eq!(fired(&rules, &sku).as_deref(), Some("by-sku"));
        let dept = facts(None, "4 RT-9", None, None);
        assert_eq!(fired(&rules, &dept).as_deref(), Some("by-sku"));
        let category = facts(None, "Widget", Some("VENDOR X:PARTS"), None);
        assert_eq!(fired(&rules, &category).as_deref(), Some("by-category"));
        let account = facts(None, "Widget sign", None, Some("cnc income"));
        assert_eq!(fired(&rules, &account).as_deref(), Some("by-account"));
        let neither = facts(Some("X-1"), "Widget", None, Some("Sales"));
        assert_eq!(fired(&rules, &neither), None);
    }

    #[test]
    fn a_bad_rules_file_is_refused_with_every_problem() {
        let err = Rules::parse(
            r#"
            [[rule]]
            name = "a"
            class = "unclassified"
            sku_prefix = ["X"]
            [[rule]]
            name = "a"
            class = "foam"
            [[rule]]
            name = "c"
            class = "foam"
            name_contains = [""]
            "#,
            "aquamentor",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"unclassified\" is not one of"), "{err}");
        assert!(err.contains("same name as rule 1"), "{err}");
        assert!(err.contains("no patterns"), "{err}");
        assert!(err.contains("empty pattern"), "{err}");

        let waterline = Rules::parse(DEFAULT_RULES, "waterline")
            .unwrap_err()
            .to_string();
        assert!(
            waterline.contains("\"foam\" is not one of cnc, uv"),
            "{waterline}"
        );
    }

    #[test]
    fn reviewed_csv_reads_qbo_id_and_class_in_any_column_order() {
        let text =
            "\u{feff}name,class,qbo_id\r\n\"Tube, red\",Foam,10\r\n\r\n\"Mat \"\"big\"\"\",,11\r\n";
        let rows = parse_reviewed(text).unwrap();
        assert_eq!(
            rows,
            vec![
                ReviewedRow {
                    line: 2,
                    qbo_id: "10".into(),
                    class: "foam".into(),
                    name: Some("Tube, red".into()),
                },
                ReviewedRow {
                    line: 4,
                    qbo_id: "11".into(),
                    class: String::new(),
                    name: Some("Mat \"big\"".into()),
                },
            ]
        );
        assert!(matches!(
            parse_reviewed("qbo_id,suggested_class\n1,foam\n"),
            Err(WorksheetError::Refused(_))
        ));
    }

    #[test]
    fn the_window_is_the_twelve_months_ending_on_the_as_of_date() {
        let as_of = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        assert_eq!(
            sales_window(as_of),
            (NaiveDate::from_ymd_opt(2025, 10, 17).unwrap(), as_of)
        );
    }
}
