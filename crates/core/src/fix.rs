//! Fix engine types.
//!
//! Defines the data model for planned fixes: text edits grouped by file,
//! with support for pattern-based (deterministic) and LLM-assisted fixes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Re-export shared types from konveyor-core so existing code continues to compile.
pub use konveyor_core::fix::{
    FixConfidence, FixSource, FixStrategyEntry, MappingEntry as StrategyMappingEntry,
    MemberMappingEntry,
};

/// A single text replacement within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// 1-indexed line number where the edit applies.
    pub line: u32,
    /// The original text to find on this line.
    pub old_text: String,
    /// The replacement text.
    pub new_text: String,
    /// Rule ID that generated this fix.
    pub rule_id: String,
    /// Human-readable description of what this fix does.
    pub description: String,
}

/// A planned fix for a single incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFix {
    /// The text edits to apply.
    pub edits: Vec<TextEdit>,
    /// Confidence level.
    pub confidence: FixConfidence,
    /// How the fix was generated.
    pub source: FixSource,
    /// The rule ID this fix addresses.
    pub rule_id: String,
    /// File URI from the incident.
    pub file_uri: String,
    /// Line number from the incident.
    pub line: u32,
    /// Description of what this fix does.
    pub description: String,
}

/// A fix plan: all planned fixes grouped by file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixPlan {
    /// Fixes grouped by file path.
    pub files: BTreeMap<PathBuf, Vec<PlannedFix>>,
    /// Incidents that could not be auto-fixed and need manual attention.
    pub manual: Vec<ManualFixItem>,
    /// Incidents pending LLM-assisted fix.
    pub pending_llm: Vec<LlmFixRequest>,
}

/// An incident that requires manual fixing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualFixItem {
    pub rule_id: String,
    pub file_uri: String,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
}

/// A request to send to the LLM for fix generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFixRequest {
    pub rule_id: String,
    pub file_uri: String,
    pub file_path: PathBuf,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
    /// The full source content of the file (for context).
    pub source: Option<String>,
}

/// Result of applying a fix plan.
#[derive(Debug, Default)]
pub struct FixResult {
    /// Number of files modified.
    pub files_modified: usize,
    /// Number of edits applied.
    pub edits_applied: usize,
    /// Number of edits skipped (already applied or conflict).
    pub edits_skipped: usize,
    /// Errors encountered.
    pub errors: Vec<String>,
}

/// A rename mapping: old name -> new name.
/// Used for prop renames, component renames, import renames, etc.
#[derive(Debug, Clone)]
pub struct RenameMapping {
    pub old: String,
    pub new: String,
}

/// Known fix strategies keyed by rule ID.
/// Each entry defines how to transform incidents from that rule into text edits.
#[derive(Debug, Clone)]
pub enum FixStrategy {
    /// Simple text replacement: rename the matched text.
    /// The mapping is propName/componentName/importedName old -> new.
    Rename(Vec<RenameMapping>),
    /// Remove the matched prop (delete the entire attribute from the JSX tag).
    RemoveProp,
    /// Replace an import source path.
    ImportPathChange { old_path: String, new_path: String },
    /// Replace a CSS variable/class prefix.
    CssVariablePrefix {
        old_prefix: String,
        new_prefix: String,
    },
    /// Update a dependency version in package.json.
    UpdateDependency {
        package: String,
        new_version: String,
    },
    /// No auto-fix available — flag for manual review.
    Manual,
    /// Send to LLM for fix generation.
    Llm,
}

