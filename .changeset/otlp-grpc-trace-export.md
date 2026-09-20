---
harnx: minor
---

Add OTLP/gRPC trace export to `harnx-telemetry` alongside the existing HTTP exporter. Honors `OTEL_EXPORTER_OTLP_PROTOCOL` and `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` (`grpc` or `http/protobuf`).

**Operator note**: Explicit endpoints like `http://localhost:4318` are preserved as configured. The default port 4317 applies only when no endpoint is configured and `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`. If `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` was previously configured in an environment pointing to an HTTP-only OTLP collector on port 4318, trace export will now attempt gRPC to port 4317. Update endpoint configuration to match the desired collector transport.
