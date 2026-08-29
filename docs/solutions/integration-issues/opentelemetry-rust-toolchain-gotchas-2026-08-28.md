---
title: "OpenTelemetry Rust toolchain gotchas — async batch processor and rmcp `_meta` lifecycle"
date: 2026-08-28
last_verified: 2026-08-28
component: "harnx-telemetry, harnx-toolset-server"
problem_type: integration_issue
status: current
anchors:
  - crates/harnx-telemetry/Cargo.toml:opentelemetry_sdk-features
  - crates/harnx-telemetry/src/lib.rs:131
  - crates/harnx-toolset-server/src/lib.rs:757
tags:
  - opentelemetry
  - rust
  - tokio
  - rmcp
  - tracing
  - batch-processor
plan_ref: "opentelemetry-tracing"
---

# OpenTelemetry Rust Toolchain Gotchas

## When this is relevant

Adding or updating OpenTelemetry tracing in a Rust project using Tokio and/or rmcp. Symptoms include "there is no reactor running" panics during span export, or trace context not propagating through MCP tool servers.

## Durable lesson

### 1. BatchSpanProcessor requires async runtime

The default `opentelemetry_sdk::trace::BatchSpanProcessor` polls the exporter on a plain OS thread. The OTLP HTTP exporter uses `reqwest`, which requires a Tokio runtime. Without the async variant, export triggers "there is no reactor running" panic.

**Fix**: Use the experimental async-runtime batch processor:

```rust
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

let batch = BatchSpanProcessor::builder(exporter, opentelemetry_sdk::runtime::Tokio).build();
```

Required `opentelemetry_sdk` features:
- `experimental_trace_batch_span_processor_with_async_runtime`
- `rt-tokio`

### 2. rmcp moves `_meta` before handler invocation

In rmcp 3.1.4, `CallToolRequestParams._meta` is moved into `RequestContext::meta` before `ServerHandler::call_tool` is invoked. Server-side trace extraction must read from `context.meta`, not `request.meta` (which is `None` at serve time).

**Correct**:
```rust
async fn call_tool(
    &self,
    request: CallToolRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResponse, ErrorData> {
    let parent_cx = extract_context_from_mcp_meta(&context.meta);
    // ...
}
```

**Incorrect** (returns empty context):
```rust
let parent_cx = extract_context_from_mcp(&request);  // request.meta is None!
```

### 3. Aligned OTel version set for Rust 1.97.1

Under this repo's pinned toolchain, the following version set resolves and compiles:

| Crate | Version |
|-------|---------|
| `opentelemetry` | 0.32 |
| `opentelemetry_sdk` | 0.32 |
| `opentelemetry-otlp` | 0.32 |
| `opentelemetry-semantic-conventions` | 0.32 |
| `opentelemetry-http` | 0.32 |
| `opentelemetry-proto` | 0.32 |
| `tracing-opentelemetry` | 0.33 |
| `prost` | 0.14 |

Uses current builder API: `SpanExporter::builder().with_http()`, `SdkTracerProvider::builder()`. NOT the removed `new_pipeline()`/`.tonic()` APIs.

`opentelemetry-otlp` features: `["http-proto", "reqwest-client", "trace"]`. Keeps rustls-only (no native-tls), satisfying workspace TLS constraints.

## Evidence and current anchors

- `crates/harnx-telemetry/Cargo.toml`: `opentelemetry_sdk` features and version
- `crates/harnx-telemetry/src/lib.rs:131`: BatchSpanProcessor with `runtime::Tokio`
- `crates/harnx-toolset-server/src/lib.rs:757`: `extract_context_from_mcp_meta(&context.meta)`
- `crates/harnx-telemetry/src/propagate.rs`: MCP carrier implementation using rmcp's `RequestParamsMeta` trait

## Failed approaches or trade-offs

- Using default `BatchSpanProcessor` — panics at export time with async HTTP exporter
- Extracting from `request.meta` in rmcp handler — always `None` because rmcp moves it before handler runs
- Using tonic/gRPC OTLP transport — requires prost/tonic dependency tree; http/protobuf is simpler for test collector
- Enabling `native-tls` feature — violates workspace rustls-only constraint
