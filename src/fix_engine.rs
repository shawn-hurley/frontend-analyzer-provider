//! Fix engine: maps analysis violations to concrete text edits.
//!
//! Two-tier approach:
//! 1. Pattern-based: deterministic renames/removals driven by incident variables
//! 2. LLM-assisted: complex structural changes sent to an LLM endpoint

use anyhow::Result;
use frontend_core::fix::*;
use frontend_core::report::{RuleSet, ViolationIncident};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Build a fix plan from analysis output.
pub fn plan_fixes(output: &[RuleSet], project_root: &std::path::Path) -> Result<FixPlan> {
    let strategies = build_strategy_table();
    let mut plan = FixPlan::default();

    for ruleset in output {
        for (rule_id, violation) in &ruleset.violations {
            let strategy = strategies
                .get(rule_id.as_str())
                .or_else(|| infer_strategy_from_labels(&violation.labels))
                .cloned()
                .unwrap_or(FixStrategy::Llm);

            for incident in &violation.incidents {
                let file_path = uri_to_path(&incident.uri, project_root);

                match &strategy {
                    FixStrategy::Rename(mappings) => {
                        if let Some(fix) = plan_rename(rule_id, incident, mappings, &file_path) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::RemoveProp => {
                        if let Some(fix) = plan_remove_prop(rule_id, incident, &file_path) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::ImportPathChange { old_path, new_path } => {
                        if let Some(fix) = plan_import_path_change(
                            rule_id, incident, old_path, new_path, &file_path,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::Manual => {
                        plan.manual.push(ManualFixItem {
                            rule_id: rule_id.clone(),
                            file_uri: incident.uri.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                        });
                    }
                    FixStrategy::Llm => {
                        plan.pending_llm.push(LlmFixRequest {
                            rule_id: rule_id.clone(),
                            file_uri: incident.uri.clone(),
                            file_path: file_path.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                            source: None, // filled lazily if LLM is invoked
                        });
                    }
                }
            }
        }
    }

    // Sort edits within each file by line number (descending) so we can apply bottom-up
    for fixes in plan.files.values_mut() {
        fixes.sort_by(|a, b| b.line.cmp(&a.line));
    }

    Ok(plan)
}

/// Apply a fix plan to disk.
pub fn apply_fixes(plan: &FixPlan) -> Result<FixResult> {
    let mut result = FixResult::default();

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: {}", file_path.display(), e));
                continue;
            }
        };

        let mut lines: Vec<String> = source.lines().map(String::from).collect();
        let mut any_changed = false;

        // Deduplicate edits: when multiple incidents generate the same whole-file
        // rename, we get duplicate (line, old_text, new_text) tuples. Only apply each once.
        let mut seen_edits: std::collections::HashSet<(u32, String, String)> =
            std::collections::HashSet::new();

        for fix in fixes {
            for edit in &fix.edits {
                let key = (edit.line, edit.old_text.clone(), edit.new_text.clone());
                if !seen_edits.insert(key) {
                    continue; // already applied this exact edit
                }
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let line = &lines[idx];
                    if line.contains(&edit.old_text) {
                        lines[idx] = line.replacen(&edit.old_text, &edit.new_text, 1);
                        result.edits_applied += 1;
                        any_changed = true;
                    } else {
                        result.edits_skipped += 1;
                    }
                } else {
                    result.edits_skipped += 1;
                }
            }
        }

        if any_changed {
            // Bug 1 fix: deduplicate import specifiers on renamed lines
            dedup_import_specifiers(&mut lines);

            // Remove empty lines left by prop removal
            lines.retain(|l| !l.is_empty() || true); // keep empty lines for now

            // Preserve original trailing newline
            let mut output = lines.join("\n");
            if source.ends_with('\n') {
                output.push('\n');
            }
            std::fs::write(file_path, output)?;
            result.files_modified += 1;
        }
    }

    Ok(result)
}

