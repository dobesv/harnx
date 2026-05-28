---
title: "Sentinel env vars for hook scripts: two-process env gap and jaq variable injection"
date: 2026-05-27
category: "integration-issues"
problem_type: integration_issue
component: "harnx-proxy-auth"
root_cause: "process boundary env isolation and jaq API contract mismatch"
resolution_type: code_fix
severity: high
tags:
  - jaq
  - proxy
  - environment-variables
  - hooks
  - sentinel
  - process-isolation
plan_ref: "harnx-issue-632-sentinel-env-vars"
---

## Problem

`harnx-proxy-auth`'s `--env '<jaq-script>'` flag generates session-unique UUID sentinel values and injects them into bash tool call environments, enabling hook scripts to match sentinel values and replace them with real credentials. However, hook jaq filters evaluating `env.SENTINEL_VAR` saw `null` instead of the sentinel values because the proxy process's own environment was not updated — creating a two-process env gap.

Additionally, jaq's variable injection API requires specific patterns that differ from typical jq usage.

## Symptoms

- Hook filters reading `env.GITHUB_TOKEN_FAKE` returned `null` even though `--env` scripts executed successfully
- Sentinel values appeared in bash tool environments but not in HTTPS request modification hooks
- Non-string values from `--env` scripts caused MCP deserialization failures downstream in `harnx-mcp-bash`
- Attempting to inject variables into jaq scripts failed with compile errors or runtime `null` values

## Investigation Steps

1. Traced `--env` script evaluation through `filter::eval_env_scripts` — confirmed scripts executed and produced correct output
2. Verified `extra_env` map passed to `hook::run_jsonl_loop` — confirmed values present
3. Added debug logging to hook filter execution — `env.VAR` returned `null`
4. Examined jaq's `env` implementation — reads from `std::env::var` at runtime, not from injected context
5. Identified process boundary: bash tool env injection (one process) vs. MITM proxy filter evaluation (another logical flow in same process, but reading from OS env)
6. Traced jaq-all `compile_with` API — variable names passed without `$` prefix, values supplied positionally via `Vars::new`

## Root Cause

### Two-Process Env Gap

`harnx-proxy-auth` runs two separate logical flows in the same process:
1. **JSONL loop**: Injects env vars into bash tool calls via `extra_env` parameter
2. **HTTPS MITM proxy**: Applies jaq filters to outbound requests; `env.VAR` reads from proxy process's own OS environment via `std::env::var`

The `extra_env` map is only passed to bash tool call augmentation — it never touches the proxy process's own environment. Hook filters executing at request time call `std::env::var` directly, bypassing `extra_env` entirely.

### jaq Variable Injection API

`jaq_all::compile_with(code, defs, funs, &var_names)` expects variable names without `$` prefix. At runtime, `jaq_core::Vars::new([val1, val2, ...])` positionally maps values to those names. Omitting either step causes compile errors or runtime `null`.

### String-Only Env Var Contract

`harnx-mcp-bash` expects `HashMap<String, String>` for env vars. Non-string values cause deserialization failures. Validation at injection time is too late — must validate at `eval_env_scripts` output time.

## Solution

### Export Sentinel Vars to Proxy Process Environment

After `eval_env_scripts`, call `std::env::set_var` for each key-value pair:

```rust
// main.rs
let extra_env = filter::eval_env_scripts(&args.env, &sentinels)?;

// Export sentinel env vars into this process's own environment so that
// --hook jaq filters can read them via env.VARNAME at request time.
// Without this, jaq's `env.VAR` reads from the proxy process environment,
// not from `extra_env` which is only passed to bash tool calls.
// `eval_env_scripts` guarantees all values are strings; unwrap is safe.
for (key, value) in &extra_env {
    std::env::set_var(key, value.as_str().unwrap_or_default());
}
```

### Jaq Variable Injection Pattern

