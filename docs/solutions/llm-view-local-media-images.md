---
title: "LLM local image viewing via fs read tool"
date: 2026-06-11
category: "media"
problem_type: integration_issue
component: "harnx-engine"
root_cause: "fs read tool rejected binary; tool images never surfaced as vision input"
resolution_type: feature_implementation
severity: high
tags:
  - media
  - image
  - fs
  - mcp
  - tool-result
  - vision
  - gemini
  - camelCase
plan_ref: "harnx-175-llm-view-local-media"
last_updated: 2026-06-11
---

# LLM local image viewing via fs read tool

## Problem

The `fs` MCP `read` tool previously rejected binary files (triggering `is_binary_content` → `tool_error`), and MCP tool results flowed back as untyped `serde_json::Value` in `ToolResult.output`. Provider serializers embedded that output as JSON/string in tool-result blocks, meaning images were never surfaced as vision input even if a tool returned them.

## Solution

The "A-lite" approach adds a dedicated `content` field to `ToolResult` to carry image data while maintaining backward compatibility with existing session persistence and agent-switch detection logic.

### 1. Additive ToolResult content

`ToolResult` in `harnx-core` now includes an optional `content` field:

```rust
pub struct ToolResult {
    pub output: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<MessageContentPart>,
}
```

This ensures that existing saved sessions (which lack this field) load correctly, and `output` remains the source of truth for text-based analysis.

### 2. Pipeline: Extraction and Redaction

When a tool returns an rmcp `CallToolResult` containing image blocks, `harnx-engine` processes it at the boundary:

1. **Extraction**: `extract_image_parts` in `harnx-engine/src/media.rs` identifies `RawContent::Image` blocks and converts them to `MessageContentPart::ImageUrl` (base64 data URIs).
2. **Redaction**: `redact_image_data` replaces the massive base64 string in the `output` field with a lightweight placeholder: `<image: {mime}, {n} base64 chars>`. This prevents session logs and OpenAI-style tool text from bloating.
3. **Storage**: The full image data lives in `ToolResult.content` (a data URI). This is consumed by provider serializers **and** persisted in the session log (see "Persistence" below), so the image survives across turns and across interrupt/resume.

**Critical ordering**: Redaction occurs *before* the emit callback (`emit_tool_result_fn`) to ensure UI/log/event sinks receive the redacted output, not raw base64.

### 3. Per-Provider Implementation

Different LLM providers handle images in tool results via diverse API shapes:

- **Claude & Bedrock**: Support native image blocks directly inside the tool result content array.
- **OpenAI / Azure / openai_compatible**: The `tool` role only accepts strings. The engine emits the tool message with the redacted text placeholder, followed by a trailing `user` message containing the `image_url` parts.
- **Gemini / VertexAI**: Emits `functionResponse` parts first, followed by sibling `inlineData` parts in the same turn.

### 4. Vision Gating

To prevent API errors when using models without vision capabilities, `patch_messages` in `harnx-runtime` strips image parts from **all message origins** (tool results and user input) and logs a warning if `!model.supports_vision()`.

## Why This Works

- **Backward Compatibility**: `output` remains a `Value` and is preserved for tools and agent-switch logic.
- **Memory Efficiency**: The base64 is stored exactly once — in `content` (as a data URI). `output` keeps only a small redacted placeholder, so `output.to_string()` paths (OpenAI tool text, display) stay small and the bytes are never duplicated.
- **Provider Agnostic**: The engine handles the extraction, and specialized serializers adapt the output to the specific requirements of each LLM provider's API.

## Key Decisions

- **Images Only**: Support is limited to PNG, JPG, GIF, and WebP. Video and audio are deferred.
- **Size Cap**: The `fs` read tool enforces a ~5MB cap per image. Oversized images return a text error instead of a vision block.
- **No Resizing**: Images are passed through exactly as read from disk; no compression or resizing is performed by the engine.
- **Engine-Core Separation**: Core remains RMCP-agnostic. All RMCP-specific parsing logic lives in `harnx-engine` and `harnx-mcp-fs`.

## Persistence

Tool-returned images are persisted in the session log, exactly like user-attached (`.attach`) images — the data URI is inlined in the `tool_results` entry. Mechanically: `harnx_core::session::ToolOutput` carries an additive `content: Vec<MessageContentPart>` field (same `#[serde(default, skip_serializing_if = "Vec::is_empty")]` pattern as `ToolResult.content`); `add_tool_results` copies `content` into both the in-memory message and the persisted log entry; and `assemble_tool_message` rehydrates it on load. As a result the image survives across turns in a live session **and** across interrupt/resume. `output` stays redacted (placeholder only), so the bytes are stored once.

## Known Limitations

- **TUI Rendering**: The terminal UI does not yet render the images returned by tools.

## Gotchas / Lessons Learned

