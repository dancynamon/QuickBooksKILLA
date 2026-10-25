//! Class mapping. `LEDGER-DESIGN.md` §3, W15.

use qbo_local::project::ParsedClass;

use crate::chart;
use crate::types::{ClassId, CompanyId};

#[derive(Clone, Debug, PartialEq)]
pub struct MappedClass {
    /// The QBO class id (`classes.source_ref`).
    pub source_ref: String,
    pub class_id: ClassId,
    /// QBO's own class name.
    pub name: String,
    /// `false` when no §3 class matched and `class_id` is the QBO name kept
    /// as-is (§3: "unmatched classes are kept with their QBO name as id").
    pub matched: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClassMapping {
    pub mapped: Vec<MappedClass>,
    /// `source_ref`s of the classes that did not match, for §7-style reporting.
    pub unmatched: Vec<String>,
}

impl ClassMapping {
    pub fn by_source_ref(&self, qbo_id: &str) -> Option<&MappedClass> {
        self.mapped.iter().find(|class| class.source_ref == qbo_id)
    }
}

/// Map every replica class onto the §3 taxonomy for `company` — Aquamentor's
/// six classes or WaterLine's two (`chart::AQUAMENTOR_CLASSES` /
/// `chart::WATERLINE_CLASSES`). Deterministic: sorted by `qbo_id` first.
pub fn map_classes(replica_classes: &[ParsedClass], company: &CompanyId) -> ClassMapping {
    let allowed: &[(&str, &str)] = if company.0.eq_ignore_ascii_case("waterline") {
        chart::WATERLINE_CLASSES
    } else {
        chart::AQUAMENTOR_CLASSES
    };

    let mut ordered: Vec<&ParsedClass> = replica_classes.iter().collect();
    ordered.sort_by(|a, b| a.qbo_id.cmp(&b.qbo_id));

    let mut mapped = Vec::with_capacity(ordered.len());
    let mut unmatched = Vec::new();

    for class in ordered {
        match match_class_name(&class.name, allowed) {
            Some(id) => mapped.push(MappedClass {
                source_ref: class.qbo_id.clone(),
                class_id: ClassId(id.to_string()),
                name: class.name.clone(),
                matched: true,
            }),
            None => {
                mapped.push(MappedClass {
                    source_ref: class.qbo_id.clone(),
                    class_id: ClassId(class.name.clone()),
                    name: class.name.clone(),
                    matched: false,
                });
                unmatched.push(class.qbo_id.clone());
            }
        }
    }

    ClassMapping { mapped, unmatched }
}

/// Name (or prefix) to §3 class id, per the mapping the design gives:
/// "Foam" -> foam, "Dropship" -> drop, "CNC" -> cnc, "UV Print" -> uv,
/// "Chair"/"Lifeguard" -> chair, "Sign" -> sign. Only returned when `allowed`
/// (the company's own class list) actually carries that id — WaterLine has no
/// `foam`, `sign`, `chair` or `drop`, and a QBO class named "Foam" in that
/// book is correctly unmatched rather than silently cross-mapped.
fn match_class_name(name: &str, allowed: &[(&str, &str)]) -> Option<&'static str> {
    let lower = name.to_lowercase();
    let candidate = if lower.contains("foam") {
        Some("foam")
    } else if lower.contains("dropship") || lower.starts_with("drop") {
        Some("drop")
    } else if lower.contains("cnc") {
        Some("cnc")
    } else if lower.contains("uv print") || lower.contains("uv") {
        Some("uv")
    } else if lower.contains("chair") || lower.contains("lifeguard") {
        Some("chair")
    } else if lower.contains("sign") {
        Some("sign")
    } else {
        None
    };
    candidate.filter(|id| allowed.iter().any(|(allowed_id, _)| allowed_id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(id: &str, name: &str) -> ParsedClass {
        ParsedClass {
            qbo_id: id.to_string(),
            name: name.to_string(),
            fully_qualified_name: None,
            parent_id: None,
            is_active: true,
            is_deleted: false,
        }
    }

    fn aquamentor() -> CompanyId {
        CompanyId("aquamentor".to_string())
    }

    fn waterline() -> CompanyId {
        CompanyId("waterline".to_string())
    }

    #[test]
    fn aquamentor_classes_match_by_name_and_prefix() {
        let classes = vec![
            class("1", "Foam Products"),
            class("2", "Dropship"),
            class("3", "CNC Cutting"),
            class("4", "UV Print"),
            class("5", "Lifeguard Chairs"),
            class("6", "Pool Signs"),
        ];
        let mapping = map_classes(&classes, &aquamentor());
        assert_eq!(
            mapping.by_source_ref("1").unwrap().class_id,
            ClassId("foam".into())
        );
        assert_eq!(
            mapping.by_source_ref("2").unwrap().class_id,
            ClassId("drop".into())
        );
        assert_eq!(
            mapping.by_source_ref("3").unwrap().class_id,
            ClassId("cnc".into())
        );
        assert_eq!(
            mapping.by_source_ref("4").unwrap().class_id,
            ClassId("uv".into())
        );
        assert_eq!(
            mapping.by_source_ref("5").unwrap().class_id,
            ClassId("chair".into())
        );
        assert_eq!(
            mapping.by_source_ref("6").unwrap().class_id,
            ClassId("sign".into())
        );
        assert!(mapping.unmatched.is_empty());
    }

    #[test]
    fn waterline_only_has_cnc_and_uv_so_foam_is_unmatched() {
        let classes = vec![class("1", "Foam Products"), class("2", "CNC Cutting")];
        let mapping = map_classes(&classes, &waterline());
        let foam = mapping.by_source_ref("1").unwrap();
        assert!(!foam.matched);
        assert_eq!(foam.class_id, ClassId("Foam Products".into()));
        assert_eq!(mapping.unmatched, vec!["1".to_string()]);

        let cnc = mapping.by_source_ref("2").unwrap();
        assert!(cnc.matched);
        assert_eq!(cnc.class_id, ClassId("cnc".into()));
    }

    #[test]
    fn an_unrecognized_class_is_kept_under_its_own_qbo_name() {
        let classes = vec![class("9", "Miscellaneous")];
        let mapping = map_classes(&classes, &aquamentor());
        let mapped = mapping.by_source_ref("9").unwrap();
        assert!(!mapped.matched);
        assert_eq!(mapped.class_id, ClassId("Miscellaneous".into()));
        assert_eq!(mapping.unmatched, vec!["9".to_string()]);
    }
}
