//! Capability definitions for the frontend provider.
//!
//! Each capability maps to a distinct analysis domain:
//! - `referenced`: Semantic JS/TS/JSX/TSX symbol search (imports, JSX usage, props, etc.)
//! - `cssclass`: CSS class name search across CSS and JS/TS files
//! - `cssvar`: CSS custom property search
//! - `dependency`: package.json dependency checking

use serde::{Deserialize, Serialize};

/// All capabilities this provider supports.
pub const CAPABILITIES: &[&str] = &["referenced", "cssclass", "cssvar", "dependency"];

/// The location within source code to search for references.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReferenceLocation {
    /// Match import declarations: `import { X } from '...'`
    Import,
    /// Match JSX component usage: `<Button ...>`
    JsxComponent,
    /// Match JSX prop usage: `<Button isActive={...}>`
    JsxProp,
    /// Match function/hook calls: `useButton(...)`
    FunctionCall,
    /// Match type references: `const x: ButtonProps = ...`
    TypeReference,
}

/// Condition for the `referenced` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedCondition {
    /// Pattern to search for (supports regex).
    pub pattern: String,
    /// Optional location filter. If absent, searches all locations.
    pub location: Option<ReferenceLocation>,
    /// Optional component name filter for JSX_PROP location.
    /// When set, only matches props on components whose name matches this pattern.
    pub component: Option<String>,
    /// Optional parent component filter for JSX_COMPONENT location.
    /// When set, only matches components that are direct children of a parent
    /// JSX element whose name matches this pattern.
    pub parent: Option<String>,
    /// Optional parent import source filter for JSX_COMPONENT location.
    /// When set, only matches when the parent JSX component was imported from
    /// a module whose path matches this pattern. Requires `parent` to be set.
    /// Example: `parent_from: "@patternfly/react-core"` ensures the parent
    /// `Button` is from PatternFly, not a custom app component.
    #[serde(rename = "parentFrom", skip_serializing_if = "Option::is_none")]
    pub parent_from: Option<String>,
    /// Optional prop value filter for JSX_PROP location.
    /// When set, only matches props whose value matches this pattern.
    /// Matches against string literal values (e.g., variant="plain") and
    /// JSX expression text (e.g., variant={SelectVariant.checkbox}).
    pub value: Option<String>,
    /// Optional import source path filter for IMPORT location.
    /// When set, only matches imports from modules whose path matches this pattern.
    /// Example: `from: "@patternfly/react-core/deprecated"` matches
    /// `import { Select } from '@patternfly/react-core/deprecated'` but not
    /// `import { Select } from '@patternfly/react-core'`.
    pub from: Option<String>,
    /// Optional file path filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `cssclass` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CssClassCondition {
    /// CSS class name pattern (supports regex).
    pub pattern: String,
    /// Optional file pattern filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `cssvar` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CssVarCondition {
    /// CSS variable pattern (supports regex), e.g. `--pf-v5-.*`
    pub pattern: String,
    /// Optional file pattern filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `dependency` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyCondition {
    /// Dependency name (exact or regex via `nameregex`).
    pub name: Option<String>,
    /// Regex pattern for dependency name.
    pub nameregex: Option<String>,
    /// Match versions <= this.
    pub upperbound: Option<String>,
    /// Match versions >= this.
    pub lowerbound: Option<String>,
}

/// A parsed condition from the Konveyor rule YAML.
#[derive(Debug, Clone)]
pub enum ProviderCondition {
    Referenced(ReferencedCondition),
    CssClass(CssClassCondition),
    CssVar(CssVarCondition),
    Dependency(DependencyCondition),
}

