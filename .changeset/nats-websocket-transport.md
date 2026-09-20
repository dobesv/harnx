---
harnx: minor
---
Support the NATS WebSocket transport (`ws://`, `wss://`) in `nats_servers/<cluster>.yaml`
and `HARNX_NATS_URL`, so harnx can reach a broker behind an HTTP load balancer such as an
AWS ALB that requires x509 client certificates. `tls_ca` and `tls_cert`/`tls_key` can now
also be used together, and a new `ignore_discovered_servers` setting controls whether the
peers a clustered broker advertises are added to the connection's server pool.
