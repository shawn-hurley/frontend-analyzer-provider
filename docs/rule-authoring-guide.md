# Rule Authoring Guide

This guide covers the condition types, parameters, and patterns available when writing rules for the frontend analyzer provider.

## Rule Structure

Rules are written in YAML and follow the [Konveyor](https://github.com/konveyor/analyzer-lsp) rule format. Each rule file must be in a directory with a `ruleset.yaml` that defines the ruleset metadata.

```yaml
- ruleID: my-rule-id
  description: "Short description"
  labels:
    - change-type=component-rename
    - has-codemod=true
  effort: 1              # 1-5 estimate of fix effort
  category: mandatory     # mandatory | optional | potential
  when:
    frontend.referenced:
      pattern: "^OldName$"
      location: IMPORT
  message: "Explain what changed and how to fix it."
  links:
    - url: "https://example.com"
      title: "Reference docs"
```

### Compound Conditions

Use `or`, `and`, and `not` to compose conditions:

```yaml
# Match either condition
when:
  or:
    - frontend.referenced: { pattern: "^Chip$", location: JSX_COMPONENT }
    - frontend.referenced: { pattern: "^ChipGroup$", location: JSX_COMPONENT }
```

```yaml
# Match both conditions (with negation)
when:
  and:
    - builtin.filecontent:
        pattern: "@patternfly/react-core/dist/styles/base\\.css"
        filePattern: "\\.(ts|tsx|js|jsx)$"
    - not: true
      builtin.filecontent:
        pattern: "@patternfly/patternfly/utilities/_index\\.css"
        filePattern: "\\.(ts|tsx|js|jsx)$"
```

> **Note:** `and`/`not` conditions use kantra's rule engine. They work when running through kantra but not when using the provider in isolation.

---

## Condition Types

The provider supports 4 condition types. All pattern parameters use full regex syntax.

### `frontend.referenced` — JS/TS/JSX/TSX Semantic Search

The primary condition type. Searches for symbols, components, props, and types in JavaScript and TypeScript source files.

**Files scanned:** `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts`

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | regex | **Yes** | Pattern to match against the symbol name |
| `location` | enum | No | Restrict to a specific AST location (see below). If omitted, searches all locations |
| `component` | regex | No | JSX_PROP filter: only match props on components with this name |
| `parent` | regex | No | JSX_COMPONENT filter: only match components that are direct children of this parent |
| `value` | regex | No | JSX_PROP filter: only match props with this value |
| `from` | regex | No | IMPORT filter: only match imports from this module path |
| `filePattern` | regex | No | Only scan files whose path matches this pattern |

#### Locations

##### `IMPORT`

Matches import declarations.

```yaml
# Match any import of 'Select' from the deprecated path
frontend.referenced:
  pattern: "^Select$"
  location: IMPORT
  from: "@patternfly/react-core/deprecated"
```

Matches:
```tsx
import { Select } from '@patternfly/react-core/deprecated';  // yes
import { Select } from '@patternfly/react-core';              // no (wrong module)
import { SelectOption } from '@patternfly/react-core/deprecated'; // no (wrong name)
```

Incident variables set: `importedName`, `localName`, `module`, `matchingText`

##### `JSX_COMPONENT`

Matches JSX element usage. Use `parent` to scope to a specific parent element.

```yaml
# Match <BarsIcon> only when used inside <PageToggleButton> or <Button>
frontend.referenced:
  pattern: "^BarsIcon$"
  location: JSX_COMPONENT
  parent: "^(PageToggleButton|Button)$"
```

Matches:
```tsx
<Button><BarsIcon /></Button>          // yes
<PageToggleButton><BarsIcon /></PageToggleButton>  // yes
<div><BarsIcon /></div>                // no (wrong parent)
```

The `parent` filter checks the **direct parent** JSX element. The scanner walks into JSX children, expression containers (`{cond && <X/>}`), and prop value expressions (`toggle={ref => (<X/>)}`).

Incident variables set: `componentName`, `parentName`

##### `JSX_PROP`

Matches JSX props/attributes. Use `component` and `value` to scope.

```yaml
# Match variant="plain" on MenuToggle only
frontend.referenced:
  pattern: "^variant$"
  location: JSX_PROP
  component: "^MenuToggle$"
  value: "^plain$"
```

Matches:
```tsx
<MenuToggle variant="plain">    // yes
<MenuToggle variant="primary">  // no (wrong value)
<Button variant="plain">        // no (wrong component)
<MenuToggle isDisabled>         // no (wrong prop name)
```

The `value` filter matches against:
- String literal values: `variant="plain"` matches `^plain$`
- Expression text: `variant={SelectVariant.checkbox}` matches against `SelectVariant.checkbox`
- Boolean/valueless props (e.g., `isDisabled`) have no value and are filtered out when `value` is set

Incident variables set: `propName`, `propValue`, `componentName`

##### `FUNCTION_CALL`

Matches function and hook calls.

```yaml
frontend.referenced:
  pattern: "^useButton$"
  location: FUNCTION_CALL
```

Matches: `useButton()`, `useButton(options)`

Incident variables set: `functionName`

##### `TYPE_REFERENCE`

Matches TypeScript type references in type annotations, interface declarations, and type aliases.

```yaml
frontend.referenced:
  pattern: "^ButtonProps$"
  location: TYPE_REFERENCE
```

Matches:
```tsx
const props: ButtonProps = {};     // yes
interface MyProps extends ButtonProps {}  // yes
type Alias = ButtonProps;          // yes
```

Incident variables set: `typeName`

---

### `frontend.cssclass` — CSS Class Name Search

Searches for CSS class names in both CSS/SCSS files and JS/TS files.

**CSS files scanned:** `.css`, `.scss`, `.less`, `.sass` — scans class selectors (`.my-class`)
**JS/TS files scanned:** `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts` — scans `className` prop values, string literals, and template literals

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | regex | **Yes** | CSS class name pattern |
| `filePattern` | regex | No | Only scan files matching this path pattern |

```yaml
# Match all PF v5 class prefixes
frontend.cssclass:
  pattern: "pf-v5-"
```

```yaml
# Match validated state modifiers only in CSS/SCSS files
frontend.cssclass:
  pattern: "pf-m-(success|warning|error)"
  filePattern: "\\.(css|scss)$"
```

This finds classes in:
- CSS selectors: `.pf-v5-c-button { ... }`
- JSX className props: `className="pf-v5-c-button"`
- String literals: `const cls = "pf-v5-c-button"`
- Template literals: `` className={`pf-v5-c-button ${modifier}`} ``

Incident variables set: `className` or `matchingText`

---

### `frontend.cssvar` — CSS Custom Property Search

Searches for CSS custom properties (variables) in both CSS/SCSS and JS/TS files.

**CSS files scanned:** `.css`, `.scss`, `.less`, `.sass`
**JS/TS files scanned:** `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts` — scans string/template literals and JSX attribute values

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | regex | **Yes** | CSS variable pattern |
| `filePattern` | regex | No | Only scan files matching this path pattern |

```yaml
# Match old PF v5 CSS variable prefix
frontend.cssvar:
  pattern: "--pf-v5-"
```

```yaml
# Match physical property suffixes that need logical property renames
frontend.cssvar:
  pattern: "--pf-(v5|v6)-.*--(Padding|Margin)(Top|Bottom|Left|Right)\\b"
```

Incident variables set: `variableName` or `matchingText`

---

### `frontend.dependency` — package.json Dependency Check

Checks dependencies in the project's `package.json`. Scans `dependencies`, `devDependencies`, and `peerDependencies`.

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | No | Exact dependency name to match |
| `nameregex` | regex | No | Regex pattern for dependency name |
| `upperbound` | string | No | Match versions <= this (not yet implemented) |
| `lowerbound` | string | No | Match versions >= this (not yet implemented) |

At least one of `name` or `nameregex` should be specified.

```yaml
frontend.dependency:
  name: "@patternfly/react-core"
```

```yaml
frontend.dependency:
  nameregex: "^@patternfly/"
```

Incident variables set: `dependencyName`, `dependencyVersion`, `dependencyType`

---

### `builtin.filecontent` — Raw Text Search (via kantra)

This condition is handled by kantra's builtin provider, not the frontend provider. It does a regex search over file contents. Useful for patterns that don't fit the semantic conditions above.

Requires `builtin` in your `provider_settings.json`:

```json
{
  "name": "builtin",
  "initConfig": [{ "location": "/path/to/project" }]
}
```

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | regex | **Yes** | Regex pattern to search in file contents |
| `filePattern` | regex | No | Only search files matching this path pattern |

```yaml
# Match alignLeft/alignRight in TS/TSX files
builtin.filecontent:
  pattern: "alignLeft|alignRight"
  filePattern: "\\.(tsx?|jsx)$"
```

---

## File Scanning Behavior

### Directories Always Skipped

The scanner skips these directories:
`node_modules`, `.git`, `dist`, `build`, `target`, `.next`, `.nuxt`, `coverage`, `__pycache__`

### File Extensions

| Condition | Extensions |
|-----------|-----------|
| `frontend.referenced` | `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts` |
| `frontend.cssclass` | CSS: `.css`, `.scss`, `.less`, `.sass` — JS: `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts` |
| `frontend.cssvar` | CSS: `.css`, `.scss`, `.less`, `.sass` — JS: `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts` |
| `frontend.dependency` | `package.json` only |
| `builtin.filecontent` | Controlled by `filePattern` |

### `filePattern` Behavior

The `filePattern` parameter applies a regex filter to the full file path. If omitted, all files with matching extensions are scanned.

```yaml
# Only scan TypeScript files (not plain JS)
filePattern: "\\.(tsx?)$"

# Only scan CSS files (not SCSS)
filePattern: "\\.css$"

# Only scan files in the src/ directory
filePattern: "src/.*\\.(tsx?|jsx)$"
```

---

## Filter Parameter Details

The `component`, `parent`, `value`, and `from` filters on `frontend.referenced` are **post-scan filters**. The scanner first finds all matches for `pattern` + `location`, then filters the results.

### How Filters Interact

| Filter | Applies to | Incidents without the variable | Example |
|--------|-----------|-------------------------------|---------|
| `component` | `JSX_PROP` | Kept (not filtered) | `component: "^Button$"` only matches props on `<Button>` |
| `parent` | `JSX_COMPONENT` | Removed | `parent: "^Masthead$"` only matches components inside `<Masthead>` |
| `value` | `JSX_PROP` | Removed | `value: "^plain$"` only matches `variant="plain"`, skips `variant="primary"` |
| `from` | `IMPORT` | Kept (not filtered) | `from: "@patternfly/react-core"` only matches imports from that module |

The "Kept" vs "Removed" distinction matters when `location` is omitted and the scanner searches all locations. For example, with `from: "@patternfly/react-core"`:
- Import incidents are filtered to only that module
- JSX incidents (which don't have a `module` variable) are kept

---

## Practical Patterns

### Detect a Renamed Component

```yaml
- ruleID: pfv6-rename-chip-to-label
  when:
    or:
      - frontend.referenced: { pattern: "^Chip$", location: JSX_COMPONENT }
      - frontend.referenced: { pattern: "^ChipGroup$", location: JSX_COMPONENT }
  message: "Chip/ChipGroup renamed to Label/LabelGroup in PF v6."
```

### Detect a Renamed Prop

```yaml
- ruleID: pfv6-prop-rename-innerref-to-ref
  when:
    frontend.referenced:
      pattern: "^innerRef$"
      location: JSX_PROP
      component: "^(MenuToggle|Table|Td|Th|Tr)$"
  message: "innerRef renamed to ref in PF v6."
```

### Detect a Prop Value Change

```yaml
- ruleID: pfv6-prop-value-toolbar-group-variant
  when:
    frontend.referenced:
      pattern: "^variant$"
      location: JSX_PROP
      component: "^(ToolbarGroup|ToolbarToggleGroup)$"
      value: "^(button-group|icon-button-group)$"
  message: "variant values renamed: 'button-group'->'action-group', 'icon-button-group'->'action-group-plain'."
```

### Detect a Removed Import from a Specific Package

```yaml
- ruleID: pfv6-deprecated-application-launcher
  when:
    frontend.referenced:
      pattern: "^ApplicationLauncher"
      location: IMPORT
      from: "@patternfly/react-core"
  message: "ApplicationLauncher removed from @patternfly/react-core in PF v6."
```

### Detect a Component Used as a Child of Another

```yaml
- ruleID: pfv6-behavioral-button-icon-children
  when:
    frontend.referenced:
      pattern: "Icon$"
      location: JSX_COMPONENT
      parent: "^Button$"
  message: "Icons as Button children should use the icon prop instead."
```

### Detect Stale CSS Class Prefixes

```yaml
- ruleID: pfv6-css-v5-prefix
  when:
    frontend.cssclass:
      pattern: "pf-v5-"
  message: "PF v5 CSS class prefix must be updated for PF v6."
```

### Detect a Missing Import (using and/not)

```yaml
- ruleID: pfv6-missing-utilities-css-import
  when:
    and:
      - builtin.filecontent:
          pattern: "@patternfly/react-core/dist/styles/base\\.css"
          filePattern: "\\.(ts|tsx|js|jsx)$"
      - not: true
        builtin.filecontent:
          pattern: "@patternfly/patternfly/utilities/_index\\.css"
          filePattern: "\\.(ts|tsx|js|jsx)$"
  message: "PF v6 requires a separate utilities CSS import."
```

---

## Incident Variables Reference

When a condition matches, the scanner sets variables on the incident. These variables are used by filters (see above) and by the fix engine to determine what was matched.

| Scanner | Variable | Description |
|---------|----------|-------------|
| Import | `importedName` | The imported symbol name |
| Import | `localName` | The local alias (if renamed via `as`) |
| Import | `module` | The module path (e.g., `@patternfly/react-core`) |
| Import | `matchingText` | Full import text when module matches |
| JSX Component | `componentName` | The component name (e.g., `Button`) |
| JSX Component | `parentName` | The direct parent JSX element name |
| JSX Prop | `propName` | The prop name (e.g., `variant`) |
| JSX Prop | `propValue` | The prop value as text |
| JSX Prop | `componentName` | The component the prop is on |
| Function Call | `functionName` | The called function name |
| Type Reference | `typeName` | The referenced type name |
| CSS Class | `className` | The matched class name |
| CSS Variable | `variableName` | The matched CSS variable name |
| Dependency | `dependencyName` | The package name |
| Dependency | `dependencyVersion` | The version string |
| Dependency | `dependencyType` | `dependencies`, `devDependencies`, or `peerDependencies` |

These variables appear in the kantra analysis output under each incident's `variables` field and can be referenced in rule messages using `{{variableName}}` syntax.