/// Generate a unified diff preview of the planned changes.
pub fn preview_fixes(plan: &FixPlan) -> Result<String> {
    let mut output = String::new();

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let lines: Vec<&str> = source.lines().collect();
        let mut changed_lines: HashMap<usize, String> = HashMap::new();

        // Apply edits to get the "after" lines
        for fix in fixes {
            for edit in &fix.edits {
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let current = changed_lines
                        .get(&idx)
                        .map(String::as_str)
                        .unwrap_or(lines[idx]);
                    if current.contains(&edit.old_text) {
                        let new_line = current.replacen(&edit.old_text, &edit.new_text, 1);
                        changed_lines.insert(idx, new_line);
                    }
                }
            }
        }

        if changed_lines.is_empty() {
            continue;
        }

        // Bug 1 fix: deduplicate import specifiers in changed lines
        for (_, line_content) in changed_lines.iter_mut() {
            let mut single = [line_content.clone()];
            dedup_import_specifiers(&mut single);
            *line_content = single.into_iter().next().unwrap();
        }

        output.push_str(&format!(
            "--- a/{}\n+++ b/{}\n",
            file_path.display(),
            file_path.display()
        ));

        // Group consecutive changed lines into hunks
        let mut changed_indices: Vec<usize> = changed_lines.keys().copied().collect();
        changed_indices.sort();

        for &idx in &changed_indices {
            let context = 3;
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(lines.len());

            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                start + 1,
                end - start,
                start + 1,
                end - start
            ));

            for i in start..end {
                if let Some(new_line) = changed_lines.get(&i) {
                    output.push_str(&format!("-{}\n", lines[i]));
                    output.push_str(&format!("+{}\n", new_line));
                } else {
                    output.push_str(&format!(" {}\n", lines[i]));
                }
            }
        }
    }

    Ok(output)
}

// ── Pattern-based fix generators ──────────────────────────────────────────

fn plan_rename(
    rule_id: &str,
    incident: &ViolationIncident,
    mappings: &[RenameMapping],
    file_path: &PathBuf,
) -> Option<PlannedFix> {
    let line = incident.line_number?;

    // Determine what text to look for from incident variables
    let matched_text = get_matched_text(incident);

    // Check if this is a component/import rename (detected via importedName variable).
    // For these, we need to scan the entire file since JSX usage of the component
    // appears on many lines beyond the import.
    let is_import_rename = incident.variables.contains_key("importedName");

    // Try to find a mapping that matches the incident's matched text
    let primary_mapping = mappings.iter().find(|m| m.old == matched_text);

    // Read the file — we always need it for value-level scans and whole-file renames
    let source = std::fs::read_to_string(file_path).ok()?;
    let mut edits = Vec::new();

    if is_import_rename {
        // Component/import rename: scan the ENTIRE file for all occurrences of
        // every mapping's old value. This catches imports, JSX opening tags
        // (<TextContent>), closing tags (</TextContent>), and type references.
        //
        // Sort mappings longest-first to avoid substring false matches:
        // e.g., "TextContent" should be matched before "Text" since "Text"
        // is a substring of "TextContent".
        let mut sorted_mappings: Vec<&RenameMapping> =
            mappings.iter().filter(|m| m.old != m.new).collect();
        sorted_mappings.sort_by(|a, b| b.old.len().cmp(&a.old.len()));

        for (idx, file_line) in source.lines().enumerate() {
            let line_num = (idx + 1) as u32;
            // Track which ranges on this line have been claimed by longer mappings
            // to prevent shorter substring matches from generating duplicate edits.
            let mut consumed: Vec<&str> = Vec::new();
            for m in &sorted_mappings {
                if file_line.contains(m.old.as_str()) {
                    // Skip if a longer mapping already covers this match.
                    // e.g., skip "Text" if "TextContent" already matched on this line.
                    let is_substring_of_consumed =
                        consumed.iter().any(|c| c.contains(m.old.as_str()));
                    if is_substring_of_consumed {
                        continue;
                    }
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                    });
                    consumed.push(&m.old);
                }
            }
        }
    } else if let Some(mapping) = primary_mapping {
        // Standard rename: apply the primary mapping on the incident line
        if mapping.old == mapping.new {
            return None;
        }
        edits.push(TextEdit {
            line,
            old_text: mapping.old.clone(),
            new_text: mapping.new.clone(),
            rule_id: rule_id.to_string(),
            description: format!("Rename '{}' to '{}'", mapping.old, mapping.new),
        });

        // Also scan the incident line for value-level renames from other mappings.
        // e.g., when the prop key `spacer` -> `gap` is the primary match, also
        // rename `spacerNone` -> `gapNone`, `spacerMd` -> `gapMd` etc. on the
        // same line or nearby lines in the same prop value expression.
        let line_idx = (line as usize).saturating_sub(1);
        // Scan a small window around the incident line to catch multi-line prop values
        let scan_start = line_idx.saturating_sub(3);
        let scan_end = (line_idx + 5).min(source.lines().count());
        for (idx, file_line) in source
            .lines()
            .enumerate()
            .skip(scan_start)
            .take(scan_end - scan_start)
        {
            let line_num = (idx + 1) as u32;
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                // Skip the primary mapping on the primary line (already added)
                if std::ptr::eq(m, mapping) && line_num == line {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                    });
                }
            }
        }
    } else {
        // Fallback: no primary match found. Scan the incident line for any
        // applicable mappings (handles prop-value-change and CSS rules where
        // the incident captures the prop/class name but mappings are value-level).
        if let Some(file_line) = source.lines().nth((line as usize).saturating_sub(1)) {
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                    });
                }
            }
        }
    }

    if edits.is_empty() {
        return None;
    }

    let desc = edits
        .iter()
        .map(|e| format!("'{}' → '{}'", e.old_text, e.new_text))
        .collect::<Vec<_>>()
        .join(", ");

    Some(PlannedFix {
        edits,
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.uri.clone(),
        line,
        description: format!("Rename {}", desc),
    })
}

