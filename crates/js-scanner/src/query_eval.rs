//! Query-based condition evaluation using the React project index.
//!
//! Replaces the file-by-file AST walking scanner with queries against the
//! pre-built [`ReactProjectIndex`].  Each [`ReferencedCondition`] is translated
//! into one or more index queries, using `filter_defs` and `filter_refs` for
//! language-specific filtering.
//!
//! [`ReactProjectIndex`]: ast_index_react::types::ReactProjectIndex

use std::path::Path;

use ast_index_react::types::{ReactDefData, ReactProjectIndex, ReactRefData};
use frontend_core::capabilities::{ReferencedCondition, ReferenceLocation};
use frontend_core::incident::{self, Incident, Location, Position};
use regex::Regex;

/// Evaluate a [`ReferencedCondition`] against the index and return incidents.
///
/// Dispatches to a specialised evaluator per `location`:
///
/// | Location        | Index query strategy                      |
/// |-----------------|-------------------------------------------|
/// | `IMPORT`        | Query for the imported symbol by name      |
/// | `JSX_COMPONENT` | Query for the component, filter JSX refs   |
/// | `JSX_PROP`      | Query for the *component*, filter by prop  |
/// | `TYPE_REFERENCE`| Query for the type, filter type-position   |
/// | `FUNCTION_CALL` | Query for the function, filter call refs   |
/// | `None`          | Query for symbol, accept any ref           |
pub fn evaluate_referenced(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
) -> anyhow::Result<Vec<Incident>> {
    let file_pattern_re = condition
        .file_pattern
        .as_deref()
        .map(Regex::new)
        .transpose()?;

    let pattern_re = Regex::new(&condition.pattern)?;

    match condition.location.as_ref() {
        Some(ReferenceLocation::Import) => {
            eval_import(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
        Some(ReferenceLocation::JsxProp) => {
            eval_jsx_prop(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
        Some(ReferenceLocation::JsxComponent) => {
            eval_jsx_component(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
        Some(ReferenceLocation::TypeReference) => {
            eval_type_reference(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
        Some(ReferenceLocation::FunctionCall) => {
            eval_function_call(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
        None => {
            // No location filter -- match any ref kind.
            eval_any(condition, index, root, &pattern_re, file_pattern_re.as_ref())
        }
    }
}

// ---------------------------------------------------------------------------
// IMPORT evaluation
// ---------------------------------------------------------------------------

fn eval_import(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    let from_re = condition.from.as_deref().map(Regex::new).transpose()?;
    let mut incidents = Vec::new();

    // Walk every file in the index and check import declarations directly.
    // This avoids the query system's definition-centric model and instead
    // operates on the raw import data, which is how IMPORT rules work:
    // "does this file import symbol X from module Y?"
    for entry in index.packages().iter() {
        for file in entry.value() {
            if !file_matches(root, &file.path, file_pattern_re) {
                continue;
            }
            for import in &file.imports {
                // Check `from` filter against the import source.
                // The import's raw_specifier has the original specifier;
                // fall back to source (the resolved package name).
                let specifier = import
                    .language_data
                    .raw_specifier
                    .as_deref()
                    .unwrap_or(&import.source);

                if let Some(ref from_re) = from_re {
                    // Match against both raw specifier and resolved source.
                    if !from_re.is_match(specifier) && !from_re.is_match(&import.source) {
                        continue;
                    }
                }

                for sym in &import.symbols {
                    let local_name = sym.alias.as_deref().unwrap_or(&sym.name);
                    let original_name = &sym.name;

                    // Match against both the original exported name and the
                    // local binding name.  For default imports the original
                    // is "default" so we only check the local name.  For
                    // namespace imports the original is "*" so we only check
                    // the local name.
                    let matched = if original_name == "default" || original_name == "*" {
                        pattern_re.is_match(local_name)
                    } else {
                        pattern_re.is_match(original_name)
                            || pattern_re.is_match(local_name)
                    };

                    if matched {
                        // Use the most meaningful name for matchingText:
                        // prefer the original export name, fall back to local.
                        let display_name = if original_name == "default" || original_name == "*" {
                            local_name
                        } else {
                            original_name
                        };

                        let mut inc = make_incident_from_span(
                            root,
                            &file.path,
                            &file.source_text,
                            sym.span,
                        );
                        inc.variables.insert(
                            "matchingText".into(),
                            serde_json::Value::String(display_name.to_string()),
                        );
                        inc.variables.insert(
                            "module".into(),
                            serde_json::Value::String(specifier.to_string()),
                        );
                        incidents.push(inc);
                    }
                }
            }
        }
    }

    Ok(incidents)
}

// ---------------------------------------------------------------------------
// JSX_PROP evaluation
// ---------------------------------------------------------------------------

fn eval_jsx_prop(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    let component_re = condition
        .component
        .as_deref()
        .map(Regex::new)
        .transpose()?;
    let value_re = condition.value.as_deref().map(Regex::new).transpose()?;
    let from_re = condition.from.as_deref().map(Regex::new).transpose()?;
    let mut incidents = Vec::new();

    // Walk all files and find JSX component refs.
    for entry in index.packages().iter() {
        for file in entry.value() {
            if !file_matches(root, &file.path, file_pattern_re) {
                continue;
            }
            for sym_ref in &file.symbol_refs {
                if !sym_ref.language_data.ts.is_jsx_component {
                    continue;
                }

                // Check component name filter.
                // For member expressions like `<Toolbar.Item>`, the ref name
                // is "Toolbar.Item" but the rule may use `^Item$`.  Check
                // both the full name and the last segment.
                if let Some(ref comp_re) = component_re {
                    let short_name = sym_ref.name.rsplit('.').next().unwrap_or(&sym_ref.name);
                    if !comp_re.is_match(&sym_ref.name) && !comp_re.is_match(short_name) {
                        continue;
                    }
                }

                // Check `from` filter: the component must be imported from a matching module.
                // For member expressions, look up the object part (e.g., "Toolbar" from "Toolbar.Item").
                if let Some(ref from_re) = from_re {
                    let lookup_name = sym_ref.name.split('.').next().unwrap_or(&sym_ref.name);
                    if !import_matches_from(file, lookup_name, from_re) {
                        continue;
                    }
                }

                // Check each prop against the pattern.
                for prop in &sym_ref.language_data.jsx_props {
                    if prop.is_spread {
                        // For spread props, check if the spread object's
                        // definition has matching properties.
                        if let Some(spread_incidents) = check_spread_prop(
                            index,
                            root,
                            file,
                            &sym_ref.name,
                            prop,
                            pattern_re,
                            value_re.as_ref(),
                        ) {
                            incidents.extend(spread_incidents);
                        }
                        continue;
                    }

                    // Check nested object keys: `formGroupProps={{ labelIcon: ... }}`
                    // If the prop name doesn't match, check if any nested key matches.
                    if !pattern_re.is_match(&prop.name) {
                        let nested_matches: Vec<_> = prop
                            .nested_object_keys
                            .iter()
                            .filter(|k| pattern_re.is_match(k))
                            .collect();
                        if !nested_matches.is_empty() {
                            for key_name in nested_matches {
                                let mut inc = make_incident_from_span(
                                    root,
                                    &file.path,
                                    &file.source_text,
                                    prop.span,
                                );
                                inc.variables.insert(
                                    "propName".into(),
                                    serde_json::Value::String(key_name.clone()),
                                );
                                inc.variables.insert(
                                    "componentName".into(),
                                    serde_json::Value::String(sym_ref.name.clone()),
                                );
                                inc.variables.insert(
                                    "nestedObjectKey".into(),
                                    serde_json::Value::String("true".to_string()),
                                );
                                if let Some(module) = find_import_module(file, &sym_ref.name) {
                                    inc.variables.insert(
                                        "module".into(),
                                        serde_json::Value::String(module),
                                    );
                                }
                                incidents.push(inc);
                            }
                        }
                        continue;
                    }

                    // Check value filter.
                    if let Some(ref val_re) = value_re {
                        let prop_value = prop
                            .string_value
                            .as_deref()
                            .or(prop.expression_text.as_deref())
                            .unwrap_or("");
                        if !val_re.is_match(prop_value) {
                            continue;
                        }
                    }

                    let mut inc = make_incident_from_span(
                        root,
                        &file.path,
                        &file.source_text,
                        prop.span,
                    );
                    inc.variables.insert(
                        "propName".into(),
                        serde_json::Value::String(prop.name.clone()),
                    );
                    inc.variables.insert(
                        "componentName".into(),
                        serde_json::Value::String(sym_ref.name.clone()),
                    );
                    if let Some(ref sv) = prop.string_value {
                        inc.variables.insert(
                            "propValue".into(),
                            serde_json::Value::String(sv.clone()),
                        );
                    } else if let Some(ref et) = prop.expression_text {
                        inc.variables.insert(
                            "propValue".into(),
                            serde_json::Value::String(et.clone()),
                        );
                    }
                    // Add module info for the component.
                    if let Some(module) = find_import_module(file, &sym_ref.name) {
                        inc.variables.insert(
                            "module".into(),
                            serde_json::Value::String(module),
                        );
                    }
                    incidents.push(inc);
                }
            }

            // ── Typed object literal scanning ────────────────────────
            // Detect `const config: ToolbarItemProps = { align: ... }`
            // by checking SymbolDefs with type annotations ending in "Props".
            if component_re.is_some() {
                for def in &file.symbol_defs {
                    let type_ann = match &def.kind {
                        ast_index::SymbolKind::Const(td) | ast_index::SymbolKind::Variable(td) => {
                            td.type_annotation.as_ref()
                        }
                        _ => None,
                    };
                    let type_ann = match type_ann {
                        Some(ta) => ta,
                        None => continue,
                    };

                    // Extract the component name from the type annotation.
                    // Direct: `ToolbarItemProps` → `ToolbarItem`
                    // Wrapped: `Partial<ToolbarItemProps>` → `ToolbarItem`
                    let type_name = extract_props_type_name(type_ann);
                    let type_name = match type_name {
                        Some(n) => n,
                        None => continue,
                    };

                    // Strip "Props" suffix to get the component name
                    let comp_name = match type_name.strip_suffix("Props") {
                        Some(n) => n,
                        None => continue,
                    };

                    if let Some(ref comp_re) = component_re {
                        if !comp_re.is_match(comp_name) {
                            continue;
                        }
                    }

                    // Check object_properties for matching prop names
                    for prop_name in &def.language_data.object_properties {
                        if !pattern_re.is_match(prop_name) {
                            continue;
                        }

                        // Check value filter -- for typed objects we don't
                        // have individual prop values, so skip if value filter
                        // is present.
                        // TODO: extract prop values from object literals at
                        // index time for more precise matching.
                        if value_re.is_some() {
                            // Can't verify value, but still report the incident
                            // since the prop name matches.
                        }

                        let mut inc = make_incident_from_span(
                            root,
                            &file.path,
                            &file.source_text,
                            def.span,
                        );
                        inc.variables.insert(
                            "propName".into(),
                            serde_json::Value::String(prop_name.clone()),
                        );
                        inc.variables.insert(
                            "componentName".into(),
                            serde_json::Value::String(comp_name.to_string()),
                        );
                        inc.variables.insert(
                            "typedObjectLiteral".into(),
                            serde_json::Value::String("true".to_string()),
                        );
                        if let Some(module) = find_import_module(file, &type_name) {
                            inc.variables.insert(
                                "module".into(),
                                serde_json::Value::String(module),
                            );
                        }
                        incidents.push(inc);
                    }
                }
            }

            // ── Typed local objects inside function defs ─────────
            // Detect `const x: ToolbarItemProps = { align: ... }` inside
            // function bodies (hooks, components, helpers).
            if component_re.is_some() {
                for def in &file.symbol_defs {
                    for typed_obj in &def.language_data.typed_local_objects {
                        let type_name =
                            extract_props_type_name_from_raw(&typed_obj.type_name);
                        let type_name = match type_name {
                            Some(n) => n,
                            None => continue,
                        };
                        let comp_name = match type_name.strip_suffix("Props") {
                            Some(n) => n,
                            None => continue,
                        };
                        if let Some(ref comp_re) = component_re {
                            if !comp_re.is_match(comp_name) {
                                continue;
                            }
                        }
                        for prop_name in &typed_obj.properties {
                            if !pattern_re.is_match(prop_name) {
                                continue;
                            }
                            let mut inc = make_incident_from_span(
                                root,
                                &file.path,
                                &file.source_text,
                                typed_obj.span,
                            );
                            inc.variables.insert(
                                "propName".into(),
                                serde_json::Value::String(prop_name.clone()),
                            );
                            inc.variables.insert(
                                "componentName".into(),
                                serde_json::Value::String(comp_name.to_string()),
                            );
                            inc.variables.insert(
                                "typedObjectLiteral".into(),
                                serde_json::Value::String("true".to_string()),
                            );
                            if let Some(module) = find_import_module(file, &type_name) {
                                inc.variables.insert(
                                    "module".into(),
                                    serde_json::Value::String(module),
                                );
                            }
                            incidents.push(inc);
                        }
                    }
                }
            }
        }
    }

    Ok(incidents)
}

/// Extract the inner Props type name from a raw type annotation string.
///
/// Handles:
/// - `"ToolbarItemProps"` → `Some("ToolbarItemProps")`
/// - `"Partial<ToolbarItemProps>"` → `Some("ToolbarItemProps")`
/// - `"Omit<ToolbarItemProps, 'key'>"` → `Some("ToolbarItemProps")`
/// - `"string"` → `None`
fn extract_props_type_name_from_raw(raw: &str) -> Option<String> {
    // Direct: ends with "Props"
    if raw.ends_with("Props") && !raw.contains('<') {
        return Some(raw.to_string());
    }

    // Wrapper: `Partial<ToolbarItemProps>`, `Omit<ToolbarItemProps, "key">`
    if let Some(inner_start) = raw.find('<') {
        let wrapper = &raw[..inner_start];
        if matches!(wrapper, "Partial" | "Required" | "Readonly" | "Omit" | "Pick") {
            // Extract the first type argument
            let inner = &raw[inner_start + 1..];
            // Find the end of the first type arg (before ',' or '>')
            let end = inner.find(|c| c == ',' || c == '>').unwrap_or(inner.len());
            let first_arg = inner[..end].trim();
            if first_arg.ends_with("Props") {
                return Some(first_arg.to_string());
            }
        }
    }

    // Direct check without generics
    if raw.ends_with("Props") {
        return Some(raw.to_string());
    }

    None
}

/// Extract the inner Props type name from a type annotation, handling
/// wrapper types like `Partial<T>`, `Omit<T, K>`, `Required<T>`.
fn extract_props_type_name(ta: &ast_index::TypeAnnotation) -> Option<String> {
    // Direct: `ToolbarItemProps`
    if let Some(ref name) = ta.name {
        if name.ends_with("Props") {
            return Some(name.clone());
        }
        // Wrapper: `Partial<ToolbarItemProps>`, `Omit<ToolbarItemProps, "key">`
        if matches!(name.as_str(), "Partial" | "Required" | "Readonly" | "Omit" | "Pick") {
            if let Some(inner) = ta.type_arguments.first() {
                return extract_props_type_name(inner);
            }
        }
    }

    // Check components (union/intersection types)
    for comp in &ta.components {
        if let Some(name) = extract_props_type_name(comp) {
            return Some(name);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// JSX_COMPONENT evaluation
// ---------------------------------------------------------------------------

fn eval_jsx_component(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    let parent_re = condition.parent.as_deref().map(Regex::new).transpose()?;
    let not_parent_re = condition
        .not_parent
        .as_deref()
        .map(Regex::new)
        .transpose()?;
    let from_re = condition.from.as_deref().map(Regex::new).transpose()?;
    let parent_from_re = condition
        .parent_from
        .as_deref()
        .map(Regex::new)
        .transpose()?;
    let child_re = condition.child.as_deref().map(Regex::new).transpose()?;
    let not_child_re = condition
        .not_child
        .as_deref()
        .map(Regex::new)
        .transpose()?;
    let requires_child_re = condition
        .requires_child
        .as_deref()
        .map(Regex::new)
        .transpose()?;
    let mut incidents = Vec::new();

    for entry in index.packages().iter() {
        for file in entry.value() {
            if !file_matches(root, &file.path, file_pattern_re) {
                continue;
            }
            for sym_ref in &file.symbol_refs {
                if !sym_ref.language_data.ts.is_jsx_component {
                    continue;
                }
                // For member expressions like `<Toolbar.Item>`, check both
                // the full name and the last segment.
                let short_name = sym_ref.name.rsplit('.').next().unwrap_or(&sym_ref.name);
                if !pattern_re.is_match(&sym_ref.name) && !pattern_re.is_match(short_name) {
                    continue;
                }

                // Check `from` filter.
                if let Some(ref from_re) = from_re {
                    let lookup_name = sym_ref.name.split('.').next().unwrap_or(&sym_ref.name);
                    if !import_matches_from(file, lookup_name, from_re) {
                        continue;
                    }
                }

                // Resolve effective parent (collapsing transparent wrappers).
                let raw_parent = sym_ref.language_data.parent_component.as_deref();
                let mut effective_parent =
                    resolve_effective_parent(index, file, raw_parent);

                // Function-return parent tracing: if the component has no
                // parent but is inside a function, check if that function is
                // called inside JSX children somewhere, and inherit the
                // call-site parent.
                if effective_parent.is_none() {
                    if let Some(enc_fn_span) = sym_ref.language_data.enclosing_function_span {
                        effective_parent =
                            resolve_fn_call_site_parent(index, file, enc_fn_span);
                    }
                }

                // Check `parent` filter.
                if let Some(ref p_re) = parent_re {
                    match effective_parent.as_deref() {
                        Some(p) if p_re.is_match(p) => {}
                        _ => continue,
                    }
                }

                // Check `notParent` filter.
                // When the effective parent is None (render boundary — top of
                // a hook, React.FC, or standalone component), suppress the
                // rule.  We can't confirm the component has a "wrong" parent
                // when there is no parent at all.
                if let Some(ref np_re) = not_parent_re {
                    match effective_parent.as_deref() {
                        Some(p) if np_re.is_match(p) => continue,
                        None => continue, // render boundary — suppress
                        _ => {}
                    }
                }

                // Check `parentFrom` filter.
                if let Some(ref pf_re) = parent_from_re {
                    if let Some(ref parent_name) = effective_parent {
                        if !import_matches_from(file, parent_name, pf_re) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }

                // Collect direct JSX children of this component instance
                // (needed for child/notChild/requiresChild filters).
                let children = if child_re.is_some()
                    || not_child_re.is_some()
                    || requires_child_re.is_some()
                {
                    find_jsx_children(file, &sym_ref.name, &sym_ref.language_data)
                } else {
                    Vec::new()
                };

                // Check `child` filter: only fire when a matching child IS present.
                if let Some(ref c_re) = child_re {
                    let has_matching = children.iter().any(|c| c_re.is_match(c));
                    if !has_matching {
                        continue;
                    }
                }

                // Check `requiresChild` filter: fire when NO matching child exists.
                if let Some(ref rc_re) = requires_child_re {
                    let has_matching = children.iter().any(|c| rc_re.is_match(c));
                    if has_matching {
                        continue;
                    }
                }

                // Check `notChild` filter: emit an incident per non-matching child.
                if let Some(ref nc_re) = not_child_re {
                    for child_name in &children {
                        if !nc_re.is_match(child_name) {
                            let mut inc = make_incident_from_span(
                                root,
                                &file.path,
                                &file.source_text,
                                sym_ref.span,
                            );
                            inc.variables.insert(
                                "componentName".into(),
                                serde_json::Value::String(sym_ref.name.clone()),
                            );
                            inc.variables.insert(
                                "childName".into(),
                                serde_json::Value::String(child_name.clone()),
                            );
                            if let Some(module) = find_import_module(file, &sym_ref.name) {
                                inc.variables.insert(
                                    "module".into(),
                                    serde_json::Value::String(module),
                                );
                            }
                            incidents.push(inc);
                        }
                    }
                    continue; // notChild emits its own incidents, skip normal emission
                }

                let mut inc = make_incident_from_span(
                    root,
                    &file.path,
                    &file.source_text,
                    sym_ref.span,
                );
                inc.variables.insert(
                    "componentName".into(),
                    serde_json::Value::String(sym_ref.name.clone()),
                );
                if let Some(ref p) = effective_parent {
                    inc.variables.insert(
                        "parentName".into(),
                        serde_json::Value::String(p.clone()),
                    );
                }
                if let Some(module) = find_import_module(file, &sym_ref.name) {
                    inc.variables.insert(
                        "module".into(),
                        serde_json::Value::String(module),
                    );
                }
                if let Some(ref parent_name) = effective_parent {
                    if let Some(parent_mod) = find_import_module(file, parent_name) {
                        inc.variables.insert(
                            "parentFrom".into(),
                            serde_json::Value::String(parent_mod),
                        );
                    }
                }
                incidents.push(inc);
            }
        }
    }

    Ok(incidents)
}

// ---------------------------------------------------------------------------
// TYPE_REFERENCE evaluation
// ---------------------------------------------------------------------------

fn eval_type_reference(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    let from_re = condition.from.as_deref().map(Regex::new).transpose()?;
    let mut incidents = Vec::new();

    for entry in index.packages().iter() {
        for file in entry.value() {
            if !file_matches(root, &file.path, file_pattern_re) {
                continue;
            }
            for sym_ref in &file.symbol_refs {
                if !sym_ref.language_data.ts.is_type_position {
                    continue;
                }
                if !pattern_re.is_match(&sym_ref.name) {
                    continue;
                }
                if let Some(ref from_re) = from_re {
                    if !import_matches_from(file, &sym_ref.name, from_re) {
                        continue;
                    }
                }
                let mut inc = make_incident_from_span(
                    root,
                    &file.path,
                    &file.source_text,
                    sym_ref.span,
                );
                inc.variables.insert(
                    "matchingText".into(),
                    serde_json::Value::String(sym_ref.name.clone()),
                );
                if let Some(module) = find_import_module(file, &sym_ref.name) {
                    inc.variables.insert(
                        "module".into(),
                        serde_json::Value::String(module),
                    );
                }
                incidents.push(inc);
            }
        }
    }

    Ok(incidents)
}

// ---------------------------------------------------------------------------
// FUNCTION_CALL evaluation
// ---------------------------------------------------------------------------

fn eval_function_call(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    let from_re = condition.from.as_deref().map(Regex::new).transpose()?;
    let mut incidents = Vec::new();

    for entry in index.packages().iter() {
        for file in entry.value() {
            if !file_matches(root, &file.path, file_pattern_re) {
                continue;
            }
            for sym_ref in &file.symbol_refs {
                // Function calls are refs that are not JSX components and
                // not type positions. The name matching handles the rest.
                if sym_ref.language_data.ts.is_jsx_component
                    || sym_ref.language_data.ts.is_type_position
                {
                    continue;
                }
                if !pattern_re.is_match(&sym_ref.name) {
                    continue;
                }
                if let Some(ref from_re) = from_re {
                    if !import_matches_from(file, &sym_ref.name, from_re) {
                        continue;
                    }
                }
                let mut inc = make_incident_from_span(
                    root,
                    &file.path,
                    &file.source_text,
                    sym_ref.span,
                );
                inc.variables.insert(
                    "matchingText".into(),
                    serde_json::Value::String(sym_ref.name.clone()),
                );
                if let Some(module) = find_import_module(file, &sym_ref.name) {
                    inc.variables.insert(
                        "module".into(),
                        serde_json::Value::String(module),
                    );
                }
                if let Some(ref arg_val) = sym_ref.language_data.ts.call_arg_value {
                    inc.variables.insert(
                        "callArgValue".into(),
                        serde_json::Value::String(arg_val.clone()),
                    );
                }
                incidents.push(inc);
            }
        }
    }

    Ok(incidents)
}

// ---------------------------------------------------------------------------
// No-location (match all) evaluation
// ---------------------------------------------------------------------------

fn eval_any(
    condition: &ReferencedCondition,
    index: &ReactProjectIndex,
    root: &Path,
    pattern_re: &Regex,
    file_pattern_re: Option<&Regex>,
) -> anyhow::Result<Vec<Incident>> {
    // Combine results from all location types.
    let mut all = Vec::new();
    all.extend(eval_import(condition, index, root, pattern_re, file_pattern_re)?);
    all.extend(eval_jsx_component(condition, index, root, pattern_re, file_pattern_re)?);
    all.extend(eval_jsx_prop(condition, index, root, pattern_re, file_pattern_re)?);
    all.extend(eval_type_reference(condition, index, root, pattern_re, file_pattern_re)?);
    all.extend(eval_function_call(condition, index, root, pattern_re, file_pattern_re)?);
    Ok(all)
}

// ===========================================================================
// Helpers
// ===========================================================================

type ReactCachedFile = ast_index::CachedFile<
    ast_index_typescript::types::TsImportData,
    ReactDefData,
    ReactRefData,
>;

/// Check whether a file path matches the optional `filePattern` regex.
fn file_matches(root: &Path, file_path: &Path, file_pattern_re: Option<&Regex>) -> bool {
    match file_pattern_re {
        None => true,
        Some(re) => {
            let relative = file_path
                .strip_prefix(root)
                .unwrap_or(file_path)
                .to_string_lossy();
            re.is_match(&relative)
        }
    }
}

/// Check whether an import of `symbol_name` in `file` comes from a module
/// matching `from_re`.
fn import_matches_from(
    file: &ReactCachedFile,
    symbol_name: &str,
    from_re: &Regex,
) -> bool {
    for import in &file.imports {
        let specifier = import
            .language_data
            .raw_specifier
            .as_deref()
            .unwrap_or(&import.source);

        let has_symbol = import.symbols.iter().any(|s| {
            let local = s.alias.as_deref().unwrap_or(&s.name);
            local == symbol_name || s.name == symbol_name
        });

        if has_symbol && (from_re.is_match(specifier) || from_re.is_match(&import.source)) {
            return true;
        }
    }
    false
}

/// Find the module specifier that a symbol was imported from.
fn find_import_module(file: &ReactCachedFile, symbol_name: &str) -> Option<String> {
    for import in &file.imports {
        let has_symbol = import.symbols.iter().any(|s| {
            let local = s.alias.as_deref().unwrap_or(&s.name);
            local == symbol_name || s.name == symbol_name
        });
        if has_symbol {
            return Some(
                import
                    .language_data
                    .raw_specifier
                    .clone()
                    .unwrap_or_else(|| import.source.clone()),
            );
        }
    }
    None
}

/// Resolve the effective parent by collapsing transparent wrappers.
///
/// If the direct parent component is transparent (renders children through),
/// walks up the parent chain until a non-transparent ancestor is found.
fn resolve_effective_parent(
    index: &ReactProjectIndex,
    file: &ReactCachedFile,
    raw_parent: Option<&str>,
) -> Option<String> {
    let mut current = raw_parent.map(str::to_owned);
    let mut seen = std::collections::HashSet::new();

    while let Some(ref parent_name) = current {
        if !seen.insert(parent_name.clone()) {
            break; // Cycle detected.
        }

        // Check if this parent is transparent by querying its definition.
        if is_component_transparent(index, file, parent_name) {
            // The transparent wrapper's own parent becomes the candidate.
            // Find the wrapper's parent_component from the same file's refs.
            current = find_parent_of_component(file, parent_name);
        } else {
            break; // Found a non-transparent ancestor.
        }
    }

    current
}

/// Resolve the parent context for a JSX ref inside a function body by finding
/// where that function is called from within JSX children.
///
/// For `function renderItems() { return <DropdownItem />; }` called as
/// `<Dropdown>{renderItems()}</Dropdown>`, this returns `Some("Dropdown")`.
fn resolve_fn_call_site_parent(
    index: &ReactProjectIndex,
    file: &ReactCachedFile,
    enc_fn_span: ast_index::Span,
) -> Option<String> {
    // 1. Find the function definition whose span matches enc_fn_span.
    //    If no exact match (e.g., the span is an inline callback), find
    //    the smallest enclosing named function definition.
    let fn_name = file
        .symbol_defs
        .iter()
        .find(|d| d.span == enc_fn_span)
        .or_else(|| {
            // Find the smallest enclosing function def (for callbacks
            // inside named functions like `items.map(cb)` inside `renderItems`)
            file.symbol_defs
                .iter()
                .filter(|d| {
                    d.span.start <= enc_fn_span.start && d.span.end >= enc_fn_span.end
                })
                .min_by_key(|d| d.span.end - d.span.start)
        })
        .map(|d| d.name.clone())?;

    // 2. Find call sites of this function in the SAME file that are
    //    inside JSX children.  A call site ref has the function name and
    //    is inside some component's jsx_element_span.
    let call_site_parent = find_call_site_parent_in_file(file, &fn_name);
    if call_site_parent.is_some() {
        return call_site_parent;
    }

    // 3. Cross-file: check if this function is exported and called from
    //    or used as a prop value in another file.
    let is_exported = file
        .symbol_defs
        .iter()
        .any(|d| d.name == fn_name && d.exported);

    if is_exported {
        for entry in index.packages().iter() {
            for other_file in entry.value() {
                if other_file.path == file.path {
                    continue;
                }
                // Check if other_file imports this function
                let local_name = other_file.imports.iter().find_map(|imp| {
                    imp.symbols.iter().find_map(|s| {
                        let local = s.alias.as_deref().unwrap_or(&s.name);
                        if local == fn_name || s.name == fn_name {
                            Some(local.to_string())
                        } else {
                            None
                        }
                    })
                });
                if let Some(local) = local_name {
                    // Check for direct call site: {fn_name()}
                    let parent = find_call_site_parent_in_file(other_file, &local);
                    if parent.is_some() {
                        return parent;
                    }
                    // Check for prop-value usage: <Component toggle={fn_name}>
                    let parent = find_prop_value_parent_in_file(other_file, &local);
                    if parent.is_some() {
                        return parent;
                    }
                }
            }
        }
    }

    None
}

/// Find the parent component where a function is used as a JSX prop value.
///
/// For `<Select toggle={toggle}>`, returns `Some("Select")`.
fn find_prop_value_parent_in_file(file: &ReactCachedFile, fn_name: &str) -> Option<String> {
    for sym_ref in &file.symbol_refs {
        if !sym_ref.language_data.ts.is_jsx_component {
            continue;
        }
        for prop in &sym_ref.language_data.jsx_props {
            if let Some(ref expr) = prop.expression_text {
                // Check if the prop value is the function name
                if expr.trim() == fn_name {
                    return Some(sym_ref.name.clone());
                }
            }
        }
    }
    None
}

/// Find the parent component at a function's call site within a file.
///
/// Looks for a SymbolRef with the given name whose span is inside some
/// JSX component's `jsx_element_span`, and returns that component's name.
fn find_call_site_parent_in_file(file: &ReactCachedFile, fn_name: &str) -> Option<String> {
    // Collect all JSX component refs with element spans in this file.
    let jsx_components: Vec<_> = file
        .symbol_refs
        .iter()
        .filter(|r| {
            r.language_data.ts.is_jsx_component && r.language_data.jsx_element_span.is_some()
        })
        .collect();

    // Find refs that reference the function name (call sites).
    for call_ref in &file.symbol_refs {
        if call_ref.name != fn_name || call_ref.language_data.ts.is_jsx_component {
            continue;
        }
        // Find the innermost JSX component whose element span contains this call site.
        let mut best_parent: Option<(&str, u32)> = None; // (name, span_size)
        for jsx_ref in &jsx_components {
            if let Some(el_span) = jsx_ref.language_data.jsx_element_span {
                if call_ref.span.start >= el_span.start && call_ref.span.end <= el_span.end {
                    let size = el_span.end - el_span.start;
                    match best_parent {
                        None => best_parent = Some((&jsx_ref.name, size)),
                        Some((_, prev_size)) if size < prev_size => {
                            best_parent = Some((&jsx_ref.name, size));
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some((name, _)) = best_parent {
            return Some(name.to_string());
        }
    }

    None
}

/// Check if a component is transparent by looking up its definition in the index.
fn is_component_transparent(
    index: &ReactProjectIndex,
    file: &ReactCachedFile,
    component_name: &str,
) -> bool {
    // First, find what module the component is imported from.
    let import_source = file.imports.iter().find_map(|imp| {
        let has_it = imp.symbols.iter().any(|s| {
            let local = s.alias.as_deref().unwrap_or(&s.name);
            local == component_name
        });
        if has_it {
            Some(imp.source.clone())
        } else {
            None
        }
    });

    let Some(source_pkg) = import_source else {
        return false;
    };

    // Search the source package for the definition.
    let pkg_key = ast_index::Package {
        dir_path: None,
        name: source_pkg,
        language_data: None,
    };

    if let Some(pkg_files) = index.packages().get(&pkg_key) {
        for pkg_file in pkg_files.value() {
            for def in &pkg_file.symbol_defs {
                if def.exported && def.name == component_name {
                    if let Some(ref t) = def.language_data.transparency {
                        return t.is_transparent;
                    }
                }
            }
            // Also check re-exports: if the file re-exports this symbol,
            // follow the chain.
            for imp in &pkg_file.imports {
                if !imp.is_reexport {
                    continue;
                }
                let re_exports_symbol = imp.symbols.iter().any(|s| {
                    let exported_name = s.alias.as_deref().unwrap_or(&s.name);
                    exported_name == component_name
                }) || imp.symbols.is_empty(); // export * -- might re-export it

                if re_exports_symbol {
                    // Follow the re-export to the source package.
                    let re_pkg_key = ast_index::Package::<
                        ast_index_typescript::types::TsPackageData,
                    > {
                        dir_path: None,
                        name: imp.source.clone(),
                        language_data: None,
                    };
                    if let Some(re_files) = index.packages().get(&re_pkg_key) {
                        for re_file in re_files.value() {
                            for def in &re_file.symbol_defs {
                                if def.exported && def.name == component_name {
                                    if let Some(ref t) = def.language_data.transparency {
                                        return t.is_transparent;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    false
}

/// Find the parent_component of a component usage within a file.
///
/// Scans the file's symbol refs for the JSX ref matching `component_name`
/// and returns its `parent_component`.
fn find_parent_of_component(file: &ReactCachedFile, component_name: &str) -> Option<String> {
    file.symbol_refs
        .iter()
        .find(|r| r.language_data.ts.is_jsx_component && r.name == component_name)
        .and_then(|r| r.language_data.parent_component.clone())
}

/// Find the names of direct JSX children of a component instance.
///
/// Uses `parent_component` on refs: a child is any JSX component ref in the
/// same file whose `parent_component` matches the component name AND whose
/// span is contained within the component's `jsx_element_span`.
fn find_jsx_children(
    file: &ReactCachedFile,
    component_name: &str,
    ref_data: &ReactRefData,
) -> Vec<String> {
    let element_span = match ref_data.jsx_element_span {
        Some(span) => span,
        None => return Vec::new(),
    };

    file.symbol_refs
        .iter()
        .filter(|r| {
            r.language_data.ts.is_jsx_component
                && r.language_data
                    .parent_component
                    .as_deref()
                    == Some(component_name)
                && r.span.start >= element_span.start
                && r.span.end <= element_span.end
        })
        .map(|r| r.name.clone())
        .collect()
}

/// Check if a spread attribute contains a prop matching the pattern.
///
/// Looks up the spread identifier's definition in the index and checks
/// its `object_properties`.
fn check_spread_prop(
    index: &ReactProjectIndex,
    root: &Path,
    file: &ReactCachedFile,
    component_name: &str,
    prop: &ast_index_react::types::JsxPropInfo,
    pattern_re: &Regex,
    value_re: Option<&Regex>,
) -> Option<Vec<Incident>> {
    // value_re cannot be checked on spread-resolved props (we don't have values).
    if value_re.is_some() {
        return None;
    }

    // 1. Check inline spread properties extracted at index time.
    //    Handles `{...{ actions }}`, `{...(cond && { actions })}`, etc.
    if !prop.spread_properties.is_empty() {
        let matching: Vec<_> = prop
            .spread_properties
            .iter()
            .filter(|p| pattern_re.is_match(p))
            .collect();
        if !matching.is_empty() {
            let mut results = Vec::new();
            for prop_name in matching {
                let mut inc = make_incident_from_span(
                    root,
                    &file.path,
                    &file.source_text,
                    prop.span,
                );
                inc.variables.insert(
                    "propName".into(),
                    serde_json::Value::String(prop_name.clone()),
                );
                inc.variables.insert(
                    "componentName".into(),
                    serde_json::Value::String(component_name.to_string()),
                );
                inc.variables.insert(
                    "spreadSource".into(),
                    serde_json::Value::String("true".to_string()),
                );
                results.push(inc);
            }
            return Some(results);
        }
        return None;
    }

    // 2. Fall back to definition-based lookup for identifier/call spreads.
    let spread_text = prop.expression_text.as_deref()?;

    // For function call spreads like `{...getProps()}`, strip the call
    // parens to get the function name and look up its object_properties.
    let spread_ident = if spread_text.ends_with("()") {
        &spread_text[..spread_text.len() - 2]
    } else {
        spread_text
    };

    // Find the definition of the spread identifier (or function name).
    // First check local definitions in this file.
    for def in &file.symbol_defs {
        if def.name == spread_ident {
            let matching: Vec<_> = def
                .language_data
                .object_properties
                .iter()
                .filter(|p| pattern_re.is_match(p))
                .collect();
            if !matching.is_empty() {
                let mut results = Vec::new();
                for prop_name in matching {
                    let mut inc = make_incident_from_span(
                        root,
                        &file.path,
                        &file.source_text,
                        prop.span,
                    );
                    inc.variables.insert(
                        "propName".into(),
                        serde_json::Value::String(prop_name.clone()),
                    );
                    inc.variables.insert(
                        "componentName".into(),
                        serde_json::Value::String(component_name.to_string()),
                    );
                    inc.variables.insert(
                        "spreadSource".into(),
                        serde_json::Value::String("true".to_string()),
                    );
                    results.push(inc);
                }
                return Some(results);
            }
        }
    }

    // Check imported definitions.
    let import_source = file.imports.iter().find_map(|imp| {
        let has_it = imp.symbols.iter().any(|s| {
            let local = s.alias.as_deref().unwrap_or(&s.name);
            local == spread_ident
        });
        if has_it {
            Some(imp.source.clone())
        } else {
            None
        }
    })?;

    let pkg_key = ast_index::Package {
        dir_path: None,
        name: import_source,
        language_data: None,
    };

    if let Some(pkg_files) = index.packages().get(&pkg_key) {
        for pkg_file in pkg_files.value() {
            for def in &pkg_file.symbol_defs {
                if def.exported && def.name == spread_ident {
                    let matching: Vec<_> = def
                        .language_data
                        .object_properties
                        .iter()
                        .filter(|p| pattern_re.is_match(p))
                        .collect();
                    if !matching.is_empty() {
                        let mut results = Vec::new();
                        for prop_name in matching {
                            let mut inc = make_incident_from_span(
                                root,
                                &file.path,
                                &file.source_text,
                                prop.span,
                            );
                            inc.variables.insert(
                                "propName".into(),
                                serde_json::Value::String(prop_name.clone()),
                            );
                            inc.variables.insert(
                                "componentName".into(),
                                serde_json::Value::String(component_name.to_string()),
                            );
                            inc.variables.insert(
                                "spreadSource".into(),
                                serde_json::Value::String("true".to_string()),
                            );
                            results.push(inc);
                        }
                        return Some(results);
                    }
                }
            }
        }
    }

    None
}

/// Create an [`Incident`] from an index span, computing line/column from source.
fn make_incident_from_span(
    _root: &Path,
    file_path: &Path,
    source_text: &str,
    span: ast_index::Span,
) -> Incident {
    let file_uri = format!("file://{}", file_path.display());

    let (line, col) = offset_to_line_col(source_text, span.start as usize);
    let (end_line, end_col) = offset_to_line_col(source_text, span.end as usize);

    let code_location = Location {
        start: Position {
            line: line as u32,
            character: col as u32,
        },
        end: Position {
            line: end_line as u32,
            character: end_col as u32,
        },
    };

    let line_number = (line + 1) as u32; // 1-indexed

    let code_snip = incident::extract_code_snip(source_text, line_number, 2);

    let mut inc = Incident::new(file_uri, line_number, code_location);
    inc.code_snip = Some(code_snip);
    inc
}

/// Convert a byte offset to (0-indexed line, 0-indexed column).
fn offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.matches('\n').count();
    let col = before.rfind('\n').map_or(offset, |pos| offset - pos - 1);
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offset_to_line_col() {
        let src = "line1\nline2\nline3";
        assert_eq!(offset_to_line_col(src, 0), (0, 0));
        assert_eq!(offset_to_line_col(src, 5), (0, 5)); // newline char
        assert_eq!(offset_to_line_col(src, 6), (1, 0)); // start of line2
        assert_eq!(offset_to_line_col(src, 12), (2, 0)); // start of line3
    }

    #[test]
    fn test_file_matches_no_pattern() {
        let root = Path::new("/project");
        assert!(file_matches(root, Path::new("/project/src/App.tsx"), None));
    }

    #[test]
    fn test_file_matches_with_pattern() {
        let root = Path::new("/project");
        let re = Regex::new(r".*\.tsx$").unwrap();
        assert!(file_matches(
            root,
            Path::new("/project/src/App.tsx"),
            Some(&re)
        ));
        assert!(!file_matches(
            root,
            Path::new("/project/src/utils.ts"),
            Some(&re)
        ));
    }

    // ══════════════════════════════════════════════════════════════════════
    // Integration tests: evaluate_referenced against ReactProjectIndex
    // ══════════════════════════════════════════════════════════════════════

    use frontend_core::capabilities::ReferencedCondition;

    /// Build a ReactProjectIndex from test fixture files.
    fn build_index(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReactProjectIndex) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let pkg = r#"{"name": "@test/app", "version": "1.0.0"}"#;
        std::fs::write(dir.path().join("package.json"), pkg).unwrap();

        for (rel_path, content) in files {
            let full_path = dir.path().join(rel_path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full_path, content).unwrap();
        }

        let analyzer = ast_index_react::analyzer::ReactAnalyzer::new(dir.path());
        let index = ast_index::ProjectIndex::new(analyzer);
        index.build(dir.path());
        (dir, index)
    }

    fn cond(pattern: &str, location: Option<ReferenceLocation>) -> ReferencedCondition {
        ReferencedCondition {
            pattern: pattern.to_string(),
            location,
            component: None,
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // IMPORT tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_import_basic() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
import { Table } from '@patternfly/react-table';
const App = () => <Button>Click</Button>;
"#,
        )]);
        let c = cond("^Button$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find 1 Button import");
        assert!(incidents[0].variables.get("matchingText")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "Button"));
    }

    #[test]
    fn query_import_with_from_filter() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Select, SelectOption } from '@patternfly/react-core/deprecated';
import { Button } from '@patternfly/react-core';
"#,
        )]);
        let mut c = cond("^(Select|SelectOption)$", Some(ReferenceLocation::Import));
        c.from = Some("@patternfly/react-core/deprecated".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 2, "Should find Select + SelectOption from deprecated");
    }

    #[test]
    fn query_import_multiple_emptystate() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { EmptyState, EmptyStateHeader, EmptyStateIcon, EmptyStateBody } from '@patternfly/react-core';
"#,
        )]);
        let c = cond("^(EmptyStateHeader|EmptyStateIcon)$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 2, "Should find EmptyStateHeader + EmptyStateIcon");
    }

    #[test]
    fn query_import_no_false_positive() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
"#,
        )]);
        let c = cond("^Modal$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Should not find Modal import");
    }

    // ── JSX_PROP tests ───────────────────────────────────────────────────

    #[test]
    fn query_jsx_prop_modal_title() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const App = () => (
    <Modal title="Confirm" actions={[]}>
        <p>Are you sure?</p>
    </Modal>
);
"#,
        )]);
        let mut c = cond("^title$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propName").and_then(|v| v.as_str()),
            Some("title")
        );
        assert_eq!(
            incidents[0].variables.get("componentName").and_then(|v| v.as_str()),
            Some("Modal")
        );
    }

    #[test]
    fn query_jsx_prop_value_filter() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { PageSection } from '@patternfly/react-core';
const App = () => (
    <PageSection variant="light">
        <p>Content</p>
    </PageSection>
);
"#,
        )]);
        let mut c = cond("^variant$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^PageSection$".to_string());
        c.value = Some("^(dark|darker|light)$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propValue").and_then(|v| v.as_str()),
            Some("light")
        );
    }

    #[test]
    fn query_jsx_prop_value_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { PageSection } from '@patternfly/react-core';
const App = () => <PageSection variant="default"><p>ok</p></PageSection>;
"#,
        )]);
        let mut c = cond("^variant$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^PageSection$".to_string());
        c.value = Some("^(dark|darker|light)$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "variant=default should not match");
    }

    #[test]
    fn query_jsx_prop_wrong_component_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
