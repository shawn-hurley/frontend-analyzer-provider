//! JSX scanning.
//!
//! Finds JSX component usage (`<Button ...>`) and JSX prop usage (`<X isActive={...}>`).
//! Walks the AST recursively to find JSXOpeningElement nodes.

use crate::scanner::make_incident;
use frontend_core::capabilities::ReferenceLocation;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for JSX component and prop usage.
pub fn scan_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
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
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
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
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
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
        if let Some(parent) = parent_name {
            incident.variables.insert(
                "parentName".into(),
                serde_json::Value::String(parent.to_string()),
            );
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
                                    // Strip the { } wrapper
                                    let text = &source[(expr_span.start as usize + 1)
                                        ..(expr_span.end as usize).saturating_sub(1)];
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
