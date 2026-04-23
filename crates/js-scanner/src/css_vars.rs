//! CSS variable reference scanning in JS/TS files.
//!
//! Finds string/template literals containing CSS custom property names
//! like `--pf-v5-global--Color--100`.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for CSS variable references.
pub fn scan_css_var_usage(
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
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_expr(&if_stmt.test, source, pattern, file_uri, incidents);
            walk_stmt(&if_stmt.consequent, source, pattern, file_uri, incidents);
            if let Some(alt) = &if_stmt.alternate {
                walk_stmt(alt, source, pattern, file_uri, incidents);
            }
        }
        Statement::ForStatement(f) => {
            if let Some(init) = &f.init {
                match init {
                    ForStatementInit::VariableDeclaration(v) => {
                        for d in &v.declarations {
                            if let Some(expr) = &d.init {
                                walk_expr(expr, source, pattern, file_uri, incidents);
                            }
                        }
                    }
                    _ => {
                        if let Some(expr) = init.as_expression() {
                            walk_expr(expr, source, pattern, file_uri, incidents);
                        }
                    }
                }
            }
            if let Some(test) = &f.test {
                walk_expr(test, source, pattern, file_uri, incidents);
            }
            if let Some(update) = &f.update {
                walk_expr(update, source, pattern, file_uri, incidents);
            }
            walk_stmt(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::ForInStatement(f) => {
            walk_stmt(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::ForOfStatement(f) => {
            walk_stmt(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::WhileStatement(w) => {
            walk_expr(&w.test, source, pattern, file_uri, incidents);
            walk_stmt(&w.body, source, pattern, file_uri, incidents);
        }
        Statement::DoWhileStatement(d) => {
            walk_expr(&d.test, source, pattern, file_uri, incidents);
            walk_stmt(&d.body, source, pattern, file_uri, incidents);
        }
        Statement::SwitchStatement(s) => {
            walk_expr(&s.discriminant, source, pattern, file_uri, incidents);
            for case in &s.cases {
                if let Some(test) = &case.test {
                    walk_expr(test, source, pattern, file_uri, incidents);
                }
                for stmt in &case.consequent {
                    walk_stmt(stmt, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::TryStatement(t) => {
            for s in &t.block.body {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
            if let Some(handler) = &t.handler {
                for s in &handler.body.body {
                    walk_stmt(s, source, pattern, file_uri, incidents);
                }
            }
            if let Some(finalizer) = &t.finalizer {
                for s in &finalizer.body {
                    walk_stmt(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::LabeledStatement(l) => {
            walk_stmt(&l.body, source, pattern, file_uri, incidents);
        }
        Statement::ThrowStatement(t) => {
            walk_expr(&t.argument, source, pattern, file_uri, incidents);
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
            walk_jsx_element(el, source, pattern, file_uri, incidents);
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(child, source, pattern, file_uri, incidents);
            }
        }
        Expression::CallExpression(call) => {
            // Walk the callee — needed for IIFEs: (() => { ... })()
            walk_expr(&call.callee, source, pattern, file_uri, incidents);
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            walk_expr(&p.expression, source, pattern, file_uri, incidents);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        Expression::ConditionalExpression(c) => {
            walk_expr(&c.test, source, pattern, file_uri, incidents);
            walk_expr(&c.consequent, source, pattern, file_uri, incidents);
            walk_expr(&c.alternate, source, pattern, file_uri, incidents);
        }
        Expression::LogicalExpression(logical) => {
            walk_expr(&logical.left, source, pattern, file_uri, incidents);
            walk_expr(&logical.right, source, pattern, file_uri, incidents);
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    // Check property keys (e.g. { "--pf-v5-c-label--Color": value })
                    if let PropertyKey::StringLiteral(s) = &p.key {
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
                    // Also walk property values
                    walk_expr(&p.value, source, pattern, file_uri, incidents);
                }
                if let ObjectPropertyKind::SpreadProperty(spread) = prop {
                    walk_expr(&spread.argument, source, pattern, file_uri, incidents);
                }
            }
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        Expression::TSAsExpression(ts) => {
            walk_expr(&ts.expression, source, pattern, file_uri, incidents);
        }
        Expression::TSSatisfiesExpression(ts) => {
            walk_expr(&ts.expression, source, pattern, file_uri, incidents);
        }
        Expression::TSNonNullExpression(ts) => {
            walk_expr(&ts.expression, source, pattern, file_uri, incidents);
        }
        Expression::TSTypeAssertion(ts) => {
            walk_expr(&ts.expression, source, pattern, file_uri, incidents);
        }
        // Optional chaining: items?.map((item) => style with CSS var)
        Expression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        walk_expr(e, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        // Await/yield: const el = await getStyles(); may contain CSS vars
        Expression::AwaitExpression(a) => {
            walk_expr(&a.argument, source, pattern, file_uri, incidents);
        }
        Expression::YieldExpression(y) => {
            if let Some(arg) = &y.argument {
                walk_expr(arg, source, pattern, file_uri, incidents);
            }
        }
        // Sequence: (expr1, expr2)
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                walk_expr(e, source, pattern, file_uri, incidents);
            }
        }
        // Assignment: variable = { "--pf-v5-...": value }
        Expression::AssignmentExpression(assign) => {
            walk_expr(&assign.right, source, pattern, file_uri, incidents);
        }
        // new Constructor(style with CSS var)
        Expression::NewExpression(new_expr) => {
            for arg in &new_expr.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        // Tagged templates: css`var(--pf-v5-...)`
        Expression::TaggedTemplateExpression(tagged) => {
            for quasi in &tagged.quasi.quasis {
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
        // Static member expressions: styles.property (walk object in case it's complex)
        Expression::StaticMemberExpression(member) => {
            walk_expr(&member.object, source, pattern, file_uri, incidents);
        }
        // Computed member: obj[expr] — walk both
        Expression::ComputedMemberExpression(member) => {
            walk_expr(&member.object, source, pattern, file_uri, incidents);
            walk_expr(&member.expression, source, pattern, file_uri, incidents);
        }
        _ => {}
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
            walk_jsx_element(el, source, pattern, file_uri, incidents);
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(c, source, pattern, file_uri, incidents);
            }
        }
        JSXChild::ExpressionContainer(expr_container) => {
            if let Some(expr) = expr_container.expression.as_expression() {
                walk_expr(expr, source, pattern, file_uri, incidents);
            }
        }
        _ => {}
    }
}

/// Scan a JSXElement's attributes and children for CSS variable references.
fn walk_jsx_element(
    el: &JSXElement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    for attr in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(a) = attr {
            if let Some(JSXAttributeValue::StringLiteral(s)) = &a.value {
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
            if let Some(JSXAttributeValue::ExpressionContainer(expr_container)) = &a.value {
                if let Some(expr) = expr_container.expression.as_expression() {
                    walk_expr(expr, source, pattern, file_uri, incidents);
                }
            }
        }
    }
    for child in &el.children {
        walk_jsx_child(child, source, pattern, file_uri, incidents);
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
            .flat_map(|stmt| scan_css_var_usage(stmt, source, &re, "file:///test.tsx"))
            .collect()
    }

    #[test]
    fn test_string_literal_css_var() {
        let source = r#"const color = "--pf-v5-global--Color--100";"#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("matchingText"),
            Some(&serde_json::Value::String(
                "--pf-v5-global--Color--100".to_string()
            ))
        );
    }

    #[test]
    fn test_template_literal_css_var() {
        let source = r#"const style = `var(--pf-v5-global--spacer--md)`;"#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_no_match() {
        let source = r#"const color = "--pf-v6-global--Color--100";"#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_css_var_in_function_call() {
        let source = r#"getComputedStyle(el).getPropertyValue("--pf-v5-global--Color--100");"#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_in_variable_declaration() {
        let source = r#"const varName = "--pf-v5-global--spacer--lg";"#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_in_style_object_key() {
        let source = r#"
            const el = <div style={{ "--pf-v5-c-label--Color": "red" }}>text</div>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("matchingText"),
            Some(&serde_json::Value::String(
                "--pf-v5-c-label--Color".to_string()
            ))
        );
    }

    #[test]
    fn test_css_var_in_style_object_value() {
        let source = r#"
            const el = <div style={{ color: "var(--pf-v5-global--Color--100)" }}>text</div>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_inside_map_callback() {
        let source = r#"
            const el = <ul>{items.map((item) => (
                <li style={{ "--pf-v5-c-label--Color": item.color }}>text</li>
            ))}</ul>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_inside_ternary() {
        let source = r#"
            const el = <div>{condition ? (
                <span style={{ "--pf-v5-c-icon--Color": "blue" }}>icon</span>
            ) : null}</div>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_inside_logical_and() {
        let source = r#"
            const el = <div>{show && <span style={{ "--pf-v5-c-icon--Color": "blue" }}>icon</span>}</div>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_in_object_expression() {
        let source = r#"
            const styles = {
                "--pf-v5-c-label__content--before--BorderColor": borderColor,
                "--pf-v5-c-label--BackgroundColor": backgroundColor,
            };
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 2);
    }

    #[test]
    fn test_css_var_in_ts_as_expression() {
        let source = r#"
            const el = <Label style={{
                "--pf-v5-c-label__content--before--BorderColor": borderColor,
                "--pf-v5-c-label--BackgroundColor": backgroundColor,
            } as React.CSSProperties} />;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 2);
    }

    #[test]
    fn test_css_var_in_ts_satisfies_expression() {
        let source = r#"
            const styles = {
                "--pf-v5-c-label--Color": "red",
            } satisfies Record<string, string>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    // --- Tests for previously missing statement types ---

    #[test]
    fn test_css_var_inside_if_block() {
        // Reproduces the PipelineRunsStatusCard.tsx Gap 1:
        // CSS var inside an if-body assignment expression
        let source = r#"
            function render() {
                let xAxisStyle = { tickLabels: { fill: 'var(--pf-v5-global--Color--100)' } };
                if (tickValues.length > 7) {
                    xAxisStyle = {
                        tickLabels: {
                            fill: 'var(--pf-v5-global--Color--100)',
                            angle: 320,
                        },
                    };
                }
                return xAxisStyle;
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            2,
            "Should find CSS var in both variable declaration AND if-body assignment"
        );
    }

    #[test]
    fn test_css_var_inside_if_else_chain() {
        // Reproduces the pipeline-topology/utils.ts Gap 2:
        // CSS vars inside if/else-if/else assignment branches
        let source = r#"
            function getDiamondState() {
                let diamondColor: string;
                if (isPipelineRun) {
                    diamondColor = 'var(--pf-v5-global--active-color--100)';
                } else if (!isFinallyTask) {
                    diamondColor = 'var(--pf-v5-global--BackgroundColor--200)';
                } else {
                    diamondColor = 'var(--pf-v5-global--BackgroundColor--light-100)';
                }
                return diamondColor;
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            3,
            "Should find CSS var in all three if/else-if/else branches"
        );
    }

    #[test]
    fn test_css_var_inside_for_loop() {
        let source = r#"
            function buildStyles() {
                const styles = [];
                for (let i = 0; i < items.length; i++) {
                    styles.push({ color: 'var(--pf-v5-global--Color--100)' });
                }
                return styles;
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1, "Should find CSS var inside for loop");
    }

    #[test]
    fn test_css_var_inside_for_of_loop() {
        let source = r#"
            function buildStyles() {
                for (const item of items) {
                    item.style = 'var(--pf-v5-global--spacer--md)';
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1, "Should find CSS var inside for-of loop");
    }

    #[test]
    fn test_css_var_inside_switch() {
        let source = r#"
            function getColor(type: string) {
                switch (type) {
                    case "success":
                        return 'var(--pf-v5-global--success-color--100)';
                    case "danger":
                        return 'var(--pf-v5-global--danger-color--100)';
                    default:
                        return 'var(--pf-v5-global--Color--100)';
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            3,
            "Should find CSS var in all switch cases"
        );
    }

    #[test]
    fn test_css_var_inside_try_catch() {
        let source = r#"
            function getStyle() {
                try {
                    return { color: 'var(--pf-v5-global--Color--100)' };
                } catch (e) {
                    return { color: 'var(--pf-v5-global--danger-color--100)' };
                } finally {
                    cleanup('var(--pf-v5-global--spacer--md)');
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            3,
            "Should find CSS var in try, catch, and finally"
        );
    }

    #[test]
    fn test_css_var_inside_while_loop() {
        let source = r#"
            function process() {
                while (hasMore) {
                    applyStyle('var(--pf-v5-global--spacer--lg)');
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(incidents.len(), 1, "Should find CSS var inside while loop");
    }

    // --- Tests for previously missing expression types ---

    #[test]
    fn test_css_var_in_assignment_expression() {
        // Direct test for AssignmentExpression handling
        let source = r#"
            function update() {
                style = { fill: 'var(--pf-v5-global--Color--100)' };
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in assignment expression"
        );
    }

    #[test]
    fn test_css_var_inside_optional_chain() {
        let source = r#"
            const el = <ul>{items?.map((item) => (
                <li style={{ color: 'var(--pf-v5-global--Color--100)' }}>text</li>
            ))}</ul>;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var inside optional chain .map() callback"
        );
    }

    #[test]
    fn test_css_var_in_tagged_template() {
        let source = r#"
            const styles = css`
                color: var(--pf-v5-global--Color--100);
                background: var(--pf-v5-global--BackgroundColor--200);
            `;
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in tagged template literal"
        );
    }

    #[test]
    fn test_css_var_in_spread_property() {
        let source = r#"
            const baseStyles = { "--pf-v5-c-label--Color": "red" };
            const styles = { ...baseStyles };
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        // Only the original object key should match, not the spread reference
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_css_var_in_computed_member() {
        let source = r#"
            const val = styles["--pf-v5-global--Color--100"];
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in computed member expression"
        );
    }

    #[test]
    fn test_css_var_in_await_expression() {
        let source = r#"
            async function getStyle() {
                const color = await Promise.resolve('var(--pf-v5-global--Color--100)');
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in await expression"
        );
    }

    #[test]
    fn test_css_var_in_labeled_statement() {
        let source = r#"
            function process() {
                outer: for (const item of items) {
                    item.color = 'var(--pf-v5-global--Color--100)';
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var inside labeled statement"
        );
    }

    // -- condition/test expression walking tests --

    #[test]
    fn test_css_var_in_if_condition() {
        let source = r#"
            function check() {
                if (el.style.getPropertyValue('--pf-v5-c-button--Color')) {
                    doSomething();
                }
            }
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in if condition"
        );
    }

    #[test]
    fn test_css_var_in_ternary_condition() {
        let source = r#"
            const color = el.style.getPropertyValue('--pf-v5-c-button--Color')
                ? 'blue'
                : 'red';
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var in ternary condition"
        );
    }

    #[test]
    fn test_css_var_in_iife() {
        // IIFE: (() => { ... })()
        let source = r#"
            const el = (() => {
                return <div style={{ marginTop: 'var(--pf-v5-global--spacer--sm)' }} />;
            })();
        "#;
        let incidents = scan_source(source, r"--pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find CSS var inside IIFE"
        );
    }
}