const App = () => <Button title="foo">Click</Button>;
"#,
        )]);
        let mut c = cond("^title$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "title on Button should not match Modal filter");
    }

    // ── JSX_COMPONENT tests ──────────────────────────────────────────────

    #[test]
    fn query_jsx_component_basic() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Chip } from '@patternfly/react-core';
const App = () => <Chip>Tag</Chip>;
"#,
        )]);
        let c = cond("^Chip$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName").and_then(|v| v.as_str()),
            Some("Chip")
        );
    }

    #[test]
    fn query_jsx_component_with_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Masthead, MastheadToggle } from '@patternfly/react-core';
const App = () => (
    <Masthead>
        <MastheadToggle>toggle</MastheadToggle>
    </Masthead>
);
"#,
        )]);
        let mut c = cond("^MastheadToggle$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Masthead$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName").and_then(|v| v.as_str()),
            Some("Masthead")
        );
    }

    #[test]
    fn query_jsx_component_parent_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { MastheadToggle } from '@patternfly/react-core';
const App = () => (
    <div>
        <MastheadToggle>toggle</MastheadToggle>
    </div>
);
"#,
        )]);
        let mut c = cond("^MastheadToggle$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Masthead$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "MastheadToggle inside div should not match parent=Masthead");
    }

    #[test]
    fn query_jsx_component_parent_transparent_wrapper() {
        let (dir, index) = build_index(&[
            (
                "src/components/Wrapper.tsx",
                r#"
import React from 'react';
export const Wrapper = ({ children }) => <>{children}</>;
"#,
            ),
            (
                "src/App.tsx",
                r#"
import { Table, Tbody, Tr, Td } from '@patternfly/react-table';
import { Wrapper } from './components/Wrapper';
const App = () => (
    <Table>
        <Wrapper>
            <Tbody><Tr><Td>Data</Td></Tr></Tbody>
        </Wrapper>
    </Table>
);
"#,
            ),
        ]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Tbody inside transparent Wrapper under Table should match parent=Table"
        );
        assert_eq!(
            incidents[0].variables.get("parentName").and_then(|v| v.as_str()),
            Some("Table")
        );
    }

    // ── TYPE_REFERENCE tests ─────────────────────────────────────────────

    #[test]
    fn query_type_reference() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ModalBoxProps } from '@patternfly/react-core';
