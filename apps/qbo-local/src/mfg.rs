//! Manufacturing costing: the part QuickBooks Online has no tier for.
//!
//! Cost flows from the sheet to the finished product:
//!
//! 1. **Landed cost.** A sheet's true cost is its base price plus surcharge plus
//!    its share of inbound freight. Freight is allocated by **board-foot**,
//!    never by line count or by dollars: splitting a mixed pallet evenly makes
//!    the cheap material look dear, and every margin downstream inherits it.
//! 2. **Build sheets.** A product's unit cost is its material (net board-feet
//!    divided by nesting yield, at the landed $/bf), its bought parts, and its
//!    labour. Yield *divides*: shipping 2.1 bf of part at 78% yield means
//!    buying 2.69 bf, and all of it was paid for.
//! 3. **Builds.** What a production run actually consumed against what its
//!    build sheet said, with the variance priced.
//!
//! Everything here is pure arithmetic over [`Decimal`], so it is exact and
//! tested without a database. Unit costs carry four decimal places, as cost
//! rates do; anything that is a sum of money (a freight share, a run's cost)
//! is rounded to the cent once, with the shares made to add up exactly.
//!
//! Nothing here posts to a ledger or writes to QuickBooks. It is costing — an
//! answer to "what does this product cost me" — and lives in the replica's
//! own tables, which QuickBooks never sees.

use std::str::FromStr;

use ledger_core::{round_money, Money, RoundingPolicy};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CostError {
    #[error("{0}")]
    Invalid(String),
    #[error("arithmetic overflow")]
    Overflow,
}

fn invalid(message: impl Into<String>) -> CostError {
    CostError::Invalid(message.into())
}

/// Parse a decimal typed by a person: commas allowed, nothing else.
pub fn decimal(raw: &str, what: &str) -> Result<Decimal, CostError> {
    Decimal::from_str(raw.trim().replace(',', "").as_str()).map_err(|_| invalid(format!("{what} {raw:?} is not a number")))
}

/// Cost rates ($/bf, $/unit) are kept to four places; that is the precision a
/// vendor's price sheet uses and enough that rounding never shows in a total.
/// Trailing zeros are dropped, so the same rate always prints the same way.
pub fn rate(value: Decimal) -> Decimal {
    value.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero).normalize()
}

fn money(value: Decimal) -> Result<Money, CostError> {
    round_money(value, RoundingPolicy::LineExtension).map_err(|_| CostError::Overflow)
}

fn to_decimal(m: Money) -> Decimal {
    Decimal::new(m.minor(), 2)
}

/// Board-feet in one piece: thickness × width × length in inches, over 144.
pub fn board_feet(thickness_in: Decimal, width_in: Decimal, length_in: Decimal) -> Result<Decimal, CostError> {
    if thickness_in <= Decimal::ZERO || width_in <= Decimal::ZERO || length_in <= Decimal::ZERO {
        return Err(invalid("every dimension must be more than zero"));
    }
    thickness_in
        .checked_mul(width_in)
        .and_then(|a| a.checked_mul(length_in))
        .map(|v| v / Decimal::from(144))
        .ok_or(CostError::Overflow)
}

// ---------------------------------------------------------------------------
// Landed cost
// ---------------------------------------------------------------------------

/// One line of a material receipt: sheets of one material on one delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptLine {
    pub material_id: String,
    pub sheets: Decimal,
    /// Board-feet in one sheet of this material.
    pub bf_per_sheet: Decimal,
    /// Vendor's price per sheet before surcharge.
    pub price_per_sheet: Decimal,
    /// Per-sheet surcharge (fuel, resin), if the vendor bills one.
    pub surcharge_per_sheet: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LandedLine {
    pub material_id: String,
    pub board_feet: Decimal,
    /// Base plus surcharge for the whole line, to the cent.
    pub goods: Money,
    /// This line's share of the freight, to the cent.
    pub freight: Money,
    pub landed: Money,
    /// Landed cost per board-foot, four places.
    pub landed_per_bf: Decimal,
}