/// Convert a `FixStrategyEntry` (from the shared `konveyor-core` crate) to
/// a runtime `FixStrategy`.
///
/// When `mappings` is populated (consolidated rule), builds a multi-mapping
/// `FixStrategy::Rename` or extracts multiple `RemoveProp` targets.
pub fn strategy_entry_to_fix_strategy(entry: &FixStrategyEntry) -> FixStrategy {
    match entry.strategy.as_str() {
        "Rename" => {
            let mut renames: Vec<RenameMapping> = Vec::new();
            // Collect from mappings array (consolidated rule)
            for m in &entry.mappings {
                if let (Some(from), Some(to)) = (&m.from, &m.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            // Fall back to top-level from/to (single-rule strategy)
            if renames.is_empty() {
                if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            if renames.is_empty() {
                FixStrategy::Manual
            } else {
                FixStrategy::Rename(renames)
            }
        }
        "RemoveProp" => FixStrategy::RemoveProp,
        "CssVariablePrefix" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::CssVariablePrefix {
                    old_prefix: from.clone(),
                    new_prefix: to.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "ImportPathChange" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::ImportPathChange {
                    old_path: from.clone(),
                    new_path: to.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "UpdateDependency" => {
            if let (Some(package), Some(new_version)) = (&entry.package, &entry.new_version) {
                FixStrategy::UpdateDependency {
                    package: package.clone(),
                    new_version: new_version.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "PropValueChange" | "PropTypeChange" => FixStrategy::Llm,
        "LlmAssisted" => FixStrategy::Llm,
        _ => FixStrategy::Manual,
    }
}

/// Load fix strategies from a JSON file.
///
/// Returns a map of rule_id -> FixStrategy.
pub fn load_strategies_from_json(
    path: &Path,
) -> Result<BTreeMap<String, FixStrategy>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let entries: BTreeMap<String, FixStrategyEntry> = serde_json::from_str(&content)?;
    let strategies = entries
        .iter()
        .map(|(rule_id, entry)| (rule_id.clone(), strategy_entry_to_fix_strategy(entry)))
        .collect();
    Ok(strategies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_strategy_entry(strategy: &str) -> FixStrategyEntry {
        FixStrategyEntry::new(strategy)
    }

    #[test]
    fn test_rename_with_top_level_from_to() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("Chip".to_string());
        entry.to = Some("Label".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_with_mappings_array() {
        let mut entry = make_strategy_entry("Rename");
        entry.mappings = vec![
            StrategyMappingEntry {
                from: Some("Chip".to_string()),
                to: Some("Label".to_string()),
                component: None,
                prop: None,
            },
            StrategyMappingEntry {
                from: Some("ChipGroup".to_string()),
                to: Some("LabelGroup".to_string()),
                component: None,
                prop: None,
            },
        ];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 2);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
                assert_eq!(mappings[1].old, "ChipGroup");
                assert_eq!(mappings[1].new, "LabelGroup");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_mappings_take_precedence_over_top_level() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("TopLevel".to_string());
        entry.to = Some("ShouldBeIgnored".to_string());
        entry.mappings = vec![StrategyMappingEntry {
            from: Some("FromMapping".to_string()),
            to: Some("ToMapping".to_string()),
            component: None,
            prop: None,
        }];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "FromMapping");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix() {
        let mut entry = make_strategy_entry("CssVariablePrefix");
        entry.from = Some("pf-v5-".to_string());
        entry.to = Some("pf-v6-".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::CssVariablePrefix {
                old_prefix,
                new_prefix,
            } => {
                assert_eq!(old_prefix, "pf-v5-");
                assert_eq!(new_prefix, "pf-v6-");
            }
            other => panic!("Expected CssVariablePrefix, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix_missing_fields_falls_to_manual() {
        let entry = make_strategy_entry("CssVariablePrefix");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("@patternfly/react-core/deprecated".to_string());
        entry.to = Some("@patternfly/react-core".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::ImportPathChange { old_path, new_path } => {
                assert_eq!(old_path, "@patternfly/react-core/deprecated");
                assert_eq!(new_path, "@patternfly/react-core");
            }
            other => panic!("Expected ImportPathChange, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("something".to_string());
        // missing `to`
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_update_dependency() {
        let mut entry = make_strategy_entry("UpdateDependency");
        entry.package = Some("@patternfly/react-core".to_string());
        entry.new_version = Some("^6.0.0".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::UpdateDependency {
                package,
                new_version,
            } => {
                assert_eq!(package, "@patternfly/react-core");
                assert_eq!(new_version, "^6.0.0");
            }
            other => panic!("Expected UpdateDependency, got {:?}", other),
        }
    }

    #[test]
    fn test_update_dependency_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("UpdateDependency");
        entry.package = Some("something".to_string());
        // missing new_version
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_prop_value_change_maps_to_llm() {
        let entry = make_strategy_entry("PropValueChange");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_prop_type_change_maps_to_llm() {
        let entry = make_strategy_entry("PropTypeChange");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_llm_assisted_maps_to_llm() {
        let entry = make_strategy_entry("LlmAssisted");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_strategy_maps_to_manual() {
        let entry = make_strategy_entry("SomethingUnknown");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_strategy_entry_json_deserialization() {
        let json = r#"{
            "strategy": "Rename",
            "mappings": [
                {"from": "Chip", "to": "Label"},
                {"from": "ChipGroup", "to": "LabelGroup"}
            ]
        }"#;
        let entry: FixStrategyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.strategy, "Rename");
        assert_eq!(entry.mappings.len(), 2);
        assert_eq!(entry.mappings[0].from.as_deref(), Some("Chip"));
        assert_eq!(entry.mappings[0].to.as_deref(), Some("Label"));
    }

    #[test]
    fn test_strategy_entry_json_with_top_level_fields() {
        let json = r#"{
            "strategy": "ImportPathChange",
            "from": "@patternfly/react-core/deprecated",
            "to": "@patternfly/react-core"
        }"#;
        let entry: FixStrategyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.strategy, "ImportPathChange");
        assert_eq!(
            entry.from.as_deref(),
            Some("@patternfly/react-core/deprecated")
        );
        assert_eq!(entry.to.as_deref(), Some("@patternfly/react-core"));
        assert!(entry.mappings.is_empty());
    }

    #[test]
    fn test_fix_plan_default_is_empty() {
        let plan = FixPlan::default();
        assert!(plan.files.is_empty());
        assert!(plan.manual.is_empty());
        assert!(plan.pending_llm.is_empty());
    }

    #[test]
    fn test_fix_result_default_is_zero() {
        let result = FixResult::default();
        assert_eq!(result.files_modified, 0);
        assert_eq!(result.edits_applied, 0);
        assert_eq!(result.edits_skipped, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_fix_confidence_serde() {
        assert_eq!(
            serde_json::to_string(&FixConfidence::Exact).unwrap(),
            "\"exact\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Low).unwrap(),
            "\"low\""
        );
    }

    #[test]
    fn test_fix_source_serde() {
        assert_eq!(
            serde_json::to_string(&FixSource::Pattern).unwrap(),
            "\"pattern\""
        );
        assert_eq!(serde_json::to_string(&FixSource::Llm).unwrap(), "\"llm\"");
        assert_eq!(
            serde_json::to_string(&FixSource::Manual).unwrap(),
            "\"manual\""
        );
    }
}