const handler: ModalBoxProps = {};
"#,
        )]);
        let c = cond("^ModalBoxProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
    }

    // ── Spread prop tests ────────────────────────────────────────────────

    #[test]
    fn query_spread_object_import() {
        let (dir, index) = build_index(&[
            (
                "src/utils.ts",
                r#"
export const modalProps = { actions: [], title: "Hello" };
"#,
            ),
            (
                "src/App.tsx",
                r#"
import { Modal } from '@patternfly/react-core';
import { modalProps } from './utils';
const el = <Modal {...modalProps}>content</Modal>;
"#,
            ),
        ]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find 'actions' via spread resolution");
        assert_eq!(
            incidents[0].variables.get("spreadSource").and_then(|v| v.as_str()),
            Some("true")
        );
    }

    // ── Multi-file / barrel re-export tests ──────────────────────────────

    #[test]
    fn query_import_through_barrel() {
        let (dir, index) = build_index(&[
            (
                "src/components/Button.tsx",
                r#"
export const Button = ({ children }) => <button>{children}</button>;
"#,
            ),
            (
                "src/components/index.ts",
                r#"
export { Button } from './Button';
"#,
            ),
            (
                "src/App.tsx",
                r#"
import { Button } from './components';
const App = () => <Button>Click</Button>;
"#,
            ),
        ]);
        let c = cond("^Button$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(
            incidents.len() >= 1,
            "Should find Button import through barrel re-export"
        );
    }

    #[test]
    fn query_file_pattern_filter() {
        let (dir, index) = build_index(&[
            (
                "src/App.tsx",
                r#"
import { Button } from '@patternfly/react-core';
"#,
            ),
            (
                "src/utils.ts",
                r#"
import { Button } from '@patternfly/react-core';
"#,
            ),
        ]);
        let mut c = cond("^Button$", Some(ReferenceLocation::Import));
        c.file_pattern = Some(r".*\.tsx$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should only match .tsx files");
    }

    // ── Additional IMPORT tests ──────────────────────────────────────

    #[test]
    fn query_import_default() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            "import React from 'react';\nconst x = React;\n",
        )]);
        let c = cond("^React$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find default import React");
    }

    #[test]
    fn query_import_aliased() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            "import { Chip as OldChip } from '@patternfly/react-core';\n",
        )]);
        // The old scanner matched by local name (OldChip) or original name (Chip).
        // The index stores name="Chip", alias=Some("OldChip").
        // We should match by the original exported name.
        let c = cond("^Chip$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find aliased import by original name");
    }

    #[test]
    fn query_import_aliased_matches_local_name() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            "import { Chip as OldChip } from '@patternfly/react-core';\n",
        )]);
        // Should also match by local name
        let c = cond("^OldChip$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find aliased import by local name");
    }

    #[test]
    fn query_import_namespace() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            "import * as PF from '@patternfly/react-core';\nconst x = PF.Button;\n",
        )]);
        let c = cond("^PF$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find namespace import");
    }

    #[test]
    fn query_import_from_deprecated_no_cross_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Select } from '@patternfly/react-core';
