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
        Statement::ExportDefaultDeclaration(decl) => match &decl.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_statement(s, source, pattern, file_uri, incidents);
                    }
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                walk_class_body(&class.body, source, pattern, file_uri, incidents);
            }
            _ => {
                if let Some(expr) = decl.declaration.as_expression() {
                    walk_expr(expr, source, pattern, file_uri, incidents);
                }
            }
        },
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(ref declaration) = decl.declaration {
                walk_declaration(declaration, source, pattern, file_uri, incidents);
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
        Statement::ClassDeclaration(class) => {
            walk_class_body(&class.body, source, pattern, file_uri, incidents);
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
            walk_expr(&if_stmt.test, source, pattern, file_uri, incidents);
            walk_statement(&if_stmt.consequent, source, pattern, file_uri, incidents);
            if let Some(alt) = &if_stmt.alternate {
                walk_statement(alt, source, pattern, file_uri, incidents);
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
            walk_statement(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::ForInStatement(f) => {
            walk_statement(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::ForOfStatement(f) => {
            walk_statement(&f.body, source, pattern, file_uri, incidents);
        }
        Statement::WhileStatement(w) => {
            walk_expr(&w.test, source, pattern, file_uri, incidents);
            walk_statement(&w.body, source, pattern, file_uri, incidents);
        }
        Statement::DoWhileStatement(d) => {
            walk_expr(&d.test, source, pattern, file_uri, incidents);
            walk_statement(&d.body, source, pattern, file_uri, incidents);
        }
        Statement::SwitchStatement(s) => {
            walk_expr(&s.discriminant, source, pattern, file_uri, incidents);
            for case in &s.cases {
                if let Some(test) = &case.test {
                    walk_expr(test, source, pattern, file_uri, incidents);
                }
                for stmt in &case.consequent {
                    walk_statement(stmt, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::TryStatement(t) => {
            for s in &t.block.body {
                walk_statement(s, source, pattern, file_uri, incidents);
            }
            if let Some(handler) = &t.handler {
                for s in &handler.body.body {
                    walk_statement(s, source, pattern, file_uri, incidents);
                }
            }
            if let Some(finalizer) = &t.finalizer {
                for s in &finalizer.body {
                    walk_statement(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::LabeledStatement(l) => {
            walk_statement(&l.body, source, pattern, file_uri, incidents);
        }
        Statement::ThrowStatement(t) => {
            walk_expr(&t.argument, source, pattern, file_uri, incidents);
        }
        _ => {}
    }
}

fn walk_declaration(
    decl: &Declaration<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match decl {
        Declaration::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    walk_statement(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    walk_expr(init, source, pattern, file_uri, incidents);
                }
            }
        }
        Declaration::ClassDeclaration(class) => {
            walk_class_body(&class.body, source, pattern, file_uri, incidents);
        }
        _ => {}
    }
}

fn walk_class_body(
    body: &ClassBody<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    for element in &body.body {
        match element {
            // Standard methods: render() { ... }
            ClassElement::MethodDefinition(method) => {
                if let Some(body) = &method.value.body {
                    for s in &body.statements {
                        walk_statement(s, source, pattern, file_uri, incidents);
                    }
                }
            }
            // Arrow property methods: render = () => { ... }
            ClassElement::PropertyDefinition(prop) => {
                if let Some(init) = &prop.value {
                    walk_expr(init, source, pattern, file_uri, incidents);
                }
            }
            _ => {}
        }
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
            walk_expr(&c.test, source, pattern, file_uri, incidents);
            walk_expr(&c.consequent, source, pattern, file_uri, incidents);
            walk_expr(&c.alternate, source, pattern, file_uri, incidents);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                walk_statement(s, source, pattern, file_uri, incidents);
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
        Expression::LogicalExpression(logical) => {
            walk_expr(&logical.left, source, pattern, file_uri, incidents);
            walk_expr(&logical.right, source, pattern, file_uri, incidents);
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    walk_expr(&p.value, source, pattern, file_uri, incidents);
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
        // Optional chaining: items?.map((item) => <div className="pf-v5-...">)
        Expression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        walk_expr(e, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        // Await/yield: const el = await getElement(); may contain JSX
        Expression::AwaitExpression(a) => {
            walk_expr(&a.argument, source, pattern, file_uri, incidents);
        }
        Expression::YieldExpression(y) => {
            if let Some(arg) = &y.argument {
                walk_expr(arg, source, pattern, file_uri, incidents);
            }
        }
        // Sequence: (expr1, expr2) — last expression may be JSX
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                walk_expr(e, source, pattern, file_uri, incidents);
            }
        }
        // Assignment: variable = <div className="...">
        Expression::AssignmentExpression(assign) => {
            walk_expr(&assign.right, source, pattern, file_uri, incidents);
        }
        // new Constructor(<div className="...">)
        Expression::NewExpression(new_expr) => {
            for arg in &new_expr.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        // Tagged templates: css`...pf-v5-...`
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
        // Binary expressions: (<Spinner className="pf-v5-..." />) + text
        Expression::BinaryExpression(bin) => {
            walk_expr(&bin.left, source, pattern, file_uri, incidents);
            walk_expr(&bin.right, source, pattern, file_uri, incidents);
        }
        // Unary expressions: !expr, typeof expr, void expr
        Expression::UnaryExpression(unary) => {
            walk_expr(&unary.argument, source, pattern, file_uri, incidents);
        }
        // Static member expressions: styles.className (walk object in case it's complex)
        Expression::StaticMemberExpression(member) => {
            walk_expr(&member.object, source, pattern, file_uri, incidents);
        }
        // Computed member: obj[expr] — walk both
        Expression::ComputedMemberExpression(member) => {
            walk_expr(&member.object, source, pattern, file_uri, incidents);
            walk_expr(&member.expression, source, pattern, file_uri, incidents);
        }
        // Class expression: const Foo = class extends React.Component { render() { ... } }
        Expression::ClassExpression(class) => {
            walk_class_body(&class.body, source, pattern, file_uri, incidents);
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

    // Walk into attribute values that contain expressions or string literals.
    // This catches patterns like:
    //   - `labelIcon={<Popover><button className="pf-v5-...">}` (expression containers)
    //   - `additionalClassNames="pf-v5-u-my-md"` (string on non-className props)
    for attr in &el.opening_element.attributes {
        if let JSXAttributeItem::Attribute(a) = attr {
            // Skip className/class — already handled above
            let is_classname = if let JSXAttributeName::Identifier(ident) = &a.name {
                let n = ident.name.as_str();
                n == "className" || n == "class"
            } else {
                false
            };
            match &a.value {
                Some(JSXAttributeValue::ExpressionContainer(expr_container)) => {
                    if let Some(expr) = expr_container.expression.as_expression() {
                        walk_expr(expr, source, pattern, file_uri, incidents);
                    }
                }
                Some(JSXAttributeValue::StringLiteral(s)) if !is_classname => {
                    // Check string literal values on any prop (not just className)
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
                _ => {}
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
        JSXChild::ExpressionContainer(expr_container) => {
            if let Some(expr) = expr_container.expression.as_expression() {
                walk_expr(expr, source, pattern, file_uri, incidents);
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

    #[test]
    fn test_classname_inside_map_callback() {
        let source = r#"
            const el = <ul>{items.map((item) => (
                <li className="pf-v5-c-tabs__item">text</li>
            ))}</ul>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("matchingText"),
            Some(&serde_json::Value::String("pf-v5-c-tabs__item".to_string()))
        );
    }

    #[test]
    fn test_classname_inside_ternary() {
        let source = r#"
            const el = <div>{condition ? (
                <span className="pf-v5-c-button pf-m-plain">click</span>
            ) : null}</div>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_classname_inside_logical_and() {
        let source = r#"
            const el = <div>{show && <span className="pf-v5-c-icon">icon</span>}</div>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_classname_inside_nested_map_and_ternary() {
        let source = r#"
            const el = <div>{items.map((item) => (
                item.visible ? <span className="pf-v5-c-label">label</span> : null
            ))}</div>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_classname_in_get_elements_by_classname() {
        let source = r#"
            const els = document.getElementsByClassName("pf-c-wizard__main-body");
        "#;
        let incidents = scan_source(source, r"pf-c-wizard");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_classname_inside_prop_value_ternary() {
        // Simulates platform-form.tsx: className on <button> inside a ternary
        // inside a JSX prop value on a non-exported component
        let source = r#"
            const MyForm: React.FC = () => {
                return (
                    <FormGroup
                        labelIcon={
                            condition ? (
                                <Popover>
                                    <button className="pf-v5-c-button pf-m-plain">
                                        <HelpIcon />
                                    </button>
                                </Popover>
                            ) : undefined
                        }
                    />
                );
            };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 inside prop value ternary"
        );
    }

    #[test]
    fn test_classname_inside_optional_chain_map() {
        // Bug: items?.map() is a ChainExpression — was not traversed
        let source = r#"
            const Table: React.FC = () => {
                return (
                    <tbody>{currentPageItems?.map((item) => (
                        <tr>
                            <td className="pf-v5-c-tooltip__content">text</td>
                        </tr>
                    ))}</tbody>
                );
            };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 inside optional chain .map() callback"
        );
    }

    #[test]
    fn test_classname_inside_optional_chain_filter_map() {
        // Chained optional: items?.filter(...)?.map(...)
        let source = r#"
            const el = <ul>{items?.filter(Boolean)?.map((item) => (
                <li className="pf-v5-c-tabs__item">text</li>
            ))}</ul>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_classname_inside_for_loop() {
        let source = r#"
            function render() {
                const els = [];
                for (let i = 0; i < items.length; i++) {
                    els.push(<div className="pf-v5-c-button">btn</div>);
                }
                return els;
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1, "Should find pf-v5 inside for loop");
    }

    #[test]
    fn test_classname_inside_switch() {
        let source = r#"
            function render(type: string) {
                switch (type) {
                    case "a":
                        return <div className="pf-v5-c-alert">alert</div>;
                    default:
                        return null;
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1, "Should find pf-v5 inside switch");
    }

    #[test]
    fn test_classname_inside_try_catch() {
        let source = r#"
            function render() {
                try {
                    return <div className="pf-v5-c-card">card</div>;
                } catch (e) {
                    return <span className="pf-v5-c-alert">error</span>;
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 2, "Should find pf-v5 in try and catch");
    }

    #[test]
    fn test_classname_inside_ts_as_expression() {
        let source = r#"
            const el = <div style={{
                color: "red"
            } as React.CSSProperties} className="pf-v5-c-button">click</div>;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(incidents.len(), 1);
    }

    // ── Tests for class component patterns (create-secret.tsx, file-input.tsx) ──

    #[test]
    fn test_classname_in_class_component_render() {
        // Mirrors create-secret.tsx: class component with render() returning JSX
        // containing native <input> elements with className
        let source = r#"
            class BasicAuthSubform extends React.Component<Props, State> {
                render() {
                    return (
                        <>
                            <div className="form-group">
                                <input
                                    className="pf-v5-c-form-control"
                                    type="text"
                                    name="username"
                                />
                            </div>
                            <div className="form-group">
                                <input
                                    className="pf-v5-c-form-control"
                                    type="password"
                                    name="password"
                                />
                            </div>
                        </>
                    );
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            2,
            "Should find pf-v5 in class component render() method"
        );
    }

    #[test]
    fn test_classname_in_jsx_returned_from_function_call() {
        // Mirrors file-input.tsx: render() returns connectDropTarget(<div>...</div>)
        // where JSX is passed as argument to a function, not in a return statement
        let source = r#"
            class FileInput extends React.Component<Props, State> {
                render() {
                    return connectDropTarget(
                        <div className="co-file-dropzone">
                            <div className="pf-v5-c-input-group">
                                <input className="pf-v5-c-form-control" type="text" />
                                <span className="pf-v5-c-button pf-m-tertiary">Browse</span>
                            </div>
                        </div>,
                    );
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            3,
            "Should find pf-v5 in JSX passed as function call argument"
        );
    }

    #[test]
    fn test_classname_in_jsx_binary_expression() {
        // Mirrors ApprovalTaskActionDropdown.tsx line 120-128:
        // (<Spinner className="pf-v5-u-mr-xs" />) + t('text')
        // JSX in a binary expression (addition with string)
        let source = r#"
            const tooltipContent = () => {
                return (
                    (<Spinner className="pf-v5-u-mr-xs" size="sm" />) + t('Checking...')
                );
            };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in JSX inside binary expression"
        );
    }

    #[test]
    fn test_classname_in_class_component_with_map_callback() {
        // Mirrors create-secret.tsx render() with _.map() returning JSX
        let source = r#"
            class CreateConfigSubform extends React.Component<Props, State> {
                render() {
                    const list = items.map((item, index) => {
                        return (
                            <div key={item.uid}>
                                <input className="pf-v5-c-form-control" type="text" />
                            </div>
                        );
                    });
                    return <>{list}</>;
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in class component map callback"
        );
    }

    #[test]
    fn test_classname_on_custom_prop_name() {
        // Mirrors resource-sidebar.tsx: additionalClassNames="pf-v5-u-my-md"
        // The scanner checks className/class attrs specifically, but should also
        // catch this via string literal matching
        let source = r#"
            const el = <SimpleTabNav additionalClassNames="pf-v5-u-my-md" />;
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in custom prop string value via string literal matching"
        );
    }

    #[test]
    fn test_classname_in_class_component_exported_with_hoc() {
        // Mirrors create-secret.tsx: class component wrapped in withTranslation() HOC
        // export const BasicAuthSubform = withTranslation()(BasicAuthSubformWithTranslation);
        let source = r#"
            class BasicAuthSubformWithTranslation extends React.Component<Props, State> {
                render() {
                    return (
                        <input className="pf-v5-c-form-control" type="text" />
                    );
                }
            }
            export const BasicAuthSubform = withTranslation()(BasicAuthSubformWithTranslation);
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in class component exported via HOC"
        );
    }

    #[test]
    fn test_classname_in_css_in_js_object_key() {
        // CSS-in-JS (emotion): class name used as object property key selector
        // e.g. css({ '& > .pf-v5-c-card__body': { padding: '0 !important' } })
        let source = r#"
            const section = css({
                '& > .pf-v5-c-card__body': {
                    padding: '0 !important',
                },
            });
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in CSS-in-JS object key selector"
        );
        assert_eq!(
            incidents[0].variables.get("matchingText").and_then(|v| v.as_str()),
            Some("& > .pf-v5-c-card__body"),
        );
    }

    #[test]
    fn test_classname_in_direct_object_key() {
        // Direct class selector as object key
        let source = r#"
            const styles = {
                '.pf-v5-c-button': { color: 'red' },
            };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in direct object key"
        );
    }

    #[test]
    fn test_classname_object_key_no_false_positive() {
        // Non-class-name object keys should not match
        let source = r#"
            const config = {
                'padding': '10px',
                'margin-top': '5px',
            };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            0,
            "Should not find pf-v5 in non-class object keys"
        );
    }

    #[test]
    fn test_classname_object_spread_property() {
        // Spread properties should be walked
        let source = r#"
            const base = "pf-v5-c-alert";
            const styles = { ...{ key: base } };
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in spread property value"
        );
    }

    // -- condition/test expression walking tests --

    #[test]
    fn test_classname_in_if_condition() {
        // Mirrors theme.tsx: if (el.classList.contains('pf-v5-theme-dark'))
        let source = r#"
            function toggle() {
                if (document.documentElement.classList.contains('pf-v5-theme-dark')) {
                    setLight(true);
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in if condition"
        );
    }

    #[test]
    fn test_classname_in_ternary_condition() {
        // Mirrors theme.ts: !x.contains('pf-v5-theme-dark') ? 'a' : 'b'
        let source = r#"
            const result = !document.documentElement.classList.contains('pf-v5-theme-dark')
                ? 'console-light'
                : 'console-dark';
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in ternary condition"
        );
    }

    #[test]
    fn test_classname_in_while_condition() {
        let source = r#"
            function process() {
                while (el.classList.contains('pf-v5-c-spinner')) {
                    doWork();
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in while condition"
        );
    }

    #[test]
    fn test_classname_in_switch_discriminant() {
        let source = r#"
            function check() {
                switch (getClass('pf-v5-c-button')) {
                    case 'a': break;
                }
            }
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 in switch discriminant"
        );
    }

    #[test]
    fn test_classname_in_iife() {
        // IIFE pattern: (() => { ... })()
        let source = r#"
            const el = (() => {
                return <div className="pf-v5-c-button">click</div>;
            })();
        "#;
        let incidents = scan_source(source, r"pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 inside IIFE"
        );
    }
}
