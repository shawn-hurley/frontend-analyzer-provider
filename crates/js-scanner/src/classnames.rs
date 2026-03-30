//! CSS class name usage scanning in JS/TS/JSX/TSX files.
//!
//! Finds className="pf-m-expandable", className={styles.foo}, and
//! string literals containing CSS class names.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for CSS class name usage.
pub fn scan_classname_usage(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    walk_statement(stmt, source, pattern, file_uri, &mut incidents);
    incidents
}

fn walk_statement(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match stmt {
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_statement(s, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_statement(s, source, pattern, file_uri, incidents);
                    }
                }
            }
            if let Some(Declaration::VariableDeclaration(v)) = &decl.declaration {
                for d in &v.declarations {
                    if let Some(init) = &d.init {
                        walk_expr(init, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    walk_statement(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    walk_expr(init, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expr(arg, source, pattern, file_uri, incidents);
            }
        }
        Statement::ExpressionStatement(expr) => {
            walk_expr(&expr.expression, source, pattern, file_uri, incidents);
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_statement(s, source, pattern, file_uri, incidents);
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_statement(&if_stmt.consequent, source, pattern, file_uri, incidents);
            if let Some(alt) = &if_stmt.alternate {
                walk_statement(alt, source, pattern, file_uri, incidents);
            }
        }
        _ => {}
    }
}

fn walk_expr(
    expr: &Expression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match expr {
        Expression::StringLiteral(s) => {
            let text = s.value.as_str();
            if pattern.is_match(text) {
                let span = s.span();
                let mut incident = make_incident(source, file_uri, span.start, span.end);
                incident.variables.insert(
                    "matchingText".into(),
                    serde_json::Value::String(text.to_string()),
                );
                incidents.push(incident);
            }
        }
        Expression::TemplateLiteral(tpl) => {
            for quasi in &tpl.quasis {
                let raw = quasi.value.raw.as_str();
                if pattern.is_match(raw) {
                    let span = quasi.span();
                    let mut incident = make_incident(source, file_uri, span.start, span.end);
                    incident.variables.insert(
                        "matchingText".into(),
                        serde_json::Value::String(raw.to_string()),
                    );
                    incidents.push(incident);
                }
            }
        }
        Expression::JSXElement(el) => {
            check_jsx_classnames(el, source, pattern, file_uri, incidents);
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(child, source, pattern, file_uri, incidents);
            }
        }
        Expression::ParenthesizedExpression(p) => {
            walk_expr(&p.expression, source, pattern, file_uri, incidents);
        }
        Expression::ConditionalExpression(c) => {
            walk_expr(&c.consequent, source, pattern, file_uri, incidents);
            walk_expr(&c.alternate, source, pattern, file_uri, incidents);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                walk_statement(s, source, pattern, file_uri, incidents);
            }
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        _ => {}
    }
}

fn check_jsx_classnames(
    el: &JSXElement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    for attr in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(a) = attr {
            if let JSXAttributeName::Identifier(ident) = &a.name {
                let attr_name = ident.name.as_str();
                if attr_name == "className" || attr_name == "class" {
                    if let Some(JSXAttributeValue::StringLiteral(s)) = &a.value {
                        let text = s.value.as_str();
                        if pattern.is_match(text) {
                            let span = s.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "matchingText".into(),
                                serde_json::Value::String(text.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                }
            }
        }
    }

    for child in &el.children {
        walk_jsx_child(child, source, pattern, file_uri, incidents);
    }
}

fn walk_jsx_child(
    child: &JSXChild<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match child {
        JSXChild::Element(el) => {
            check_jsx_classnames(el, source, pattern, file_uri, incidents);
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(c, source, pattern, file_uri, incidents);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn scan_source(source: &str, pattern: &str) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();

        ret.program
            .body
            .iter()
            .flat_map(|stmt| scan_classname_usage(stmt, source, &re, "file:///test.tsx"))
            .collect()
    }

    #[test]
    fn test_jsx_classname_string_literal() {
        let source = r#"const el = <div className="pf-m-expandable">hello</div>;"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("matchingText"),
            Some(&serde_json::Value::String("pf-m-expandable".to_string()))
        );
    }

    #[test]
    fn test_jsx_class_attribute() {
        let source = r#"const el = <div class="pf-m-expandable">hello</div>;"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_string_literal_in_variable() {
        let source = r#"const cls = "pf-m-expandable";"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_template_literal() {
        let source = r#"const cls = `pf-m-expandable ${other}`;"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_no_match() {
        let source = r#"const el = <div className="something-else">hello</div>;"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_nested_jsx_classname() {
        let source = r#"const el = <div><span className="pf-m-expandable">hi</span></div>;"#;
        let incidents = scan_source(source, r"pf-m-expandable");
        assert_eq!(incidents.len(), 1);
    }
}
