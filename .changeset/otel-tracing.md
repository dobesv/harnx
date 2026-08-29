---
harnx: minor
---

Add OpenTelemetry distributed tracing. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export OTLP traces covering agent turns, LLM calls with token count attributes, and cross-process tool calls; off by default.
