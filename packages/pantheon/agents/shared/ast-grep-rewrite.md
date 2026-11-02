## Structural Code Rewrite with ast-grep

**Prefer ast-grep for edits selected by code structure**, including a single call inside a specific function, repeated API migrations, and changes that must preserve arguments or bodies. Before reaching for `fs_re_replace`, a replacement script, or a large `fs_edit`, try a structural match via `bash_exec`. Use `fs_edit` for prose, literal text, unsupported languages, or edits that are clearer as a small patch; use `fs_write` for new files.

Check `ast-grep --version` first. The examples use `ast-grep`; `sg` is an alias only when `sg --version` identifies ast-grep (some systems have an unrelated `sg`). If unavailable, use a targeted file edit and the normal validation workflow.

AST matching targets syntax nodes and preserves captured source text; it does **not** guarantee valid syntax, type correctness, symbol identity, or unchanged behavior. Replacements are text. Inspect the surrounding code and imports, then run the project's formatter, type checker/build, and relevant tests.

### Apply and Inspect Narrow Edits; Preview Broad or Uncertain Ones

1. Read the target code and its existing diff, and choose explicit file paths or a narrow directory. Keep the original content available so you can undo only your changes. Use `typescript` for `.ts` and `tsx` for `.tsx`.
2. For a small, reversible edit with understood matches, run the rewrite with `--update-all` followed by `&& git diff -- <paths>` in **one `bash_exec` call**. A separate preview is optional here. `--update-all` reports an applied-change count; the following `git diff` supplies the diff hunks for inspection.
3. For broad migrations or uncertain matching rules, first run the rewrite without `--update-all` or `--interactive` and review the proposed diff. Then apply the same rule and paths; preview again if files changed in between. Avoid interactive mode in agent shell calls.
4. Review the resulting diff and match count before continuing. Zero or unexpected matches mean refine the rule, not broaden the write blindly. If an edit is wrong, revert only your own hunks using the saved original content; do not use blanket `git restore` or `git reset` that would discard pre-existing work. `git diff` includes any pre-existing unstaged edits, so compare against the diff you read before applying.
5. Run project validation after each transformation. Widen the scope only after verifying a small sample. If a rewrite fails, inspect for partial changes before retrying.

Single-quote patterns and replacements so the shell does not expand `$NAME`. `$NAME` captures one node; `$$$ARGS` captures zero or more arguments or statements. Reuse captures to keep their original contents.

### Migrate Calls While Preserving Arguments

When an API changes from positional arguments to an options object:

```bash
# After reading the target code, apply this narrow edit and inspect it in one tool call.
ast-grep run --lang typescript --pattern 'createUser($NAME, $EMAIL)' \
  --rewrite 'createUser({ name: $NAME, email: $EMAIL })' src/users.ts --update-all &&
git diff -- src/users.ts
```

For example, `createUser(displayName(user), normalize(email))` becomes `createUser({ name: displayName(user), email: normalize(email) })`. Verify that matched calls refer to the intended API; ast-grep does not resolve imports or shadowed names.

### Edit Only Within a Named Function

Suppose `saveUser` and `previewUser` both call `console.log`, but only `saveUser` should use `logger.info`. Match the **call**, then constrain its enclosing function. Do not replace the entire function body to change one statement.

```bash
# Apply and inspect calls directly owned by saveUser, including its if/loop blocks.
ast-grep scan --inline-rules 'id: save-user-logging
language: typescript
rule:
  pattern: console.log($$$ARGS)
  inside:
    kind: function_declaration
    has:
      field: name
      regex: ^saveUser$
    stopBy:
      any:
        - kind: function_declaration
        - kind: function_expression
        - kind: arrow_function
        - kind: method_definition
        - kind: generator_function
        - kind: generator_function_declaration
fix: logger.info($$$ARGS)' src/users.ts --update-all &&
git diff -- src/users.ts
```

Use this combined apply-and-inspect call when the target function and expected matches are understood; otherwise preview first by omitting `--update-all` and the trailing `&& git diff` command. The `fix` replaces only the matched call and preserves all arguments. Other functions, comments, and string literals remain unchanged. Ensure `logger` is available in this scope.

`has` with `field: name` checks the function's own name, not an identifier somewhere in its body. The `stopBy` rule stops at the nearest function boundary, excluding nested callbacks and functions. Use `stopBy: end` instead only when nested scopes should also match. Adapt the enclosing `kind` for a method or arrow function; inspect unfamiliar syntax with `--debug-query=cst` before writing a rule.

### Rust: Replace a Call Without Rewriting Its Surroundings

When migrating calls to a replacement helper with the same signature:

```bash
# After confirming the helper and matches, apply and inspect in one tool call.
ast-grep run --lang rust --pattern 'old_api($$$ARGS)' \
  --rewrite 'new_api($$$ARGS)' src/service.rs --update-all &&
git diff -- src/service.rs
```

This changes call expressions, including multiline argument lists, while leaving surrounding `?`, assignments, and control flow in place. It is not a symbol rename: definitions and differently qualified calls need separate inspection. For a Rust function-scoped rule, use `function_item` and its `name` field, with boundaries for both `function_item` and `closure_expression`.

Always finish with the repository's formatter and checks. Structural selection reduces accidental text matches; review and validation establish whether the edit is correct.