impl ProviderCondition {
    /// Parse a condition from capability name and YAML condition string.
    pub fn parse(capability: &str, condition_yaml: &str) -> anyhow::Result<Self> {
        match capability {
            "referenced" => {
                let cond: ReferencedCondition = yaml_serde::from_str(condition_yaml)?;
                Ok(ProviderCondition::Referenced(cond))
            }
            "cssclass" => {
                let cond: CssClassCondition = yaml_serde::from_str(condition_yaml)?;
                Ok(ProviderCondition::CssClass(cond))
            }
            "cssvar" => {
                let cond: CssVarCondition = yaml_serde::from_str(condition_yaml)?;
                Ok(ProviderCondition::CssVar(cond))
            }
            "dependency" => {
                let cond: DependencyCondition = yaml_serde::from_str(condition_yaml)?;
                Ok(ProviderCondition::Dependency(cond))
            }
            _ => anyhow::bail!("Unknown capability: {capability}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_list() {
        assert_eq!(CAPABILITIES.len(), 4);
        assert!(CAPABILITIES.contains(&"referenced"));
        assert!(CAPABILITIES.contains(&"cssclass"));
        assert!(CAPABILITIES.contains(&"cssvar"));
        assert!(CAPABILITIES.contains(&"dependency"));
    }

    #[test]
    fn test_parse_referenced_basic() {
        let yaml = r#"pattern: "Button""#;
        let cond = ProviderCondition::parse("referenced", yaml).unwrap();
        match cond {
            ProviderCondition::Referenced(c) => {
                assert_eq!(c.pattern, "Button");
                assert!(c.location.is_none());
                assert!(c.component.is_none());
                assert!(c.parent.is_none());
                assert!(c.parent_from.is_none());
                assert!(c.value.is_none());
                assert!(c.from.is_none());
                assert!(c.file_pattern.is_none());
            }
            _ => panic!("Expected Referenced variant"),
        }
    }

    #[test]
    fn test_parse_referenced_with_location() {
        let yaml = r#"
pattern: "isActive"
location: JSX_PROP
component: "Button"
"#;
        let cond = ProviderCondition::parse("referenced", yaml).unwrap();
        match cond {
            ProviderCondition::Referenced(c) => {
                assert_eq!(c.pattern, "isActive");
                assert_eq!(c.location, Some(ReferenceLocation::JsxProp));
                assert_eq!(c.component.as_deref(), Some("Button"));
            }
            _ => panic!("Expected Referenced variant"),
        }
    }

    #[test]
    fn test_parse_referenced_with_all_fields() {
        let yaml = r#"
pattern: "EmptyStateHeader"
location: JSX_COMPONENT
parent: "EmptyState"
parentFrom: "@patternfly/react-core"
value: "primary"
from: "@patternfly/react-core"
filePattern: ".*\\.tsx$"
"#;
        let cond = ProviderCondition::parse("referenced", yaml).unwrap();
        match cond {
            ProviderCondition::Referenced(c) => {
                assert_eq!(c.pattern, "EmptyStateHeader");
                assert_eq!(c.location, Some(ReferenceLocation::JsxComponent));
                assert_eq!(c.parent.as_deref(), Some("EmptyState"));
                assert_eq!(c.parent_from.as_deref(), Some("@patternfly/react-core"));
                assert_eq!(c.value.as_deref(), Some("primary"));
                assert_eq!(c.from.as_deref(), Some("@patternfly/react-core"));
                assert_eq!(c.file_pattern.as_deref(), Some(".*\\.tsx$"));
            }
            _ => panic!("Expected Referenced variant"),
        }
    }

    #[test]
    fn test_parse_cssclass() {
        let yaml = r#"pattern: "pf-m-expandable""#;
        let cond = ProviderCondition::parse("cssclass", yaml).unwrap();
        match cond {
            ProviderCondition::CssClass(c) => {
                assert_eq!(c.pattern, "pf-m-expandable");
                assert!(c.file_pattern.is_none());
            }
            _ => panic!("Expected CssClass variant"),
        }
    }

    #[test]
    fn test_parse_cssclass_with_file_pattern() {
        let yaml = r#"
pattern: "pf-v5-.*"
filePattern: ".*\\.css$"
"#;
        let cond = ProviderCondition::parse("cssclass", yaml).unwrap();
        match cond {
            ProviderCondition::CssClass(c) => {
                assert_eq!(c.pattern, "pf-v5-.*");
                assert_eq!(c.file_pattern.as_deref(), Some(".*\\.css$"));
            }
            _ => panic!("Expected CssClass variant"),
        }
    }

    #[test]
    fn test_parse_cssvar() {
        let yaml = r#"pattern: "--pf-v5-.*""#;
        let cond = ProviderCondition::parse("cssvar", yaml).unwrap();
        match cond {
            ProviderCondition::CssVar(c) => {
                assert_eq!(c.pattern, "--pf-v5-.*");
                assert!(c.file_pattern.is_none());
            }
            _ => panic!("Expected CssVar variant"),
        }
    }

    #[test]
    fn test_parse_dependency() {
        let yaml = r#"
name: "@patternfly/react-core"
upperbound: "6.0.0"
"#;
        let cond = ProviderCondition::parse("dependency", yaml).unwrap();
        match cond {
            ProviderCondition::Dependency(c) => {
                assert_eq!(c.name.as_deref(), Some("@patternfly/react-core"));
                assert_eq!(c.upperbound.as_deref(), Some("6.0.0"));
                assert!(c.lowerbound.is_none());
                assert!(c.nameregex.is_none());
            }
            _ => panic!("Expected Dependency variant"),
        }
    }

    #[test]
    fn test_parse_dependency_with_nameregex() {
        let yaml = r#"
nameregex: "@patternfly/.*"
lowerbound: "5.0.0"
upperbound: "6.0.0"
"#;
        let cond = ProviderCondition::parse("dependency", yaml).unwrap();
        match cond {
            ProviderCondition::Dependency(c) => {
                assert!(c.name.is_none());
                assert_eq!(c.nameregex.as_deref(), Some("@patternfly/.*"));
                assert_eq!(c.lowerbound.as_deref(), Some("5.0.0"));
                assert_eq!(c.upperbound.as_deref(), Some("6.0.0"));
            }
            _ => panic!("Expected Dependency variant"),
        }
    }

    #[test]
    fn test_parse_unknown_capability_errors() {
        let result = ProviderCondition::parse("unknown", r#"pattern: "foo""#);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown capability"));
    }

    #[test]
    fn test_parse_invalid_yaml_errors() {
        let result = ProviderCondition::parse("referenced", "not: [valid: yaml:");
        assert!(result.is_err());
    }

    #[test]
    fn test_reference_location_serde_roundtrip() {
        let locations = vec![
            (ReferenceLocation::Import, "\"IMPORT\""),
            (ReferenceLocation::JsxComponent, "\"JSX_COMPONENT\""),
            (ReferenceLocation::JsxProp, "\"JSX_PROP\""),
            (ReferenceLocation::FunctionCall, "\"FUNCTION_CALL\""),
            (ReferenceLocation::TypeReference, "\"TYPE_REFERENCE\""),
        ];
        for (loc, expected_json) in locations {
            let json = serde_json::to_string(&loc).unwrap();
            assert_eq!(json, expected_json);
            let back: ReferenceLocation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, loc);
        }
    }
}
