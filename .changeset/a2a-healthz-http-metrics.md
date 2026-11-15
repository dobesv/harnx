---
harnx: minor
---
Add opt-in health/readiness checks to `harnx-a2a-server` with `--healthz-addr` or `HARNX_HEALTHZ_ADDR`. Readiness becomes healthy after bootstrap and the A2A listener bind, then returns to 503 before runner shutdown on SIGTERM or Ctrl-C. Add bounded-route HTTP request metrics to the existing `--metrics-addr` Prometheus listener and document separate application, readiness and metrics ports for Kubernetes deployments.
