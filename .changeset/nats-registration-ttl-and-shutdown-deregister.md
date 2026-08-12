---
harnx: minor
---

Tool and hook server registrations now expire: the registry and expectations buckets carry a 90s TTL (three refresh intervals), so a registration can no longer outlive the process that published it and grow the bucket without bound. Servers also deregister themselves on graceful shutdown, including SIGTERM/Ctrl+C — independently deployed tool/hook server pods have no parent supervisor to clean up after them, and Kubernetes terminates pods with SIGTERM. Without the SIGTERM wiring, a terminated pod's registration would keep being routed to until the TTL expired.
