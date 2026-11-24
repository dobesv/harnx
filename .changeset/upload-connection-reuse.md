---
harnx: patch
---

Web UI attachment uploads now reuse a per-cluster NATS connection and bucket handle instead of connecting and ensuring the `harnx_attachments` bucket on every request. The first upload to a cluster initializes the connection single-flight and later uploads share it, so repeat and concurrent uploads are faster and the broker sees one upload connection per cluster per `harnx-serve` process. A deleted object store is detected on the next upload's failure and re-ensured; a failed best-effort replica reconciliation is retried at a bounded rate instead of being re-attempted (or forgotten) on every upload.
