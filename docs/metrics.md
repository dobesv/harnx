# Prometheus Metrics

Harnx provides an opt-in, pull-based Prometheus `/metrics` HTTP endpoint across long-running workspace binaries.

## Overview

Metrics collection is **off by default**. When `--metrics-addr` (or `HARNX_METRICS_ADDR` where supported) is unset, no HTTP listener starts, no metrics recorder is installed, and process behavior remains unchanged.

When enabled, each binary runs a dedicated HTTP listener on the requested port and serves metrics at `/metrics`. This listener runs independently of the binary's main transport, whether the binary communicates via NATS, stdio, Hyper, Axum, or Hudsucker.

Prometheus metrics operate independently from OpenTelemetry distributed tracing (`docs/tracing.md`) and standard application logging (`HARNX_LOG_LEVEL`).

## Configuration & Environment Variables

You can enable metrics using either the CLI flag or an environment variable fallback:

- `--metrics-addr <ADDR>`: CLI flag available on most binaries. **Caveat:** `harnx-claude-compatible-hook-server` rejects `--metrics-addr` as an unknown argument due to its strict clap parser. Use `HARNX_METRICS_ADDR` instead. Accepts `IP:PORT` or `:PORT`. Passing a blank host (e.g. `--metrics-addr :8456`) binds `0.0.0.0`, allowing scrapers from other containers or Kubernetes pods to reach the endpoint. Passing `127.0.0.1:9109` restricts the listener to loopback.
- `HARNX_METRICS_ADDR`: Environment variable fallback honored by shared-entrypoint binaries: `harnx-attachment-tools`, `harnx-bash-tools`, `harnx-fs-tools`, `harnx-exa-tools`, `harnx-fetch-tools`, `harnx-grep-tools`, `harnx-k8s-sandbox-tools`, `harnx-time-tools`, `harnx-plans-tools` (non-HTTP mode), `harnx-claude-compatible-hook-server`, `harnx-mcp-remote`, and `harnx-mcp-bridge`. If both the CLI flag and environment variable are set, the CLI flag takes precedence.

## Binary Coverage

Metrics support is implemented across 18 long-running binaries:

- **Core runtime & proxies**: `harnx-serve`, `harnx-worker`, `harnx-a2a-server`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`
- **Tool & hook servers**: `harnx-attachment-tools`, `harnx-bash-tools`, `harnx-fs-tools`, `harnx-exa-tools`, `harnx-fetch-tools`, `harnx-grep-tools`, `harnx-k8s-sandbox-tools`, `harnx-plans-tools`, `harnx-time-tools`, `harnx-claude-compatible-hook-server`
- **MCP bridges & servers**: `harnx-mcp-bridge`, `harnx-mcp-remote`

**Out of scope (unchanged)**:
- `harnx` (interactive TUI/CLI)
- `harnx-pkg` (package manager)
- Short-lived sandbox helpers (`harnx-sandbox-exec`, `harnx-sandbox-run`)
- Utility binaries (`harnx_tty_probe`)

## Metric Families

All exported metrics use the `harnx_` prefix.

| Metric Name | Type | Labels | Description | Binaries |
|-------------|------|--------|-------------|----------|
| `harnx_llm_tokens_total` | Counter | `agent`, `client`, `provider`, `model`, `type` | Chat-completion token count (`type` is `input`, `output`, `cache_read`, `cache_write`, or deprecated alias `cached`). | `harnx-worker` |
| `harnx_llm_cost_dollars` | Gauge | `agent`, `client`, `provider`, `model` | Cumulative estimated LLM cost in USD. Monotonically increases over process lifetime. | `harnx-worker` |
| `harnx_http_requests_total` | Counter | `method`, `route`, `status` | HTTP request count. `route` uses template patterns or static names. | HTTP servers (`harnx-serve`, `aws-creds`, `k8s-creds`, `proxy-auth`, rmcp `--mcp-http` servers) |
| `harnx_http_request_duration_seconds` | Histogram | `method`, `route` | HTTP request latency histogram (buckets: 0.005s to 10s). | HTTP servers |
| `harnx_tool_calls_total` | Counter | `tool`, `status` | Tool execution count (`status` is `ok` or `error`). | Tool & MCP servers |
| `harnx_tool_call_duration_seconds` | Histogram | `tool` | Tool execution duration histogram. | Tool & MCP servers |
| `harnx_sandbox_wakes_total` | Counter | none | Kubernetes sandboxes resumed from `Suspended` mode. | `harnx-k8s-sandbox-tools` |
| `harnx_sandbox_hibernations_total` | Counter | `reason` | Kubernetes sandboxes suspended explicitly or after idle timeout. | `harnx-k8s-sandbox-tools` |
| `harnx_activation_phase_seconds` | Histogram | `phase` | Duration of worker activation phases (`publish_to_delivery`, `delivery_to_admission`, `admission_to_lease`, `lease_to_turn_start`). Extended buckets: 0.005s to 300s. | `harnx-worker` |
| `harnx_activations_received_total` | Counter | `attempt` | Activation deliveries received by workers (`first` or `redelivery`). | `harnx-worker` |
| `harnx_activation_claims_total` | Counter | `outcome` | Activation claim dispositions (`claimed`, `busy`, `lease_held`, `same_delivery`, `preflight_not_ready`, `failure_budget_term`, `refused_term`, `delivery_limit_term`, `error`). | `harnx-worker` |
| `harnx_activation_naks_total` | Counter | `reason` | Successfully NAKed activation deliveries by bounded reason (`busy`, `preflight_not_ready`, `shutdown`, `settlement_rejection`, `claim_error`, `preparation_error`). | `harnx-worker` |
| `harnx_activation_redeliveries_total` | Counter | none | Activation redeliveries received by workers (delivery count > 1). | `harnx-worker` |
| `harnx_activation_claim_deadlines_total` | Counter | none | Client/parent watchdog timeouts waiting for a worker to claim an activated session. | Client / parent (`harnx`, `harnx-serve`) |
| `harnx_worker_admission_permits` | Gauge | none | Available admission semaphore permits on this worker (0 to 8). | `harnx-worker` |
| `harnx_worker_activation_handlers` | Gauge | none | In-flight activation handler tasks on this worker. | `harnx-worker` |
| `harnx_worker_activations_waiting` | Gauge | none | Activations waiting locally for an admission permit on this worker (0 or 1). | `harnx-worker` |
| `harnx_nats_operation_seconds` | Histogram | `op` | JetStream and KV operation duration by bounded operation type. Extended buckets: 0.005s to 300s. | `harnx`, `harnx-serve`, `harnx-worker` |
| `harnx_nats_operations_total` | Counter | `op`, `outcome` | JetStream and KV operation count by bounded operation and outcome (`ok`, `timeout`, `error`). | `harnx`, `harnx-serve`, `harnx-worker` |

Histogram buckets for standard duration metrics use default boundaries: `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` seconds. `harnx_activation_phase_seconds` and `harnx_nats_operation_seconds` use extended boundaries: `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0, 60.0, 120.0, 300.0]` seconds.

## Design & Label Details

- **`client` label semantics**: The `client` label matches the configured client name (`model.client_name()`). This may be a package-qualified alias like `mypkg/openai` rather than a canonical backend name.
- **`provider` label semantics**: The `provider` label is the canonical backend kind (`openai`, `claude`, `bedrock`, `openai-compatible`, …) resolved from the model's configured client. It is empty (`""`) when the client cannot be resolved. Unlike `client`, it is never package-qualified.
- **Chat completions only**: Token usage and cost metrics apply exclusively to chat completions. Embeddings and reranker requests do not emit token metrics.
- **Cost metric mechanics**: `harnx_llm_cost_dollars` is exported as a gauge because the underlying metrics facade does not support floating-point counters. It increases monotonically per process and is emitted only when required unit prices are configured for the model. If a request includes cached tokens (cache-read or cache-write) but the matching cache price is missing, the cost calculation returns `None` and the gauge is not incremented for that call.
- **Cached token pricing**: Cost calculations include cache-read (`cache_read_price`) and cache-write (`cache_write_price`) pricing using a uniform formula: `(uncached_input × input_price + cache_read × cache_read_price + cache_write × cache_write_price + output × output_price) / 1,000,000`. `harnx_llm_tokens_total` emits `cache_read` and `cache_write` series in addition to `input` and `output`. The `cached` type is retained as a deprecated alias of `cache_read` for dashboard backward compatibility.
- **Token counting semantics change**: `input_tokens` uses the OpenTelemetry subset convention, where `input_tokens` includes all cache tokens (`input_tokens >= cache_read + cache_write`). For Anthropic and Bedrock providers, `input_tokens` now includes cached tokens (previously Anthropic excluded cache-read tokens; Bedrock excluded cache-read and discarded cache-write). OpenAI and Gemini were already subset models and remain unchanged. Dashboards or queries that sum Anthropic or Bedrock `input_tokens` will reflect higher values than before.
- **Cardinality protection**: Metrics omit `session_id` and raw dynamic URL paths to prevent cardinality explosion. HTTP route labels use matched path templates (such as `/token/{context}`) or fixed route identifiers (`proxy` for `harnx-proxy-auth`).

## Worker Activation and Lease Claims

When an agent turn is initiated, the submitting frontend or parent agent appends user input to the session log, publishes a `SessionActivate` message to the cluster work queue (`WORK_NOTIFY_<cluster>`), and starts a lease acquisition watchdog (configured by `nats_lease_acquisition_timeout_secs`, default 60s). A worker daemon pulls the message, acquires an admission permit, claims the session lease in the `harnx_leases` KV bucket, and launches the turn.

If no worker claims the lease before the deadline, the client watchdog fires:

```text
No worker claimed this session within 60 seconds after activation. Check that a worker is running and subscribed to this agent's cluster.
```

### Activation and NATS Metrics

The activation pipeline exports metrics on both the initiating client/parent process and worker daemons:

| Metric Name | Type | Recorded In | Labels | Meaning |
|-------------|------|-------------|--------|---------|
| `harnx_activation_phase_seconds` | Histogram | Worker | `phase` | Duration of each worker activation pipeline stage (buckets: 0.005s to 300s). |
| `harnx_activations_received_total` | Counter | Worker | `attempt` | Total activation deliveries received from JetStream (`first` or `redelivery`). |
| `harnx_activation_claims_total` | Counter | Worker | `outcome` | Activation claim dispositions. |
| `harnx_activation_naks_total` | Counter | Worker | `reason` | Count of successfully NAKed activation deliveries by bounded reason. |
| `harnx_activation_redeliveries_total` | Counter | Worker | none | Count of activations received with JetStream delivery count > 1. |
| `harnx_activation_claim_deadlines_total` | Counter | Client / Parent | none | Count of client-side timeouts waiting for a worker to claim the session. |
| `harnx_worker_admission_permits` | Gauge | Worker | none | Available admission permits in this worker process (semaphore max 8). |
| `harnx_worker_activation_handlers` | Gauge | Worker | none | Active activation handler tasks in flight in this worker process. |
| `harnx_worker_activations_waiting` | Gauge | Worker | none | Activations waiting locally for an admission permit in this worker (strictly 0 or 1). |
| `harnx_nats_operation_seconds` | Histogram | Worker, Client | `op` | Latency of individual JetStream and KV operations (buckets: 0.005s to 300s). |
| `harnx_nats_operations_total` | Counter | Worker, Client | `op`, `outcome` | Count of JetStream and KV operations by bounded op and outcome (`ok`, `timeout`, `error`). |

#### Label Cardinality and Recording Seams

- **Bounded cardinality**: Metrics omit `session_id`, `agent_name`, and stream/subject names to prevent cardinality explosion. All labels are strictly bounded enums. To inspect specific sessions, correlate using structured log events.
- **Process locality**: The worker gauges (`harnx_worker_admission_permits`, `harnx_worker_activation_handlers`, `harnx_worker_activations_waiting`) are process-local. Prometheus scrape targets identify individual worker instances via the `instance` or `pod` label.
- **Worker waiting gauge vs true backlog**: `harnx_worker_activations_waiting` is strictly binary (`0` or `1`). The worker loop buffers at most one pending message while its 8 admission permits are busy. It does not reflect total backlog in NATS JetStream. For true consumer backlog across the cluster, check JetStream consumer `num_pending` via `nats consumer info <stream> <consumer>` (CLI) or the Prometheus NATS exporter metric `nats_consumer_num_pending`.
- **Client vs Worker separation**: `harnx_activation_claim_deadlines_total` is recorded in the client process (`harnx`, `harnx-serve`, or parent worker running a sub-agent) when its lease watchdog expires. All other activation lifecycle metrics are recorded in `harnx-worker`.

#### Activation Phases

The `harnx_activation_phase_seconds` histogram segments time into four sequential phases:

1. `publish_to_delivery`: Time elapsed between broker message publication (`info.published`) and worker stream delivery receipt (`delivered_wall`). Reflects JetStream queueing delay and network delivery.
   - **Clock skew caveat**: Compares the NATS server's publish timestamp against the worker host's local wall clock. If clock skew makes the worker clock appear behind the broker clock (negative elapsed time), the sample is clamped to 0s (`Duration::ZERO`) so it remains accounted for in the lowest histogram bucket (`le="0.005"`). Synchronize system clocks with NTP across NATS servers and worker hosts.
2. `delivery_to_admission`: Time a delivered activation waited to acquire one of the worker's 8 concurrent admission permits. Reflects worker admission queueing and worker saturation.
3. `admission_to_lease`: Time taken to run preflight checks and acquire the session lease in the KV bucket (`lease_create`). Reflects lease acquisition latency.
4. `lease_to_turn_start`: Time from lease acquisition to active turn dispatch (task launch and active session registration). Reflects local turn preparation.

#### Claim Outcomes

The `harnx_activation_claims_total` counter categorizes the worker's decision for each delivered message:

- `claimed`: Worker acquired the session lease and accepted the turn.
- `busy`: Another delivery of the same session is already reserved on this worker; the message was delayed-NAKed (10s plus jitter, NAK reason `busy`).
- `lease_held`: Lease creation in the KV bucket found another holder for this session; the message was delayed-NAKed (NAK reason `busy`).
- `same_delivery`: Duplicate delivery of an in-flight delivery attempt already being processed.
- `preflight_not_ready`: `activation_is_ready` declined the activation before lease acquisition. This covers several cases: the lease-holder check found another worker holding the session (NAK reason `busy`), session metadata or routing was not ready (NAK reason `preflight_not_ready`), the worker was shutting down (NAK reason `shutdown`), or the activation was stale, cancelled, or missing metadata and was acked or terminated without a turn. Compare with `harnx_activation_naks_total` by reason to tell these apart; the `activation_claim_deferred` event only reports `reason="not_claimed"`.
- `failure_budget_term`: The worker recorded an `Error` on the session and terminated the message with a JetStream `Term` ack, because the activation failed ten times before its turn started or its turn failed.
- `refused_term`: The worker refused the activation on two deliveries, recorded why as an `Error` on the session, and terminated the message. A prompt with no durable run admission, such as one sent before run admissions existed, is refused this way.
- `delivery_limit_term`: The activation had been delivered `MAX_ACTIVATION_DELIVERIES` (100) times, and the worker terminated it instead of NAKing it again. Usually a session that stayed busy for hours; the worker logs the session and the reason it would have NAKed.
- `error`: Unexpected error occurred while attempting to claim the activation (e.g. NATS communication error).

#### NAK Reasons

The `harnx_activation_naks_total` counter tracks activation deliveries that the worker explicitly and successfully NAKed back to JetStream for redelivery or failover:

- `busy`: The session lease is held by another worker (lease preflight or lease acquisition), or another delivery of the same session is already reserved on this worker. Delayed-NAKed for 10s plus jitter. A steady `busy` rate means activations for sessions that are already running, not worker saturation.
- `preflight_not_ready`: Session metadata or route preflight failed (or the lease-holder lookup errored); delayed-NAKed for retry.
- `shutdown`: Worker is shutting down; immediately NAKs unhandled activations (including any buffered message waiting for an admission permit) so replacement workers can resume execution immediately.
- `settlement_rejection`: Activation was rejected during active turn settlement.
- `claim_error`: Error occurred while attempting to claim the activation (e.g. NATS communication error).
- `preparation_error`: Error occurred while preparing session state or journal before turn execution.

#### Bounded NATS Operations

The `harnx_nats_operation_seconds` and `harnx_nats_operations_total` metrics track 12 bounded operations:

- `stream_lookup`: Fetch stream state (`get_stream`).
- `stream_create`: Create session or notification stream (`create_stream`).
- `consumer_create`: Create or retrieve pull consumer (`get_or_create_consumer`).
- `consumer_fetch`: Request message batch from pull consumer (`consumer.stream().max_messages_per_batch(...).messages()`).
- `consumer_messages`: Pull next message from stream (`messages.next()`). **Caveat:** When the worker is idle and waiting for incoming work, this latency includes idle wait time on the broker stream, not solely server processing.
- `kv_bucket_open`: Open KV bucket (`get_key_value`).
- `kv_bucket_create`: Create KV bucket (`create_key_value`). Expected conflict errors during startup fallback to `kv_bucket_open`.
- `lease_get`: Read session lease record (`bucket.get`).
- `lease_create`: Acquire session lease with TTL (`bucket.create_with_ttl`).
- `lease_renew`: CAS update to renew session lease fence token (`publish_with_headers`).
- `lease_release`: Delete lease key with revision fencing (`bucket.delete_expect_revision`).
- `activation_publish`: Publish `SessionActivate` message to work queue.

The `outcome` label is classified as:
- `ok`: Operation succeeded.
- `timeout`: Operation timed out (error message contains `timed out`, `timeout`, `deadline`, or `elapsed`).
- `error`: Operation failed with any other error.

### Diagnosing Activation Timeouts

When a parent process or client logs `No worker claimed this session within 60 seconds after activation`, use the following PromQL queries and dashboard panels to identify which stage failed.

#### 1. No Subscribed Worker

**Symptom**: Activations are published and parent claim deadlines expire, but workers never receive the messages.

- **Published activations rate**:
  ```promql
  sum(rate(harnx_nats_operations_total{op="activation_publish",outcome="ok"}[5m]))
  ```
- **Parent claim deadline timeouts rate**:
  ```promql
  sum(rate(harnx_activation_claim_deadlines_total[5m]))
  ```
- **Worker activations received rate**:
  ```promql
  sum(rate(harnx_activations_received_total[5m]))
  ```

**Diagnosis**: If published rate > 0 and claim deadlines rate > 0, but activations received rate == 0 across all workers, no worker is subscribed to the cluster's activation work queue (`WORK_NOTIFY_<cluster>`). Check worker daemon status and cluster subscription configuration.

#### 2. Undelivered or Queued Activation

**Symptom**: Activations are published and workers are running, but messages sit in the JetStream queue or redeliver repeatedly.

- **Publish to delivery latency (p99)**:
  ```promql
  histogram_quantile(0.99, sum(rate(harnx_activation_phase_seconds_bucket{phase="publish_to_delivery"}[5m])) by (le))
  ```
- **Redelivery rate**:
  ```promql
  sum(rate(harnx_activation_redeliveries_total[5m]))
  ```
- **NAK rate by reason**:
  ```promql
  sum(rate(harnx_activation_naks_total[5m])) by (reason)
  ```
- **Activations waiting for permit on worker (0 or 1 per worker)**:
  ```promql
  max(harnx_worker_activations_waiting) by (instance)
  ```

**Diagnosis**: High `publish_to_delivery` latency indicates messages are sitting unacknowledged in JetStream before reaching workers (if values cluster at 0s, check host NTP synchronization against the NATS server). A rising redelivery rate or NAK rate indicates workers are rejecting or failing activations (for example, due to worker shutdown, a session lease held elsewhere, or preflight failures) and JetStream is redelivering them.

**Queue backlog note**: `harnx_worker_activations_waiting` is strictly 0 or 1 per worker (at most one buffered message waiting for an admission permit). To inspect true queue backlog across the entire cluster, check JetStream consumer `num_pending` using the NATS CLI (`nats consumer info <stream> <consumer>`) or the Prometheus NATS exporter metric `nats_consumer_num_pending`.

#### 3. Worker Admission Saturation

**Symptom**: Workers pull messages from JetStream, but local concurrency limits delay processing. Each worker admits at most 8 concurrent activations.

- **Available admission permits (0 to 8)**:
  ```promql
  min(harnx_worker_admission_permits) by (instance)
  ```
- **In-flight activation handlers**:
  ```promql
  max(harnx_worker_activation_handlers) by (instance)
  ```
- **Delivery to admission latency (p99)**:
  ```promql
  histogram_quantile(0.99, sum(rate(harnx_activation_phase_seconds_bucket{phase="delivery_to_admission"}[5m])) by (le))
  ```
**Diagnosis**: If `harnx_worker_admission_permits` drops to 0, `harnx_worker_activation_handlers` stays at 8, and `delivery_to_admission` p99 latency spikes, workers are saturated. While all permits are taken the daemon holds at most one delivered message (`harnx_worker_activations_waiting == 1`) and stops pulling more; the rest stay pending in JetStream, so look for `publish_to_delivery` latency and consumer `num_pending` rising at the same time. Saturation does not produce NAKs. Add worker replicas or inspect handler execution duration.

#### 4. Slow JetStream or KV Operations

**Symptom**: Worker admission is fast, but acquiring leases, renewing leases, or opening streams stalls or fails.

- **NATS operation latency by op (p99)**:
  ```promql
  histogram_quantile(0.99, sum(rate(harnx_nats_operation_seconds_bucket{op!="consumer_messages"}[5m])) by (le, op))
  ```
- **NATS operation failure and timeout rate**:
  ```promql
  sum(rate(harnx_nats_operations_total{outcome!="ok"}[5m])) by (op, outcome)
  ```
- **Admission to lease latency (p99)**:
  ```promql
  histogram_quantile(0.99, sum(rate(harnx_activation_phase_seconds_bucket{phase="admission_to_lease"}[5m])) by (le))
  ```

**Diagnosis**: Spikes in `admission_to_lease` latency accompanied by elevated `lease_create` or `kv_bucket_open` duration indicate NATS broker latency or KV storage contention. (Exclude `consumer_messages` from latency alerts because it measures pull wait time when workers are idle.)

#### 5. Turn Started But Did Not Return Before Parent Deadline

**Symptom**: Parent logs a 60-second claim deadline timeout, but the activation pipeline completed normally.

- **Lease to turn start latency (p99)**:
  ```promql
  histogram_quantile(0.99, sum(rate(harnx_activation_phase_seconds_bucket{phase="lease_to_turn_start"}[5m])) by (le))
  ```
- **Successful claims rate**:
  ```promql
  sum(rate(harnx_activation_claims_total{outcome="claimed"}[5m]))
  ```

**Metrics vs logs**:
Metrics confirm whether the worker successfully claimed the lease and handed off to the turn runner within milliseconds. All four activation phases (`publish_to_delivery`, `delivery_to_admission`, `admission_to_lease`, `lease_to_turn_start`) will report low latencies (typically under 20ms).
However, metrics cannot show why the turn itself took longer than expected. Once `lease_to_turn_start` completes, execution enters the agent loop: calling LLM providers, executing tool calls, or waiting on sandboxes.
Check logs for `activation_turn_started`. If `activation_turn_started` was logged within 1 second of publication, the claim succeeded; the timeout was caused by slow downstream execution (such as LLM rate limits or slow tool runs) rather than activation delivery failure.

### Log Correlation Recipe

When investigating an activation failure for a specific session, correlate structured tracing events in worker and client logs.

#### Tracing Events

The runtime emits 7 structured `info` events during activation lifecycle:

| Event Name | Emitted At | Key Fields | Description |
|------------|------------|------------|-------------|
| `activation_published` | Client / Parent | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Client published `SessionActivate` to NATS work queue. |
| `activation_delivered` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker received message from JetStream consumer. `elapsed_ms` is publish-to-receipt latency. |
| `activation_admitted` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker acquired admission permit. `elapsed_ms` is receipt-to-permit latency. |
| `activation_lease_acquired` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker claimed lease in KV store. `elapsed_ms` is permit-to-lease latency. |
| `activation_turn_started` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker launched active session task. `elapsed_ms` is lease-to-turn-start latency. |
| `activation_claim_deferred` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker did not claim activation (`reason="not_claimed"`). |
| `activation_claim_failed` | Worker | `session_id`, `activation_id`, `agent`, `cluster`, `delivery_attempt`, `elapsed_ms`, `reason` | Worker encountered an error claiming activation (`reason="error"`). |

Routine lease renewal events (`nats lease renewed: ...`) are emitted at `debug` level to prevent high log volume across long-running sessions. Lease losses and renewal errors remain at `warn` and `error` levels.

#### Filtering Logs by Session ID

Filter client and worker logs by `session_id` to trace the full lifecycle:

```bash
# Grep text logs
grep "s_01J8K9P2X" /var/log/harnx/*.log

# Parse JSON logs with jq
jq -c 'select(.session_id == "s_01J8K9P2X") | {timestamp, event, elapsed_ms, delivery_attempt, reason}' worker.log
```

#### Example Healthy Timeline

```text
2026-09-30T22:15:00.010Z INFO event="activation_published" session_id="s_01J8K9P2X" activation_id="2026-09-30T22:15:00.005Z" agent="coding" cluster="prod" delivery_attempt=0 elapsed_ms=5 reason="published" activation published
2026-09-30T22:15:00.018Z INFO event="activation_delivered" session_id="s_01J8K9P2X" activation_id="2026-09-30T22:15:00.005Z" agent="coding" cluster="prod" delivery_attempt=1 elapsed_ms=8 reason="delivered" activation delivered
2026-09-30T22:15:00.019Z INFO event="activation_admitted" session_id="s_01J8K9P2X" activation_id="2026-09-30T22:15:00.005Z" agent="coding" cluster="prod" delivery_attempt=1 elapsed_ms=1 reason="admitted" activation admitted
2026-09-30T22:15:00.032Z INFO event="activation_lease_acquired" session_id="s_01J8K9P2X" activation_id="2026-09-30T22:15:00.005Z" agent="coding" cluster="prod" delivery_attempt=1 elapsed_ms=13 reason="claimed" activation lease acquired
2026-09-30T22:15:00.035Z INFO event="activation_turn_started" session_id="s_01J8K9P2X" activation_id="2026-09-30T22:15:00.005Z" agent="coding" cluster="prod" delivery_attempt=1 elapsed_ms=3 reason="started" activation turn started
```

In this trace:
- Publication took 5 ms.
- JetStream delivery took 8 ms (`elapsed_ms` in `activation_delivered`).
- Admission permit was acquired in 1 ms (`elapsed_ms` in `activation_admitted`).
- Lease was acquired in 13 ms (`elapsed_ms` in `activation_lease_acquired`).
- Turn launched 3 ms later (`elapsed_ms` in `activation_turn_started`).
Total time from client publish to worker turn start was 30 ms.

If a step stalls or fails:
- Missing `activation_delivered` means the worker never received the message.
- High `elapsed_ms` in `activation_admitted` indicates worker permit contention.
- `activation_claim_deferred` means this worker did not claim the delivery: another worker holds the lease, the worker NAKed it (see `harnx_activation_naks_total`), or preflight settled it without a turn.
- `activation_claim_failed` indicates a failure during lease acquisition (check the preceding warning/error log for details).

For distributed trace spans covering downstream LLM calls and tool executions once the turn is running, see [OpenTelemetry Tracing](tracing.md).

## Extending Metrics

### Adding LLM token/cost recording

Token and cost metrics are recorded once at the runtime retry wrapper (`harnx-runtime/src/client/retry.rs`), **not** at `ModelEvent::Usage` or `Final`. The event stream double-counts: tool-loop turns emit multiple `Usage` events, and `Final.usage` duplicates the terminal call. Recording at the retry seam ensures correct attribution across fallbacks, sub-agents, title generation, and compaction.

To add per-call LLM instrumentation (billing, cost attribution, audit), add it at the same seam.

### Adding tool dispatch instrumentation

`run_toolset_main` has two mutually exclusive dispatch paths:

- **NATS mode** → `invoke_uncached_tool` (shared entrypoint)
- **MCP stdio mode** → `McpToolsetAdapter::dispatch_call_tool` (calls `toolset.invoke` directly)

Any cross-cutting concern (metrics, tracing, auth) added at one seam does **not** automatically cover the other. rmcp `--mcp-http` servers use their own `ServerHandler::call_tool` method, a third seam. When adding instrumentation, check all relevant paths.

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
harnx_llm_tokens_total{agent="coding",client="openai",provider="openai",model="gpt-4o",type="input"} 1420
harnx_llm_tokens_total{agent="coding",client="openai",provider="openai",model="gpt-4o",type="output"} 385
harnx_llm_tokens_total{agent="coding",client="openai",provider="openai",model="gpt-4o",type="cache_read"} 512
harnx_llm_tokens_total{agent="coding",client="openai",provider="openai",model="gpt-4o",type="cache_write"} 0
harnx_llm_tokens_total{agent="coding",client="openai",provider="openai",model="gpt-4o",type="cached"} 512

# HELP harnx_llm_cost_dollars Cumulative estimated LLM cost in USD
# TYPE harnx_llm_cost_dollars gauge
harnx_llm_cost_dollars{agent="coding",client="openai",provider="openai",model="gpt-4o"} 0.0074
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

### Worker Activation and NATS Metrics

Start `harnx-worker` with a metrics listener:

```bash
harnx-worker --cluster prod --metrics-addr 127.0.0.1:9109
```

Query the activation and NATS metric families:

```bash
curl -s http://127.0.0.1:9109/metrics | grep -E "harnx_(activation|worker|nats)_"
```

Example output:

```text
# HELP harnx_activation_phase_seconds Worker activation phase duration in seconds
# TYPE harnx_activation_phase_seconds histogram
harnx_activation_phase_seconds_bucket{phase="publish_to_delivery",le="0.01"} 14
harnx_activation_phase_seconds_bucket{phase="publish_to_delivery",le="0.05"} 15
harnx_activation_phase_seconds_count{phase="publish_to_delivery"} 15
harnx_activation_phase_seconds_sum{phase="publish_to_delivery"} 0.124

# HELP harnx_activations_received_total Activation deliveries by attempt
# TYPE harnx_activations_received_total counter
harnx_activations_received_total{attempt="first"} 15
harnx_activations_received_total{attempt="redelivery"} 0

# HELP harnx_activation_claims_total Claim dispositions by bounded outcome
# TYPE harnx_activation_claims_total counter
harnx_activation_claims_total{outcome="claimed"} 15

# HELP harnx_activation_naks_total Successfully NAKed activation deliveries by bounded reason
# TYPE harnx_activation_naks_total counter
harnx_activation_naks_total{reason="busy"} 0
harnx_activation_naks_total{reason="shutdown"} 0

# HELP harnx_worker_admission_permits Admission permits currently available in this worker process
# TYPE harnx_worker_admission_permits gauge
harnx_worker_admission_permits 8

# HELP harnx_worker_activation_handlers Activation handlers currently in flight in this worker process
# TYPE harnx_worker_activation_handlers gauge
harnx_worker_activation_handlers 0

# HELP harnx_worker_activations_waiting Activations waiting for admission in this worker process
# TYPE harnx_worker_activations_waiting gauge
harnx_worker_activations_waiting 0

# HELP harnx_nats_operations_total JetStream/KV operation results by bounded operation and outcome
# TYPE harnx_nats_operations_total counter
harnx_nats_operations_total{op="lease_create",outcome="ok"} 15
harnx_nats_operations_total{op="lease_renew",outcome="ok"} 42
harnx_nats_operations_total{op="lease_release",outcome="ok"} 15
```

## Follow-ups

- **Canonical provider label** ([#1592](https://github.com/dobesv/harnx/issues/1592)): Added a distinct `provider` label alongside `client` to reflect the underlying provider backend.

## A2A coordination diagnostics

`harnx-a2a-server --metrics-addr :8456` exports `harnx_a2a_owner_lost_total`,
`harnx_a2a_pending_age_seconds{phase="admission|cancel|outbox"}`,
`harnx_a2a_sweep_lag_seconds`,
`harnx_a2a_events_purged_total{phase="checkpoint|terminal|deleted"}` and
`harnx_a2a_operations_total{op="publish|recovery|sweep",outcome="ok|error"}`. Ages are
observations during recovery/publication, not an exact pending-count gauge or
completion latency. Sweep lag measures elapsed time between completed registry
scan cycles. Owner loss counts successful takeover of unresolved retained work.
No dynamic identity labels are added. See [A2A operations](a2a-operations.md) for
backpressure diagnosis and capacity limits.
