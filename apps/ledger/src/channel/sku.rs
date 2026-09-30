//! SKU to product-family mapping. `CHANNEL_POSTING_MAP.md` "Family items".
//!
//! The rule mapper is prefix/contains logic over the SKU, then over the
//! title. Rules cannot know every SKU (Amazon issues random SKUs like
//! `NI-20YW-OO4E`), so a title fallback exists, and anything neither
//! recognises is `Custom / Other` **and flagged** so it lands on the review
//! list instead of silently becoming revenue of a known family. A `manual`
//! row in `sku_map` always wins over the rules.

use std::collections::HashMap;

pub const TUBE_50: &str = "Rescue Tube 50\"";
pub const TUBE_40: &str = "Rescue Tube 40\"";
pub const TUBE_SECOND: &str = "Rescue Tube Factory Second";
pub const KICKBOARD: &str = "Kickboard";
pub const MAT: &str = "Floating Mat";
pub const NOODLE: &str = "Pool Noodle";
pub const BELT: &str = "Swim Belt / Flotation";
pub const WRISTBANDS: &str = "Wristbands & Tags";
pub const CHAIR: &str = "Lifeguard Chair";
pub const GUARD_EQUIP: &str = "Lifeguard Equipment";
pub const ROBOTIC: &str = "Robotic Pool Cleaner & Parts";
pub const SIGNS: &str = "Signs";
pub const POOL_GAMES: &str = "Pool Games & Basketball";
pub const WATER_FITNESS: &str = "Water Fitness";
pub const DOCK_BUMPERS: &str = "Dock Bumpers";
pub const KNEELING_PADS: &str = "Kneeling Pads";
pub const OTHER: &str = "Custom / Other";

/// (family, §3 class, display order). The class is what a family's revenue
/// posts under; editable per company in `product_families`.
pub const FAMILIES: &[(&str, &str, i64)] = &[
    (TUBE_50, "foam", 1),
    (TUBE_40, "foam", 2),
    (TUBE_SECOND, "foam", 3),
    (KICKBOARD, "foam", 4),
    (MAT, "foam", 5),
    (NOODLE, "foam", 6),
    (BELT, "foam", 7),
    (WRISTBANDS, "foam", 8),
    (CHAIR, "chair", 9),
    (GUARD_EQUIP, "drop", 10),
    (ROBOTIC, "drop", 11),
    (SIGNS, "sign", 12),
    (POOL_GAMES, "drop", 13),
    (WATER_FITNESS, "foam", 14),
    (DOCK_BUMPERS, "foam", 15),
    (KNEELING_PADS, "foam", 16),
    (OTHER, "foam", 17),
];

pub fn is_family(name: &str) -> bool {
    FAMILIES.iter().any(|(f, _, _)| *f == name)
}

/// The class a family posts under when the company has not changed it.
pub fn default_class(family: &str) -> &'static str {
    FAMILIES
        .iter()
        .find(|(f, _, _)| *f == family)
        .map_or("foam", |(_, class, _)| *class)
}

/// How a family was decided.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Basis {
    Sku,
    Title,
    /// Nothing matched: `Custom / Other`, flagged for review.
    Unmapped,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
    pub family: &'static str,
    pub basis: Basis,
}

impl Mapped {
    pub fn is_flagged(self) -> bool {
        self.basis == Basis::Unmapped
    }
}

/// Drops a leading numeric vendor prefix such as `13-` or `3 ` (`3 BT 01`,
/// `3 RWB-GR-100`, `13-XRT-50`).
fn strip_numeric_prefix(sku: &str) -> &str {
    let digits = sku.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &sku[digits..];
        if let Some(r) = rest.strip_prefix('-').or_else(|| rest.strip_prefix(' ')) {
            return r;
        }
    }
    sku
}

fn tube_size(sku: &str) -> Option<&'static str> {
    let rest = sku
        .strip_prefix("XRT")
        .or_else(|| sku.strip_prefix("RT"))?
        .trim_start_matches('-');
    if rest.starts_with("50") {
        Some(TUBE_50)
    } else if rest.starts_with("40") {
        Some(TUBE_40)
    } else {
        None
    }
}

fn is_color_code_wristband(s: &str) -> bool {
    // BL100, RD50, OR20-fbm, GR100, YL50, WH..., BK..., PK...
    const COLORS: &[&str] = &["BL", "RD", "GR", "YL", "OR", "WH", "BK", "PK", "PU"];
    let b = s.as_bytes();
    b.len() >= 3
        && COLORS.contains(&&s[..2])
        && b[2].is_ascii_digit()
        && s[2..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count()
            >= 2
}

/// Backboards `BB*`, hoops `B6000*`/`B1000*`/`RIM*`, balls `B1nn`, pool
/// stools `PS*`.
fn is_pool_game_sku(s: &str) -> bool {
    if s.starts_with("B6000") || s.starts_with("B1000") || s.starts_with("RIM") {
        return true;
    }
    if s.starts_with("BB") {
        return true;
    }
    // Balls: B1 followed by two more digits (B100, B125), not a hoop.
    let b = s.as_bytes();
    if b.len() >= 4 && s.starts_with("B1") && b[2].is_ascii_digit() && b[3].is_ascii_digit() {
        return true;
    }
    // Pool stools: PS followed by a digit, dash or space.
    if let Some(rest) = s.strip_prefix("PS") {
        return rest.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == ' ');
    }
    false
}