### Gemini/VertexAI REST API Requires camelCase for Image Parts

The Gemini and Vertex AI `generateContent` REST API requires **camelCase** JSON keys for image inline-data parts:

- `inlineData` (not `inline_data`)
- `mimeType` (not `mime_type`)

**Trap**: Using snake_case keys (`inline_data`, `mime_type`) results in **silent failure** — the fields are treated as unknown and dropped by Google's API. The image never reaches the model, and no error is returned.

This bug existed in the pre-implementation codebase for user-input images and was replicated in the new tool-result image path during initial implementation. Fixed by updating both request-body serializer sites in `vertexai.rs` to use camelCase.

Verified against Google's official REST API documentation: [`inlineData` must use camelCase](https://cloud.google.com/vertex-ai/docs/reference/rest/v1/GenerateContentRequest#inlineData).

### Redaction Must Precede Emit Callback

The redaction step (`redact_image_data`) must run **before** the tool-result emit callback (`emit_tool_result_fn`). If redaction runs after emit, raw base64 payloads leak to UI/log/event sinks. The implementation now extracts images and redacts `output` in the correct sequence before invoking the emit callback.

### Tool Affordance: Describe the Image Capability or the Model Won't Use It

Returning images from the `read` tool is necessary but **not sufficient**. The model decides whether to call a tool from its **description**, not its implementation. With `read` described only as "Read a text file...", agents (observed with Claude Opus 4.8) would refuse or skip reading an image path entirely — the vision pipeline never even ran. User-attached images (`.attach`, which flow through `MessageContent::Array`) worked fine, which isolated the problem to tool discoverability.

Fix: keep overloading the normal filesystem `read` tool (consistent with Claude Code, opencode, Cline, Aider, Cursor — none expose a dedicated media tool) and **advertise image support in the model-facing metadata**: the tool description, the `path` parameter description, and the server instructions all state that `read` returns local image files (PNG/JPEG/GIF/WebP) as viewable content. A dedicated `read_media`/`view_image` tool was considered and rejected as inconsistent with prevailing harness conventions.

### The Runtime Drops `content` Unless Every Hand-Off Copies It

Populating `ToolResult.content` in the engine is not enough — the image is lost unless **every** layer that reconstructs a tool-result message also carries `content`. The original implementation missed `add_tool_results` in `harnx-runtime`, which rebuilds the in-memory session message copying only `output` and `switch_agent`. Result: the image was dropped on the **same turn** (not just on resume, as originally assumed), and `build_messages` sent empty content to the provider → the model hallucinated the image. The unit tests passed because they exercised the engine and provider serializers in isolation with synthetic `content`; none ran the real `add_tool_results → build_messages` path. **Lesson**: for a field that threads through multiple representations (live message, persisted log, rehydrated message, wire format), add an end-to-end integration test through the real assembly path, not just isolated unit tests of each stage.

### Diagnosing Model Capability: `.info model`

When images silently fail to reach a vision model, the usual cause is model resolution: an id that doesn't match the catalog falls back to `Model::new(...)` with `supports_vision = false`, and vision gating then strips the image. The `.info model` command surfaces the active model's `supports_vision`, `supports_tool_use`, pricing, token limits, and crucially its **source** (`catalog` vs `fallback/default`) so this footgun is visible. A `fallback/default` source is the tell that capabilities (including vision) may be wrong because the configured model id didn't match any catalog entry.

## File Pointers

- `crates/harnx-mcp-fs/src/server/handlers.rs`: Handler for `read` tool with `detect_image_mime` and 5MB cap.
- `crates/harnx-engine/src/media.rs`: `extract_image_parts` and `redact_image_data` helpers.
- `crates/harnx-engine/src/tool.rs`: Integration of extraction/redaction into the tool execution flow (extract → redact → emit).
- `crates/harnx-client/src/claude.rs`: Native image blocks in tool results.
- `crates/harnx-client/src/bedrock.rs`: Native image blocks in `toolResult.content`.
- `crates/harnx-client/src/openai.rs`: Follow-up user message pattern for images.
- `crates/harnx-client/src/vertexai.rs`: Sibling `inlineData` parts with camelCase keys.
- `crates/harnx-runtime/src/client/message.rs`: `patch_messages` vision gating for both tool results and user input.
- `crates/harnx-core/src/tool.rs`: Additive `content` field on `ToolResult`.
- `crates/harnx-core/src/model.rs`: `supports_vision()` flag.
- `crates/harnx-core/src/session.rs` & `crates/harnx-runtime/src/config/session.rs`: `ToolOutput.content` persistence; `add_tool_results` / `assemble_tool_message` carry image data URIs across turns and resume.
- `crates/harnx-runtime/src/commands.rs`: `.info model` diagnostic (model id, client, source catalog/fallback, vision, tool-use, pricing, token limits).
