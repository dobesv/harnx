---
title: "Attachment upload-by-reference"
date: 2026-06-18
category: "performance"
problem_type: architectural_refactor
component: "harnx-client"
root_cause: "High memory/bandwidth usage from re-uploading base64 attachments every turn"
resolution_type: feature_implementation
severity: medium
tags:
  - attachments
  - upload
  - cache
  - gemini
  - claude
  - openai
  - performance
plan_ref: "harnx-attachment-upload-encoders"
last_updated: 2026-06-18
---

# Attachment upload-by-reference

## Problem

Historically, `harnx` treated all image attachments as ephemeral base64 blobs. Every turn in a session, the `harnx-runtime` would re-read attachment files from disk, re-encode them to base64, and re-send them in the JSON request body. 

For long-running sessions with multiple images, this led to:
1. **Bandwidth waste**: Uploading the same megabyte-scale images repeatedly.
2. **Memory spikes**: The runtime and client had to buffer massive JSON strings containing redundant base64 data.
3. **Latency**: Each turn was gated by the time taken to re-upload all historical images.

## Design

The goal was to upload an attachment exactly once to a provider's "Files API," cache the remote reference ID, and use that reference in subsequent turns.

### 1. Shared Types & Crate Graph

To avoid circular dependencies (`harnx-runtime` -> `harnx-client` -> `harnx-runtime`), the core data structures were moved to `harnx-core/src/attachments.rs`:

- **`ExpandedAttachment`**: A structured enum (`RemoteRef { ref_id, mime_type, expires_at }` or `DataUri { data, mime_type }`). This allows the client to decide whether to emit a file reference or fall back to base64 while remaining inspectable for logs and tests.
- **`CachedRef`**: Stores the remote ID and expiry for a given content-addressed `cid:`.
- **`CID_PREFIX`**: The standard `cid:` string prefix for local attachment references.

### 2. Process-Global Attachment Cache

A critical discovery was that `harnx` clients are often re-created per-turn (e.g., during retries or agent switches). An instance-level cache would be lost immediately.

The solution is a **process-global, provider-scoped cache** managed via `shared_attachment_cache(scope)`. 
- Scoping ensures that a Gemini `file_id` is never accidentally sent to Anthropic.
- The cache is in-memory only (no disk persistence) and uses poison-tolerant `Mutex` locks to survive panics in a shared context.

### 3. Hand-written Async Clients

The existing sync `prepare_chat_completions` methods (often macro-generated) could not support the `async` nature of file uploads. 

The `GeminiClient` and `ClaudeClient` were converted to hand-written `#[async_trait] impl Client` blocks. A new capability flag, `expands_attachments_internally()`, informs the runtime whether to skip the default base64 pre-pass and instead thread the `attachments_dir` down to the client via `ChatCompletionsData`.

### 4. Fallback Chain & Expansion Gating

The expansion logic follows a strict fallback chain inside each client's async request build:
1. **Cache Hit**: Use existing valid remote reference.
2. **Upload**: Attempt to upload the local file to the provider API.
3. **Base64**: Fall back to base64 if upload fails, the provider is unsupported (e.g., Vertex AI, Bedrock), or the file is missing.

This ensures that "upload-by-reference" is a transparent optimization—it never breaks the correctness of the turn.

## Provider Details

### Gemini (Developer API)
- **Mechanism**: 2-step resumable upload (`/upload/v1beta/files` start -> `x-goog-upload-url` -> finalize).
- **Wire Shape**: `fileData { mimeType, fileUri }`.
- **Expiry**: `expirationTime` is optional in the response. The implementation falls back to `uploaded_at + 47h`.
- **Constraint**: Vertex AI does NOT support the Files API; it is restricted to the Gemini Developer API.

### Anthropic (Claude)
- **Mechanism**: Single-step multipart `POST /v1/files`.
- **Wire Shape**: `source { type: "file", file_id: "file_..." }`.
- **Expiry**: Anthropic files do not expire; `expires_at` is set to `None`.
- **Constraint**: Requires the `anthropic-beta: files-api-2025-04-14` header on both the upload and the `/messages` request.

### OpenAI
- **Decision**: **Base64-only.** 
- **Reasoning**: The OpenAI Chat Completions API does not support referencing images by `file_id` (this is an Assistants-only or Responses-only feature). Referencing via `image_url` requires a public URL or data URI.
- **Outcome**: `OpenAIClient` remains on the base64 path to avoid unnecessary infrastructure (public hosting).

## Reliability & Testing

### Timeout Fallback
A 30-second timeout was added to all upload requests. If a provider's File API stalls, the client will time out and immediately fall back to sending base64 data, preventing a network hang from blocking the user's turn.

### In-Test Hyper Mocking
To test this without real network calls, a `hyper`-based mock server was implemented in `crates/harnx-client/tests/`. 
- **Upload Count Validation**: The mock tracks the number of `POST` calls to the upload endpoints.
- **State Verification**: Tests assert that the first turn performs an upload while the second turn (using the same `cid:`) performs zero uploads and uses the cached reference.

## Gotchas / Lessons Learned

- **Case Sensitivity**: Gemini/Vertex requires **camelCase** (`fileData`, `mimeType`). Using `file_data` results in a silent failure where the image is simply ignored by Google's API.
- **Tool Results**: Attachment expansion must traverse both top-level message parts AND `tool_result.content`. Missing the tool-result path caused images returned by tools to break in resumed sessions.
- **Crate Cycles**: Placing shared types in `harnx-core` is the only way to allow both `harnx-runtime` (which knows about files) and `harnx-client` (which knows about APIs) to share the same attachment schema.
- **HTTP/1 Host Header**: When building a mock server, do not rely on `req.uri()` to reconstruct absolute URLs for the Gemini "finalize" header; `req.uri()` is often in origin-form (relative) for HTTP/1. Use the `Host` header instead.
- **Block On**: Never use `block_on` inside the sync `prepare` methods of a client running on a Tokio worker. It will panic. Move all resolution logic into the async `chat_completions_inner` path.

## File Pointers

- `crates/harnx-core/src/attachments.rs`: Shared types, global cache, and `collect_cid_refs`.
- `crates/harnx-client/src/gemini_upload.rs`: Gemini 2-step resumable upload implementation.
- `crates/harnx-client/src/claude_upload.rs`: Anthropic multipart upload implementation.
- `crates/harnx-client/src/gemini.rs` & `crates/harnx-client/src/claude.rs`: Async client implementations and request body builders.
- `crates/harnx-runtime/src/config/session.rs`: Capability-aware attachment expansion pre-pass.