```rust
// filter.rs
const ENV_SCRIPT_VAR_NAMES: [&str; 5] = [
    "fake_uuid_key",
    "fake_base64_key",
    "fake_url_base64_key",
    "fake_hex_key",
    "fake_email",
];

fn run_env_script(script: &str, sentinels: &Sentinels) -> Result<Map<String, Value>> {
    let var_names = ENV_SCRIPT_VAR_NAMES.map(str::to_owned);
    let filter = compile_with(script, defs(), data::funs().chain(auth_funs()), &var_names)
        .map_err(|errors| anyhow!("jaq env script compile error: {errors:?}"))?;

    let vars = Vars::new([
        sentinel_string_to_jaq_value(&sentinels.uuid_key)?,
        sentinel_string_to_jaq_value(&sentinels.base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.url_base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.hex_key)?,
        sentinel_string_to_jaq_value(&sentinels.email)?,
    ]);
    // ... execute filter with vars
}
```

### Native Jaq Helper Functions

Pattern for adding native jaq functions:

```rust
fn auth_funs() -> impl Iterator<Item = jaq_core::native::Fun<data::DataKind>> {
    // 1-arg function using unary helper
    fn bearer(cv: jaq_core::Cv<'_, data::DataKind>) -> jaq_core::ValXs<'_, json::Val> {
        unary(cv, |_input, token| {
            let token = token.try_as_bytes()?;
            Ok(json::Val::from_utf8_bytes(
                format!("Bearer {}", String::from_utf8_lossy(token)).into_bytes(),
            ))
        })
    }

    // 2-arg function using pop_var twice (last pop = first arg)
    fn basic(mut cv: jaq_core::Cv<'_, data::DataKind>) -> jaq_core::ValXs<'_, json::Val> {
        let pass = cv.0.pop_var();
        let user = cv.0.pop_var();
        bome((|| {
            let user = user.try_as_bytes()?;
            let pass = pass.try_as_bytes()?;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode([user, b":", pass].concat());
            Ok(json::Val::from_utf8_bytes(
                format!("Basic {encoded}").into_bytes(),
            ))
        })())
    }

    [
        run::<data::DataKind>(("bearer", v(1), bearer)),
        run::<data::DataKind>(("basic", v(2), basic)),
    ]
    .into_iter()
}
```

Key patterns:
- `v(n)` specifies number of filter arguments
- `unary(cv, |input, arg1| ...)` for single-arg helpers
- For multi-arg: `cv.0.pop_var()` repeatedly — last pop is first argument
- `Val::from_utf8_bytes(bytes)` constructs string values
- `val.try_as_bytes()` extracts bytes from input values

### String Validation at Script Output Time

```rust
// filter.rs - eval_env_scripts validates all values are strings
for (key, val) in &object {
    if !val.is_string() {
        return Err(anyhow!(
            "jaq env script output value for key {key:?} must be a string, got {val}"
        ));
    }
}
```

## Why This Works

**Env export**: `std::env::set_var` modifies the process's OS environment, which jaq's `env.VAR` reads from at runtime. This bridges the gap between the `extra_env` map (bash tool injection) and the proxy's actual environment (hook filter evaluation).

**Variable injection**: jaq's `compile_with` requires variable names at compile time (without `$` prefix) and values at runtime (positionally via `Vars::new`). Both must match in order and count.

**String validation**: Validating at `eval_env_scripts` time fails fast before any injection attempt, providing clear error messages about which key produced a non-string value. This prevents confusing MCP deserialization errors downstream.

## Prevention Strategies

**Test Cases:**
- Add E2E test proving hook filter can read sentinel values via `env.SENTINEL_VAR`
- Test that `eval_env_scripts` rejects non-string values with specific error message
- Test variable injection round-trip: define variable, reference in script, verify output

**Best Practices:**
- When spawning background processes that read env vars, distinguish between "env map passed to subprocess" and "current process's OS env"
- For jaq variable injection: compile with names (no `$`), run with `Vars::new([values...])`
- Validate env var values are strings immediately after script output, not at injection time

**Code Review Checklist:**
- [ ] Does the proxy process export env vars it needs for filter execution?
- [ ] Are jaq variables passed without `$` prefix to `compile_with`?
- [ ] Are variable values positionally matched to names in `Vars::new`?
- [ ] Are all env var values validated as strings before injection?

## Related Issues

- **Issue:** #632 — Add `--env` sentinel env vars feature
- **Solution:** [integration-issues/atlassian-cli-proxy-auth-2026-05-20.md](../integration-issues/atlassian-cli-proxy-auth-2026-05-20.md) — Similar pattern of env var injection in hook filters
- **Solution:** [security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Env var precedence and injection patterns