/// Allocate `freight` across a receipt's lines in proportion to board-feet,
/// to the cent, with the shares summing exactly to the freight.
///
/// Rounding each share independently can leave a cent unaccounted for or
/// double-counted. The largest-remainder method gives each line its floor
/// share and hands the leftover cents, one each, to the lines whose exact
/// shares were cut the most — so the freight on the bill and the freight in
/// the costs are always the same number.
pub fn landed_cost(lines: &[ReceiptLine], freight: Money) -> Result<Vec<LandedLine>, CostError> {
    if lines.is_empty() {
        return Err(invalid("a receipt needs at least one line"));
    }
    if freight.is_negative() {
        return Err(invalid("freight cannot be negative"));
    }
    let mut bf = Vec::with_capacity(lines.len());
    for line in lines {
        if line.sheets <= Decimal::ZERO || line.bf_per_sheet <= Decimal::ZERO {
            return Err(invalid(format!("{}: sheets and board-feet per sheet must be more than zero", line.material_id)));
        }
        if line.price_per_sheet < Decimal::ZERO || line.surcharge_per_sheet < Decimal::ZERO {
            return Err(invalid(format!("{}: prices cannot be negative", line.material_id)));
        }
        bf.push(line.sheets.checked_mul(line.bf_per_sheet).ok_or(CostError::Overflow)?);
    }
    let total_bf: Decimal = bf.iter().sum();

    // Shares in cents: floor first, then distribute the remainder.
    let freight_cents = Decimal::from(freight.minor());
    let exact: Vec<Decimal> = bf.iter().map(|b| freight_cents * b / total_bf).collect();
    let mut cents: Vec<i64> = exact.iter().map(|e| e.floor().try_into().unwrap_or(0)).collect();
    let mut leftover = freight.minor() - cents.iter().sum::<i64>();
    let mut order: Vec<usize> = (0..lines.len()).collect();
    // Largest fractional part first; ties to the earlier line, so the result
    // does not depend on sort stability.
    order.sort_by(|&a, &b| (exact[b] - exact[b].floor()).cmp(&(exact[a] - exact[a].floor())).then(a.cmp(&b)));
    for &index in order.iter().cycle() {
        if leftover <= 0 {
            break;
        }
        cents[index] += 1;
        leftover -= 1;
    }

    lines
        .iter()
        .zip(bf)
        .zip(cents)
        .map(|((line, board_feet), share)| {
            let per_sheet = line.price_per_sheet + line.surcharge_per_sheet;
            let goods = money(per_sheet.checked_mul(line.sheets).ok_or(CostError::Overflow)?)?;
            let freight = Money::from_minor(share);
            let landed = goods.checked_add(freight).map_err(|_| CostError::Overflow)?;
            Ok(LandedLine {
                material_id: line.material_id.clone(),
                board_feet,
                goods,
                freight,
                landed,
                landed_per_bf: rate(to_decimal(landed) / board_feet),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Build sheets
// ---------------------------------------------------------------------------

/// One component of a build sheet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Component {
    /// Cut from sheet stock. `net_bf` is what ends up in the part; `yield_pct`
    /// is how much of the bought sheet becomes part, after nesting and kerf.
    #[serde(rename_all = "camelCase")]
    Material { material_id: String, net_bf: Decimal, yield_pct: Decimal },
    /// Bought in finished: hardware, webbing, a printed decal.
    #[serde(rename_all = "camelCase")]
    Part { name: String, item_id: Option<String>, qty: Decimal, unit_cost: Decimal },
    /// Machine or bench time.
    #[serde(rename_all = "camelCase")]
    Labour { name: String, minutes: Decimal, rate_per_hour: Decimal },
}

/// What one component costs per finished unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostLine {
    pub label: String,
    /// Board-feet bought per unit, for materials: net over yield.
    pub gross_bf: Option<Decimal>,
    /// The part of `gross_bf` that is waste.
    pub waste_bf: Option<Decimal>,
    pub cost: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitCost {
    pub lines: Vec<CostLine>,
    pub material: Decimal,
    pub parts: Decimal,
    pub labour: Decimal,
    /// Four places: a rate, not yet a sum of money.
    pub total: Decimal,
    /// Cost of the material bought and thrown away, per unit.
    pub waste: Decimal,
}

/// Look up a material's landed $/bf and display name.
pub trait MaterialPrices {
    fn per_bf(&self, material_id: &str) -> Option<Decimal>;
    fn name(&self, material_id: &str) -> String {
        material_id.to_string()
    }
}

impl MaterialPrices for std::collections::HashMap<String, Decimal> {
    fn per_bf(&self, material_id: &str) -> Option<Decimal> {
        self.get(material_id).copied()
    }
}

fn check_yield(yield_pct: Decimal) -> Result<Decimal, CostError> {
    if yield_pct <= Decimal::ZERO || yield_pct > Decimal::from(100) {
        return Err(invalid(format!("yield {yield_pct}% must be more than 0 and at most 100")));
    }
    Ok(yield_pct / Decimal::from(100))
}

/// Unit cost of a build sheet at the given prices, with every material price
/// moved by `price_move_pct` and every yield moved by `yield_move_pts`
/// percentage points (both zero for the plain cost).
pub fn unit_cost_with(
    components: &[Component],
    prices: &dyn MaterialPrices,
    price_move_pct: Decimal,
    yield_move_pts: Decimal,
) -> Result<UnitCost, CostError> {
    if components.is_empty() {
        return Err(invalid("a build sheet needs at least one component"));
    }
    let price_factor = Decimal::ONE + price_move_pct / Decimal::from(100);
    let mut lines = Vec::new();
    let (mut material, mut parts, mut labour, mut waste) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    for component in components {
        match component {
            Component::Material { material_id, net_bf, yield_pct } => {
                if *net_bf <= Decimal::ZERO {
                    return Err(invalid(format!("{}: net board-feet must be more than zero", prices.name(material_id))));
                }
                let per_bf = prices
                    .per_bf(material_id)
                    .ok_or_else(|| invalid(format!("{} has no landed cost yet; record a receipt for it", prices.name(material_id))))?
                    * price_factor;
                let y = check_yield(*yield_pct + yield_move_pts)?;
                let gross = *net_bf / y;
                let cost = gross * per_bf;
                let waste_bf = gross - *net_bf;
                material += cost;
                waste += waste_bf * per_bf;
                lines.push(CostLine {
                    label: prices.name(material_id),
                    gross_bf: Some(rate(gross)),
                    waste_bf: Some(rate(waste_bf)),
                    cost: rate(cost),
                });
            }
            Component::Part { name, qty, unit_cost, .. } => {
                if *qty <= Decimal::ZERO || *unit_cost < Decimal::ZERO {
                    return Err(invalid(format!("{name}: quantity must be more than zero and cost not negative")));
                }
                let cost = *qty * *unit_cost;
                parts += cost;
                lines.push(CostLine { label: name.clone(), gross_bf: None, waste_bf: None, cost: rate(cost) });
            }
            Component::Labour { name, minutes, rate_per_hour } => {
                if *minutes < Decimal::ZERO || *rate_per_hour < Decimal::ZERO {
                    return Err(invalid(format!("{name}: minutes and rate cannot be negative")));
                }
                let cost = *minutes / Decimal::from(60) * *rate_per_hour;
                labour += cost;
                lines.push(CostLine { label: name.clone(), gross_bf: None, waste_bf: None, cost: rate(cost) });
            }
        }
    }
    Ok(UnitCost {
        lines,
        material: rate(material),
        parts: rate(parts),
        labour: rate(labour),
        total: rate(material + parts + labour),
        waste: rate(waste),
    })
}

pub fn unit_cost(components: &[Component], prices: &dyn MaterialPrices) -> Result<UnitCost, CostError> {
    unit_cost_with(components, prices, Decimal::ZERO, Decimal::ZERO)
}

/// Gross margin as a percentage of price, one place. `None` without a price.
pub fn margin_pct(price: Option<Decimal>, cost: Decimal) -> Option<Decimal> {
    let price = price.filter(|p| *p > Decimal::ZERO)?;
    Some(((price - cost) / price * Decimal::from(100)).round_dp_with_strategy(1, RoundingStrategy::MidpointAwayFromZero))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitivityCell {
    pub price_move_pct: Decimal,
    pub yield_move_pts: Decimal,
    pub unit_cost: Decimal,
    pub margin_pct: Option<Decimal>,
}

/// Unit cost and margin across material price moves and yield moves: the
/// "is a 5% foam discount worth more than five points of nesting" table.
/// Yield moves that would leave a material outside (0, 100] are skipped.
pub fn sensitivity(
    components: &[Component],
    prices: &dyn MaterialPrices,
    price: Option<Decimal>,
    price_moves: &[Decimal],
    yield_moves: &[Decimal],
) -> Result<Vec<SensitivityCell>, CostError> {
    let mut out = Vec::new();
    for &y in yield_moves {
        for &p in price_moves {
            match unit_cost_with(components, prices, p, y) {
                Ok(cost) => out.push(SensitivityCell {
                    price_move_pct: p,
                    yield_move_pts: y,
                    unit_cost: cost.total,
                    margin_pct: margin_pct(price, cost.total),
                }),
                Err(CostError::Invalid(message)) if message.starts_with("yield") => {}
                Err(other) => return Err(other),
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

/// What a production run actually used of one material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActualMaterial {
    pub material_id: String,
    pub board_feet: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VarianceLine {
    pub material_id: String,
    pub standard_bf: Decimal,
    pub actual_bf: Decimal,
    /// Positive when the run used more than the sheet said: an unfavourable variance.
    pub variance_bf: Decimal,
    pub variance_cost: Money,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildVariance {
    pub quantity: Decimal,
    pub standard_cost: Money,
    pub actual_cost: Money,
    pub variance: Money,
    pub lines: Vec<VarianceLine>,
    /// More than this share over standard on any material is flagged: one
    /// overrun is noise, the same product over every time is a build sheet
    /// that is wrong.
    pub flagged: bool,
}

pub const OVERRUN_FLAG_PCT: i64 = 5;

/// Board-feet of each material a run took off the shelf: what was reported,
/// or the sheet's gross board-feet where nothing was.
pub fn material_use(components: &[Component], quantity: Decimal, actual: &[ActualMaterial]) -> Result<Vec<(String, Decimal)>, CostError> {
    let mut out: Vec<(String, Decimal)> = Vec::new();
    for component in components {
        if let Component::Material { material_id, net_bf, yield_pct } = component {
            let reported: Vec<_> = actual.iter().filter(|a| &a.material_id == material_id).collect();
            let used = if reported.is_empty() {
                *net_bf / check_yield(*yield_pct)? * quantity
            } else {
                reported.iter().map(|a| a.board_feet).sum()
            };
            match out.iter_mut().find(|(m, _)| m == material_id) {
                // The same material twice on a sheet: a reported figure covers both.
                Some((_, total)) if reported.is_empty() => *total += used,
                Some(_) => {}
                None => out.push((material_id.clone(), used)),
            }
        }
    }
    Ok(out)
}

/// Price a run against its build sheet. Materials the run did not report are
/// taken as used at standard; parts and labour are taken at standard.
pub fn build_variance(
    components: &[Component],
    prices: &dyn MaterialPrices,
    quantity: Decimal,
    actual: &[ActualMaterial],
) -> Result<BuildVariance, CostError> {
    if quantity <= Decimal::ZERO {
        return Err(invalid("a build makes at least one unit"));
    }
    let standard = unit_cost(components, prices)?;
    let mut lines = Vec::new();
    let mut variance_total = Decimal::ZERO;
    let mut flagged = false;
    for component in components {
        if let Component::Material { material_id, net_bf, yield_pct } = component {
            let standard_bf = *net_bf / check_yield(*yield_pct)? * quantity;
            let reported: Vec<_> = actual.iter().filter(|a| &a.material_id == material_id).collect();
            let actual_bf = if reported.is_empty() { standard_bf } else { reported.iter().map(|a| a.board_feet).sum() };
            let per_bf = prices.per_bf(material_id).unwrap_or(Decimal::ZERO);
            let variance_bf = actual_bf - standard_bf;
            let cost = variance_bf * per_bf;
            variance_total += cost;
            if variance_bf * Decimal::from(100) > standard_bf * Decimal::from(OVERRUN_FLAG_PCT) {
                flagged = true;
            }
            lines.push(VarianceLine {
                material_id: material_id.clone(),
                standard_bf: rate(standard_bf),
                actual_bf: rate(actual_bf),
                variance_bf: rate(variance_bf),
                variance_cost: money(cost)?,
            });
        }
    }
    let standard_cost = money(standard.total * quantity)?;
    let variance = money(variance_total)?;
    Ok(BuildVariance {
        quantity,
        standard_cost,
        actual_cost: standard_cost.checked_add(variance).map_err(|_| CostError::Overflow)?,
        variance,
        lines,
        flagged,
    })
}

// ---------------------------------------------------------------------------
// Stock on hand and moving-average cost
// ---------------------------------------------------------------------------

/// Something that changed how much of a material is on the shelf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StockEvent {
    /// A delivery: board-feet in, at its landed cost.
    Received { date: String, reference: String, board_feet: Decimal, landed: Money },
    /// A production run used this much.
    Consumed { date: String, reference: String, board_feet: Decimal },
    /// A physical count: the shelf holds exactly this much.
    Counted { date: String, reference: String, board_feet: Decimal },
}

impl StockEvent {
    fn date(&self) -> &str {
        match self {
            StockEvent::Received { date, .. } | StockEvent::Consumed { date, .. } | StockEvent::Counted { date, .. } => date,
        }
    }

    /// Same-day order: deliveries land before the day's cutting, and a count
    /// is taken at the end of the day it is dated.
    fn rank(&self) -> u8 {
        match self {
            StockEvent::Received { .. } => 0,
            StockEvent::Consumed { .. } => 1,
            StockEvent::Counted { .. } => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StockMove {
    pub date: String,
    pub kind: &'static str,
    pub reference: String,
    /// Board-feet in (positive) or out (negative).
    pub change_bf: Decimal,
    pub on_hand_bf: Decimal,
    pub average_per_bf: Decimal,
    /// What the move did to the value on the shelf, to the cent.
    pub value_change: Money,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StockState {
    pub on_hand_bf: Decimal,
    /// Perpetual weighted average: each delivery blends in at its landed
    /// cost; each use and each count difference leaves at the average.
    pub average_per_bf: Option<Decimal>,
    pub value: Money,
    /// Board-feet a count found missing (positive) or extra (negative), in total.
    pub shrinkage_bf: Decimal,
    pub shrinkage_value: Money,
    /// Set when cutting ran the shelf below zero: a delivery or a count is
    /// missing, and the average is not to be trusted until it is entered.
    pub went_negative: bool,
    pub moves: Vec<StockMove>,
}

/// Replay a material's history in date order into on-hand, average cost and
/// value. Value is carried exactly and rounded only when reported, so a long
/// history does not accumulate rounding.
pub fn stock(mut events: Vec<StockEvent>) -> Result<StockState, CostError> {
    events.sort_by(|a, b| a.date().cmp(b.date()).then(a.rank().cmp(&b.rank())));
    let mut on_hand = Decimal::ZERO;
    let mut value = Decimal::ZERO;
    let mut average: Option<Decimal> = None;
    let (mut shrink_bf, mut shrink_value) = (Decimal::ZERO, Decimal::ZERO);
    let mut went_negative = false;
    let mut moves = Vec::new();
    for event in events {
        let before = value;
        let (kind, reference, change) = match &event {
            StockEvent::Received { reference, board_feet, landed, .. } => {
                if *board_feet <= Decimal::ZERO {
                    return Err(invalid("a delivery brings in more than zero board-feet"));
                }
                // Negative stock is a gap in the record; a delivery fills it
                // at its own price rather than averaging against a hole.
                let landed = to_decimal(*landed);
                if on_hand <= Decimal::ZERO {
                    value = landed + on_hand * (landed / *board_feet);
                } else {
                    value += landed;
                }
                on_hand += *board_feet;
                average = Some(if on_hand > Decimal::ZERO { value / on_hand } else { landed / *board_feet });
                ("received", reference.clone(), *board_feet)
            }
            StockEvent::Consumed { reference, board_feet, .. } => {
                let avg = average.unwrap_or(Decimal::ZERO);
                on_hand -= *board_feet;
                value -= *board_feet * avg;
                if on_hand < Decimal::ZERO {
                    went_negative = true;
                }
                ("used", reference.clone(), -*board_feet)
            }
            StockEvent::Counted { reference, board_feet, .. } => {
                if *board_feet < Decimal::ZERO {
                    return Err(invalid("a count cannot be negative"));
                }
                let avg = average.unwrap_or(Decimal::ZERO);
                let difference = *board_feet - on_hand;
                shrink_bf -= difference;
                shrink_value -= difference * avg;
                on_hand = *board_feet;
                value = on_hand * avg;
                ("counted", reference.clone(), difference)
            }
        };
        moves.push(StockMove {
            date: event.date().to_string(),
            kind,
            reference,
            change_bf: rate(change),
            on_hand_bf: rate(on_hand),
            average_per_bf: rate(average.unwrap_or(Decimal::ZERO)),
            value_change: money(value - before)?,
        });
    }
    Ok(StockState {
        on_hand_bf: rate(on_hand),
        average_per_bf: average.map(rate),
        value: money(value)?,
        shrinkage_bf: rate(shrink_bf),
        shrinkage_value: money(shrink_value)?,
        went_negative,
        moves,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rust_decimal_macros::dec;

    use super::*;

    fn line(id: &str, sheets: Decimal, bf: Decimal, price: Decimal) -> ReceiptLine {
        ReceiptLine { material_id: id.into(), sheets, bf_per_sheet: bf, price_per_sheet: price, surcharge_per_sheet: Decimal::ZERO }
    }

    #[test]
    fn board_feet_of_a_standard_sheet() {
        assert_eq!(board_feet(dec!(2), dec!(48), dec!(96)).unwrap(), dec!(64));
        assert_eq!(board_feet(dec!(0.5), dec!(24), dec!(12)).unwrap(), dec!(1));
        assert!(board_feet(dec!(0), dec!(48), dec!(96)).is_err());
    }

    #[test]
    fn freight_follows_board_feet_not_dollars_or_lines() {
        // 40 thick sheets and 40 thin ones, the thin ones dearer per sheet.
        let lines = [line("thick", dec!(40), dec!(64), dec!(90)), line("thin", dec!(40), dec!(16), dec!(120))];
        let landed = landed_cost(&lines, Money::from_minor(40_000)).unwrap();
        assert_eq!(landed[0].freight, Money::from_minor(32_000), "four fifths of the board-feet, four fifths of the freight");
        assert_eq!(landed[1].freight, Money::from_minor(8_000));
        // 40 × 90 + 320 = 3,920 over 2,560 bf.
        assert_eq!(landed[0].landed_per_bf, dec!(1.5313));
    }

    #[test]
    fn freight_shares_add_up_to_the_cent() {
        // Three equal lines and a freight that does not divide by three.
        let lines = [
            line("a", dec!(1), dec!(10), dec!(1)),
            line("b", dec!(1), dec!(10), dec!(1)),
            line("c", dec!(1), dec!(10), dec!(1)),
        ];
        let landed = landed_cost(&lines, Money::from_minor(100)).unwrap();
        let shares: Vec<i64> = landed.iter().map(|l| l.freight.minor()).collect();
        assert_eq!(shares.iter().sum::<i64>(), 100);
        assert_eq!(shares, vec![34, 33, 33], "the leftover cent goes to the earliest of equal remainders");
    }

    #[test]
    fn surcharge_is_part_of_landed_cost() {
        let mut l = line("eva", dec!(10), dec!(8), dec!(50));
        l.surcharge_per_sheet = dec!(3.25);
        let landed = landed_cost(&[l], Money::from_minor(0)).unwrap();
        assert_eq!(landed[0].goods, Money::from_minor(53_250));
        assert_eq!(landed[0].landed_per_bf, dec!(6.6563));
    }

    fn prices() -> HashMap<String, Decimal> {
        HashMap::from([("xlpe-2".to_string(), dec!(1.50))])
    }

    fn tube() -> Vec<Component> {
        vec![
            Component::Material { material_id: "xlpe-2".into(), net_bf: dec!(2.1), yield_pct: dec!(78) },
            Component::Part { name: "Webbing strap".into(), item_id: None, qty: dec!(1), unit_cost: dec!(1.85) },
            Component::Labour { name: "CNC and finish".into(), minutes: dec!(6), rate_per_hour: dec!(30) },
        ]
    }

    #[test]
    fn yield_divides_rather_than_subtracts() {
        let cost = unit_cost(&tube(), &prices()).unwrap();
        // 2.1 / 0.78 = 2.6923 bf bought, at 1.50.
        assert_eq!(cost.lines[0].gross_bf, Some(dec!(2.6923)));
        assert_eq!(cost.material, dec!(4.0385));
        assert_eq!(cost.waste, dec!(0.8885));
        assert_eq!(cost.labour, dec!(3.0000));
        assert_eq!(cost.total, dec!(8.8885));
    }

    #[test]
    fn a_material_with_no_receipt_is_named_not_zeroed() {
        let error = unit_cost(&tube(), &HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("no landed cost"));
    }

    #[test]
    fn five_points_of_yield_against_five_percent_off_foam() {
        let grid = sensitivity(&tube(), &prices(), Some(dec!(24.95)), &[dec!(-5), dec!(0)], &[dec!(0), dec!(5)]).unwrap();
        let at = |p: Decimal, y: Decimal| grid.iter().find(|c| c.price_move_pct == p && c.yield_move_pts == y).unwrap().unit_cost;
        let discount = at(dec!(-5), dec!(0));
        let better_nesting = at(dec!(0), dec!(5));
        assert!(better_nesting < discount, "on this part, nesting beats the discount: {better_nesting} vs {discount}");
        assert_eq!(margin_pct(Some(dec!(24.95)), dec!(8.8885)), Some(dec!(64.4)));
    }

    #[test]
    fn impossible_yields_are_skipped_in_the_grid() {
        let grid = sensitivity(&tube(), &prices(), None, &[dec!(0)], &[dec!(25), dec!(0)]).unwrap();
        assert_eq!(grid.len(), 1, "78 + 25 is over 100");
    }

    fn received(date: &str, bf: Decimal, landed_cents: i64) -> StockEvent {
        StockEvent::Received { date: date.into(), reference: "PO".into(), board_feet: bf, landed: Money::from_minor(landed_cents) }
    }

    #[test]
    fn deliveries_blend_into_a_moving_average() {
        let state = stock(vec![
            received("2026-09-01", dec!(640), 96_000), // 1.50
            StockEvent::Consumed { date: "2026-09-10".into(), reference: "run".into(), board_feet: dec!(320) },
            received("2026-10-01", dec!(640), 128_000), // 2.00
        ])
        .unwrap();
        // 320 left at 1.50 plus 640 at 2.00 = 960 bf worth 1,760.
        assert_eq!(state.on_hand_bf, dec!(960));
        assert_eq!(state.value, Money::from_minor(176_000));
        assert_eq!(state.average_per_bf, Some(dec!(1.8333)));
        assert_eq!(state.moves[1].value_change, Money::from_minor(-48_000), "used at the average then, 1.50");
        assert!(!state.went_negative);
    }

    #[test]
    fn events_replay_in_date_order_whatever_order_they_arrive() {
        let a = stock(vec![
            StockEvent::Consumed { date: "2026-09-10".into(), reference: "run".into(), board_feet: dec!(100) },
            received("2026-09-01", dec!(640), 96_000),
        ])
        .unwrap();
        assert_eq!(a.on_hand_bf, dec!(540));
        assert!(!a.went_negative);
    }

    #[test]
    fn a_count_sets_the_shelf_and_prices_the_shrinkage() {
        let state = stock(vec![
            received("2026-09-01", dec!(640), 96_000),
            StockEvent::Counted { date: "2026-09-30".into(), reference: "month end".into(), board_feet: dec!(600) },
        ])
        .unwrap();
        assert_eq!(state.on_hand_bf, dec!(600));
        assert_eq!(state.shrinkage_bf, dec!(40));
        assert_eq!(state.shrinkage_value, Money::from_minor(6_000));
        assert_eq!(state.value, Money::from_minor(90_000));
    }

    #[test]
    fn cutting_past_zero_is_flagged_and_the_next_delivery_fills_the_gap_at_its_price() {
        let state = stock(vec![
            received("2026-09-01", dec!(100), 15_000),
            StockEvent::Consumed { date: "2026-09-02".into(), reference: "run".into(), board_feet: dec!(150) },
            received("2026-09-03", dec!(200), 40_000), // 2.00
        ])
        .unwrap();
        assert!(state.went_negative);
        assert_eq!(state.on_hand_bf, dec!(150));
        assert_eq!(state.average_per_bf, Some(dec!(2)));
        assert_eq!(state.value, Money::from_minor(30_000));
    }

    #[test]
    fn a_build_prices_its_overrun_and_flags_it() {
        // 100 tubes should use 269.2308 bf; the run used 290.
        let actual = [ActualMaterial { material_id: "xlpe-2".into(), board_feet: dec!(290) }];
        let run = build_variance(&tube(), &prices(), dec!(100), &actual).unwrap();
        assert_eq!(run.lines[0].standard_bf, dec!(269.2308));
        assert_eq!(run.lines[0].variance_bf, dec!(20.7692));
        assert_eq!(run.variance, Money::from_minor(3_115));
        assert_eq!(run.standard_cost, Money::from_minor(88_885));
        assert_eq!(run.actual_cost, Money::from_minor(92_000));
        assert!(run.flagged);

        let on_standard = build_variance(&tube(), &prices(), dec!(100), &[]).unwrap();
        assert_eq!(on_standard.variance, Money::from_minor(0));
        assert!(!on_standard.flagged);
    }
}
