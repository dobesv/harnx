## Structural Code Search with ast-grep

ast-grep (`sg`) searches code by its **syntactic structure**, not just text. It understands the AST (Abstract Syntax Tree) of the code, so it can match patterns regardless of formatting, whitespace, or variable names. Run it via `bash_exec` when you need structural precision that ripgrep cannot provide.

### When to Use ast-grep vs ripgrep

| Use ripgrep (`rg`) when... | Use ast-grep (`sg`) when... |
|---|---|
| Searching for literal strings, comments, or log messages | Finding structural patterns (function signatures, class definitions, import shapes) |
| Simple regex matches (e.g. `rg "TODO\|FIXME"`) | Matching code with variable parts ("any function that calls X") |
| Listing files containing a term (`rg -l "pattern"`) | Finding code that is **missing** expected patterns (e.g. async without error handling) |
| Counting occurrences across files | Language-aware matching that ignores whitespace and formatting differences |

**Rule of thumb**: Start with `rg` for quick discovery, switch to `sg` when you need structural precision.

### Pattern Syntax — Meta-Variables

ast-grep patterns use **meta-variables** as structural wildcards:

| Meta-variable | Matches | Example |
|---|---|---|
| `$NAME` | One AST node (identifier, expression, statement, etc.) | `console.log($ARG)` matches `console.log("hello")` and `console.log(getMsg())` |
| `$$$` | Zero or more AST nodes (arguments, parameters, statements) | `function $F($$$) { $$$ }` matches any function regardless of parameters or body |
| `$$OP` | One unnamed node (operators, punctuation) | Used with `kind` rules for operator matching |

**Meta-variable rules:**
- **Consistency**: `$A == $A` matches `x == x` but NOT `x == y` — same name means same content.
- **Sole content**: A meta-variable must be the **entire** content of one AST node. Patterns like `"hello $NAME"` or `obj.$METHOD` will NOT capture the meta-variable. Use a full pattern with context instead.
- **Naming**: Must be `$UPPER_CASE` — e.g. `$VAR`, `$FUNC_NAME`, `$$$ARGS`. Lowercase like `$var` won't work.

### Simple Pattern Search

Use `sg --pattern` for straightforward structural matching:

```bash
# Find all console.log calls (any number of arguments)
sg --pattern 'console.log($$$)' --lang javascript

# Find exported functions in TypeScript
sg --pattern 'export function $NAME($$$) { $$$ }' --lang typescript

# Find React useState hooks
sg --pattern 'const [$STATE, $SETTER] = useState($$$)' --lang tsx

# Find import statements from a specific module
sg --pattern 'import { $$$ } from "react"' --lang typescript

# Find Python class definitions with inheritance
sg --pattern 'class $NAME($BASE):' --lang python

# Scope search to a specific directory
sg --pattern 'describe($$$)' --lang typescript src/tests/

# Get JSON output for programmatic processing
sg --pattern 'export default $EXPR' --lang typescript --json
```

### Complex Searches with YAML Rules

For searches requiring relational logic (`has`, `inside`) or composite logic (`all`, `any`, `not`), use inline YAML rules with `sg scan`:

```bash
# Find async functions that contain await expressions
sg scan --inline-rules 'id: find-async-await
language: typescript
rule:
  kind: function_declaration
  has:
    pattern: await $EXPR
    stopBy: end'

# Find console.log calls inside class methods
sg scan --inline-rules 'id: console-in-method
language: typescript
rule:
  pattern: console.log($$$)
  inside:
    kind: method_definition
    stopBy: end'

# Find async functions WITHOUT try-catch (missing error handling)
sg scan --inline-rules 'id: async-no-trycatch
language: typescript
rule:
  all:
    - kind: function_declaration
    - has:
        pattern: await $EXPR
        stopBy: end
    - not:
        has:
          kind: try_statement
          stopBy: end'

# Find any console method call (log, warn, error, debug)
sg scan --inline-rules 'id: any-console
language: typescript
rule:
  any:
    - pattern: console.log($$$)
    - pattern: console.warn($$$)
    - pattern: console.error($$$)
    - pattern: console.debug($$$)'
```

**Rule types:**
- **`pattern`**: Match by code pattern — `pattern: console.log($ARG)`
- **`kind`**: Match by AST node type — `kind: function_declaration`, `kind: call_expression`, `kind: class_declaration`
- **`has`**: Node must contain a descendant matching the sub-rule
- **`inside`**: Node must be inside an ancestor matching the sub-rule
- **`all`**: All sub-rules must match (AND)
- **`any`**: At least one sub-rule must match (OR)
- **`not`**: Sub-rule must NOT match (negation)

### Debugging When Patterns Don't Match

```bash
# Inspect the AST structure of code to find correct node kinds
sg --pattern 'async function example() { await fetch(); }' --lang typescript --debug-query=cst

# See how ast-grep interprets your pattern (are meta-variables detected?)
sg --pattern 'const [$A, $B] = useState($INIT)' --lang tsx --debug-query=pattern
```

Use `--debug-query=cst` to discover the correct `kind` values for your language (e.g., `function_declaration`, `arrow_function`, `call_expression`).

### Key Gotchas

1. **Always use `stopBy: end`** on relational rules (`has`, `inside`). Without it, the search stops at the first non-matching child node and misses deeper matches.
2. **Escape `$` in double-quoted shell strings**: Use `\$VAR` or wrap the entire argument in single quotes (`'$VAR'`).
3. **Always specify `--lang`**: AST structure differs between languages. Use `typescript` for `.ts`/`.tsx`, `javascript` for `.js`, `python` for `.py`, `go` for `.go`, etc.
4. **Start simple**: Begin with a basic `pattern`, test it, then add `kind`, relational rules, and composite logic incrementally. Complex rules that don't match are hard to debug.
5. **Patterns match one node**: A `pattern` matches a single AST node. To find relationships between nodes (e.g., "function containing X"), use YAML rules with `has`/`inside`.
