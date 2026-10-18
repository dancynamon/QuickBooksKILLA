//! Storage for manufacturing costing (migration 4). See [`crate::mfg`].
//!
//! Inserts and updates only; nothing here deletes. A receipt recorded wrong
//! is corrected by recording another, and the history keeps both.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use ledger_core::Money;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::Serialize;

use super::{Store, StoreError};
use crate::domain::RealmId;
use crate::mfg::{ActualMaterial, BuildVariance, Component, LandedLine, MaterialPrices, ReceiptLine};

fn dec(raw: &str) -> Result<Decimal, StoreError> {
    Decimal::from_str(raw).map_err(|_| StoreError::Corrupt(format!("not a decimal: {raw:?}")))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Material {
    pub material_id: String,
    pub name: String,
    pub thickness_in: Decimal,
    pub width_in: Decimal,
    pub length_in: Decimal,
    pub color: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialRow {
    #[serde(flatten)]
    pub material: Material,
    pub bf_per_sheet: Decimal,
    /// The most recent receipt's landed cost per board-foot.
    pub landed_per_bf: Option<Decimal>,
    pub last_received: Option<String>,
    pub receipts: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptRow {
    pub receipt_id: String,
    pub received_on: String,
    pub vendor_id: Option<String>,
    pub reference: Option<String>,
    pub freight_minor: i64,
    pub lines: Vec<ReceiptLineRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptLineRow {
    pub material_id: String,
    pub sheets: Decimal,
    pub bf_per_sheet: Decimal,
    pub price_per_sheet: Decimal,
    pub surcharge_per_sheet: Decimal,
    pub goods_minor: i64,
    pub freight_minor: i64,
    pub landed_per_bf: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRow {
    pub build_id: String,
    pub item_id: String,
    pub built_on: String,
    pub quantity: Decimal,
    pub standard_minor: i64,
    pub actual_minor: i64,
    pub variance_minor: i64,
    pub flagged: bool,
    pub note: Option<String>,
    pub actual: Vec<ActualMaterial>,
}

/// Landed $/bf by material, plus names, for costing.
pub struct Prices {
    pub per_bf: HashMap<String, Decimal>,
    pub names: HashMap<String, String>,
}

impl MaterialPrices for Prices {
    fn per_bf(&self, material_id: &str) -> Option<Decimal> {
        self.per_bf.get(material_id).copied()
    }
    fn name(&self, material_id: &str) -> String {
        self.names.get(material_id).cloned().unwrap_or_else(|| material_id.to_string())
    }
}

impl Store {
    pub fn save_material(&self, realm: &RealmId, material: &Material, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO materials (realm_id, material_id, name, thickness_in, width_in, length_in, color, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(realm_id, material_id) DO UPDATE SET
                 name = excluded.name, thickness_in = excluded.thickness_in,
                 width_in = excluded.width_in, length_in = excluded.length_in, color = excluded.color",
            params![
                realm.as_str(), material.material_id, material.name, material.thickness_in.to_string(),
                material.width_in.to_string(), material.length_in.to_string(), material.color, now.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn material(&self, realm: &RealmId, material_id: &str) -> Result<Option<Material>, StoreError> {
        Ok(self.materials(realm)?.into_iter().map(|r| r.material).find(|m| m.material_id == material_id))
    }

    pub fn materials(&self, realm: &RealmId) -> Result<Vec<MaterialRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT m.material_id, m.name, m.thickness_in, m.width_in, m.length_in, m.color,
                    (SELECT l.landed_per_bf FROM material_receipt_lines l
                       JOIN material_receipts r ON r.realm_id = l.realm_id AND r.receipt_id = l.receipt_id
                      WHERE l.realm_id = m.realm_id AND l.material_id = m.material_id
                      ORDER BY r.received_on DESC, r.created_at DESC, l.line_no DESC LIMIT 1),
                    (SELECT MAX(r.received_on) FROM material_receipt_lines l
                       JOIN material_receipts r ON r.realm_id = l.realm_id AND r.receipt_id = l.receipt_id
                      WHERE l.realm_id = m.realm_id AND l.material_id = m.material_id),
                    (SELECT COUNT(DISTINCT l.receipt_id) FROM material_receipt_lines l
                      WHERE l.realm_id = m.realm_id AND l.material_id = m.material_id)
             FROM materials m WHERE m.realm_id = ?1 ORDER BY m.name COLLATE NOCASE",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?, row.get::<_, i64>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, t, w, l, color, landed, last, receipts) = row?;
            let (t, w, l) = (dec(&t)?, dec(&w)?, dec(&l)?);
            out.push(MaterialRow {
                bf_per_sheet: crate::mfg::board_feet(t, w, l).map_err(|e| StoreError::Corrupt(e.to_string()))?,
                material: Material { material_id: id, name, thickness_in: t, width_in: w, length_in: l, color },
                landed_per_bf: landed.as_deref().map(dec).transpose()?,
                last_received: last,
                receipts,
            });
        }
        Ok(out)
    }

    pub fn material_prices(&self, realm: &RealmId) -> Result<Prices, StoreError> {
        let rows = self.materials(realm)?;
        Ok(Prices {
            per_bf: rows.iter().filter_map(|r| r.landed_per_bf.map(|p| (r.material.material_id.clone(), p))).collect(),
            names: rows.into_iter().map(|r| (r.material.material_id, r.material.name)).collect(),
        })
    }

    /// Record a receipt and its landed lines in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn record_receipt(
        &self,
        realm: &RealmId,
        receipt_id: &str,
        received_on: &str,
        vendor_id: Option<&str>,
        reference: Option<&str>,
        freight: Money,
        lines: &[ReceiptLine],
        landed: &[LandedLine],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO material_receipts (realm_id, receipt_id, received_on, vendor_id, reference, freight_minor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![realm.as_str(), receipt_id, received_on, vendor_id, reference, freight.minor(), now.to_rfc3339()],
        )?;
        for (index, (line, out)) in lines.iter().zip(landed).enumerate() {
            transaction.execute(
                "INSERT INTO material_receipt_lines
                     (realm_id, receipt_id, line_no, material_id, sheets, bf_per_sheet, price_per_sheet,
                      surcharge_per_sheet, goods_minor, freight_minor, landed_per_bf)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    realm.as_str(), receipt_id, index as i64 + 1, line.material_id, line.sheets.to_string(),
                    line.bf_per_sheet.to_string(), line.price_per_sheet.to_string(), line.surcharge_per_sheet.to_string(),
                    out.goods.minor(), out.freight.minor(), out.landed_per_bf.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Receipts, newest first, optionally only those carrying one material.
    pub fn receipts(&self, realm: &RealmId, material_id: Option<&str>) -> Result<Vec<ReceiptRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT receipt_id, received_on, vendor_id, reference, freight_minor FROM material_receipts
             WHERE realm_id = ?1 ORDER BY received_on DESC, created_at DESC",
        )?;
        let heads = statement
            .query_map(params![realm.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?
            .collect::<Result<Vec<(String, String, Option<String>, Option<String>, i64)>, _>>()?;
        let mut lines_statement = self.connection.prepare(
            "SELECT material_id, sheets, bf_per_sheet, price_per_sheet, surcharge_per_sheet, goods_minor, freight_minor, landed_per_bf
             FROM material_receipt_lines WHERE realm_id = ?1 AND receipt_id = ?2 ORDER BY line_no",
        )?;
        let mut out = Vec::new();
        for (receipt_id, received_on, vendor_id, reference, freight_minor) in heads {
            let raw = lines_statement
                .query_map(params![realm.as_str(), receipt_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?, row.get::<_, String>(7)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut lines = Vec::new();
            for (material_id, sheets, bf, price, surcharge, goods_minor, freight_minor, landed) in raw {
                lines.push(ReceiptLineRow {
                    material_id, sheets: dec(&sheets)?, bf_per_sheet: dec(&bf)?, price_per_sheet: dec(&price)?,
                    surcharge_per_sheet: dec(&surcharge)?, goods_minor, freight_minor, landed_per_bf: dec(&landed)?,
                });
            }
            if material_id.is_none_or(|m| lines.iter().any(|l| l.material_id == m)) {
                out.push(ReceiptRow { receipt_id, received_on, vendor_id, reference, freight_minor, lines });
            }
        }
        Ok(out)
    }

    pub fn save_build_sheet(&self, realm: &RealmId, item_id: &str, components: &[Component], now: DateTime<Utc>) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO build_sheets (realm_id, item_id, components_json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(realm_id, item_id) DO UPDATE SET components_json = excluded.components_json, updated_at = excluded.updated_at",
            params![realm.as_str(), item_id, serde_json::to_string(components)?, now.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn build_sheet(&self, realm: &RealmId, item_id: &str) -> Result<Option<Vec<Component>>, StoreError> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT components_json FROM build_sheets WHERE realm_id = ?1 AND item_id = ?2",
                params![realm.as_str(), item_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.map(|r| serde_json::from_str(&r)).transpose()?)
    }

    /// Every build sheet as `(item id, components, updated at)`.
    pub fn build_sheets(&self, realm: &RealmId) -> Result<Vec<(String, Vec<Component>, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT item_id, components_json, updated_at FROM build_sheets WHERE realm_id = ?1")?;
        let rows = statement.query_map(params![realm.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (item, json, updated) = row?;
            out.push((item, serde_json::from_str(&json)?, updated));
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_build(
        &self,
        realm: &RealmId,
        build_id: &str,
        item_id: &str,
        built_on: &str,
        components: &[Component],
        actual: &[ActualMaterial],
        variance: &BuildVariance,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO builds (realm_id, build_id, item_id, built_on, quantity, components_json, actual_json,
                                 standard_minor, actual_minor, variance_minor, flagged, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                realm.as_str(), build_id, item_id, built_on, variance.quantity.to_string(),
                serde_json::to_string(components)?, serde_json::to_string(actual)?,
                variance.standard_cost.minor(), variance.actual_cost.minor(), variance.variance.minor(),
                variance.flagged as i64, note, now.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Builds, newest first, optionally for one product.
    pub fn builds(&self, realm: &RealmId, item_id: Option<&str>) -> Result<Vec<BuildRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT build_id, item_id, built_on, quantity, standard_minor, actual_minor, variance_minor, flagged, note, actual_json
             FROM builds WHERE realm_id = ?1 AND (?2 IS NULL OR item_id = ?2)
             ORDER BY built_on DESC, created_at DESC",
        )?;
        let rows = statement.query_map(params![realm.as_str(), item_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?, row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?, row.get::<_, String>(9)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (build_id, item_id, built_on, quantity, standard_minor, actual_minor, variance_minor, flagged, note, actual) = row?;
            out.push(BuildRow {
                build_id, item_id, built_on, quantity: dec(&quantity)?, standard_minor, actual_minor, variance_minor,
                flagged: flagged != 0, note, actual: serde_json::from_str(&actual)?,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::mfg::landed_cost;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 18, 12, 0, 0).unwrap()
    }

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test", now()).unwrap();
        store
            .save_material(&realm(), &Material {
                material_id: "xlpe-2-blue".into(), name: "XLPE 2 in blue".into(),
                thickness_in: dec!(2), width_in: dec!(48), length_in: dec!(96), color: Some("blue".into()),
            }, now())
            .unwrap();
        store
    }

    fn receive(store: &Store, id: &str, on: &str, price: Decimal) {
        let lines = vec![ReceiptLine { material_id: "xlpe-2-blue".into(), sheets: dec!(10), bf_per_sheet: dec!(64), price_per_sheet: price, surcharge_per_sheet: dec!(0) }];
        let landed = landed_cost(&lines, Money::from_minor(6_400)).unwrap();
        store.record_receipt(&realm(), id, on, None, Some("PO-1"), Money::from_minor(6_400), &lines, &landed, now()).unwrap();
    }

    #[test]
    fn the_newest_receipt_sets_the_landed_cost() {
        let store = store();
        assert_eq!(store.materials(&realm()).unwrap()[0].landed_per_bf, None);
        receive(&store, "r1", "2026-09-01", dec!(96));
        receive(&store, "r2", "2026-10-01", dec!(128));
        let row = &store.materials(&realm()).unwrap()[0];
        assert_eq!(row.bf_per_sheet, dec!(64));
        // (1280 + 64) / 640 bf
        assert_eq!(row.landed_per_bf, Some(dec!(2.1)));
        assert_eq!(row.receipts, 2);
        assert_eq!(store.receipts(&realm(), Some("xlpe-2-blue")).unwrap()[0].receipt_id, "r2");
        assert_eq!(store.material_prices(&realm()).unwrap().name("xlpe-2-blue"), "XLPE 2 in blue");
    }

    #[test]
    fn a_receipt_for_an_unknown_material_is_refused() {
        let store = store();
        let lines = vec![ReceiptLine { material_id: "ghost".into(), sheets: dec!(1), bf_per_sheet: dec!(1), price_per_sheet: dec!(1), surcharge_per_sheet: dec!(0) }];
        let landed = landed_cost(&lines, Money::from_minor(0)).unwrap();
        assert!(store.record_receipt(&realm(), "r9", "2026-10-01", None, None, Money::from_minor(0), &lines, &landed, now()).is_err());
        assert!(store.receipts(&realm(), None).unwrap().is_empty(), "the header rolled back with the line");
    }
}