fn by_sku(sku_upper: &str) -> Option<&'static str> {
    let s = strip_numeric_prefix(sku_upper);
    if s.starts_with("2ND-RT") {
        return Some(TUBE_SECOND);
    }
    if let Some(f) = tube_size(s) {
        return Some(f);
    }
    if s.starts_with("KX") || s.starts_with("AMKX") || s.starts_with("SA-KX") {
        return Some(KICKBOARD);
    }
    if s.starts_with("PM-STOP") {
        return Some(SIGNS);
    }
    if s.starts_with("PM-") {
        return Some(MAT);
    }
    if s.starts_with("AM-32") {
        return Some(NOODLE);
    }
    if s.starts_with("AM-LGC") || s.starts_with("LGC-") {
        return Some(CHAIR);
    }
    if s.starts_with("DBB-") || s.starts_with("DBB ") {
        return Some(DOCK_BUMPERS);
    }
    if s.starts_with("AKP") || s.starts_with("AMKP") {
        return Some(KNEELING_PADS);
    }
    if s.starts_with("HR-WD") || s.starts_with("MR-WD") || s.contains("SB-") {
        return Some(WATER_FITNESS);
    }
    if is_pool_game_sku(s) {
        return Some(POOL_GAMES);
    }
    if s.contains("WB") || s.starts_with("BT ") || s.starts_with("BT-") {
        return Some(WRISTBANDS);
    }
    if is_color_code_wristband(s) {
        return Some(WRISTBANDS);
    }
    if s.starts_with("PR-") && s.contains("SIG") {
        return Some(SIGNS);
    }
    if s.starts_with("DPH") {
        return Some(ROBOTIC);
    }
    None
}