fn plan_remove_prop(
    rule_id: &str,
    incident: &ViolationIncident,
    file_path: &PathBuf,
) -> Option<PlannedFix> {
    let line = incident.line_number?;
    let prop_name = incident
        .variables
        .get("propName")
        .and_then(|v| v.as_str())?;

    // Read the actual file line to construct a precise removal edit.
    let source = std::fs::read_to_string(file_path).ok()?;
    let file_line = source.lines().nth((line as usize).saturating_sub(1))?;
    let trimmed = file_line.trim();

    // If the entire line is just the prop (common in formatted JSX), remove the whole line
    // Patterns: `propName`, `propName={...}`, `propName="..."`, `propName={true}`
    if trimmed.starts_with(prop_name) {
        // The whole line is the prop — replace line content with empty
        Some(PlannedFix {
            edits: vec![TextEdit {
                line,
                old_text: file_line.to_string(),
                new_text: String::new(),
                rule_id: rule_id.to_string(),
                description: format!("Remove prop '{}' (entire line)", prop_name),
            }],
            confidence: FixConfidence::High,
            source: FixSource::Pattern,
            rule_id: rule_id.to_string(),
            file_uri: incident.uri.clone(),
            line,
            description: format!("Remove prop '{}'", prop_name),
        })
    } else {
        // Prop is inline with other content — try to remove just the prop fragment.
        // Match: ` propName={...}` or ` propName="..."` or ` propName`
        // Use a simple regex to find the prop and its value.
        let prop_re = regex::Regex::new(&format!(
            r#"\s+{prop_name}(?:=\{{[^}}]*\}}|="[^"]*"|='[^']*'|=\{{.*?\}})?"#
        ))
        .ok()?;

        if let Some(m) = prop_re.find(file_line) {
            Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: m.as_str().to_string(),
                    new_text: String::new(),
                    rule_id: rule_id.to_string(),
                    description: format!("Remove prop '{}'", prop_name),
                }],
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            // Can't parse — flag for manual review
            Some(PlannedFix {
                edits: vec![],
                confidence: FixConfidence::Low,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.uri.clone(),
                line,
                description: format!("Remove prop '{}' (manual)", prop_name),
            })
        }
    }
}

