---
harnx: patch
---
Fix a panic on every TLS connection to a NATS broker that did not set `tls_ca`, including
plain `tls: true` and the client-certificate mTLS configuration. Harnx now builds the rustls
config itself, naming the crypto provider, instead of leaving async-nats to resolve a
process default that this workspace makes ambiguous.
