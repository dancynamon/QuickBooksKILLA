//! Loading real manufacturing data from a spreadsheet: materials, build
//! sheets and opening counts, pasted or uploaded as CSV.
//!
//! Parsing is separate from saving. Every row is checked and either becomes a
//! typed record or an error naming its line, so a preview can show exactly
//! what would happen before anything is written — and a file with one bad row
//! writes nothing until it is fixed.

use rust_decimal::Decimal;
use serde::Serialize;

use crate::mfg::{self, Component};

/// Split CSV text into rows of fields. Handles quoted fields with commas,
/// doubled quotes and line breaks inside quotes; ignores blank lines.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if field.is_empty() => quoted = true,
            (',', false) => row.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|f| !f.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            (c, _) => field.push(c),
        }
    }
    row.push(field);
    if row.iter().any(|f| !f.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowError {
    /// 1-based, counting the header, as a spreadsheet numbers it.
    pub line: usize,
    pub message: String,
}

/// Columns by header name, case and spacing ignored, so a spreadsheet's own
/// headings ("Thickness (in)") work as long as they start with the name.
struct Header(Vec<String>);

impl Header {
    fn new(row: &[String]) -> Self {
        Header(row.iter().map(|h| normalize(h)).collect())
    }

    fn find(&self, names: &[&str]) -> Option<usize> {
        self.0
            .iter()
            .position(|h| names.iter().any(|n| h.starts_with(&normalize(n))))
    }
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn cell(row: &[String], index: Option<usize>) -> &str {
    index
        .and_then(|i| row.get(i))
        .map(|s| s.trim())
        .unwrap_or("")
}

fn number(raw: &str, what: &str) -> Result<Decimal, String> {
    let cleaned = raw.trim().trim_start_matches('$').trim_end_matches('%');
    mfg::decimal(cleaned, what).map_err(|e| e.to_string())
}

fn required(index: Option<usize>, name: &str) -> Result<usize, RowError> {
    index.ok_or_else(|| RowError {
        line: 1,
        message: format!("no {name} column in the header"),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialImport {
    pub name: String,
    pub thickness_in: Decimal,
    pub width_in: Decimal,
    pub length_in: Decimal,
    pub color: Option<String>,
    pub reorder_bf: Option<Decimal>,
}

/// `name, thickness, width, length[, color][, reorder]` — inches and board-feet.
pub fn materials(text: &str) -> Result<Vec<MaterialImport>, Vec<RowError>> {
    let rows = parse_csv(text);
    let Some((head, body)) = rows.split_first() else {
        return Err(vec![RowError {
            line: 1,
            message: "the file is empty".into(),
        }]);
    };
    let h = Header::new(head);
    let columns = (|| {
        Ok((
            required(h.find(&["name", "material"]), "name")?,
            required(h.find(&["thickness", "thick"]), "thickness")?,
            required(h.find(&["width"]), "width")?,
            required(h.find(&["length"]), "length")?,
        ))
    })()
    .map_err(|e| vec![e])?;
    let (name, t, w, l) = columns;
    let (color, reorder) = (h.find(&["color", "colour"]), h.find(&["reorder"]));
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2;
        let parsed = (|| -> Result<MaterialImport, String> {
            let n = cell(row, Some(name));
            if n.is_empty() {
                return Err("no name".into());
            }
            let m = MaterialImport {
                name: n.to_string(),
                thickness_in: number(cell(row, Some(t)), "thickness")?,
                width_in: number(cell(row, Some(w)), "width")?,
                length_in: number(cell(row, Some(l)), "length")?,
                color: Some(cell(row, color).to_string()).filter(|c| !c.is_empty()),
                reorder_bf: Some(cell(row, reorder))
                    .filter(|r| !r.is_empty())
                    .map(|r| number(r, "reorder point"))
                    .transpose()?,
            };
            mfg::board_feet(m.thickness_in, m.width_in, m.length_in).map_err(|e| e.to_string())?;
            Ok(m)
        })();
        match parsed {
            Ok(m)
                if out
                    .iter()
                    .any(|o: &MaterialImport| o.name.eq_ignore_ascii_case(&m.name)) =>
            {
                errors.push(RowError {
                    line,
                    message: format!("{} appears twice", m.name),
                })
            }
            Ok(m) => out.push(m),
            Err(message) => errors.push(RowError { line, message }),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// One product's sheet, before names are resolved to ids.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetImport {
    /// SKU or product name as written; matched against QuickBooks items.
    pub product: String,
    pub lines: Vec<SheetLineImport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetLineImport {
    pub line: usize,
    pub kind: String,
    /// Material name, part name or labour step.
    pub what: String,
    pub amount: Decimal,
    pub rate: Decimal,
}

/// `product, kind, what, amount, rate` — one row per component:
///
/// - `material`: amount is net board-feet per unit, rate is yield %
/// - `part`: amount is quantity, rate is cost each
/// - `labour`: amount is minutes, rate is dollars per hour
///
/// Rows for the same product, in any order, make one sheet.
pub fn build_sheets(text: &str) -> Result<Vec<SheetImport>, Vec<RowError>> {
    let rows = parse_csv(text);
    let Some((head, body)) = rows.split_first() else {
        return Err(vec![RowError {
            line: 1,
            message: "the file is empty".into(),
        }]);
    };
    let h = Header::new(head);
    let columns = (|| {
        Ok((
            required(h.find(&["product", "sku", "item"]), "product")?,
            required(h.find(&["kind", "type"]), "kind")?,
            required(h.find(&["what", "component", "name"]), "what")?,
            required(h.find(&["amount", "qty", "quantity"]), "amount")?,
            required(h.find(&["rate", "yield", "cost"]), "rate")?,
        ))
    })()
    .map_err(|e| vec![e])?;
    let (product, kind, what, amount, rate) = columns;
    let mut sheets: Vec<SheetImport> = Vec::new();
    let mut errors = Vec::new();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2;
        let parsed = (|| -> Result<(String, SheetLineImport), String> {
            let p = cell(row, Some(product));
            if p.is_empty() {
                return Err("no product".into());
            }
            let k = cell(row, Some(kind)).to_lowercase();
            let k = match k.as_str() {
                "material" | "foam" | "sheet" => "material",
                "part" | "hardware" | "bought" => "part",
                "labour" | "labor" | "time" => "labour",
                other => return Err(format!("kind {other:?} is not material, part or labour")),
            };
            let w = cell(row, Some(what));
            if w.is_empty() {
                return Err("no component named".into());
            }
            Ok((
                p.to_string(),
                SheetLineImport {
                    line,
                    kind: k.to_string(),
                    what: w.to_string(),
                    amount: number(cell(row, Some(amount)), "amount")?,
                    rate: number(cell(row, Some(rate)), "rate")?,
                },
            ))
        })();
        match parsed {
            Ok((p, l)) => match sheets
                .iter_mut()
                .find(|s| s.product.eq_ignore_ascii_case(&p))
            {
                Some(sheet) => sheet.lines.push(l),
                None => sheets.push(SheetImport {
                    product: p,
                    lines: vec![l],
                }),
            },
            Err(message) => errors.push(RowError { line, message }),
        }
    }
    if errors.is_empty() {
        Ok(sheets)
    } else {
        Err(errors)
    }
}

/// Turn a sheet's lines into components, resolving material names with
/// `material_id`. Errors carry the CSV line they came from.
pub fn components(
    sheet: &SheetImport,
    material_id: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Component>, Vec<RowError>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for l in &sheet.lines {
        match l.kind.as_str() {
            "material" => match material_id(&l.what) {
                Some(id) => out.push(Component::Material {
                    material_id: id,
                    net_bf: l.amount,
                    yield_pct: l.rate,
                }),
                None => errors.push(RowError {
                    line: l.line,
                    message: format!("no material named {:?}", l.what),
                }),
            },
            "part" => out.push(Component::Part {
                name: l.what.clone(),
                item_id: None,
                qty: l.amount,
                unit_cost: l.rate,
            }),
            _ => out.push(Component::Labour {
                name: l.what.clone(),
                minutes: l.amount,
                rate_per_hour: l.rate,
            }),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountImport {
    pub line: usize,
    /// Material name, or product SKU or name.
    pub what: String,
    pub date: String,
    pub amount: Decimal,
    /// The unit the amount was counted in: "sheets", "bf", "units", or
    /// "default" when blank — sheets for a material, units for a product.
    pub unit: String,
}

/// `what, date, amount[, unit]` — unit is sheets (the default for a material),
/// bf, or units (for a product).
pub fn counts(text: &str) -> Result<Vec<CountImport>, Vec<RowError>> {
    let rows = parse_csv(text);
    let Some((head, body)) = rows.split_first() else {
        return Err(vec![RowError {
            line: 1,
            message: "the file is empty".into(),
        }]);
    };
    let h = Header::new(head);
    let columns = (|| {
        Ok((
            required(
                h.find(&["what", "material", "product", "sku", "name"]),
                "what",
            )?,
            required(h.find(&["date", "counted"]), "date")?,
            required(
                h.find(&["amount", "count", "qty", "quantity", "sheets"]),
                "amount",
            )?,
        ))
    })()
    .map_err(|e| vec![e])?;
    let (what, date, amount) = columns;
    let unit = h.find(&["unit"]);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2;
        let parsed = (|| -> Result<CountImport, String> {
            let w = cell(row, Some(what));
            if w.is_empty() {
                return Err("nothing named".into());
            }
            let d = cell(row, Some(date));
            let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .or_else(|_| chrono::NaiveDate::parse_from_str(d, "%m/%d/%Y"))
                .map_err(|_| format!("date {d:?} is not YYYY-MM-DD or MM/DD/YYYY"))?;
            let a = number(cell(row, Some(amount)), "count")?;
            if a < Decimal::ZERO {
                return Err("a count cannot be negative".into());
            }
            let u = match cell(row, unit).to_lowercase().as_str() {
                "" => "default",
                "sheet" | "sheets" | "sh" => "sheets",
                "bf" | "board-feet" | "boardfeet" | "board feet" => "bf",
                "unit" | "units" | "each" | "ea" | "pcs" => "units",
                other => return Err(format!("unit {other:?} is not sheets, bf or units")),
            };
            Ok(CountImport {
                line,
                what: w.to_string(),
                date: date.to_string(),
                amount: a,
                unit: u.to_string(),
            })
        })();
        match parsed {
            Ok(c) => out.push(c),
            Err(message) => errors.push(RowError { line, message }),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn csv_handles_quotes_commas_and_blank_lines() {
        let rows = parse_csv("a,\"b, c\",\"say \"\"hi\"\"\"\r\n\n1,2,3\n");
        assert_eq!(
            rows,
            vec![vec!["a", "b, c", "say \"hi\""], vec!["1", "2", "3"]]
        );
        assert_eq!(
            parse_csv("x,y"),
            vec![vec!["x", "y"]],
            "no trailing newline"
        );
    }

    #[test]
    fn materials_read_spreadsheet_headings_and_report_bad_rows_by_line() {
        let ok = materials("Material name,Thickness (in),Width (in),Length (in),Colour,Reorder at (bf)\nXLPE 2 in blue,2,48,96,blue,640\nEVA 1 in black,1,48,96,,\n").unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].reorder_bf, Some(dec!(640)));
        assert_eq!(ok[1].color, None);

        let bad =
            materials("name,thickness,width,length\nA,2,48,96\nB,0,48,96\na,1,1,1\n").unwrap_err();
        assert_eq!(bad.iter().map(|e| e.line).collect::<Vec<_>>(), vec![3, 4]);
        assert!(materials("name,width,length\nA,1,1\n").unwrap_err()[0]
            .message
            .contains("thickness"));
    }

    #[test]
    fn build_sheet_rows_group_by_product() {
        let csv = "Product,Kind,What,Amount,Rate\nRT-RED,material,XLPE 2 in red,2.1,78%\nRT-RED,labour,CNC,6,$30\nKB-BLU,material,XLPE 2 in blue,0.9,80\nrt-red,part,Strap,1,1.85\n";
        let sheets = build_sheets(csv).unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].lines.len(), 3, "the product match ignores case");
        assert_eq!(sheets[0].lines[0].rate, dec!(78));
        let resolve = |name: &str| (name == "XLPE 2 in red").then(|| "xlpe-red".to_string());
        let first = components(&sheets[0], &resolve).unwrap();
        assert!(
            matches!(&first[0], Component::Material { material_id, .. } if material_id == "xlpe-red")
        );
        assert_eq!(components(&sheets[1], &resolve).unwrap_err()[0].line, 4);
        assert!(
            build_sheets("product,kind,what,amount,rate\nX,glue,Y,1,1\n").unwrap_err()[0]
                .message
                .contains("not material")
        );
    }

    #[test]
    fn counts_take_either_date_style_and_a_unit() {
        let c = counts("What,Date,Count,Unit\nXLPE 2 in blue,2026-10-31,12,sheets\nRT-RED,10/31/2026,140,each\nEVA 1 in black,2026-10-31,300,bf\n").unwrap();
        assert_eq!(c[1].date, "2026-10-31");
        assert_eq!(c[1].unit, "units");
        assert_eq!(c[2].unit, "bf");
        assert!(counts("what,date,amount\nX,31-10-2026,1\n").unwrap_err()[0]
            .message
            .contains("date"));
    }
}
