//! Fix engine: maps analysis violations to concrete text edits.
//!
//! Two-tier approach:
//! 1. Pattern-based: deterministic renames/removals driven by incident variables
//! 2. LLM-assisted: complex structural changes sent to an LLM endpoint

use anyhow::Result;
use frontend_core::fix::*;
use konveyor_core::incident::Incident;
use konveyor_core::report::RuleSet;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Build a fix plan from analysis output.
///
/// `strategies` is a merged map of rule ID → fix strategy, loaded from one or
/// more external JSON files (rule-adjacent and/or semver-analyzer generated).
/// When no strategy is found for a rule, label-based inference is attempted,
/// falling back to LLM-assisted fixes.
pub fn plan_fixes(
    output: &[RuleSet],
    project_root: &std::path::Path,
    strategies: &BTreeMap<String, FixStrategy>,
) -> Result<FixPlan> {
    let mut plan = FixPlan::default();

    for ruleset in output {
        for (rule_id, violation) in &ruleset.violations {
            // Lookup order: strategies map → label inference → LLM fallback
            let strategy = strategies
                .get(rule_id.as_str())
                .cloned()
                .or_else(|| infer_strategy_from_labels(&violation.labels).cloned())
                .unwrap_or(FixStrategy::Llm);

            for incident in &violation.incidents {
                let file_path = uri_to_path(&incident.file_uri, project_root);

                // Skip node_modules — these are updated via package.json
                // version bumps, not by patching source directly.
                // Note: src/vendor/ is NOT skipped — vendored source code
                // (e.g., forked libraries) is compiled as part of the project
                // and needs migration alongside the rest of the codebase.
                if file_path
                    .components()
                    .any(|c| c.as_os_str() == "node_modules")
                {
                    continue;
                }

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
                    FixStrategy::CssVariablePrefix {
                        old_prefix,
                        new_prefix,
                    } => {
                        // Treat CSS prefix changes as renames
                        let mappings = vec![RenameMapping {
                            old: old_prefix.clone(),
                            new: new_prefix.clone(),
                        }];
                        if let Some(fix) = plan_rename(rule_id, incident, &mappings, &file_path) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::UpdateDependency {
                        ref package,
                        ref new_version,
                    } => {
                        if let Some(fix) = plan_update_dependency(
                            rule_id,
                            incident,
                            package,
                            new_version,
                            &file_path,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::Manual => {
                        plan.manual.push(ManualFixItem {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                        });
                    }
                    FixStrategy::Llm => {
                        plan.pending_llm.push(LlmFixRequest {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
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
            // keep empty lines for now
            lines.retain(|_l| true);

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

            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                if let Some(new_line) = changed_lines.get(&i) {
                    output.push_str(&format!("-{}\n", line));
                    output.push_str(&format!("+{}\n", new_line));
                } else {
                    output.push_str(&format!(" {}\n", line));
                }
            }
        }
    }

    Ok(output)
}

// ── Pattern-based fix generators ──────────────────────────────────────────

fn plan_rename(
    rule_id: &str,
    incident: &Incident,
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
        file_uri: incident.file_uri.clone(),
        line,
        description: format!("Rename {}", desc),
    })
}

fn plan_remove_prop(rule_id: &str, incident: &Incident, file_path: &PathBuf) -> Option<PlannedFix> {
    let line = incident.line_number?;
    let prop_name = incident
        .variables
        .get("propName")
        .and_then(|v| v.as_str())?;

    // Read the actual file line to construct a precise removal edit.
    let source = std::fs::read_to_string(file_path).ok()?;
    let all_lines: Vec<&str> = source.lines().collect();
    let line_idx = (line as usize).saturating_sub(1);
    let file_line = all_lines.get(line_idx)?;
    let trimmed = file_line.trim();

    // If the entire line is just the prop (common in formatted JSX), remove it.
    // Patterns: `propName`, `propName={...}`, `propName="..."`, `propName={true}`
    if trimmed.starts_with(prop_name) {
        // Check if the prop value is self-contained on this line by counting
        // bracket/brace depth. If the value spans multiple lines (e.g.,
        // `actions={[ <Button>...</Button> ]}`), we need to remove all of them.
        let depth = bracket_depth(file_line);
        if depth == 0 {
            // Single-line prop — safe to remove just this line
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
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            // Multi-line prop value — scan forward to find where brackets balance.
            let mut cumulative_depth = depth;
            let mut end_idx = line_idx;
            for (i, subsequent_line) in all_lines.iter().enumerate().skip(line_idx + 1) {
                cumulative_depth += bracket_depth(subsequent_line);
                end_idx = i;
                if cumulative_depth <= 0 {
                    break;
                }
            }

            if cumulative_depth > 0 {
                // Could not find matching close bracket — bail to manual review
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!(
                        "Remove prop '{}' (unbalanced brackets, manual)",
                        prop_name
                    ),
                });
            }

            // Remove all lines from prop start through closing bracket
            let mut edits = Vec::new();
            for i in line_idx..=end_idx {
                if let Some(l) = all_lines.get(i) {
                    edits.push(TextEdit {
                        line: (i + 1) as u32,
                        old_text: l.to_string(),
                        new_text: String::new(),
                        rule_id: rule_id.to_string(),
                        description: format!(
                            "Remove prop '{}' (line {} of multi-line)",
                            prop_name,
                            i - line_idx + 1
                        ),
                    });
                }
            }

            Some(PlannedFix {
                edits,
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!(
                    "Remove prop '{}' ({} lines)",
                    prop_name,
                    end_idx - line_idx + 1
                ),
            })
        }
    } else {
        // Prop is inline with other content — try to remove just the prop fragment.
        // Match: ` propName={...}` or ` propName="..."` or ` propName`
        // Use a simple regex to find the prop and its value on a single line.
        let prop_re = regex::Regex::new(&format!(
            r#"\s+{prop_name}(?:=\{{[^}}]*\}}|="[^"]*"|='[^']*'|=\{{.*?\}})?"#
        ))
        .ok()?;

        if let Some(m) = prop_re.find(file_line) {
            // Verify the matched fragment has balanced brackets. If not, the value
            // spans multiple lines and a simple single-line removal would corrupt the file.
            if bracket_depth(m.as_str()) != 0 {
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!("Remove prop '{}' (multi-line inline, manual)", prop_name),
                });
            }

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
                file_uri: incident.file_uri.clone(),
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
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}' (manual)", prop_name),
            })
        }
    }
}

