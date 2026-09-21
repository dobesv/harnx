# OpenTelemetry Tracing

Harnx supports OpenTelemetry distributed tracing across agent turns, LLM API calls, and cross-process tool executions.

## Overview

Tracing is **off by default**. When no OTLP endpoint environment variable is set, tracing is fully inert: zero exporter overhead, no network traffic, and no changes to standard terminal or log output.

When enabled, Harnx exports spans over OTLP using either HTTP (`http/protobuf`) or gRPC (`grpc`) to a collector such as Jaeger or the OpenTelemetry Collector. The transport is selected via `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` or `OTEL_EXPORTER_OTLP_PROTOCOL` (defaulting to `http/protobuf`).

Tracing is orthogonal to Harnx's existing logging system (`HARNX_LOG_LEVEL` and `HarnxLogger`). Terminal and log file output remain unchanged regardless of telemetry settings.

## Configuration & Environment Variables

Tracing is enabled by setting `OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. These OpenTelemetry environment variables describe the available configuration:

- `OTEL_EXPORTER_OTLP_ENDPOINT`: Base collector URL (`http://localhost:4318` for HTTP, `http://localhost:4317` for gRPC).
- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: Direct URL for trace export (e.g. `http://localhost:4318/v1/traces` or `http://localhost:4317`). Overrides `OTEL_EXPORTER_OTLP_ENDPOINT` and is used as-is without appending a signal path.
- `OTEL_EXPORTER_OTLP_PROTOCOL`: Transport protocol for OTLP export. Valid values are `http/protobuf` (default) and `grpc`. Unrecognized or unsupported values log a warning and disable trace export for that run.
- `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`: Signal-specific protocol override. Takes precedence over `OTEL_EXPORTER_OTLP_PROTOCOL`.
- `OTEL_SERVICE_NAME`: Service identifier for the root process (default `harnx`). Child tool servers set their own service name (e.g. `harnx-fs-tools-server`).
- `OTEL_RESOURCE_ATTRIBUTES`: Key-value resource attributes added to traces (e.g. `service.version=0.30.0,environment=production`).
- `OTEL_EXPORTER_OTLP_HEADERS`: Key-value header pairs for authentication or routing, sent as HTTP headers or gRPC metadata.
- `OTEL_EXPORTER_OTLP_TRACES_HEADERS`: Signal-specific header pairs. Takes precedence over `OTEL_EXPORTER_OTLP_HEADERS`.
- `OTEL_TRACES_SAMPLER`: Sampling strategy (e.g. `always_on`, `always_off`, `traceidratio`, `parentbased_always_on`).
- `OTEL_TRACES_SAMPLER_ARG`: Argument for sampler ratio (e.g. `0.1` for 10% sampling).

Spawned child processes (such as tool servers and sub-agents) inherit `OTEL_*` environment variables automatically, allowing downstream components to self-configure. `OTEL_SERVICE_NAME` is not forced on child processes so each tool server names itself independently (e.g., `harnx-bash-tools-server`).

### Protocol Selection

Harnx supports two OTLP export protocols:
- `http/protobuf`: Exports spans over HTTP POST using protobuf payloads.
- `grpc`: Exports spans over gRPC using HTTP/2.

Protocol selection follows these rules:
1. `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` takes precedence if set.
2. `OTEL_EXPORTER_OTLP_PROTOCOL` is used if the traces-specific variable is unset.
3. If neither variable is set, Harnx defaults to `http/protobuf`.

If an unrecognized or unsupported protocol value is provided (such as `http/json` or an invalid string), Harnx logs a warning and disables trace export for that run. The telemetry initialization succeeds as an inert no-op, preventing crashes or aborted commands.

> **Divergence from OpenTelemetry specification**: The OpenTelemetry specification states that an unrecognized protocol value should log a warning and fall back to the default transport (`http/protobuf`). Harnx deliberately disables trace export instead. Falling back to an unintended transport could send traffic to an incompatible collector endpoint or silently mask configuration errors.

### Endpoint and Port Resolution

Endpoints resolve differently based on the chosen transport:

- **HTTP (`http/protobuf`)**:
  - The generic `OTEL_EXPORTER_OTLP_ENDPOINT` acts as a base URL (default: `http://localhost:4318`). Harnx appends `/v1/traces` to base endpoints.
  - The signal-specific `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (e.g. `http://localhost:4318/v1/traces`) is used as-is without appending any path.
- **gRPC (`grpc`)**:
  - The generic `OTEL_EXPORTER_OTLP_ENDPOINT` acts as a base URL (default: `http://localhost:4317`). No path is appended.
  - The signal-specific `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (e.g. `http://localhost:4317`) is used as-is without appending any path.

A base endpoint such as `http://collector:4318` (for HTTP) or `http://collector:4317` (for gRPC) sends spans to the expected port for that transport.

### TLS and Credential Protection

- **gRPC TLS**: Endpoints using `https://` use server TLS verified against bundled Mozilla root certificates (`webpki-roots`). Cleartext endpoints (`http://`) connect using HTTP/2 prior knowledge (h2c). Client certificate authentication (mTLS) is not supported.
- **Credential Stripping on Cleartext Networks**: When exporting to a cleartext (`http://`) endpoint on a non-loopback host (any host other than `localhost`, `127.0.0.0/8`, or `::1`), Harnx logs a warning and strips credential headers (`OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_EXPORTER_OTLP_TRACES_HEADERS`). This protection applies to both HTTP headers and gRPC metadata to prevent leaking secrets over unencrypted networks. Configure an `https://` endpoint to send credentials to a remote collector. Loopback destinations allow cleartext credentials for local development.