fn plan_import_path_change(
    rule_id: &str,
    incident: &ViolationIncident,
    old_path: &str,
    new_path: &str,
    _file_path: &PathBuf,
) -> Option<PlannedFix> {
    let line = incident.line_number?;

    Some(PlannedFix {
        edits: vec![TextEdit {
            line,
            old_text: old_path.to_string(),
            new_text: new_path.to_string(),
            rule_id: rule_id.to_string(),
            description: format!("Change import path '{}' → '{}'", old_path, new_path),
        }],
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.uri.clone(),
        line,
        description: format!("Change import path to '{}'", new_path),
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Deduplicate import specifiers on lines that look like ES import statements.
///
/// After renaming multiple symbols to the same name (e.g., TextContent/TextList → Content),
/// an import line may have duplicate specifiers: `import { Content, Content, Content }`.
/// This function deduplicates them to `import { Content }`.
fn dedup_import_specifiers(lines: &mut [String]) {
    let import_re = regex::Regex::new(r"^(\s*import\s+\{)([^}]+)(\}\s*from\s+.*)$").unwrap();

    for line in lines.iter_mut() {
        if let Some(caps) = import_re.captures(line) {
            let prefix = caps.get(1).unwrap().as_str();
            let specifiers_str = caps.get(2).unwrap().as_str();
            let suffix = caps.get(3).unwrap().as_str();

            let specifiers: Vec<&str> = specifiers_str
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<&str> = specifiers
                .into_iter()
                .filter(|s| {
                    // Handle `Name as Alias` — dedup by the full specifier
                    seen.insert(s.to_string())
                })
                .collect();

            let new_specifiers = format!(" {} ", deduped.join(", "));
            let new_line = format!("{}{}{}", prefix, new_specifiers, suffix);

            if new_line != *line {
                *line = new_line;
            }
        }
    }
}

/// Extract the matched text from incident variables.
/// Checks propName, componentName, importedName, className, variableName in that order.
fn get_matched_text(incident: &ViolationIncident) -> String {
    for key in &[
        "propName",
        "componentName",
        "importedName",
        "className",
        "variableName",
    ] {
        if let Some(serde_json::Value::String(s)) = incident.variables.get(*key) {
            return s.clone();
        }
    }
    String::new()
}

/// Convert a file:// URI to a filesystem path, relative to project root.
fn uri_to_path(uri: &str, project_root: &std::path::Path) -> PathBuf {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);

    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

/// Try to infer a fix strategy from rule labels when no explicit mapping exists.
fn infer_strategy_from_labels(labels: &[String]) -> Option<&'static FixStrategy> {
    for label in labels {
        match label.as_str() {
            "change-type=prop-removal" => return Some(&FixStrategy::RemoveProp),
            "change-type=dom-structure"
            | "change-type=behavioral"
            | "change-type=accessibility" => return Some(&FixStrategy::Manual),
            _ => {}
        }
    }
    None
}

// ── Strategy table ────────────────────────────────────────────────────────

/// Build the known fix strategy table.
/// Each entry maps a rule ID to the fix transform that should be applied.
fn build_strategy_table() -> BTreeMap<&'static str, FixStrategy> {
    let mut m = BTreeMap::new();

    // ── Component renames ──
    m.insert(
        "pfv6-rename-chip-to-label",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "Chip".into(),
                new: "Label".into(),
            },
            RenameMapping {
                old: "ChipGroup".into(),
                new: "LabelGroup".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-rename-toolbar-chip-group-content",
        FixStrategy::Rename(vec![RenameMapping {
            old: "ToolbarChipGroupContent".into(),
            new: "ToolbarLabelGroupContent".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-masthead-brand",
        FixStrategy::Rename(vec![RenameMapping {
            old: "MastheadBrand".into(),
            new: "MastheadLogo".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-content-header",
        FixStrategy::Rename(vec![RenameMapping {
            old: "ContentHeader".into(),
            new: "PageHeader".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-invalid-object",
        FixStrategy::Rename(vec![RenameMapping {
            old: "InvalidObject".into(),
            new: "MissingPage".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-not-authorized",
        FixStrategy::Rename(vec![RenameMapping {
            old: "NotAuthorized".into(),
            new: "UnauthorizedAccess".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-text-to-content",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "TextContent".into(),
                new: "Content".into(),
            },
            RenameMapping {
                old: "TextList".into(),
                new: "Content".into(),
            },
            RenameMapping {
                old: "TextListItem".into(),
                new: "Content".into(),
            },
            RenameMapping {
                old: "Text".into(),
                new: "Content".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-rename-text-variants",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "TextVariants".into(),
                new: "ContentVariants".into(),
            },
            RenameMapping {
                old: "TextProps".into(),
                new: "ContentProps".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-rename-text-isvisited",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isVisited".into(),
            new: "isVisitedLink".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-textlist-isplain",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isPlain".into(),
            new: "isPlainList".into(),
        }]),
    );
    m.insert(
        "pfv6-rename-form-field-group-typo",
        FixStrategy::Rename(vec![RenameMapping {
            old: "FormFiledGroupHeaderTitleTextObject".into(),
            new: "FormFieldGroupHeaderTitleTextObject".into(),
        }]),
    );

    // ── Prop renames ──
    m.insert(
        "pfv6-prop-rename-button-isactive",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isActive".into(),
            new: "isClicked".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-page-header",
        FixStrategy::Rename(vec![RenameMapping {
            old: "header".into(),
            new: "masthead".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-page-tertiary-nav",
        FixStrategy::Rename(vec![RenameMapping {
            old: "tertiaryNav".into(),
            new: "horizontalSubnav".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-page-is-tertiary-grouped",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isTertiaryNavGrouped".into(),
            new: "isHorizontalSubnavGrouped".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-page-is-tertiary-width-limited",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isTertiaryNavWidthLimited".into(),
            new: "isHorizontalSubnavWidthLimited".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-avatar-border",
        FixStrategy::Rename(vec![RenameMapping {
            old: "border".into(),
            new: "isBordered".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-formgroup-labelicon",
        FixStrategy::Rename(vec![RenameMapping {
            old: "labelIcon".into(),
            new: "labelHelp".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-tabs-issecondary",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isSecondary".into(),
            new: "isSubtab".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-checkbox-label-position",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isLabelBeforeButton".into(),
            new: "labelPosition".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-innerref-to-ref",
        FixStrategy::Rename(vec![RenameMapping {
            old: "innerRef".into(),
            new: "ref".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-toolbar-chip-to-label",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "chips".into(),
                new: "labels".into(),
            },
            RenameMapping {
                old: "deleteChip".into(),
                new: "deleteLabel".into(),
            },
            RenameMapping {
                old: "deleteChipGroup".into(),
                new: "deleteLabelGroup".into(),
            },
            RenameMapping {
                old: "chipGroupExpandedText".into(),
                new: "labelGroupExpandedText".into(),
            },
            RenameMapping {
                old: "chipGroupCollapsedText".into(),
                new: "labelGroupCollapsedText".into(),
            },
            RenameMapping {
                old: "categoryName".into(),
                new: "categoryName".into(),
            }, // no rename needed
            RenameMapping {
                old: "customChipGroupContent".into(),
                new: "customLabelGroupContent".into(),
            },
            RenameMapping {
                old: "chipContainerRef".into(),
                new: "labelContainerRef".into(),
            },
            RenameMapping {
                old: "expandableChipContainerRef".into(),
                new: "expandableLabelContainerRef".into(),
            },
            RenameMapping {
                old: "chipGroupContentRef".into(),
                new: "labelGroupContentRef".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-prop-rename-menutoggle-splitbutton",
        FixStrategy::Rename(vec![RenameMapping {
            old: "splitButtonOptions".into(),
            new: "splitButtonItems".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-rename-toolbar-chip-interfaces",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "ToolbarChipGroup".into(),
                new: "ToolbarLabelGroup".into(),
            },
            RenameMapping {
                old: "ToolbarChip".into(),
                new: "ToolbarLabel".into(),
            },
        ]),
    );

    // ── Prop value changes ──
    m.insert(
        "pfv6-prop-value-toolbar-group-variant",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "icon-button-group".into(),
                new: "action-group-plain".into(),
            },
            RenameMapping {
                old: "button-group".into(),
                new: "action-group".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-prop-value-toolbar-item-variant",
        FixStrategy::Rename(vec![RenameMapping {
            old: "chip-group".into(),
            new: "label-group".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-value-tabs-variant",
        FixStrategy::Rename(vec![RenameMapping {
            old: "light300".into(),
            new: "secondary".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-value-nav-tertiary",
        FixStrategy::Rename(vec![RenameMapping {
            old: "tertiary".into(),
            new: "horizontal-subnav".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-value-drawer-colorvariant",
        FixStrategy::Rename(vec![RenameMapping {
            old: "light-200".into(),
            new: "secondary".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-value-label-overflow",
        FixStrategy::Rename(vec![RenameMapping {
            old: "isOverflowLabel".into(),
            new: "variant".into(),
        }]),
    );
    m.insert(
        "pfv6-prop-value-toolbar-spacer",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "spacer".into(),
                new: "gap".into(),
            },
            RenameMapping {
                old: "spaceItems".into(),
                new: "gap".into(),
            },
            RenameMapping {
                old: "spacerNone".into(),
                new: "gapNone".into(),
            },
            RenameMapping {
                old: "spacerSm".into(),
                new: "gapSm".into(),
            },
            RenameMapping {
                old: "spacerMd".into(),
                new: "gapMd".into(),
            },
            RenameMapping {
                old: "spacerLg".into(),
                new: "gapLg".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-prop-rename-toolbar-align",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "alignLeft".into(),
                new: "alignStart".into(),
            },
            RenameMapping {
                old: "alignRight".into(),
                new: "alignEnd".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-prop-value-banner-color",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "cyan".into(),
                new: "teal".into(),
            },
            RenameMapping {
                old: "gold".into(),
                new: "yellow".into(),
            },
        ]),
    );
    m.insert(
        "pfv6-prop-value-banner-variant",
        FixStrategy::Rename(vec![RenameMapping {
            old: "variant".into(),
            new: "color".into(),
        }]),
    );

    // ── Prop removals ──
    m.insert(
        "pfv6-prop-remove-accordion-ishidden",
        FixStrategy::RemoveProp,
    );
    m.insert("pfv6-prop-remove-card-various", FixStrategy::RemoveProp);
    m.insert(
        "pfv6-prop-remove-datalist-plain-button",
        FixStrategy::RemoveProp,
    );
    m.insert("pfv6-prop-remove-drawer-nopadding", FixStrategy::RemoveProp);
    m.insert(
        "pfv6-prop-remove-expandable-isactive",
        FixStrategy::RemoveProp,
    );
    m.insert("pfv6-prop-remove-helpertext-props", FixStrategy::RemoveProp);
    m.insert("pfv6-prop-remove-masthead-bgcolor", FixStrategy::RemoveProp);
    m.insert("pfv6-prop-remove-nav-theme", FixStrategy::RemoveProp);
    m.insert("pfv6-prop-remove-navitem-wrapper", FixStrategy::RemoveProp);
    m.insert("pfv6-prop-remove-switch-labeloff", FixStrategy::RemoveProp);
    m.insert("pfv6-prop-remove-toolbar-various", FixStrategy::RemoveProp);
    m.insert(
        "pfv6-prop-remove-navlist-aria-scroll",
        FixStrategy::RemoveProp,
    );
    m.insert(
        "pfv6-prop-remove-duallist-onoptionselect",
        FixStrategy::RemoveProp,
    );
    m.insert(
        "pfv6-prop-remove-accordion-toggle-isexpanded",
        FixStrategy::RemoveProp,
    );

    // ── CSS class/variable renames ──
    m.insert(
        "pfv6-css-v5-prefix",
        FixStrategy::Rename(vec![RenameMapping {
            old: "pf-v5-".into(),
            new: "pf-v6-".into(),
        }]),
    );
    m.insert(
        "pfv6-css-variable-v5-prefix",
        FixStrategy::Rename(vec![RenameMapping {
            old: "--pf-v5-".into(),
            new: "--pf-v6-".into(),
        }]),
    );
    m.insert(
        "pfv6-css-variable-logical-properties",
        FixStrategy::Rename(vec![
            RenameMapping {
                old: "PaddingTop".into(),
                new: "PaddingBlockStart".into(),
            },
            RenameMapping {
                old: "PaddingBottom".into(),
                new: "PaddingBlockEnd".into(),
            },
            RenameMapping {
                old: "PaddingLeft".into(),
                new: "PaddingInlineStart".into(),
            },
            RenameMapping {
                old: "PaddingRight".into(),
                new: "PaddingInlineEnd".into(),
            },
            RenameMapping {
                old: "MarginTop".into(),
                new: "MarginBlockStart".into(),
            },
            RenameMapping {
                old: "MarginBottom".into(),
                new: "MarginBlockEnd".into(),
            },
            RenameMapping {
                old: "MarginLeft".into(),
                new: "MarginInlineStart".into(),
            },
            RenameMapping {
                old: "MarginRight".into(),
                new: "MarginInlineEnd".into(),
            },
            RenameMapping {
                old: "InsetTop".into(),
                new: "InsetBlockStart".into(),
            },
            RenameMapping {
                old: "InsetBottom".into(),
                new: "InsetBlockEnd".into(),
            },
            RenameMapping {
                old: "InsetLeft".into(),
                new: "InsetInlineStart".into(),
            },
            RenameMapping {
                old: "InsetRight".into(),
                new: "InsetInlineEnd".into(),
            },
            RenameMapping {
                old: "--Left".into(),
                new: "--InsetInlineStart".into(),
            },
            RenameMapping {
                old: "--Right".into(),
                new: "--InsetInlineEnd".into(),
            },
        ]),
    );

    // ── Complex structural changes → LLM ──
    m.insert("pfv6-dom-masthead-layout", FixStrategy::Llm);
    m.insert("pfv6-deprecated-modal", FixStrategy::Llm);
    m.insert("pfv6-chip-deprecated-import", FixStrategy::Llm);
    m.insert("pfv6-remove-empty-state-header", FixStrategy::Llm);
    m.insert("pfv6-deprecated-select-old", FixStrategy::Llm);
    m.insert("pfv6-deprecated-dropdown-old", FixStrategy::Llm);
    m.insert("pfv6-deprecated-wizard-old", FixStrategy::Llm);
    m.insert("pfv6-behavioral-button-icon-children", FixStrategy::Llm);

    // ── Manual review required ──
    m.insert("pfv6-dom-expandable-section-toggle", FixStrategy::Manual);
    m.insert("pfv6-dom-navlist-scroll-buttons", FixStrategy::Manual);
    m.insert("pfv6-dom-progress-tooltip", FixStrategy::Manual);
    m.insert("pfv6-dom-truncate-tooltip", FixStrategy::Manual);
    m.insert("pfv6-dom-dropdown-item-tooltip", FixStrategy::Manual);
    m.insert("pfv6-dom-menu-item-tooltip", FixStrategy::Manual);
    m.insert("pfv6-dom-pagination-toggle", FixStrategy::Manual);
    m.insert("pfv6-dom-tabs-scroll-buttons", FixStrategy::Manual);

    m
}
