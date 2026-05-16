## Structural Code Rewrite with ast-grep

ast-grep (`sg`) can rewrite code by matching structural patterns and applying transformations that preserve captured meta-variables. Use this for refactoring operations where text-based find-and-replace would be fragile or miss variations.

### Simple Rewrites

Use `sg --pattern X --rewrite Y` to transform code. Meta-variables captured in the pattern are available in the rewrite:

```bash
# PREVIEW changes (default — shows diff without modifying files)
sg --pattern 'console.log($ARG)' --rewrite 'logger.info($ARG)' --lang javascript

# APPLY changes to files (adds --update-all flag)
sg --pattern 'console.log($ARG)' --rewrite 'logger.info($ARG)' --lang javascript --update-all
```

More examples:

```bash
# Rename a function across the codebase
sg --pattern 'oldFunction($$$ARGS)' --rewrite 'newFunction($$$ARGS)' --lang typescript --update-all

# Wrap an expression
sg --pattern 'await $EXPR' --rewrite 'await retry(() => $EXPR)' --lang typescript --update-all

# Unwrap an expression
sg --pattern 'Optional.of($VAL)' --rewrite '$VAL' --lang java --update-all

# Update an API call signature
sg --pattern 'createUser($NAME, $EMAIL)' --rewrite 'createUser({ name: $NAME, email: $EMAIL })' --lang typescript --update-all

# Scope rewrite to a specific directory
sg --pattern 'OLD_CONST' --rewrite 'NEW_CONST' --lang typescript --update-all src/api/
```

### Complex Rewrites with YAML Rules

For rewrites that need conditional matching, use YAML rules with a `fix` field:

```bash
# Replace fetch() with httpClient.get() only when inside async functions
sg scan --inline-rules 'id: migrate-fetch
language: typescript
rule:
  pattern: fetch($URL)
  inside:
    kind: function_declaration
    has:
      kind: async
    stopBy: end
fix: httpClient.get($URL)' --update-all

# Replace deprecated API — conditional on specific import
sg scan --inline-rules 'id: replace-deprecated
language: typescript
rule:
  pattern: oldLib.doThing($$$ARGS)
fix: newLib.doThing($$$ARGS)' --update-all
```

### Safety Protocol

**Always preview before applying.** This is non-negotiable for rewrites:

1. **Preview**: Run WITHOUT `--update-all` to see the diff output.
2. **Review**: Check that every match is correct and no false positives exist.
3. **Apply**: Add `--update-all` to modify files.
4. **Verify**: Read modified files or run tests/linters to confirm correctness.

```bash
# Step 1: Preview
sg --pattern 'foo($A, $B)' --rewrite 'foo($B, $A)' --lang typescript

# Step 2: Review the diff output...

# Step 3: Apply
sg --pattern 'foo($A, $B)' --rewrite 'foo($B, $A)' --lang typescript --update-all

# Step 4: Verify
npx tsc --noEmit
```

For large-scale rewrites, scope to specific directories to limit blast radius:

```bash
sg --pattern 'OLD' --rewrite 'NEW' --lang typescript --update-all src/api/
```

### Key Rules

1. **Preview first**: Never use `--update-all` without previewing the diff.
2. **One rewrite at a time**: Apply one transformation, verify it, then proceed to the next.
3. **Scope narrowly**: Specify file paths to limit which files are modified.
4. **Verify after**: Run tests, type checks, or linters after every rewrite to catch unintended changes.
5. **Prefer ast-grep over text replacement** when the change is structural (function calls, imports, class patterns). Use `fs_edit` for simple literal text changes.
