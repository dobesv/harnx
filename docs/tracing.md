# OpenTelemetry Tracing

Harnx supports OpenTelemetry distributed tracing across agent turns, LLM API calls, and cross-process tool executions.

## Overview

Tracing is **off by default**. When no OTLP endpoint environment variable is set, tracing is fully inert: zero exporter overhead, no network traffic, and no changes to standard terminal or log output.

When enabled, Harnx exports spans over OTLP HTTP (`http/protobuf`) to a collector such as Jaeger or the OpenTelemetry Collector. gRPC transport is not supported.

Tracing is orthogonal to Harnx's existing logging system (`HARNX_LOG_LEVEL` and `HarnxLogger`). Terminal and log file output remain unchanged regardless of telemetry settings.

## Configuration & Environment Variables

Tracing is enabled by setting `OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. These OpenTelemetry environment variables describe the available configuration:

- `OTEL_EXPORTER_OTLP_ENDPOINT`: Base URL of the OTLP HTTP collector (e.g. `http://localhost:4318`).
- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: Direct URL for trace export (e.g. `http://localhost:4318/v1/traces`). Overrides `OTEL_EXPORTER_OTLP_ENDPOINT`.
- `OTEL_EXPORTER_OTLP_PROTOCOL`: Not required or read. The exporter always uses `http/protobuf`; other values such as `grpc` are neither honored nor rejected.
- `OTEL_SERVICE_NAME`: Service identifier for the root process (default `harnx`). Child tool servers set their own service name (e.g. `harnx-fs-tools-server`).
- `OTEL_RESOURCE_ATTRIBUTES`: Key-value resource attributes added to traces (e.g. `service.version=0.30.0,environment=production`).
- `OTEL_EXPORTER_OTLP_HEADERS`: Key-value HTTP header pairs for authentication or routing.
- `OTEL_TRACES_SAMPLER`: Sampling strategy (e.g. `always_on`, `always_off`, `traceidratio`, `parentbased_always_on`).
- `OTEL_TRACES_SAMPLER_ARG`: Argument for sampler ratio (e.g. `0.1` for 10% sampling).

Spawned child processes (such as tool servers and sub-agents) inherit `OTEL_*` environment variables automatically, allowing downstream components to self-configure. `OTEL_SERVICE_NAME` is not forced on child processes so each tool server names itself independently (e.g., `harnx-bash-tools-server`).

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

Successful non-streaming spans also record token usage when the provider returns it:

- `gen_ai.usage.input_tokens`: Prompt input token count.
- `gen_ai.usage.output_tokens`: Completion output token count.
- `harnx.gen_ai.usage.cached_tokens`: Prompt cache read tokens (included only when returned by the provider).

Streaming usage is available only after the `llm_request` span closes, so streaming spans do not include these token attributes.

## Cross-Process Context Propagation

Harnx propagates W3C `traceparent` context across process boundaries to construct a single connected trace:

- **HTTP Requests**: W3C `traceparent` headers are injected into outbound HTTP requests during LLM API calls.
- **NATS Transport**: `traceparent` is injected into NATS message headers for agent tool calls and sub-agent handoffs.
- **MCP Bridge**: `traceparent` is injected into the `_meta` object (`CallToolRequestParams._meta`) when invoking tools through `harnx-mcp-bridge`. Native Harnx tool servers running in stdio mode extract context from `request.meta`.

## Binary Coverage Tiers

Tracing support across workspace binaries falls into four tiers:

- **Full instrumentation** (tracer initialization and custom spans): `harnx`, `harnx-worker`, `harnx-serve`, `harnx-mcp-bridge`.
- **Tool servers** (tracer initialization and inbound `tool_exec` server spans): all toolset-server binaries using the shared bootstrap (`harnx-fs-tools`, `harnx-bash-tools`, `harnx-plans-tools`, etc.).
- **Init-only** (tracer initialization to export telemetry if `OTEL_*` is set, without custom spans): `harnx-pkg`, `harnx-claude-compatible-hook-server`, `harnx-mcp-time`, `harnx-mcp-plans-github`, `harnx-mcp-remote`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`.
- **Out of scope (untraced)**:
  - `harnx-sandbox-exec`: Short-lived command execution helper without a persistent Tokio runtime required by the batch log exporter.
  - `harnx_tty_probe`: Brief terminal probe utility without a Tokio runtime.
  - `harnx-sandbox-run`: Executed on request-scoped invocation paths without a persistent Tokio runtime.

## Non-Goals & Limitations

- **No cost attribution**: Token usage numbers are recorded, but Harnx has no pricing model and does not calculate financial cost.
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

The protocol setting shown above is optional and ignored by Harnx. It documents the protocol expected by this example's collector configuration.

The resulting trace in your collector UI shows a connected hierarchy:
`agent_turn` → `llm_request` (with token attributes on non-streaming calls) → `tool_call` → `harnx-fs-tools-server` `tool_exec`.

## Follow-ups

- **Scaffold instrumentation**: `Engine::run_turn` in `harnx-engine` is inactive scaffold code and will be instrumented when activated.
- **Short-lived helpers**: Tracing can be extended to short-lived helper binaries (`harnx-sandbox-exec`, `harnx-sandbox-run`, `harnx_tty_probe`) if persistent runtime wrappers or custom flush logic are added in the future.