fn has_word(t: &str, word: &str) -> bool {
    t.split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

fn by_title(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    if t.contains("pool basketball")
        || t.contains("basketball hoop")
        || t.contains("pool volleyball")
        || t.contains("pool game")
        || t.contains("diving ring")
        || t.contains("diving stick")
        || t.contains("dive ring")
        || t.contains("pool stool")
        || t.contains("backboard")
    {
        return Some(POOL_GAMES);
    }
    // Stand-up paddleboard gear is not a pool game: Custom / Other, flagged.
    if has_word(&t, "sup") || t.contains("stand up paddle") || t.contains("stand-up paddle") {
        return None;
    }
    if t.contains("dock bumper") || t.contains("boat bumper") {
        return Some(DOCK_BUMPERS);
    }
    if t.contains("kneeling pad") || t.contains("kneeler") {
        return Some(KNEELING_PADS);
    }
    if t.contains("dumbbell")
        || t.contains("barbell")
        || t.contains("aqua fitness")
        || t.contains("water fitness")
        || t.contains("water aerobics")
        || t.contains("resistance")
    {
        return Some(WATER_FITNESS);
    }
    if t.contains("2nd quality") || t.contains("factory second") || t.contains("seconds") {
        return Some(TUBE_SECOND);
    }
    if t.contains("rescue tube") || t.contains("exotube") || t.contains("lifeguard tube") {
        if t.contains("50\"") || t.contains("50 inch") || t.contains("50-inch") {
            return Some(TUBE_50);
        }
        if t.contains("40\"") || t.contains("40 inch") || t.contains("40-inch") {
            return Some(TUBE_40);
        }
        return None;
    }
    if t.contains("lifeguard chair") {
        return Some(CHAIR);
    }
    if t.contains("kickboard") {
        return Some(KICKBOARD);
    }
    if t.contains("floating mat")
        || t.contains("pool mat")
        || t.contains("foam mat")
        || t.contains("swiss cheese")
    {
        return Some(MAT);
    }
    if t.contains("noodle") {
        return Some(NOODLE);
    }
    if t.contains("swim belt") || t.contains("flotation aid") || t.contains("back float swim belt") {
        return Some(BELT);
    }
    if t.contains("wristband") || t.contains("buddy tag") || t.contains("tyvek") {
        return Some(WRISTBANDS);
    }
    if has_word(&t, "sign") || has_word(&t, "signs") {
        return Some(SIGNS);
    }
    if t.contains("dolphin")
        || t.contains("maytronics")
        || t.contains("robotic")
        || t.contains("pool cleaner")
        || t.contains("pool vacuum")
        || t.contains("cordless pool")
        || t.contains("pool blaster")
        || has_word(&t, "bwt")
        || t.contains("for wave")
        || t.contains("wave 1")
        || t.contains("wave 2")
        || t.contains("wave 6")
        || t.contains("wave 8")
    {
        return Some(ROBOTIC);
    }
    if t.contains("whistle")
        || t.contains("buoy")
        || t.contains("umbrella")
        || t.contains("spineboard")
        || t.contains("manikin")
        || t.contains("paddleboard")
        || t.contains("life jacket")
        || t.contains("flotation jacket")
        || t.contains("pfd")
        || t.contains("smart pads")
        || t.contains("heartstart")
        || t.contains("lifeguard")
        || t.contains("rescue")
    {
        return Some(GUARD_EQUIP);
    }
    None
}

/// The rule mapper. SKU rules first, then the title, else `Custom / Other`
/// flagged.
pub fn map_rule(sku: &str, title: &str) -> Mapped {
    let upper = sku.trim().to_uppercase();
    if !upper.is_empty() {
        if let Some(family) = by_sku(&upper) {
            return Mapped {
                family,
                basis: Basis::Sku,
            };
        }
    }
    match by_title(title) {
        Some(family) => Mapped {
            family,
            basis: Basis::Title,
        },
        None => Mapped {
            family: OTHER,
            basis: Basis::Unmapped,
        },
    }
}

/// The operator's `manual` overrides in front of the rule mapper.
#[derive(Default, Clone)]
pub struct SkuMapper {
    manual: HashMap<(String, String), &'static str>,
}

impl SkuMapper {
    pub fn new() -> SkuMapper {
        SkuMapper::default()
    }

    /// Adds an override. `family` must be one of [`FAMILIES`]; anything else
    /// is ignored (a stored row naming a retired family must not invent one).
    pub fn set_manual(&mut self, channel: &str, sku: &str, family: &str) {
        if let Some((name, _, _)) = FAMILIES.iter().find(|(f, _, _)| *f == family) {
            self.manual
                .insert((channel.to_string(), sku.trim().to_uppercase()), name);
        }
    }

    pub fn resolve(&self, channel: &str, sku: &str, title: &str) -> Mapped {
        if let Some(family) = self
            .manual
            .get(&(channel.to_string(), sku.trim().to_uppercase()))
        {
            return Mapped {
                family,
                basis: Basis::Sku,
            };
        }
        map_rule(sku, title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fam(sku: &str, title: &str) -> &'static str {
        map_rule(sku, title).family
    }

    #[test]
    fn tube_sizes_by_sku() {
        for sku in ["XRT-50", "RT-50L", "13-XRT-50", "XRT-50RD", "RT-50-A", "XRT-50-YL"] {
            assert_eq!(fam(sku, ""), TUBE_50, "{sku}");
        }
        for sku in ["XRT-40", "RT-40L-M", "XRT-40B-01", "XRT-40-YL-fba", "RT-40-A"] {
            assert_eq!(fam(sku, ""), TUBE_40, "{sku}");
        }
    }

    #[test]
    fn factory_seconds_are_not_50_or_40() {
        assert_eq!(fam("2ND-RT-40-51", "2nd Quality Rescue Tubes"), TUBE_SECOND);
        assert_eq!(fam("", "Factory Second Rescue Tubes"), TUBE_SECOND);
    }

    #[test]
    fn spec_prefix_rules() {
        assert_eq!(fam("KX 120200 BL", ""), KICKBOARD);
        assert_eq!(fam("AMKX-1", ""), KICKBOARD);
        assert_eq!(fam("SA-KX-120200-BL", ""), KICKBOARD);
        assert_eq!(fam("KXSC-1", ""), KICKBOARD);
        assert_eq!(fam("PM-SC-YL-4824", ""), MAT);
        assert_eq!(fam("3 PM-RM-4824", ""), MAT);
        assert_eq!(fam("AM-3260BL-NCRD", ""), NOODLE);
        assert_eq!(fam("3 RWB-GR-100", ""), WRISTBANDS);
        assert_eq!(fam("3 WB 100-06", ""), WRISTBANDS);
        assert_eq!(fam("3 BT 01-Yellow", ""), WRISTBANDS);
        assert_eq!(fam("BL100", ""), WRISTBANDS);
        assert_eq!(fam("OR20-fbm", ""), WRISTBANDS);
        assert_eq!(fam("AM-LGC-72", ""), CHAIR);
        assert_eq!(fam("LGC-72-FBA", ""), CHAIR);
        assert_eq!(fam("PM-STOP-1212", "Foam STOP Sign"), SIGNS);
        assert_eq!(fam("PR-NEWJERSEYPOOLRULESSIG-2430-AL-STD", ""), SIGNS);
    }

    #[test]
    fn title_fallback_covers_random_skus() {
        assert_eq!(
            fam("PX-PIOC-A07D", "Aquamentor Lifeguard Chair for Pool, Lake and Beach (40\")"),
            CHAIR
        );
        assert_eq!(fam("", "Aquamentor Adult Swim Belt – Durable Flotation Aid"), BELT);
        assert_eq!(fam("99996240-XP", "Maytronics Dolphin Explorer E30"), ROBOTIC);
        assert_eq!(fam("125663102", "BWT F1RXT"), ROBOTIC);
        assert_eq!(fam("NI-20YW-OO4E", "Inflatable Lifeguard Rescue/Paddleboard"), GUARD_EQUIP);
        assert_eq!(fam("J9-NO41-TERT", "Fox 40 CMG Whistle with Cushioned Mouth Grip"), GUARD_EQUIP);
        assert_eq!(fam("L3-6VYY-17I7", "Aquamentor Junior Learn-to-Swim Kickboard"), KICKBOARD);
        assert_eq!(fam("XX-1", "Aquamentor 50\" ExoTube Rescue Tube (Yellow)"), TUBE_50);
    }

    #[test]
    fn unknown_is_custom_other_and_flagged() {
        let m = map_rule("ZZ-9", "Some Unrecognised Gadget");
        assert_eq!(m.family, OTHER);
        assert!(m.is_flagged());
        // A tube with no readable size is not guessed.
        assert!(map_rule("ZZ-1", "Aquamentor Rescue Tube").is_flagged());
        // A mapped SKU is not flagged.
        assert!(!map_rule("XRT-50", "").is_flagged());
    }

    #[test]
    fn manual_override_beats_rules() {
        let mut m = SkuMapper::new();
        m.set_manual("shopify", "zz-9", GUARD_EQUIP);
        assert_eq!(m.resolve("shopify", "ZZ-9", "").family, GUARD_EQUIP);
        assert_eq!(m.resolve("amazon", "ZZ-9", "").family, OTHER);
        // Unknown family names are refused, not invented.
        m.set_manual("shopify", "X", "Not A Family");
        assert!(m.resolve("shopify", "X", "").is_flagged());
    }

    #[test]
    fn pool_games_by_sku_and_title() {
        for sku in ["BB2000", "B6000 C(B)", "B6000-XL", "B1000-A", "RIM-18", "B125", "B101-R", "PS-12", "PS300"] {
            assert_eq!(fam(sku, ""), POOL_GAMES, "{sku}");
        }
        assert_eq!(fam("ZZ", "Splash & Shoot Clear Pool Basketball Hoop"), POOL_GAMES);
        assert_eq!(fam("ZZ", "Diving Rings Set of 6"), POOL_GAMES);
        // B1000 is a hoop, not a ball, and PSA-style SKUs are not stools.
        assert_eq!(fam("B1000", ""), POOL_GAMES);
        assert!(map_rule("PSA-1", "").is_flagged());
    }

    #[test]
    fn sup_gear_is_other_unless_titled_pool_game() {
        assert!(map_rule("SUP-FIN-1", "SUP Fin 9 inch").is_flagged());
        assert!(map_rule("SUP-PUMP", "Stand Up Paddle Board Hand Pump").is_flagged());
        assert_eq!(fam("SUP-X", "Pool game floating SUP basketball hoop"), POOL_GAMES);
    }

    #[test]
    fn water_fitness_dock_bumpers_kneeling_pads() {
        for sku in ["HR-WD-10", "MR-WD-8", "AM-SB-12", "SB-24"] {
            assert_eq!(fam(sku, ""), WATER_FITNESS, "{sku}");
        }
        assert_eq!(fam("ZZ", "Foam Aqua Fitness Dumbbells"), WATER_FITNESS);
        assert_eq!(fam("DBB-12", ""), DOCK_BUMPERS);
        assert_eq!(fam("ZZ", "Foam Dock Bumper 12 ft"), DOCK_BUMPERS);
        assert_eq!(fam("AKP-1", ""), KNEELING_PADS);
        assert_eq!(fam("AMKP-2", ""), KNEELING_PADS);
        assert_eq!(fam("ZZ", "Garden Kneeling Pad"), KNEELING_PADS);
        // Kickboards keep priority over the SB- rule.
        assert_eq!(fam("SA-KX-120200-BL", ""), KICKBOARD);
    }

    #[test]
    fn every_family_has_a_section_3_class() {
        for (family, class, _) in FAMILIES {
            assert!(
                ["foam", "sign", "chair", "drop", "cnc", "uv"].contains(class),
                "{family}"
            );
        }
        assert_eq!(FAMILIES.len(), 17);
    }
}
