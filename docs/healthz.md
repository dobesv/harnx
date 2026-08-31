# Health & Readiness Endpoint

Harnx provides an opt-in `/healthz` HTTP endpoint for Kubernetes readiness probes and load-balancer health checks across long-running workspace binaries.

## Overview

Readiness checks are **off by default**. When `--healthz-addr` (or `HARNX_HEALTHZ_ADDR` where supported) is unset, no HTTP listener starts and process behavior remains unchanged.

When enabled, each binary starts a dedicated HTTP listener on the specified port and serves `GET /healthz`. The endpoint returns:

- `200 OK` when the process is ready to serve traffic.
- `503 Service Unavailable` while the process is starting up or shutting down.

The response returns only HTTP status codes with no response body. The healthz listener runs independently of Prometheus metrics (`docs/metrics.md`), OpenTelemetry tracing (`docs/tracing.md`), and main application transports.

## Configuration & Environment Variables

You can enable the healthz listener using either the CLI flag or an environment variable fallback:

- `--healthz-addr <ADDR>`: Available as a CLI argument on all 15 in-scope binaries. Accepts `IP:PORT` or `:PORT`. Passing a blank host (e.g. `--healthz-addr :8081`) binds `0.0.0.0`, allowing scrapers or Kubernetes probes to reach the endpoint. Passing `127.0.0.1:8081` restricts the listener to loopback.
- `HARNX_HEALTHZ_ADDR`: Environment variable fallback honored by shared-entrypoint binaries: `harnx-bash-tools`, `harnx-fs-tools`, `harnx-grep-tools`, `harnx-time-server`, `harnx-plans-tools` (non-HTTP mode), `harnx-claude-compatible-hook-server`, `harnx-mcp-remote`, `harnx-mcp-bridge`, `harnx-mcp-time`, and `harnx-mcp-plans-github`. If both the CLI flag and environment variable are set, the CLI flag takes precedence.

## Binary Coverage

Healthz support is available across 15 long-running binaries:

- **Core runtime & proxies**: `harnx-serve`, `harnx-worker`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`
- **Tool & hook servers**: `harnx-bash-tools`, `harnx-fs-tools`, `harnx-grep-tools`, `harnx-plans-tools`, `harnx-time-server`, `harnx-claude-compatible-hook-server`
- **MCP bridges & servers**: `harnx-mcp-bridge`, `harnx-mcp-remote`, `harnx-mcp-time`, `harnx-mcp-plans-github`

Unlike `--metrics-addr` (where CLI flag support varied on `harnx-claude-compatible-hook-server`), `--healthz-addr` is supported as a CLI flag across all 15 binaries.

## Readiness Semantics & Lifecycle

Readiness is tracked per process and transitions from `503` to `200` once initialization completes:

- **HTTP servers** (`harnx-serve`, `harnx-aws-creds`, `harnx-k8s-creds`, `harnx-proxy-auth`, `harnx-plans-tools` HTTP mode, `harnx-mcp-time` HTTP mode): Become ready as soon as their HTTP listener binds.
- **NATS-consumer servers** (`harnx-bash-tools`, `harnx-fs-tools`, `harnx-grep-tools`, `harnx-time-server`, `harnx-claude-compatible-hook-server`): Become ready after NATS subscriptions connect and initial queue requests are flushed.
- **Worker daemon** (`harnx-worker`): Becomes ready once NATS worker services and activation streams are live.
- **Stdio MCP bridge & remote** (`harnx-mcp-bridge`, `harnx-mcp-remote`): Become ready once their transport setup finishes.

### Shutdown Behavior

When a process initiates shutdown, binaries with shutdown hooks transition `/healthz` back to `503 Service Unavailable` before draining in-flight requests. This allows Kubernetes ingress controllers and service meshes to de-register the pod before traffic stops.

Three binaries (`harnx-worker`, `harnx-aws-creds`, and `harnx-k8s-creds`) operate as ready-only services by design and do not flip back to `503` on shutdown.

## Distinction from Prometheus Metrics

While both listeners use HTTP, they serve distinct operational purposes:

| Feature | Readiness (`/healthz`) | Prometheus Metrics (`/metrics`) |
| --- | --- | --- |
| **Purpose** | Traffic routing & pod readiness | Telemetry & performance monitoring |
| **Default Path** | `/healthz` | `/metrics` |
| **CLI Flag** | `--healthz-addr` | `--metrics-addr` |
| **Env Var** | `HARNX_HEALTHZ_ADDR` | `HARNX_METRICS_ADDR` |
| **Response Format** | Status code (`200` or `503`), no body | Prometheus text format exposition |

Do not configure `--healthz-addr` and `--metrics-addr` to use the same port, as each feature runs a dedicated listener.

## Runnable Examples

### Local Testing with Curl

Start `harnx-serve` with a healthz listener on port `8081`:

```bash
harnx-serve --healthz-addr 127.0.0.1:8081
```

Query the endpoint:

```bash
curl -i http://127.0.0.1:8081/healthz
```

Output when ready:

```text
HTTP/1.1 200 OK
content-length: 0
```

### Kubernetes Readiness Probe

Add a `readinessProbe` to your pod manifest:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-serve
spec:
  template:
    spec:
      containers:
        - name: harnx-serve
          image: harnx-serve:latest
          args:
            - "--healthz-addr"
            - ":8081"
          ports:
            - containerPort: 8081
              name: healthz
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8081
            initialDelaySeconds: 2
            periodSeconds: 5
            failureThreshold: 3
```
