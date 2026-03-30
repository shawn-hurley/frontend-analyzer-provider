//! JSX scanning.
//!
//! Finds JSX component usage (`<Button ...>`) and JSX prop usage (`<X isActive={...}>`).
//! Walks the AST recursively to find JSXOpeningElement nodes.

#![allow(clippy::too_many_arguments)]

use crate::scanner::make_incident;
use frontend_core::capabilities::ReferenceLocation;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;
use std::collections::HashMap;

/// Import map: local identifier name → module source path.
/// Built from import declarations, passed through JSX scanning so components
/// can be resolved to their import source (e.g., Button → @patternfly/react-core).
type ImportMap = HashMap<String, String>;

/// Scan a statement for JSX component and prop usage.
pub fn scan_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    walk_statement_for_jsx(
        stmt,
        source,
        pattern,
        file_uri,
        location,
        &mut incidents,
        None,
        import_map,
    );
    incidents
}

fn walk_statement_for_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match stmt {
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    walk_function_body(
                        body,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    walk_function_body(
                        body,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
            if let Some(Declaration::VariableDeclaration(var_decl)) = &decl.declaration {
                walk_variable_declaration(
                    var_decl,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                walk_function_body(
                    body,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            walk_variable_declaration(
                var_decl,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expression_for_jsx(
                    arg,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::ExpressionStatement(expr) => {
            walk_expression_for_jsx(
                &expr.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_statement_for_jsx(
                    s,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_statement_for_jsx(
                &if_stmt.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            if let Some(alt) = &if_stmt.alternate {
                walk_statement_for_jsx(
                    alt,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        _ => {}
    }
}

fn walk_variable_declaration(
    var_decl: &VariableDeclaration<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    for declarator in &var_decl.declarations {
        if let Some(init) = &declarator.init {
            walk_expression_for_jsx(
                init,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
    }
}

fn walk_function_body(
    body: &FunctionBody<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    for stmt in &body.statements {
        walk_statement_for_jsx(
            stmt,
            source,
            pattern,
            file_uri,
            location,
            incidents,
            parent_name,
            import_map,
        );
    }
}

fn walk_expression_for_jsx(
    expr: &Expression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match expr {
        Expression::JSXElement(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(
                    child,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(
                &paren.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(
                &cond.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::LogicalExpression(logic) => {
            walk_expression_for_jsx(
                &logic.right,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::ArrowFunctionExpression(arrow) => {
            walk_function_body(
                &arrow.body,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(
                        &spread.argument,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(
                        expr,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        _ => {}
    }
}

fn walk_jsx_child(
    child: &JSXChild<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match child {
        JSXChild::Element(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(
                    c,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        JSXChild::ExpressionContainer(container) => {
            // JSXExpression inherits Expression variants via @inherit macro.
            // Walk into the expression to find nested JSX elements.
            walk_jsx_expression(
                &container.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        _ => {}
    }
}

/// Walk a JSXExpression (which inherits all Expression variants) for nested JSX.
/// This handles expression containers in JSX children ({cond && <X/>}) and
/// prop value expressions (toggle={ref => (<MenuToggle ...>)}).
fn walk_jsx_expression(
    jsx_expr: &JSXExpression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match jsx_expr {
        JSXExpression::EmptyExpression(_) => {}
        // Direct JSX nesting: {<Component />}
        JSXExpression::JSXElement(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXExpression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(
                    child,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        // Parenthesized: {(<Component />)}
        JSXExpression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(
                &paren.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Arrow functions: {ref => (<Component />)} or {() => <Component />}
        JSXExpression::ArrowFunctionExpression(arrow) => {
            walk_function_body(
                &arrow.body,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Conditionals: {condition && <Component />} or {cond ? <A/> : <B/>}
        JSXExpression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(
                &cond.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXExpression::LogicalExpression(logic) => {
            walk_expression_for_jsx(
                &logic.right,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Function calls: {renderFn(<Component />)} or {fn(arg)}
        JSXExpression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(
                        &spread.argument,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(
                        expr,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        _ => {}
    }
}

fn check_jsx_element(
    el: &JSXElement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    let opening = &el.opening_element;
    let component_name = jsx_element_name_to_string(&opening.name);

    // Check component name
    let search_component = matches!(location, Some(ReferenceLocation::JsxComponent) | None);
    if search_component && pattern.is_match(&component_name) {
        let span = opening.name.span();
        let mut incident = make_incident(source, file_uri, span.start, span.end);
        incident.variables.insert(
            "componentName".into(),
            serde_json::Value::String(component_name.clone()),
        );
        // Resolve the matched component's import source
        if let Some(module) = import_map.get(&component_name) {
            incident
                .variables
                .insert("module".into(), serde_json::Value::String(module.clone()));
        }
        if let Some(parent) = parent_name {
            incident.variables.insert(
                "parentName".into(),
                serde_json::Value::String(parent.to_string()),
            );
            // Resolve the parent component's import source
            if let Some(parent_module) = import_map.get(parent) {
                incident.variables.insert(
                    "parentFrom".into(),
                    serde_json::Value::String(parent_module.clone()),
                );
            }
        }
        incidents.push(incident);
    }

    // Check props
    let search_props = matches!(location, Some(ReferenceLocation::JsxProp) | None);
    if search_props {
        for attr in &opening.attributes {
            if let JSXAttributeItem::Attribute(a) = attr {
                if let JSXAttributeName::Identifier(ident) = &a.name {
                    let prop_name = ident.name.as_str();
                    if pattern.is_match(prop_name) {
                        let span = ident.span();
                        let mut incident = make_incident(source, file_uri, span.start, span.end);
                        incident.variables.insert(
                            "propName".into(),
                            serde_json::Value::String(prop_name.to_string()),
                        );
                        incident.variables.insert(
                            "componentName".into(),
                            serde_json::Value::String(component_name.clone()),
                        );

                        // Extract prop value for value-based filtering
                        if let Some(value) = &a.value {
                            let prop_value = match value {
                                JSXAttributeValue::StringLiteral(s) => Some(s.value.to_string()),
                                JSXAttributeValue::ExpressionContainer(expr) => {
                                    // For expressions, capture the source text
                                    let expr_span = expr.span();
                                    // Strip the { } wrapper, with bounds checking
                                    let start = (expr_span.start as usize + 1).min(source.len());
                                    let end = (expr_span.end as usize)
                                        .saturating_sub(1)
                                        .max(start)
                                        .min(source.len());
                                    let text = &source[start..end];
                                    Some(text.trim().to_string())
                                }
                                _ => None,
                            };
                            if let Some(pv) = prop_value {
                                incident
                                    .variables
                                    .insert("propValue".into(), serde_json::Value::String(pv));
                            }
                        }

                        // Resolve the owning component's import source so
                        // that the `from` filter can check it. Without this,
                        // JSX_PROP incidents bypass the `from` constraint.
                        if let Some(module) = import_map.get(&component_name) {
                            incident
                                .variables
                                .insert("module".into(), serde_json::Value::String(module.clone()));
                        }

                        incidents.push(incident);
                    }
                }
            }
        }
    }

    // Walk into prop value expressions to find nested JSX elements.
    // e.g., toggle={ref => (<MenuToggle ...>)} or icon={<Icon />}
    for attr in &opening.attributes {
        if let JSXAttributeItem::Attribute(a) = attr {
            if let Some(JSXAttributeValue::ExpressionContainer(expr)) = &a.value {
                walk_jsx_expression(
                    &expr.expression,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    Some(&component_name),
                    import_map,
                );
            }
        }
    }

    // Recurse into children — this element becomes the parent context
    for child in &el.children {
        walk_jsx_child(
            child,
            source,
            pattern,
            file_uri,
            location,
            incidents,
            Some(&component_name),
            import_map,
        );
    }
}

fn jsx_element_name_to_string(name: &JSXElementName<'_>) -> String {
    match name {
        JSXElementName::Identifier(ident) => ident.name.to_string(),
        JSXElementName::IdentifierReference(ident) => ident.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => jsx_member_expr_to_string(member),
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_expr_to_string(member: &JSXMemberExpression<'_>) -> String {
    let obj = match &member.object {
        JSXMemberExpressionObject::IdentifierReference(ident) => ident.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(nested) => jsx_member_expr_to_string(nested),
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    };
    format!("{}.{}", obj, member.property.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::build_import_map;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn scan_source_jsx(
        source: &str,
        pattern: &str,
        location: Option<&ReferenceLocation>,
    ) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();
        let import_map = build_import_map(&ret.program);

        ret.program
            .body
            .iter()
            .flat_map(|stmt| scan_jsx(stmt, source, &re, "file:///test.tsx", location, &import_map))
            .collect()
    }

    #[test]
    fn test_jsx_component_match() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button>Click</Button>;
"#;
        let incidents =
            scan_source_jsx(source, r"^Button$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("Button".to_string()))
        );
    }

    #[test]
    fn test_jsx_component_no_match() {
        let source = r#"const el = <Button>Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^Alert$", Some(&ReferenceLocation::JsxComponent));
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_jsx_prop_match() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button isActive>Click</Button>;
"#;
        let incidents = scan_source_jsx(source, r"^isActive$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propName"),
            Some(&serde_json::Value::String("isActive".to_string()))
        );
    }

    #[test]
    fn test_jsx_prop_with_string_value() {
        let source = r#"const el = <Button variant="primary">Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propValue"),
            Some(&serde_json::Value::String("primary".to_string()))
        );
    }

    #[test]
    fn test_jsx_component_with_module_resolution() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button>Click</Button>;
"#;
        let incidents =
            scan_source_jsx(source, r"^Button$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );
    }

    #[test]
    fn test_jsx_member_expression_component() {
        let source = r#"const el = <Toolbar.Item>hello</Toolbar.Item>;"#;
        let incidents = scan_source_jsx(
            source,
            r"^Toolbar\.Item$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("Toolbar.Item".to_string()))
        );
    }

    #[test]
    fn test_jsx_nested_components_tracks_parent() {
        let source = r#"
import { Page, PageSection } from '@patternfly/react-core';
const el = <Page><PageSection>content</PageSection></Page>;
"#;
        let incidents = scan_source_jsx(
            source,
            r"^PageSection$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("Page".to_string()))
        );
    }

    #[test]
    fn test_jsx_scan_without_location_filter() {
        // Without a location filter, should match both component and prop usages
        let source = r#"const el = <Button isActive>Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^Button$", None);
        // Should find Button as component
        assert!(incidents.iter().any(|i| i.variables.get("componentName")
            == Some(&serde_json::Value::String("Button".to_string()))));
    }
}