fn plan_import_path_change(
    rule_id: &str,
    incident: &Incident,
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
        file_uri: incident.file_uri.clone(),
        line,
        description: format!("Change import path to '{}'", new_path),
    })
}

fn plan_update_dependency(
    rule_id: &str,
    incident: &Incident,
    package: &str,
    new_version: &str,
    file_path: &PathBuf,
) -> Option<PlannedFix> {
    let line = incident.line_number?;
    let source = std::fs::read_to_string(file_path).ok()?;
    let file_line = source.lines().nth((line as usize).saturating_sub(1))?;

    // Verify this line references the expected package
    if !file_line.contains(package) {
        return None;
    }

    // Match the version string after the package name.
    // Handles common patterns: "^5.4.0", "~5.3.1", "5.4.0", ">=5.0.0", etc.
    let version_re = regex::Regex::new(r#"("[\^~><=]*\d+\.\d+\.\d+[^"]*")"#).ok()?;

    if let Some(m) = version_re.find(file_line) {
        let old_version = m.as_str();
        // Build new version string preserving the quote style
        let new_ver_quoted = format!("\"{}\"", new_version);

        Some(PlannedFix {
            edits: vec![TextEdit {
                line,
                old_text: old_version.to_string(),
                new_text: new_ver_quoted.clone(),
                rule_id: rule_id.to_string(),
                description: format!(
                    "Update {} from {} to {}",
                    package, old_version, new_ver_quoted
                ),
            }],
            confidence: FixConfidence::Exact,
            source: FixSource::Pattern,
            rule_id: rule_id.to_string(),
            file_uri: incident.file_uri.clone(),
            line,
            description: format!("Update {} to {}", package, new_version),
        })
    } else {
        None
    }
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

/// Count net bracket/brace depth change for a line.
/// Returns positive if more openers than closers, negative if more closers.
/// Ignores brackets inside string literals (single/double quoted).
fn bracket_depth(line: &str) -> i32 {
    let mut depth: i32 = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;
    let mut prev = '\0';
    for ch in line.chars() {
        match ch {
            '\'' if !in_double_quote && !in_backtick && prev != '\\' => {
                in_single_quote = !in_single_quote
            }
            '"' if !in_single_quote && !in_backtick && prev != '\\' => {
                in_double_quote = !in_double_quote
            }
            '`' if !in_single_quote && !in_double_quote && prev != '\\' => {
                in_backtick = !in_backtick
            }
            '{' | '[' | '(' if !in_single_quote && !in_double_quote && !in_backtick => depth += 1,
            '}' | ']' | ')' if !in_single_quote && !in_double_quote && !in_backtick => depth -= 1,
            _ => {}
        }
        prev = ch;
    }
    depth
}

/// Extract the matched text from incident variables.
/// Checks propName, componentName, importedName, className, variableName in that order.
fn get_matched_text(incident: &Incident) -> String {
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
/// This is a fallback for rules not covered by any strategy file.
fn infer_strategy_from_labels(labels: &[String]) -> Option<&'static FixStrategy> {
    for label in labels {
        match label.as_str() {
            "change-type=prop-removal" => return Some(&FixStrategy::RemoveProp),
            "change-type=dom-structure"
            | "change-type=behavioral"
            | "change-type=accessibility"
            | "change-type=interface-removal"
            | "change-type=module-export"
            | "change-type=other" => return Some(&FixStrategy::Manual),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test Incident with just the fields the fix-engine cares about.
    fn make_test_incident(
        uri: &str,
        line: u32,
        variables: BTreeMap<String, serde_json::Value>,
    ) -> Incident {
        Incident {
            file_uri: uri.to_string(),
            line_number: Some(line),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables,
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    // ── bracket_depth tests ──────────────────────────────────────────────

    #[test]
    fn test_bracket_depth_balanced() {
        assert_eq!(bracket_depth("{ foo: bar }"), 0);
        assert_eq!(bracket_depth("foo()"), 0);
        assert_eq!(bracket_depth("[1, 2, 3]"), 0);
        assert_eq!(bracket_depth("{ foo: [1, 2] }"), 0);
    }

    #[test]
    fn test_bracket_depth_open() {
        assert_eq!(bracket_depth("actions={["), 2); // { and [
        assert_eq!(bracket_depth("  <Button"), 0);
        assert_eq!(bracket_depth("foo(bar, {"), 2); // ( and {
    }

    #[test]
    fn test_bracket_depth_close() {
        assert_eq!(bracket_depth("]}"), -2);
        assert_eq!(bracket_depth(")"), -1);
    }

    #[test]
    fn test_bracket_depth_nested() {
        assert_eq!(bracket_depth("{ a: { b: [1] } }"), 0);
        assert_eq!(bracket_depth("f(g(h(x)))"), 0);
    }

    #[test]
    fn test_bracket_depth_ignores_string_literals() {
        assert_eq!(bracket_depth(r#"  foo="{not a bracket}""#), 0);
        assert_eq!(bracket_depth("  foo='[still not]'"), 0);
        assert_eq!(bracket_depth("  foo=`${`nested`}`"), 0);
    }

    #[test]
    fn test_bracket_depth_empty() {
        assert_eq!(bracket_depth(""), 0);
        assert_eq!(bracket_depth("   just text   "), 0);
    }

    #[test]
    fn test_bracket_depth_escaped_quotes() {
        // Escaped quote should not toggle string mode
        assert_eq!(bracket_depth(r#"  "escaped \" quote" "#), 0);
    }

    // ── dedup_import_specifiers tests ────────────────────────────────────

    #[test]
    fn test_dedup_import_no_duplicates() {
        let mut lines = vec!["import { Foo, Bar } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert!(lines[0].contains("Foo"));
        assert!(lines[0].contains("Bar"));
    }

    #[test]
    fn test_dedup_import_removes_duplicates() {
        let mut lines =
            vec!["import { Content, Content, Content } from '@patternfly/react-core';".to_string()];
        dedup_import_specifiers(&mut lines);
        // Should contain exactly one "Content"
        let count = lines[0].matches("Content").count();
        assert_eq!(
            count, 1,
            "Expected 1 occurrence of Content, got {}: {}",
            count, lines[0]
        );
    }

    #[test]
    fn test_dedup_import_preserves_different_specifiers() {
        let mut lines = vec!["import { Foo, Bar, Foo, Baz, Bar } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0].matches("Foo").count(), 1);
        assert_eq!(lines[0].matches("Bar").count(), 1);
        assert_eq!(lines[0].matches("Baz").count(), 1);
    }

    #[test]
    fn test_dedup_import_preserves_aliases() {
        let mut lines = vec!["import { Foo as F, Foo as F } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0].matches("Foo as F").count(), 1);
    }

    #[test]
    fn test_dedup_import_non_import_lines_unchanged() {
        let original = "const x = { Foo, Foo };".to_string();
        let mut lines = vec![original.clone()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0], original);
    }

    // ── get_matched_text tests ───────────────────────────────────────────

    #[test]
    fn test_get_matched_text_prop_name_first() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".to_string(),
            serde_json::Value::String("isActive".to_string()),
        );
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Button".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert_eq!(get_matched_text(&incident), "isActive");
    }

    #[test]
    fn test_get_matched_text_component_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Button".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text(&incident), "Button");
    }

    #[test]
    fn test_get_matched_text_imported_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text(&incident), "Chip");
    }

    #[test]
    fn test_get_matched_text_empty_when_no_known_vars() {
        let incident = make_test_incident("", 1, BTreeMap::new());
        assert_eq!(get_matched_text(&incident), "");
    }

    #[test]
    fn test_get_matched_text_ignores_non_string_values() {
        let mut vars = BTreeMap::new();
        vars.insert("propName".to_string(), serde_json::Value::Bool(true));
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Fallback".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text(&incident), "Fallback");
    }

    // ── uri_to_path tests ────────────────────────────────────────────────

    #[test]
    fn test_uri_to_path_absolute() {
        let path = uri_to_path(
            "file:///home/user/project/src/App.tsx",
            std::path::Path::new("/ignored"),
        );
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_relative() {
        let path = uri_to_path("src/App.tsx", std::path::Path::new("/home/user/project"));
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_no_file_prefix() {
        let path = uri_to_path("/absolute/path.tsx", std::path::Path::new("/root"));
        assert_eq!(path, PathBuf::from("/absolute/path.tsx"));
    }

    // ── infer_strategy_from_labels tests ─────────────────────────────────

    #[test]
    fn test_infer_prop_removal() {
        let labels = vec!["change-type=prop-removal".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveProp)));
    }

    #[test]
    fn test_infer_dom_structure_manual() {
        let labels = vec!["change-type=dom-structure".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_behavioral_manual() {
        let labels = vec!["change-type=behavioral".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_accessibility_manual() {
        let labels = vec!["change-type=accessibility".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_interface_removal_manual() {
        let labels = vec!["change-type=interface-removal".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_module_export_manual() {
        let labels = vec!["change-type=module-export".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_other_manual() {
        let labels = vec!["change-type=other".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_unknown_label_returns_none() {
        let labels = vec!["change-type=rename".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_empty_labels_returns_none() {
        let labels: Vec<String> = Vec::new();
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_first_matching_label_wins() {
        let labels = vec![
            "framework=patternfly".to_string(),
            "change-type=prop-removal".to_string(),
            "change-type=dom-structure".to_string(), // would also match, but prop-removal comes first
        ];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveProp)));
    }
}