import { Select as OldSelect } from '@patternfly/react-core/deprecated';
"#,
        )]);
        let mut c = cond("^Select$", Some(ReferenceLocation::Import));
        c.from = Some("@patternfly/react-core/deprecated".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        // Should only match the deprecated import, not the main one
        assert_eq!(incidents.len(), 1, "Should only match deprecated import");
    }

    #[test]
    fn query_import_star_reexport() {
        let (dir, index) = build_index(&[
            ("src/components/Button.tsx", "export const Button = () => <button/>;\n"),
            ("src/components/index.ts", "export * from './Button';\n"),
            ("src/App.tsx", "import { Button } from './components';\n"),
        ]);
        let c = cond("^Button$", Some(ReferenceLocation::Import));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find Button through star re-export");
    }

    // ══════════════════════════════════════════════════════════════════
    // Additional JSX_PROP tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_jsx_prop_boolean() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
const App = () => <Button isActive>Click</Button>;
"#,
        )]);
        let mut c = cond("^isActive$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Button$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find boolean prop isActive");
    }

    #[test]
    fn query_jsx_prop_enum_member_value() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { PageSection, PageSectionVariants } from '@patternfly/react-core';
const App = () => <PageSection variant={PageSectionVariants.light}>ok</PageSection>;
"#,
        )]);
        let mut c = cond("^variant$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^PageSection$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find enum member prop value");
        // The expression_text should contain the member expression
        let val = incidents[0].variables.get("propValue").and_then(|v| v.as_str());
        assert!(val.is_some(), "Should have propValue for expression prop");
    }

    #[test]
    fn query_jsx_prop_from_filter() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const App = () => <Modal title="foo">content</Modal>;
