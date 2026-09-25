---
"harnx": patch
---

fix(k8s-sandbox-tools): install rustls crypto provider before startup TLS

`harnx-k8s-sandbox-tools` panicked on startup with "Could not automatically determine the process-level CryptoProvider": its dependency graph links both `ring` (via async-nats) and `aws-lc-rs` (via the AWS SDK's hyper-rustls stack that `kube` uses), and nothing installed a process default. `kube::Client::try_default` and `reqwest::Client` both build TLS through `rustls::ClientConfig::builder()`, which then can't pick a provider. Pin `ring` as the process default at the top of `main()`, matching the NATS TLS path in `harnx-nats-common`.
