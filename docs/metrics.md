# Prometheus Metrics

Harnx provides an opt-in, pull-based Prometheus `/metrics` HTTP endpoint across long-running workspace binaries.

## Overview

Metrics collection is **off by default**. When `--metrics-addr` (or `HARNX_METRICS_ADDR` where supported) is unset, no HTTP listener starts, no metrics recorder is installed, and process behavior remains unchanged.

When enabled, each binary runs a dedicated HTTP listener on the requested port and serves metrics at `/metrics`. This listener runs independently of the binary's main transport, whether the binary communicates via NATS, stdio, Hyper, Axum, or Hudsucker.

Prometheus metrics operate independently from OpenTelemetry distributed tracing (`docs/tracing.md`) and standard application logging (`HARNX_LOG_LEVEL`).

## Configuration & Environment Variables

You can enable metrics using either the CLI flag or an environment variable fallback:

- `--metrics-addr <ADDR>`: CLI flag available on most binaries. **Caveat:** `harnx-claude-compatible-hook-server` rejects `--metrics-addr` as an unknown argument due to its strict clap parser. Use `HARNX_METRICS_ADDR` instead. Accepts `IP:PORT` or `:PORT`. Passing a blank host (e.g. `--metrics-addr :8456`) binds `0.0.0.0`, allowing scrapers from other containers or Kubernetes pods to reach the endpoint. Passing `127.0.0.1:9109` restricts the listener to loopback.
- `HARNX_METRICS_ADDR`: Environment variable fallback honored by shared-entrypoint binaries: `harnx-bash-tools`, `harnx-fs-tools`, `harnx-grep-tools`, `harnx-time-server`, `harnx-plans-tools` (non-HTTP mode), `harnx-claude-compatible-hook-server`, `harnx-mcp-remote`, `harnx-mcp-bridge`, `harnx-mcp-time`, and `harnx-mcp-plans-github`. If both the CLI flag and environment variable are set, the CLI flag takes precedence.

## Binary Coverage

Metrics support is implemented across 15 long-running binaries:

