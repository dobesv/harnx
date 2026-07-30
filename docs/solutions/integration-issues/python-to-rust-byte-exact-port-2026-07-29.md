---
title: "Python-to-Rust byte-exact port: fidelity traps and patterns"
date: 2026-07-29
category: "integration-issues"
problem_type: integration_issue
component: "harnx-vercel-grep-server"
root_cause: "Python and Rust have divergent semantics for string operations, sorting stability, and JSON key ordering"
resolution_type: code_fix
severity: medium
tags:
  - python
  - rust
  - porting
  - byte-exact
  - serde
  - unicode
  - sorting
  - rmcp
plan_ref: "harnx-vercel-grep-server"
---

# Python-to-Rust byte-exact port: fidelity traps and patterns

## Problem

A "strict 1:1 port" of a Python MCP server to Rust requires matching not just the logic, but exact byte output including JSON key order, truncation behavior, sorting stability, and error formatting. Several Python semantics have no direct Rust equivalent and require explicit handling.

## Symptoms

```text
- JSON output key order differs from Python reference
- Snippet truncation splits multi-byte UTF-8 characters
- Sorting ties produce different ordering on repeated runs
- `total_matches` string "100+" parses as 100 instead of 0
- `str.isdigit()` behavior differs between Python and Rust
- Golden fixture byte comparison fails despite "equivalent" logic
```

## Investigation Steps

1. Fetched the verbatim upstream source (galprz/grep-mcp `server.py` at SHA 97300fc) to resolve ambiguities in the plan's paraphrase
2. Discovered the plan contradicted itself on which string gets truncated: raw HTML vs cleaned text — the reference code showed `_format_code_snippet` receives already-extracted text
3. Built a golden fixture with expected output and compared byte-by-byte
4. Traced Python `str.isdigit()` semantics: True only if non-empty and ALL characters are ASCII digits
5. Verified Python `sorted()` is stable; Rust's `sort_by_key` is also stable but `sort_unstable_by` is not
6. Confirmed Python `len()` and slicing operate on Unicode code points, not bytes

## Root Cause

### 1. JSON key ordering

Python `json.dumps` preserves dict insertion order (Python 3.7+). Rust `serde_json` preserves struct field declaration order only when `preserve_order` feature is enabled, which this workspace has.

### 2. String length and slicing

Python `len(s)` counts Unicode code points. Rust `s.len()` counts bytes. For truncation at 400 characters, Rust must use `s.chars().count()` and `s.chars().take(400).collect()`.

### 3. Sorting stability

Python `sorted(..., reverse=True)` is stable — ties preserve first-seen order. Rust provides both stable (`sort_by`, `sort_by_key`) and unstable (`sort_unstable_by`) sorts. Using unstable sort breaks tie-ordering guarantees.

### 4. isdigit semantics

Python `str.isdigit()` returns True only if the string is non-empty and every character is an ASCII digit. Strings like `"100+"`, `""`, `"-3"` all return False. Rust's `s.chars().all(|c| c.is_ascii_digit())` plus an emptiness check replicates this.

### 5. Error return contract

The Python reference returns errors as plain text content, not HTTP errors or JSON-RPC errors. The handler must return `CallToolResult::success` with a text block containing "`❌ Error: ...`", not a JSON-RPC error.

### 6. External API unreachability

grep.app returns HTTP 429 with a Vercel bot-challenge HTML page from CI/datacenter IPs. Live capture is impossible; synthetic fixtures modeling the actual shape are required.

## Solution

### JSON key ordering

Enable `preserve_order` on `serde_json` (already set workspace-wide) and declare struct fields in exact contract order:

```rust
#[derive(Serialize)]
struct Output {
    query: String,
    summary: Summary,
    results_by_repository: Vec<RepositoryResult>,
}

#[derive(Serialize)]
struct Summary {
    total_results: u64,
    results_shown: usize,
    repositories_found: usize,
    top_languages: Vec<LanguageCount>,
    top_repositories: Vec<RepositoryCount>,
}
```

Serialize with `serde_json::to_string_pretty(&output)` for 2-space indentation.

### Character-aware truncation

Use `.chars()` for all length checks and slicing:

```rust
let snippet = if text.chars().count() > 400 {
    let mut truncated = text.chars().take(400).collect::<String>();
    truncated.push_str("...");
    truncated
} else {
    text.to_owned()
};
```

### Stable descending sort

Use `sort_by_key` with `Reverse` for stable descending order:

```rust
results_by_repository.sort_by_key(|repo| std::cmp::Reverse(repo.matches_count));
```

Do NOT use `sort_unstable_by` — it does not preserve first-seen order on ties.

### isdigit-equivalent check

Match Python semantics exactly:

```rust
let total_matches = if !raw_string.is_empty()
    && raw_string.chars().all(|c| c.is_ascii_digit())
{
    raw_string.parse::<u64>().unwrap_or(0)
} else {
    0
};
```

This correctly maps `"5"` → 5, `"100+"` → 0, `""` → 0, `"-3"` → 0.

### Handler error return contract

Return all validation and runtime errors as successful tool results:

```rust
impl ServerHandler for GrepServer {
    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResult, ErrorData> {
        // Validation errors → success with error text
        if let Err(message) = params.validate() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(message)]));
        }

        // Runtime errors → success with error text  
        match outcome {
            SearchOutcome::RateLimited => Ok(text_result("❌ Error: Rate limit exceeded...")),
            SearchOutcome::NotFound => Ok(text_result(build_not_found_output(query))),
            // ...
        }
    }
}
```

Only unknown tool names return JSON-RPC `ErrorData::method_not_found`.

### rmcp 2.0 patterns

- STDERR-only logging; STDOUT reserved for JSON-RPC
- `GrepServer::with_base_url(base_url)` for wiremock injection
- In-memory duplex for e2e: `tokio::io::duplex(64)` → `serve_server` + `serve_client`

### Synthetic fixtures when live capture fails

When grep.app returns 429/Vercel-challenge, craft fixtures matching the reference's expected shape:

```json
{
  "facets": {"count": 1234, "lang": {"buckets": [...]}, "repo": {"buckets": [...]}},
  "hits": {"hits": [{"repo": {"raw": "owner/repo"}, "path": {"raw": "src/main.py"}, ...}]}
}
```

Include test cases for `total_matches.raw` as both numeric strings (`"5"`) and non-numeric (`"100+"`) to cover isdigit semantics.

## Why This Works

- `preserve_order` + field declaration order = deterministic JSON key order matching Python's dict insertion order
- `.chars().count()`/`.chars().take(n)` operate on Unicode code points like Python's `len()` and slicing
- `sort_by_key` with `Reverse` is stable, preserving first-seen order for ties like Python's `sorted(..., reverse=True)`
- Explicit emptiness check + `all(|c| c.is_ascii_digit())` matches Python's `str.isdigit()` exactly
- Success-tool-text for errors matches the Python contract; agents consuming this output need no changes
- Synthetic fixtures with golden output enable byte-exact assertion without live API access

## Prevention Strategies

**Test Cases:**
- Golden fixture with byte-exact expected output (use `include_str!` and assert equality)
- Multi-repo fixture exercising descending sort with ties (verify stable ordering)
- `total_matches` fixture with `"100+"` (assert parses to 0) and `"5"` (assert parses to 5)
- Multi-byte UTF-8 truncation fixture (assert no character splitting)

**Best Practices:**
- Always fetch the verbatim upstream source when porting; plans can paraphrase incorrectly
- Use `.chars()` for any Python string length/slice operation
- Use stable sorts (`sort_by_key`, `sort_by`) when Python uses `sorted()`
- Test with golden fixtures for byte-exact contracts
- Use `preserve_order` feature and declare struct fields in contract order
- Return errors as success-tool-text when the reference does — not JSON-RPC errors

**Code Review Checklist:**
- [ ] Are struct fields declared in exact JSON output order?
- [ ] Is `.chars().count()` used instead of `.len()` for Python-compatible length?
- [ ] Is the sort stable if Python's `sorted()` is used?
- [ ] Does isdigit-equivalent check for non-empty first, then all-ASCII-digits?
- [ ] Are runtime/validation errors returned as success-tool-text, not JSON-RPC errors?
- [ ] Is there a golden fixture with byte-exact expected output?

## Related Issues

- **Plan:** [harnx-vercel-grep-server](plan:harnx-vercel-grep-server) — native Rust grep.app MCP server
- **GitHub:** [#1268](https://github.com/dobesv/harnx/issues/1268) — original issue
- **Reference:** [galprz/grep-mcp](https://github.com/galprz/grep-mcp) — Python implementation ported
- **Related Solution:** [rmcp/mcp-proxy-stdio-pattern-2026-05-19.md](../rmcp/mcp-proxy-stdio-pattern-2026-05-19.md) — rmcp stdio patterns
