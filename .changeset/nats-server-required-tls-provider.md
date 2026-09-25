---
harnx: patch
---

Prevent a NATS server-required TLS upgrade from panicking when both rustls crypto providers are linked and no process default is installed.
