//! Function call scanning.
//!
//! Finds function and hook calls like `useToolbar()`, `React.createElement(...)`.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for function call matches.
pub fn scan_function_calls(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    walk_stmt(stmt, source, pattern, file_uri, &mut incidents);
    incidents
}

fn walk_stmt(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match stmt {
        Statement::ExpressionStatement(expr) => {
            walk_expr(&expr.expression, source, pattern, file_uri, incidents);
        }
        Statement::VariableDeclaration(var_decl) => {
            for d in &var_decl.declarations {
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
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_stmt(s, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_stmt(s, source, pattern, file_uri, incidents);
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
                    walk_stmt(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_stmt(&if_stmt.consequent, source, pattern, file_uri, incidents);
            if let Some(alt) = &if_stmt.alternate {
                walk_stmt(alt, source, pattern, file_uri, incidents);
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
        Expression::CallExpression(call) => {
            let callee_name = callee_to_string(&call.callee);
            if let Some(name) = &callee_name {
                if pattern.is_match(name) {
                    let span = call.callee.span();
                    let mut incident = make_incident(source, file_uri, span.start, span.end);
                    incident.variables.insert(
                        "functionName".into(),
                        serde_json::Value::String(name.clone()),
                    );
                    incidents.push(incident);
                }
            }
            // Recurse into arguments
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        Expression::ParenthesizedExpression(p) => {
            walk_expr(&p.expression, source, pattern, file_uri, incidents);
        }
        Expression::ConditionalExpression(c) => {
            walk_expr(&c.consequent, source, pattern, file_uri, incidents);
            walk_expr(&c.alternate, source, pattern, file_uri, incidents);
        }
        _ => {}
    }
}

fn callee_to_string(callee: &Expression<'_>) -> Option<String> {
    match callee {
        Expression::Identifier(ident) => Some(ident.name.to_string()),
        Expression::StaticMemberExpression(member) => {
            let obj = callee_to_string(&member.object)?;
            Some(format!("{}.{}", obj, member.property.name))
        }
        _ => None,
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
            .flat_map(|stmt| scan_function_calls(stmt, source, &re, "file:///test.tsx"))
            .collect()
    }

    #[test]
    fn test_simple_function_call() {
        let incidents = scan_source("useToolbar();", r"^useToolbar$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("functionName"),
            Some(&serde_json::Value::String("useToolbar".to_string()))
        );
    }

    #[test]
    fn test_member_expression_call() {
        let incidents = scan_source("React.createElement('div');", r"^React\.createElement$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("functionName"),
            Some(&serde_json::Value::String(
                "React.createElement".to_string()
            ))
        );
    }

    #[test]
    fn test_function_call_no_match() {
        let incidents = scan_source("useState(0);", r"^useToolbar$");
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_function_call_in_variable_declaration() {
        let incidents = scan_source("const toolbar = useToolbar();", r"^useToolbar$");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_function_call_in_function_body() {
        let incidents = scan_source(
            "function App() { const x = useToolbar(); return x; }",
            r"^useToolbar$",
        );
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_nested_function_call_in_arguments() {
        let incidents = scan_source("console.log(useToolbar());", r"^useToolbar$");
        assert_eq!(incidents.len(), 1);
    }
}
