---
title: "refactor: Rename 'function' terminology to 'tool' for LLM-callable tools"
type: refactor
status: active
date: 2026-03-31
---

# Rename "function" to "tool" for LLM-callable tools

## Overview

The codebase uses "function" to refer to LLM-callable tools (a holdover from OpenAI's original "function calling" API naming). The industry has standardized on "tool" / "tool use." This refactor aligns the codebase terminology — code identifiers, config keys, env vars, directory names, and docs — with the modern convention.

## Problem Frame

Users configuring harnx encounter `function_calling`, `functions_dir`, `FunctionDeclaration`, etc. when the feature is universally known as "tool use" today. This creates confusion and makes the project feel dated.

## Requirements Trace

- R1. Rename all LLM-callable-tool references from "function" to "tool" in Rust source code
- R2. Rename config YAML keys: `function_calling` → `tool_use`, `functions_dir` constant → `tools`
- R3. Rename `supports_function_calling` → `supports_tool_use` in models.yaml
- R4. Rename env vars: `HARNX_FUNCTION_CALLING` → `HARNX_TOOL_USE`, `HARNX_FUNCTIONS_DIR` → `HARNX_TOOLS_DIR`
- R5. Update example configs and docs
- R6. Keep `use_tools` and `mapping_tools` names unchanged (already correct)
- R7. Clean break — no backward compatibility shims

## Scope Boundaries

- DO rename: `FunctionDeclaration`, `Functions` struct, `function_calling` config key, `functions` field, `functions_dir`/`functions_file`/`functions_bin_dir` paths, `supports_function_calling` in models.yaml, `select_functions`, `load_functions`, `function.rs` filename, env vars with "function" in name
- DO NOT rename: `use_tools`, `mapping_tools` (already say "tools"), Rust language `fn` keyword usage, unrelated uses of "function" in comments that refer to programming functions (not LLM tools)
- DO NOT rename: `ToolCall`, `ToolResult`, `eval_tool_calls` (already correct)

## Key Technical Decisions

- **Clean break**: No serde aliases or backward compat — users update configs once
- **File rename**: `src/function.rs` → `src/tool.rs`
- **Directory constant**: `FUNCTIONS_DIR_NAME` ("functions") → `TOOLS_DIR_NAME` ("tools")
- **On-disk**: The config directory `functions/` becomes `tools/`, `functions.json` becomes `tools.json`
- **models.yaml field**: `supports_function_calling` → `supports_tool_use`
- **Config key**: `function_calling: true` → `tool_use: true`

## Rename Map

| Old identifier | New identifier | Files |
|---|---|---|
| `src/function.rs` | `src/tool.rs` | filesystem |
| `FunctionDeclaration` | `ToolDeclaration` | function.rs, client/common.rs, mcp/convert.rs, mcp/client.rs, serve.rs, config/mod.rs |
| `Functions` (struct) | `Tools` | function.rs, config/mod.rs, config/agent.rs, serve.rs |
| `function_calling` (config field) | `tool_use` | config/mod.rs, config.example.yaml, config/role.rs |
| `select_functions` | `select_tools` | config/mod.rs, config/input.rs |
| `load_functions` | `load_tools` | config/mod.rs |
| `function_declarations_for_use_tools` | `tool_declarations_for_use_tools` | config/mod.rs |
| `functions` (Config field) | `tools` | config/mod.rs |
| `FUNCTIONS_DIR_NAME` | `TOOLS_DIR_NAME` | config/mod.rs |
| `FUNCTIONS_FILE_NAME` | `TOOLS_FILE_NAME` | config/mod.rs |
| `FUNCTIONS_BIN_DIR_NAME` | `TOOLS_BIN_DIR_NAME` | config/mod.rs |
| `functions_dir()` | `tools_dir()` | config/mod.rs |
| `functions_file()` | `tools_file()` | config/mod.rs |
| `functions_bin_dir()` | `tools_bin_dir()` | config/mod.rs |
| `agents_functions_dir()` | `agents_tools_dir()` | config/mod.rs |
| `agent_functions_dir()` | `agent_tools_dir()` | config/mod.rs, config/agent.rs |
| `HARNX_FUNCTION_CALLING` | `HARNX_TOOL_USE` | config/mod.rs (load_envs) |
| `HARNX_FUNCTIONS_DIR` | `HARNX_TOOLS_DIR` | config/mod.rs |
| `<AGENT>_FUNCTIONS_DIR` | `<AGENT>_TOOLS_DIR` | config/mod.rs, config/agent.rs |
| `supports_function_calling` | `supports_tool_use` | client/model.rs, models.yaml, config.example.yaml |
| `run_llm_function` | `run_llm_tool` | function.rs, config/agent.rs |
| `eval_tool_calls` | keep as-is | already correct |
| `parse_tools` | keep as-is | already correct |
| `init_from_mcp` | keep as-is | already correct |

## Implementation Units

- [ ] **Unit 1: Rename `src/function.rs` → `src/tool.rs` and update `mod` declaration**

  **Goal:** Move the file and fix the module reference in `main.rs` or `lib.rs`.

  **Files:**
  - Rename: `src/function.rs` → `src/tool.rs`
  - Modify: `src/main.rs` (mod declaration)
  - Modify: every file that does `use crate::function::` → `use crate::tool::`

  **Approach:** `git mv` the file, then fix all `mod function` and `use crate::function` references.

  **Patterns to follow:** Standard Rust module rename.

  **Verification:** `cargo check` passes.

- [ ] **Unit 2: Rename core types in `src/tool.rs`**

  **Goal:** Rename `FunctionDeclaration` → `ToolDeclaration`, `Functions` → `Tools`, `run_llm_function` → `run_llm_tool`.

  **Files:**
  - Modify: `src/tool.rs`
  - Modify: all importers (client/common.rs, mcp/convert.rs, mcp/client.rs, serve.rs, config/mod.rs, config/agent.rs, config/input.rs)

  **Approach:** LSP rename for each symbol, or global find-replace with verification.

  **Verification:** `cargo check` passes.

- [ ] **Unit 3: Rename config constants and path helpers in `config/mod.rs`**

  **Goal:** Rename `FUNCTIONS_DIR_NAME` → `TOOLS_DIR_NAME`, `FUNCTIONS_FILE_NAME` → `TOOLS_FILE_NAME`, `FUNCTIONS_BIN_DIR_NAME` → `TOOLS_BIN_DIR_NAME`, and all associated methods (`functions_dir` → `tools_dir`, etc.).

  **Files:**
  - Modify: `src/config/mod.rs`
  - Modify: `src/config/agent.rs` (calls `Config::agent_functions_dir`, etc.)
  - Modify: `src/function.rs` / `src/tool.rs` (calls `Config::functions_bin_dir`)

  **Approach:** Rename constants and methods, fix all call sites.

  **Verification:** `cargo check` passes.

- [ ] **Unit 4: Rename `function_calling` config field → `tool_use`**

  **Goal:** Rename the `function_calling` bool field in `Config` struct to `tool_use`, update serde, env var loading (`HARNX_FUNCTION_CALLING` → `HARNX_TOOL_USE`), and all references.

  **Files:**
  - Modify: `src/config/mod.rs` (struct field, `Default`, `load_envs`, `select_functions` → `select_tools`, info display, completion)
  - Modify: `src/config/role.rs` (if referenced)
  - Modify: `src/config/agent.rs` (if referenced)

  **Approach:** Rename field, fix all usages, update the env var name string.

  **Verification:** `cargo check` passes.

- [ ] **Unit 5: Rename `functions` field on Config → `tools`**

  **Goal:** Rename the `pub functions: Functions` field to `pub tools: Tools`.

  **Files:**
  - Modify: `src/config/mod.rs`
  - Modify: `src/serve.rs`
  - Modify: `src/config/agent.rs`

  **Verification:** `cargo check` passes.

- [ ] **Unit 6: Rename `supports_function_calling` → `supports_tool_use` in model code and models.yaml**

  **Goal:** Rename the field in `ModelData` and update all ~160 occurrences in `models.yaml`.

  **Files:**
  - Modify: `src/client/model.rs`
  - Modify: `models.yaml` (global replace)
  - Modify: `config.example.yaml`

  **Approach:** Rename struct field in model.rs, then global replace in YAML files.

  **Verification:** `cargo check` passes, `cargo test` passes.

- [ ] **Unit 7: Update example configs and docs**

  **Goal:** Update `config.example.yaml`, `config.agent.example.yaml`, `README.md`, and any role/asset markdown that mentions "function calling" in the LLM-tool sense.

  **Files:**
  - Modify: `config.example.yaml`
  - Modify: `config.agent.example.yaml`
  - Modify: `README.md`
  - Modify: `assets/roles/%code%.md` (if applicable)

  **Approach:** Replace "function calling" → "tool use", "function_calling" → "tool_use", "functions" → "tools" in prose/comments. Be careful not to rename references to programming functions.

  **Verification:** Read through changes for correctness.

- [ ] **Unit 8: Final verification**

  **Goal:** Full test suite, clippy, and format check pass.

  **Verification:**
  - `cargo test --all` passes
  - `cargo clippy --all --all-targets -- -D warnings` passes
  - `cargo fmt --all --check` passes

## System-Wide Impact

- **Config files:** Users must update `function_calling` → `tool_use` in their `config.yaml`
- **Env vars:** `HARNX_FUNCTION_CALLING` → `HARNX_TOOL_USE`, `HARNX_FUNCTIONS_DIR` → `HARNX_TOOLS_DIR`
- **On-disk directories:** `<config_dir>/functions/` → `<config_dir>/tools/`, `functions.json` → `tools.json`
- **Agent dirs:** `<config_dir>/functions/agents/` → `<config_dir>/tools/agents/`
- **models.yaml:** `supports_function_calling` → `supports_tool_use` (fetched from remote sync URL too)

## Risks & Dependencies

- Users with existing configs will need to update field names (clean break decision)
- The `models.yaml` sync URL serves the old field name until the rename is merged and deployed
- Agent function directories on disk won't auto-migrate; users move them manually

## Sources & References

- Related code: `src/function.rs`, `src/config/mod.rs`, `src/client/model.rs`
- Config: `config.example.yaml`, `models.yaml`