"#,
        )]);
        let mut c = cond("^title$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        c.from = Some("@patternfly/react-core".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find prop with from filter");
    }

    #[test]
    fn query_jsx_prop_from_filter_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const App = () => <Modal title="foo">content</Modal>;
"#,
        )]);
        let mut c = cond("^title$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        c.from = Some("@some-other/library".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Wrong from should not match");
    }

    // ── Spread props (same-file) ─────────────────────────────────────

    #[test]
    fn query_spread_object_literal() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const el = <Modal {...{ actions: [], title: "hi" }}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve inline object literal spread");
    }

    #[test]
    fn query_spread_conditional_and() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const hasActions = true;
const el = <Modal {...(hasActions && { actions: [] })}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve conditional AND spread");
    }

    #[test]
    fn query_spread_ternary() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const flag = true;
const el = <Modal {...(flag ? { actions: [] } : {})}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve ternary spread");
    }

    #[test]
    fn query_spread_local_identifier() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const modalProps = { actions: [], title: "Hello" };
const el = <Modal {...modalProps}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve local identifier spread via object_properties");
    }

    #[test]
    fn query_spread_multiple_matching_props() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const modalProps = { actions: [], title: "Hello" };
const el = <Modal {...modalProps}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^(actions|title)$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 2, "Should find both matching props from spread");
    }

    #[test]
    fn query_spread_unresolvable_no_false_positive() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const el = <Modal {...unknownProps}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Unresolvable spread should not produce false positives");
    }

    #[test]
    fn query_spread_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const modalProps = { isOpen: true };