## Spans & Attributes

Harnx emits five main span types across an execution trace:

- `agent_turn` (Kind: `INTERNAL`): Root span covering a single agent turn.
- `llm_request` (Kind: `CLIENT`): Span covering an LLM API request and response cycle.
- `tool_call` (Kind: `CLIENT`): Agent-side span covering a tool execution request.
- `tool_exec` (Kind: `SERVER`): Server-side span emitted by a tool server during tool execution.
- `agent_activation` (Kind: `CONSUMER`): Worker-side span when a sub-agent turn is picked up over NATS.

### Token Attributes

Each `llm_request` span records LLM model metadata:

- `gen_ai.system`: LLM provider identifier (e.g. `openai`, `anthropic`, `gemini`).
- `gen_ai.request.model`: Active model name (e.g. `gpt-4o`, `claude-3-5-sonnet-20241022`).

Successful non-streaming spans also record token usage and cost attributes when available:

- `gen_ai.usage.input_tokens`: Total prompt input token count (includes cached tokens).
- `gen_ai.usage.output_tokens`: Completion output token count.
- `gen_ai.usage.cache_read.input_tokens`: Prompt cache read token count.
- `gen_ai.usage.cache_write.input_tokens`: Prompt cache creation/write token count.
- `harnx.gen_ai.usage.cached_tokens`: Legacy alias for prompt cache read tokens (retained for backward compatibility).
- `harnx.gen_ai.cost.usd`: Estimated dollar cost for the request (f64, present only when required model pricing is available).

Streaming usage is available only after the `llm_request` span closes, so streaming spans do not include these token and cost attributes.

## Cross-Process Context Propagation

Harnx propagates W3C `traceparent` context across process boundaries to construct a single connected trace:

- **HTTP Requests**: W3C `traceparent` headers are injected into outbound HTTP requests during LLM API calls.
- **NATS Transport**: `traceparent` is injected into NATS message headers for agent tool calls and sub-agent handoffs.
- **MCP Bridge**: `traceparent` is injected into the `_meta` object (`CallToolRequestParams._meta`) when invoking tools through `harnx-mcp-bridge`. Native Harnx tool servers running in stdio mode extract context from `request.meta`.

## Binary Coverage Tiers

Tracing support across workspace binaries falls into four tiers:

- **Full instrumentation** (tracer initialization and custom spans): `harnx`, `harnx-worker`, `harnx-serve`, `harnx-mcp-bridge`.
- **Tool servers** (tracer initialization and inbound `tool_exec` server spans): all toolset-server binaries using the shared bootstrap (`harnx-fs-tools`, `harnx-bash-tools`, `harnx-plans-tools`, etc.).
- **Init-only** (tracer initialization to export telemetry if `OTEL_*` is set, without custom spans): `harnx-pkg`, `harnx-claude-compatible-hook-server`, `harnx-mcp-remote`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`.
- **Out of scope (untraced)**:
  - `harnx-sandbox-exec`: Short-lived command execution helper without a persistent Tokio runtime required by the batch log exporter.
  - `harnx_tty_probe`: Brief terminal probe utility without a Tokio runtime.
  - `harnx-sandbox-run`: Executed on request-scoped invocation paths without a persistent Tokio runtime.

## Non-Goals & Limitations

- **Span cost attribution**: Non-streaming `llm_request` spans record estimated per-call cost via the custom `harnx.gen_ai.cost.usd` attribute when required model pricing is configured (omitted when pricing is incomplete; OpenTelemetry semantic conventions currently lack a standard cost attribute, per semconv-genai #101). Cumulative process cost and token metrics remain available via the Prometheus endpoint (see [Prometheus Metrics](metrics.md)).
- **Third-party MCP servers**: External MCP servers that do not process rmcp `_meta` context will complete tool requests normally, but will not attach downstream child spans. The trace degrades gracefully by ending at the `harnx-mcp-bridge` boundary.

## Runnable Example

To export traces to a local collector (such as Jaeger or the OpenTelemetry Collector) listening for OTLP HTTP on port 4318:

1. Start an OTLP-compatible collector on `http://localhost:4318`.
2. Run Harnx with tracing enabled:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_SERVICE_NAME=harnx

harnx prompt "list the files in the current directory"
```

The protocol setting defaults to `http/protobuf` when unset. To export over gRPC instead, set `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` and point `OTEL_EXPORTER_OTLP_ENDPOINT` to the gRPC collector port (4317):

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
export OTEL_SERVICE_NAME=harnx

harnx prompt "list the files in the current directory"
```

The resulting trace in your collector UI shows a connected hierarchy:
`agent_turn` → `llm_request` (with token attributes on non-streaming calls) → `tool_call` → `harnx-fs-tools-server` `tool_exec`.

## Follow-ups

- **Scaffold instrumentation**: `Engine::run_turn` in `harnx-engine` is inactive scaffold code and will be instrumented when activated.
- **Short-lived helpers**: Tracing can be extended to short-lived helper binaries (`harnx-sandbox-exec`, `harnx-sandbox-run`, `harnx_tty_probe`) if persistent runtime wrappers or custom flush logic are added in the future.