- **Core runtime & proxies**: `harnx-serve`, `harnx-worker`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`
- **Tool & hook servers**: `harnx-bash-tools`, `harnx-fs-tools`, `harnx-grep-tools`, `harnx-plans-tools`, `harnx-time-server`, `harnx-claude-compatible-hook-server`
- **MCP bridges & servers**: `harnx-mcp-bridge`, `harnx-mcp-remote`, `harnx-mcp-time`, `harnx-mcp-plans-github`

**Out of scope (unchanged)**:
- `harnx` (interactive TUI/CLI)
- `harnx-pkg` (package manager)
- Short-lived sandbox helpers (`harnx-sandbox-exec`, `harnx-sandbox-run`)
- Utility binaries (`harnx_tty_probe`)

## Metric Families

All exported metrics use the `harnx_` prefix.

| Metric Name | Type | Labels | Description | Binaries |
|-------------|------|--------|-------------|----------|
| `harnx_llm_tokens_total` | Counter | `agent`, `client`, `model`, `type` | Chat-completion token count (`type` is `input`, `output`, or `cached`). | `harnx-worker` |
| `harnx_llm_cost_dollars` | Gauge | `agent`, `client`, `model` | Cumulative estimated LLM cost in USD. Monotonically increases over process lifetime. | `harnx-worker` |
| `harnx_http_requests_total` | Counter | `method`, `route`, `status` | HTTP request count. `route` uses template patterns or static names. | HTTP servers (`harnx-serve`, `aws-creds`, `k8s-creds`, `proxy-auth`, rmcp `--http` servers) |
| `harnx_http_request_duration_seconds` | Histogram | `method`, `route` | HTTP request latency histogram (buckets: 0.005s to 10s). | HTTP servers |
| `harnx_tool_calls_total` | Counter | `tool`, `status` | Tool execution count (`status` is `ok` or `error`). | Tool & MCP servers |
| `harnx_tool_call_duration_seconds` | Histogram | `tool` | Tool execution duration histogram. | Tool & MCP servers |

Histogram buckets for duration metrics use default boundaries: `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` seconds.

## Design & Label Details

- **`client` label semantics**: The `client` label matches the configured client name (`model.client_name()`). This may be a package-qualified alias like `mypkg/openai` rather than a canonical backend name.
- **Chat completions only**: Token usage and cost metrics apply exclusively to chat completions. Embeddings and reranker requests do not emit token metrics.
- **Cost metric mechanics**: `harnx_llm_cost_dollars` is exported as a gauge because the underlying metrics facade does not support floating-point counters. It increases monotonically per process and is emitted only when both input and output unit prices are configured for the model.
- **Cached token pricing**: Cost calculations currently exclude cached tokens. Cached token volume is tracked separately via `harnx_llm_tokens_total{type="cached"}`.
- **Cardinality protection**: Metrics omit `session_id` and raw dynamic URL paths to prevent cardinality explosion. HTTP route labels use matched path templates (such as `/token/{context}`) or fixed route identifiers (`proxy` for `harnx-proxy-auth`).

## Extending Metrics

### Adding LLM token/cost recording

Token and cost metrics are recorded once at the runtime retry wrapper (`harnx-runtime/src/client/retry.rs`), **not** at `ModelEvent::Usage` or `Final`. The event stream double-counts: tool-loop turns emit multiple `Usage` events, and `Final.usage` duplicates the terminal call. Recording at the retry seam ensures correct attribution across fallbacks, sub-agents, title generation, and compaction.

To add per-call LLM instrumentation (billing, cost attribution, audit), add it at the same seam.

### Adding tool dispatch instrumentation

`run_toolset_main` has two mutually exclusive dispatch paths:

- **NATS mode** → `invoke_uncached_tool` (shared entrypoint)
- **MCP stdio mode** → `McpToolsetAdapter::dispatch_call_tool` (calls `toolset.invoke` directly)

Any cross-cutting concern (metrics, tracing, auth) added at one seam does **not** automatically cover the other. rmcp `--http` servers use their own `ServerHandler::call_tool` method, a third seam. When adding instrumentation, check all relevant paths.

### Float accumulators

The `metrics` facade has no `f64` counter type. Cumulative floating-point values (dollar cost, other currency) must use a gauge. Name such gauges without the `_total` suffix (Prometheus reserves that for counters). See `harnx_llm_cost_dollars` for the pattern.

## Runnable Examples

### Worker Token and Cost Metrics

Start `harnx-worker` with a metrics listener on loopback port `9109`:

```bash
harnx-worker --metrics-addr 127.0.0.1:9109
```

After executing agent turns, fetch the metrics:

```bash
curl -s http://127.0.0.1:9109/metrics | grep harnx_llm_
```

Example output:

```text
# HELP harnx_llm_tokens_total Chat-completion token count
# TYPE harnx_llm_tokens_total counter
harnx_llm_tokens_total{agent="coding",client="openai",model="gpt-4o",type="input"} 1420
harnx_llm_tokens_total{agent="coding",client="openai",model="gpt-4o",type="output"} 385
harnx_llm_tokens_total{agent="coding",client="openai",model="gpt-4o",type="cached"} 512

# HELP harnx_llm_cost_dollars Cumulative estimated LLM cost in USD
# TYPE harnx_llm_cost_dollars gauge
harnx_llm_cost_dollars{agent="coding",client="openai",model="gpt-4o"} 0.0074
```

### HTTP Server Metrics

Start `harnx-serve` binding the metrics listener to all network interfaces on port `8456`:

```bash
harnx-serve --metrics-addr :8456
```

Send requests to the server, then query the endpoint:

```bash
curl -s http://127.0.0.1:8456/metrics | grep harnx_http_
```

Example output:

```text
# HELP harnx_http_requests_total HTTP request count
# TYPE harnx_http_requests_total counter
harnx_http_requests_total{method="GET",route="/v1/models",status="200"} 12

# HELP harnx_http_request_duration_seconds HTTP request latency histogram
# TYPE harnx_http_request_duration_seconds histogram
harnx_http_request_duration_seconds_bucket{method="GET",route="/v1/models",le="0.5"} 10
harnx_http_request_duration_seconds_bucket{method="GET",route="/v1/models",le="1"} 12
harnx_http_request_duration_seconds_sum{method="GET",route="/v1/models"} 4.82
harnx_http_request_duration_seconds_count{method="GET",route="/v1/models"} 12
```

## Follow-ups

- **Canonical provider label** ([#1592](https://github.com/dobesv/harnx/issues/1592)): Add a distinct `provider` label alongside `client` to reflect the underlying provider backend.
- **Cached token cost calculation** ([#1568](https://github.com/dobesv/harnx/issues/1568)): Incorporate cached token discount rates into `harnx_llm_cost_dollars`.
