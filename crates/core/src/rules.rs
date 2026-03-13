//! Parser for Konveyor YAML rules and rulesets.
//!
//! Loads rules from disk in the standard Konveyor format:
//! - A directory containing `ruleset.yaml` (metadata) and one or more rule YAML files
//! - Each rule YAML file contains a list of rules with `when` conditions

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Metadata from `ruleset.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSetMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// A single rule from a YAML rules file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Unique rule ID within the ruleset.
    #[serde(rename = "ruleID")]
    pub rule_id: String,

    /// Short description / title.
    #[serde(default)]
    pub description: String,

    /// Labels for filtering and categorization.
    #[serde(default)]
    pub labels: Vec<String>,

    /// Effort in story points.
    #[serde(default)]
    pub effort: Option<i32>,

    /// Severity category.
    #[serde(default)]
    pub category: Option<String>,

    /// The condition block.
    pub when: WhenCondition,

    /// Human-readable message for violations.
    #[serde(default)]
    pub message: String,

    /// Optional tag action.
    #[serde(default)]
    pub tag: Option<Vec<String>>,

    /// Hyperlinks.
    #[serde(default)]
    pub links: Vec<RuleLink>,
}

/// A hyperlink in a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleLink {
    pub url: String,
    #[serde(default)]
    pub title: String,
}

/// The `when` block of a rule. Supports provider conditions, and/or combinators.
///
/// IMPORTANT: And/Or must come before Provider in the enum because `serde(untagged)`
/// tries variants in order, and `ProviderWhen` (a BTreeMap) would greedily match
/// any JSON object including `{ or: [...] }` and `{ and: [...] }`.
///
/// Note: `not` is supported by kantra's rule engine but not by our standalone
/// evaluate_when. Use kantra for rules that need `not` conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WhenCondition {
    /// Logical AND of multiple conditions.
    And { and: Vec<WhenCondition> },
    /// Logical OR of multiple conditions.
    Or { or: Vec<WhenCondition> },
    /// A provider condition like `frontend.referenced: { pattern: "..." }`
    Provider(ProviderWhen),
}

/// A provider-specific condition. The key is `<provider>.<capability>`.
///
/// We use a BTreeMap to capture the dynamic key structure since
/// the provider name is part of the key (e.g., `frontend.referenced`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderWhen {
    pub conditions: std::collections::BTreeMap<String, serde_json::Value>,
}

impl ProviderWhen {
    /// Extract the provider name and capability from the condition key.
    ///
    /// E.g., `frontend.referenced` -> `("frontend", "referenced")`
    pub fn parse_conditions(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.conditions
            .iter()
            .filter_map(|(key, value)| {
                let (provider, capability) = key.split_once('.')?;
                Some((provider, capability, value))
            })
            .collect()
    }
}

/// A loaded ruleset with all its rules.
#[derive(Debug, Clone)]
pub struct LoadedRuleSet {
    pub meta: RuleSetMeta,
    pub rules: Vec<Rule>,
    pub source_dir: PathBuf,
}

/// Load a ruleset from a directory containing `ruleset.yaml` and rule files.
pub fn load_ruleset(dir: &Path) -> Result<LoadedRuleSet> {
    let ruleset_file = dir.join("ruleset.yaml");
    let meta: RuleSetMeta = if ruleset_file.exists() {
        let content = std::fs::read_to_string(&ruleset_file)
            .with_context(|| format!("Failed to read {}", ruleset_file.display()))?;
        serde_yml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", ruleset_file.display()))?
    } else {
        // No ruleset.yaml -- use directory name as ruleset name
        RuleSetMeta {
            name: dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            description: String::new(),
            labels: Vec::new(),
        }
    };

    let mut rules = Vec::new();

    // Load all YAML files in the directory (except ruleset.yaml)
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if (name.ends_with(".yaml") || name.ends_with(".yml")) && name != "ruleset.yaml" {
                let file_rules = load_rules_file(&path)?;
                rules.extend(file_rules);
            }
        }
    }

    // Inherit ruleset labels onto rules that don't have them
    for rule in &mut rules {
        for label in &meta.labels {
            if !rule.labels.contains(label) {
                rule.labels.push(label.clone());
            }
        }
    }

    Ok(LoadedRuleSet {
        meta,
        rules,
        source_dir: dir.to_path_buf(),
    })
}

/// Load rules from a single YAML file.
///
/// The file should contain a YAML list of Rule objects.
pub fn load_rules_file(path: &Path) -> Result<Vec<Rule>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let rules: Vec<Rule> = serde_yml::from_str(&content)
        .with_context(|| format!("Failed to parse rules from {}", path.display()))?;

    Ok(rules)
}

/// Load rules from a path that could be a directory (ruleset) or a single file.
pub fn load_rules(path: &Path) -> Result<LoadedRuleSet> {
    if path.is_dir() {
        load_ruleset(path)
    } else if path.is_file() {
        let rules = load_rules_file(path)?;
        Ok(LoadedRuleSet {
            meta: RuleSetMeta {
                name: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                description: String::new(),
                labels: Vec::new(),
            },
            rules,
            source_dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        })
    } else {
        anyhow::bail!("Rules path does not exist: {}", path.display())
    }
}
