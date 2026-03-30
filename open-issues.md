# Open Issues

## Bugs

### 1. Missing `ChainExpression` handler in JSX walker

**File:** `crates/js-scanner/src/jsx.rs` — `walk_expression_for_jsx` (line 241)

**Severity:** High

The `walk_expression_for_jsx` function handles `CallExpression`, `ArrowFunctionExpression`, `ConditionalExpression`, `LogicalExpression`, etc., but has **no arm for `ChainExpression`** (optional chaining).

Any JSX nested inside an optional chaining expression (e.g., `items?.map(...)`, `data?.filter(...)`) is completely invisible to the scanner. This is a common React pattern for rendering lists from optional data:

```tsx
{
  currentPageItems?.map((item) => (
    <Tr>
      <Td>
        <Switch labelOff="No" /> {/* ← never detected */}
      </Td>
    </Tr>
  ));
}
```

**Impact:** Rules that use `JSX_PROP` or `JSX_COMPONENT` with a `component` filter will produce zero incidents for any component rendered inside `?.map()` or similar optional chaining calls. Confirmed to cause `Switch.labelOff` removal rule to produce zero incidents despite correct rule definition.

**Fix:** Add a `ChainExpression` arm that unwraps the inner expression:

```rust
Expression::ChainExpression(chain) => {
    match &chain.expression {
        ChainElement::CallExpression(call) => {
            for arg in &call.arguments {
                // ... walk args same as CallExpression arm
            }
        }
        ChainElement::StaticMemberExpression(_) => {}
        ChainElement::ComputedMemberExpression(_) => {}
        ChainElement::PrivateFieldExpression(_) => {}
    }
}
```

---

### 2. Rule typo: `component: ^SwitchProps$` instead of `^Switch$`

**File:** `rules/patternfly-v5-to-v6/` (generated rules, not hand-authored)

**Severity:** Low (harmless — a duplicate rule with the correct `component: ^Switch$` exists)

Rule `semver-packages-react-core-src-components-switch-switch-tsx-switchprops-labeloff-removed` uses `component: ^SwitchProps$` in its `when` clause. `SwitchProps` is the TypeScript interface name, not the JSX component name. The correct value is `^Switch$`. A separate rule (`semver-...-switchprops-d-ts-switch-labeloff-removed`) has the correct component pattern.

---

### 3. Fix-engine: 9 files detected but never sent to goose

**Severity:** High

When running the fix-engine with `--llm-provider goose` against the tackle2-ui analysis output, 9 files that had violations in the analysis output were never processed by goose:

- `Constants.ts`
- `StateError.tsx`
- `StatusIcon.tsx`
- `TableHeaderContentWithControls.tsx`
- `application-assessment-donut-chart.tsx` (×2)
- `adoption-candidate-graph.tsx`
- `arrow.tsx`
- `donut.tsx`

All 9 files share a common trait: their primary violations are under the mega-rule `semver-patternfly-react-core-constant-removed-constantgroup-combined-group-2` which has **267 files / 286 incidents**. The fix-engine processed 292 unique files via goose but these 9 were silently dropped.

**Suspected cause:** The fix-engine's plan/grouping logic may have an issue when processing rules with very high incident counts. The `pending_llm` list may not have included these files, or they were filtered out during the merge/dedup phase.

**Investigation path:** Add logging to `plan_fixes` in `crates/fix-engine/src/engine.rs` to trace which files enter `pending_llm` vs `plan.files` vs `plan.manual` for this specific rule ID.

---

## Enhancement Requests

### 4. New location type: `TYPED_OBJECT_PROPERTY`

**Severity:** Medium

The provider cannot detect prop values assigned via typed object literals. For example:

```tsx
const paginationToolbarItemProps: ToolbarItemProps = {
  variant: "pagination",
  align: { default: "alignRight" }, // ← not detectable
};
```

A rule matching `pattern: ^align$`, `location: JSX_PROP`, `component: ^ToolbarItem$`, `value: ^alignRight$` will never fire because the value is set in a plain object, not JSX.

**Proposed:** A new `TYPED_OBJECT_PROPERTY` location that:

1. Finds `VariableDeclaration` nodes with type annotations
2. Walks the `ObjectExpression` initializer
3. Matches `ObjectProperty` keys against `pattern`
4. Optionally matches nested values against `value`
5. Resolves the type annotation back to a PF component via `component` / `from` filters

**File:** Would require a new scanner module in `crates/js-scanner/src/` and a new variant in `crates/core/src/capabilities.rs:ReferenceLocation`.

---

### 5. Interface extension type analysis

**Severity:** Medium

The provider cannot detect that a project-local interface extending a PF interface overrides props with incompatible types:

```tsx
export interface ISimpleSelectProps
  extends Omit<SelectProps, "toggle" | "isOpen" | "onSelect" | "onOpenChange"> {
  variant?: "single" | "checkbox"; // ← incompatible with SelectProps.variant: "default" | "typeahead"
}
```

This causes 22+ downstream TypeScript errors when callers pass `variant="typeahead"` to `<SimpleSelect>`. The `TYPE_REFERENCE` location detects the type name but doesn't inspect interface `extends` clauses for prop compatibility.

**Proposed:** Extend `TYPE_REFERENCE` or add a new scanner that detects interfaces extending PF types and flags overridden properties whose types are incompatible with the base interface.

---

### 6. Missing required prop detection (`frontend.missing_prop`)

**Severity:** Medium

Conformance rules only validate parent-child composition (which components appear as children of which). They cannot detect missing required props on components.

For example, `<CodeEditorControl />` with no props fails because `icon` and `onClick` are required. No rule can express "fire when `icon` prop is absent on `<CodeEditorControl>`."

**Proposed:** A new condition type `frontend.missing_prop` or a negation operator on `JSX_PROP` that fires when a specified prop is **not present** on a JSX element.