const el = <Modal {...modalProps}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Spread without target prop should not match");
    }

    #[test]
    fn query_spread_coexists_with_direct_prop() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const extra = { variant: "small" };
const el = <Modal title="hi" {...extra}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^title$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Direct prop should still be found alongside spread");
    }

    #[test]
    fn query_spread_local_fn_call_concise_arrow() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const getProps = () => ({ actions: [], title: "hi" });
const el = <Modal {...getProps()}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve local fn call spread (concise arrow)");
    }

    #[test]
    fn query_spread_local_fn_call_block_body() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
function getProps() { return { actions: [], title: "hi" }; }
const el = <Modal {...getProps()}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve local fn call spread (block body)");
    }

    #[test]
    fn query_spread_local_fn_call_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const getProps = () => ({ isOpen: true });
const el = <Modal {...getProps()}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Fn call spread without target prop should not match");
    }

    // ── Cross-file spread ────────────────────────────────────────────

    #[test]
    fn query_spread_through_barrel() {
        let (dir, index) = build_index(&[
            ("src/config/modal.ts", "export const modalProps = { actions: [], title: 'hi' };\n"),
            ("src/config/index.ts", "export { modalProps } from './modal';\n"),
            ("src/App.tsx", r#"
import { Modal } from '@patternfly/react-core';
import { modalProps } from './config';
const el = <Modal {...modalProps}>ok</Modal>;
"#),
        ]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve spread through barrel re-export");
    }

    #[test]
    fn query_spread_fn_call_import() {
        let (dir, index) = build_index(&[
            ("src/utils.ts", "export const getModalProps = () => ({ actions: [], title: 'hi' });\n"),
            ("src/App.tsx", r#"
import { Modal } from '@patternfly/react-core';
import { getModalProps } from './utils';
const el = <Modal {...getModalProps()}>ok</Modal>;
"#),
        ]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should resolve imported fn call spread");
    }

    #[test]
    fn query_spread_node_modules_skipped() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
import { someProps } from 'some-library';
const el = <Modal {...someProps}>ok</Modal>;
"#,
        )]);
        let mut c = cond("^actions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Modal$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Should not follow node_modules spread");
    }

    // ── Typed object literal scanning ────────────────────────────────

    #[test]
    fn query_typed_object_literal_basic() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const config: ToolbarItemProps = { align: 'alignRight', variant: 'separator' };
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find prop in typed object literal");
    }

    #[test]
    fn query_typed_object_literal_partial() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const config: Partial<ToolbarItemProps> = { align: 'alignRight' };
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find prop in Partial<Props> typed literal");
    }

    #[test]
    fn query_typed_object_literal_with_value() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { PageSectionProps } from '@patternfly/react-core';
const config: PageSectionProps = { variant: 'light' };
"#,
        )]);
        let mut c = cond("^variant$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^PageSection$".to_string());
        c.value = Some("^(dark|darker|light)$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should match value in typed object literal");
    }

    // ── Object value extraction ──────────────────────────────────────

    #[test]
    fn query_jsx_prop_object_values() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItem } from '@patternfly/react-core';
const App = () => <ToolbarItem align={{ default: 'alignRight', lg: 'alignLeft' }}>ok</ToolbarItem>;
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find align prop");
        // The old scanner extracted propObjectValues; check if expression_text at least captures it
        let val = incidents[0].variables.get("propValue").and_then(|v| v.as_str());
        assert!(val.is_some(), "Should capture some representation of the object value");
    }

    // ── Object key prop detection ────────────────────────────────────

    #[test]
    fn query_jsx_prop_object_key() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { FormGroup } from '@patternfly/react-core';
const App = () => <FormGroup formGroupProps={{ labelIcon: <div/> }}>ok</FormGroup>;
"#,
        )]);
        let mut c = cond("^labelIcon$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^FormGroup$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should detect prop name inside object key");
    }

    // ── Member expression component ──────────────────────────────────

    #[test]
    fn query_jsx_prop_member_expression_component() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Toolbar } from '@patternfly/react-core';
const App = () => <Toolbar.Item align="alignRight">ok</Toolbar.Item>;
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^Item$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should match member expression component Toolbar.Item");
    }

    // ══════════════════════════════════════════════════════════════════
    // Additional JSX_COMPONENT tests
    // ══════════════════════════════════════════════════════════════════

    // ── Parent edge cases ────────────────────────────────────────────

    #[test]
    fn query_component_opaque_preserves_parent() {
        let (dir, index) = build_index(&[
            ("src/components/OpaqueWrapper.tsx", r#"
export const OpaqueWrapper = ({ title }) => <div><h1>{title}</h1></div>;
"#),
            ("src/App.tsx", r#"
import { Table, Tbody } from '@patternfly/react-table';
import { OpaqueWrapper } from './components/OpaqueWrapper';
const App = () => (
    <Table>
        <OpaqueWrapper title="hi">
            <Tbody />
        </OpaqueWrapper>
    </Table>
);
"#),
        ]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        // Opaque wrapper should NOT be collapsed -- Tbody's parent is OpaqueWrapper, not Table
        assert_eq!(incidents.len(), 0, "Opaque wrapper should block parent=Table match");
    }

    #[test]
    fn query_component_npm_not_resolved() {
        // Components from npm packages should not be resolved for transparency
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Table, Tbody } from '@patternfly/react-table';
import { SomeWrapper } from 'some-npm-library';
const App = () => (
    <Table>
        <SomeWrapper>
            <Tbody />
        </SomeWrapper>
    </Table>
);
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "npm wrapper should not be resolved as transparent");
    }

    // ── notParent ────────────────────────────────────────────────────

    #[test]
    fn query_not_parent_wrong_parent_fires() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Card, Tbody } from '@patternfly/react-core';
const App = () => <Card><Tbody /></Card>;
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Wrong parent should trigger notParent rule");
    }

    #[test]
    fn query_not_parent_correct_parent_suppressed() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Table, Tbody } from '@patternfly/react-table';
const App = () => <Table><Tbody /></Table>;
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Correct parent should suppress notParent rule");
    }

    #[test]
    fn query_not_parent_fragment_boundary() {
        // React.Fragment at the top level is a render boundary.
        // notParent should be suppressed (no meaningful parent to check).
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import React from 'react';
import { Tbody } from '@patternfly/react-table';
const App = () => <React.Fragment><Tbody /></React.Fragment>;
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Fragment render boundary should suppress notParent");
    }

    #[test]
    fn query_not_parent_hook_boundary() {
        // JSX returned from a hook function is at a render boundary.
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Tbody } from '@patternfly/react-table';
function useMyHook() {
    return <Tbody />;
}
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Hook render boundary should suppress notParent");
    }

    #[test]
    fn query_not_parent_react_fc_boundary() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import React from 'react';
import { Tbody } from '@patternfly/react-table';
const MyComponent: React.FC = () => <Tbody />;
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "React.FC render boundary should suppress notParent");
    }

    #[test]
    fn query_not_parent_wrong_parent_inside_hook() {
        // Wrong parent INSIDE a hook should still fire
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Card, Tbody } from '@patternfly/react-core';
function useMyHook() {
    return <Card><Tbody /></Card>;
}
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.not_parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Wrong parent inside hook should fire notParent");
    }

    // ── tsconfig alias + barrel transparency ─────────────────────────

    #[test]
    fn query_tsconfig_alias_transparency() {
        let (dir, index) = build_index(&[
            ("src/components/Wrapper.tsx", r#"
import React from 'react';
export const Wrapper = ({ children }) => <>{children}</>;
"#),
            ("src/App.tsx", r#"
import { Tr, Th } from '@patternfly/react-table';
import { Wrapper } from './components/Wrapper';
const App = () => (
    <Tr>
        <Wrapper>
            <Th>Name</Th>
        </Wrapper>
    </Tr>
);
"#),
        ]);
        let mut c = cond("^Th$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Tr$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Transparent wrapper via relative import should collapse");
    }

    #[test]
    fn query_barrel_named_reexport_transparency() {
        let (dir, index) = build_index(&[
            ("src/components/Wrapper.tsx", r#"
import React from 'react';
export const Wrapper = ({ children }) => <>{children}</>;
"#),
            ("src/components/index.ts", "export { Wrapper } from './Wrapper';\n"),
            ("src/App.tsx", r#"
import { Table, Tbody } from '@patternfly/react-table';
import { Wrapper } from './components';
const App = () => (
    <Table>
        <Wrapper>
            <Tbody />
        </Wrapper>
    </Table>
);
"#),
        ]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Barrel re-exported transparent wrapper should collapse");
    }

    #[test]
    fn query_tsconfig_opaque_not_collapsed() {
        let (dir, index) = build_index(&[
            ("src/components/OpaqueWrapper.tsx", r#"
export const OpaqueWrapper = ({ title }) => <div><h1>{title}</h1></div>;
"#),
            ("src/App.tsx", r#"
import { Tr, Th } from '@patternfly/react-table';
import { OpaqueWrapper } from './components/OpaqueWrapper';
const App = () => (
    <Tr>
        <OpaqueWrapper title="x">
            <Th>Name</Th>
        </OpaqueWrapper>
    </Tr>
);
"#),
        ]);
        let mut c = cond("^Th$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Tr$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Opaque wrapper should not be collapsed");
    }

    // ── Optional chaining ────────────────────────────────────────────

    #[test]
    fn query_jsx_inside_optional_chain_map() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Chip } from '@patternfly/react-core';
const items: string[] | undefined = [];
const App = () => <div>{items?.map(i => <Chip key={i}>{i}</Chip>)}</div>;
"#,
        )]);
        let c = cond("^Chip$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find Chip inside ?.map()");
    }

    #[test]
    fn query_jsx_inside_optional_chain_filter_map() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Chip } from '@patternfly/react-core';
const items: string[] | undefined = [];
const App = () => <div>{items?.filter(Boolean).map(i => <Chip key={i}>{i}</Chip>)}</div>;
"#,
        )]);
        let c = cond("^Chip$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find Chip inside ?.filter().map()");
    }

    // ── Function return parent tracing ───────────────────────────────

    #[test]
    fn query_fn_return_inherits_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
function renderItems() {
    return <DropdownItem>Item 1</DropdownItem>;
}
const App = () => <Dropdown>{renderItems()}</Dropdown>;
"#,
        )]);
        let mut c = cond("^DropdownItem$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Dropdown$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Function return should inherit parent from call site");
    }

    #[test]
    fn query_fn_return_map_inherits_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
function renderItems(items: string[]) {
    return items.map(i => <DropdownItem key={i}>{i}</DropdownItem>);
}
const App = () => <Dropdown>{renderItems(["a", "b"])}</Dropdown>;
"#,
        )]);
        let mut c = cond("^DropdownItem$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Dropdown$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Function return with .map() should inherit parent");
    }

    #[test]
    fn query_fn_return_no_double_count() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
function renderItems() {
    return <DropdownItem>Item</DropdownItem>;
}
const App = () => <Dropdown>{renderItems()}</Dropdown>;
"#,
        )]);
        let c = cond("^DropdownItem$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should not double-count from fn body + call site");
    }

    // ── Cross-file function ref ──────────────────────────────────────

    #[test]
    fn query_cross_file_fn_ref_parent() {
        let (dir, index) = build_index(&[
            ("src/toggle.tsx", r#"
import { MenuToggle } from '@patternfly/react-core';
export const toggle = (toggleRef) => <MenuToggle ref={toggleRef}>Filter</MenuToggle>;
"#),
            ("src/App.tsx", r#"
import { Select } from '@patternfly/react-core';
import { toggle } from './toggle';
const App = () => <Select toggle={toggle} isOpen={false}><div>opt</div></Select>;
"#),
        ]);
        let mut c = cond("^MenuToggle$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Select$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Cross-file fn ref should resolve parent=Select");
    }

    #[test]
    fn query_cross_file_fn_ref_barrel() {
        let (dir, index) = build_index(&[
            ("src/components/toggle.tsx", r#"
import { MenuToggle } from '@patternfly/react-core';
export const toggle = (toggleRef) => <MenuToggle ref={toggleRef}>Filter</MenuToggle>;
"#),
            ("src/components/index.ts", "export { toggle } from './toggle';\n"),
            ("src/App.tsx", r#"
import { Select } from '@patternfly/react-core';
import { toggle } from './components';
const App = () => <Select toggle={toggle} isOpen={false}><div>opt</div></Select>;
"#),
        ]);
        let mut c = cond("^MenuToggle$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Select$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Cross-file fn ref through barrel should resolve parent");
    }

    // ── child ────────────────────────────────────────────────────────

    #[test]
    fn query_child_fires_when_present() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal, ModalBoxBody } from '@patternfly/react-core';
const App = () => <Modal><ModalBoxBody>content</ModalBoxBody></Modal>;
"#,
        )]);
        let mut c = cond("^Modal$", Some(ReferenceLocation::JsxComponent));
        c.child = Some("^ModalBoxBody$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "child filter should fire when matching child present");
    }

    #[test]
    fn query_child_absent_no_fire() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const App = () => <Modal><p>content</p></Modal>;
"#,
        )]);
        let mut c = cond("^Modal$", Some(ReferenceLocation::JsxComponent));
        c.child = Some("^ModalBoxBody$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "child filter should not fire when child absent");
    }

    #[test]
    fn query_child_incident_on_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal, ModalBoxBody } from '@patternfly/react-core';
const App = () => <Modal><ModalBoxBody>ok</ModalBoxBody></Modal>;
"#,
        )]);
        let mut c = cond("^Modal$", Some(ReferenceLocation::JsxComponent));
        c.child = Some("^ModalBoxBody$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName").and_then(|v| v.as_str()),
            Some("Modal"),
            "Incident should be on the parent (Modal), not the child"
        );
    }

    #[test]
    fn query_child_no_children_no_fire() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal } from '@patternfly/react-core';
const App = () => <Modal />;
"#,
        )]);
        let mut c = cond("^Modal$", Some(ReferenceLocation::JsxComponent));
        c.child = Some("^ModalBoxBody$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Self-closing element should not fire child");
    }

    #[test]
    fn query_child_sees_map_children() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal, ModalBoxBody } from '@patternfly/react-core';
const items = ["a"];
const App = () => <Modal>{items.map(i => <ModalBoxBody key={i}>{i}</ModalBoxBody>)}</Modal>;
"#,
        )]);
        let mut c = cond("^Modal$", Some(ReferenceLocation::JsxComponent));
        c.child = Some("^ModalBoxBody$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "child filter should see children inside .map()");
    }

    // ── notChild ─────────────────────────────────────────────────────

    #[test]
    fn query_not_child_basic() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { InputGroup, Button } from '@patternfly/react-core';
const App = () => <InputGroup><Button>click</Button></InputGroup>;
"#,
        )]);
        let mut c = cond("^InputGroup$", Some(ReferenceLocation::JsxComponent));
        c.not_child = Some("^InputGroupItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "notChild should fire for non-matching children");
    }

    #[test]
    fn query_not_child_all_valid() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { InputGroup, InputGroupItem } from '@patternfly/react-core';
const App = () => (
    <InputGroup>
        <InputGroupItem><input/></InputGroupItem>
        <InputGroupItem><input/></InputGroupItem>
    </InputGroup>
);
"#,
        )]);
        let mut c = cond("^InputGroup$", Some(ReferenceLocation::JsxComponent));
        c.not_child = Some("^InputGroupItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "All valid children should suppress notChild");
    }

    #[test]
    fn query_not_child_sees_map() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { InputGroup, Button } from '@patternfly/react-core';
const items = ["a"];
const App = () => <InputGroup>{items.map(i => <Button key={i}>{i}</Button>)}</InputGroup>;
"#,
        )]);
        let mut c = cond("^InputGroup$", Some(ReferenceLocation::JsxComponent));
        c.not_child = Some("^InputGroupItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "notChild should see children inside .map()");
    }

    // ── requiresChild ────────────────────────────────────────────────

    #[test]
    fn query_requires_child_no_match_fires() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { AlertGroup } from '@patternfly/react-core';
const App = () => <AlertGroup><p>no alert</p></AlertGroup>;
"#,
        )]);
        let mut c = cond("^AlertGroup$", Some(ReferenceLocation::JsxComponent));
        c.requires_child = Some("^Alert$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "requiresChild should fire when no matching child");
    }

    #[test]
    fn query_requires_child_match_suppresses() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const App = () => <AlertGroup><Alert title="hi" /></AlertGroup>;
"#,
        )]);
        let mut c = cond("^AlertGroup$", Some(ReferenceLocation::JsxComponent));
        c.requires_child = Some("^Alert$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "requiresChild should suppress when matching child present");
    }

    #[test]
    fn query_requires_child_self_closing() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { AlertGroup } from '@patternfly/react-core';
const App = () => <AlertGroup />;
"#,
        )]);
        let mut c = cond("^AlertGroup$", Some(ReferenceLocation::JsxComponent));
        c.requires_child = Some("^Alert$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Self-closing should fire requiresChild");
    }

    #[test]
    fn query_requires_child_sees_map() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const alerts = ["a"];
const App = () => <AlertGroup>{alerts.map(a => <Alert key={a} title={a} />)}</AlertGroup>;
"#,
        )]);
        let mut c = cond("^AlertGroup$", Some(ReferenceLocation::JsxComponent));
        c.requires_child = Some("^Alert$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "requiresChild should see children inside .map()");
    }

    // ══════════════════════════════════════════════════════════════════
    // TYPE_REFERENCE tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_type_alias() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps } from '@patternfly/react-core';
type MyProps = ButtonProps & { extra: string };
"#,
        )]);
        let c = cond("^ButtonProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find ButtonProps in type alias RHS");
    }

    #[test]
    fn query_type_interface_extends() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps } from '@patternfly/react-core';
interface MyButtonProps extends ButtonProps { extra: string }
"#,
        )]);
        let c = cond("^ButtonProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find ButtonProps in interface extends");
    }

    #[test]
    fn query_type_union() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps, AlertProps } from '@patternfly/react-core';
type Combined = ButtonProps | AlertProps;
"#,
        )]);
        let c = cond("^ButtonProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find ButtonProps in union type");
    }

    #[test]
    fn query_type_array() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps } from '@patternfly/react-core';
const items: ButtonProps[] = [];
"#,
        )]);
        let c = cond("^ButtonProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find ButtonProps in array type");
    }

    #[test]
    fn query_type_exported() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps } from '@patternfly/react-core';
export type MyProps = ButtonProps;
"#,
        )]);
        let c = cond("^ButtonProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "Should find ButtonProps in exported type alias");
    }

    #[test]
    fn query_type_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ButtonProps } from '@patternfly/react-core';
const x: ButtonProps = {};
"#,
        )]);
        let c = cond("^ModalProps$", Some(ReferenceLocation::TypeReference));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Non-matching type should return empty");
    }

    // ══════════════════════════════════════════════════════════════════
    // FUNCTION_CALL tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_function_call_simple() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { useWizardContext } from '@patternfly/react-core';
const ctx = useWizardContext();
"#,
        )]);
        let c = cond("^useWizardContext$", Some(ReferenceLocation::FunctionCall));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find simple function call");
    }

    #[test]
    fn query_function_call_no_match() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { useWizardContext } from '@patternfly/react-core';
const ctx = useWizardContext();
"#,
        )]);
        let c = cond("^useOtherHook$", Some(ReferenceLocation::FunctionCall));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 0, "Non-matching function call should return empty");
    }

    #[test]
    fn query_function_call_in_variable_decl() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { useToolbar } from '@patternfly/react-core';
const toolbar = useToolbar();
"#,
        )]);
        let c = cond("^useToolbar$", Some(ReferenceLocation::FunctionCall));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find function call in variable declaration");
    }

    #[test]
    fn query_function_call_string_arg() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { screen } from '@testing-library/react';
const btn = screen.getByRole('button');
"#,
        )]);
        let c = cond("^getByRole$", Some(ReferenceLocation::FunctionCall));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Should find member function call");
        // Check if first arg is captured (may fail -- index doesn't capture call args)
        let arg_val = incidents[0].variables.get("callArgValue");
        assert_eq!(
            arg_val.and_then(|v| v.as_str()),
            Some("button"),
            "Should extract first string argument"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Sentinel parent / render boundary tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_hook_sets_sentinel_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Tbody } from '@patternfly/react-table';
function useMyHook() {
    return <Tbody />;
}
"#,
        )]);
        let c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        // The parent should be None or a sentinel -- NOT some outer component
        let parent = incidents[0].variables.get("parentName");
        assert!(
            parent.is_none() || parent.and_then(|v| v.as_str()).is_some_and(|s| s.starts_with("__")),
            "Hook-returned JSX should have no real parent"
        );
    }

    #[test]
    fn query_hook_nested_jsx_correct_parent() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Toolbar, ToolbarItem } from '@patternfly/react-core';
function useMyToolbar() {
    return <Toolbar><ToolbarItem>item</ToolbarItem></Toolbar>;
}
"#,
        )]);
        let mut c = cond("^ToolbarItem$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Toolbar$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Nested JSX inside hook should have correct parent");
    }

    #[test]
    fn query_react_fc_sets_sentinel() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import React from 'react';
import { Tbody } from '@patternfly/react-table';
const MyComponent: React.FC = () => <Tbody />;
"#,
        )]);
        let c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1);
        let parent = incidents[0].variables.get("parentName");
        assert!(
            parent.is_none() || parent.and_then(|v| v.as_str()).is_some_and(|s| s.starts_with("__")),
            "React.FC top-level JSX should have no real parent"
        );
    }

    #[test]
    fn query_regular_component_no_sentinel() {
        // A regular component without type annotation should NOT get a sentinel
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Table, Tbody } from '@patternfly/react-table';
const App = () => <Table><Tbody /></Table>;
"#,
        )]);
        let mut c = cond("^Tbody$", Some(ReferenceLocation::JsxComponent));
        c.parent = Some("^Table$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(incidents.len(), 1, "Regular component should have normal parent");
    }

    // ══════════════════════════════════════════════════════════════════
    // No-location (None) tests
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_no_location_matches_import() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
const App = () => <Button>Click</Button>;
"#,
        )]);
        let c = cond("^Button$", None);
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert!(incidents.len() >= 1, "No-location should match imports");
    }

    #[test]
    fn query_no_location_matches_jsx_and_import() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Button } from '@patternfly/react-core';
const App = () => <Button>Click</Button>;
"#,
        )]);
        let c = cond("^Button$", None);
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        // Should find at least the import and the JSX usage
        assert!(incidents.len() >= 2, "No-location should match both import and JSX usage");
    }

    // ══════════════════════════════════════════════════════════════════
    // JSX inside prop value expressions
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_jsx_prop_inside_prop_value_arrow() {
        // Real-world pattern: <Dropdown toggle={toggleRef => <MenuToggle splitButtonOptions={...}>}>
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Dropdown, MenuToggle } from '@patternfly/react-core';
const App = () => (
    <Dropdown
        isOpen={false}
        toggle={toggleRef => (
            <MenuToggle ref={toggleRef} splitButtonOptions={{ items: [] }}>
                Toggle
            </MenuToggle>
        )}
    >
        <div>content</div>
    </Dropdown>
);
"#,
        )]);
        let mut c = cond("^splitButtonOptions$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^MenuToggle$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find splitButtonOptions on MenuToggle inside prop value arrow"
        );
    }

    #[test]
    fn query_jsx_component_inside_prop_value() {
        // JSX component rendered inside a prop expression
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Modal, ModalHeader } from '@patternfly/react-core';
const App = () => (
    <Modal header={<ModalHeader title="Hello" />}>
        <p>content</p>
    </Modal>
);
"#,
        )]);
        let c = cond("^ModalHeader$", Some(ReferenceLocation::JsxComponent));
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find ModalHeader inside prop value expression"
        );
    }

    #[test]
    fn query_jsx_prop_inside_render_prop() {
        // renderItem pattern
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { Select, SelectOption } from '@patternfly/react-core';
const App = () => (
    <Select
        isOpen={false}
        renderItem={item => <SelectOption value={item} isDisabled={false}>{item}</SelectOption>}
    >
        <div>content</div>
    </Select>
);
"#,
        )]);
        let mut c = cond("^isDisabled$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^SelectOption$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find isDisabled on SelectOption inside renderItem prop"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Typed local objects inside function bodies
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn query_typed_local_object_inside_arrow() {
        // Real-world pattern: hook returns typed props object
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItemProps } from '@patternfly/react-core';
export const usePaginationPropHelpers = () => {
    const paginationToolbarItemProps: ToolbarItemProps = {
        variant: 'pagination',
        align: { default: 'alignRight' }
    };
    return { paginationToolbarItemProps };
};
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find align in function-local typed object literal"
        );
    }

    #[test]
    fn query_typed_local_object_inside_function_decl() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItemProps } from '@patternfly/react-core';
function buildToolbarProps() {
    const itemProps: ToolbarItemProps = {
        variant: 'pagination',
        align: { default: 'alignRight' }
    };
    return itemProps;
}
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find align in function-local typed object literal (function decl)"
        );
    }

    #[test]
    fn query_typed_local_object_partial() {
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const useProps = () => {
    const props: Partial<ToolbarItemProps> = { align: { default: 'alignRight' } };
    return props;
};
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 1,
            "Should find align in Partial<ToolbarItemProps> typed local"
        );
    }

    #[test]
    fn query_typed_local_object_no_false_positive() {
        // Type annotation doesn't end in "Props" - should not match
        let (dir, index) = build_index(&[(
            "src/App.tsx",
            r#"
interface Config { align: string }
const useProps = () => {
    const config: Config = { align: 'right' };
    return config;
};
"#,
        )]);
        let mut c = cond("^align$", Some(ReferenceLocation::JsxProp));
        c.component = Some("^ToolbarItem$".to_string());
        let incidents = evaluate_referenced(&c, &index, dir.path()).unwrap();
        assert_eq!(
            incidents.len(), 0,
            "Non-Props type should not produce incidents"
        );
    }
}
