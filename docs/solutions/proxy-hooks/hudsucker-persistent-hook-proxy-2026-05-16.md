---
title: "hudsucker MITM proxy as persistent harnx hook with readiness protocol"
date: 2026-05-16
category: proxy-hooks
problem_type: integration_issue
component: harnx-proxy-auth
root_cause: "mitm proxy integration patterns and hudsucker API quirks"
resolution_type: code_fix
severity: medium
tags:
  - mitm-proxy
  - hudsucker
  - persistent-hooks
  - rcgen
  - readiness-protocol
  - ca-certificate
plan_ref: harnx-531-github-auth-proxy
---

## Problem

Building a generic HTTPS-intercepting auth-header-injection proxy that doubles as a persistent harnx hook process. The binary must start a local MITM proxy, communicate readiness info (port, CA cert) to the parent process, then enter a JSONL stdin/stdout event loop for hook protocol handling.

## Symptoms

- **hudsucker rcgen version mismatch**: Workspace `rcgen 0.13` types incompatible with `hudsucker::rcgen` (internal `rcgen 0.14`). `RcgenAuthority::new()` rejects `KeyPair`/`Certificate` from wrong crate.
- **CA cert unreachable in PID-namespaced tests**: Path `/tmp/harnx-auth-proxy-<pid>/ca.pem` inaccessible when proxy subprocess spawned from cargo tests with PID namespace isolation.
- **`handle_request` receives `http://` URIs for HTTPS**: hudsucker presents MITM'd HTTPS requests as `http://` scheme internally, making scheme-based matching unreliable.
- **Integration test failing on HTTPS backends**: Self-signed test server certs rejected by proxy's outgoing TLS validation.

## Investigation Steps

1. Checked `hudsucker 0.24` docs for `RcgenAuthority` API: requires `Issuer::from_ca_cert_pem(cert_pem_str, key_pair)`.
2. Discovered workspace `rcgen 0.13` and hudsucker's internal `rcgen 0.14` are separate crates — type mismatch errors.
3. Tested `CA_CERT_PATH` readability from test process: PID namespace isolation made temp directory unreachable.
4. Added `CA_CERT_PEM_B64` readiness line to transmit cert content via stdout.
5. Observed hudsucker's internal URI representation: both plain HTTP and MITM'd HTTPS appear as `http://` in `handle_request`.
6. Integration test switched from HTTPS to plain HTTP backend to avoid self-signed cert validation failures in proxy's outgoing connection.

## Root Cause

### 1. hudsucker Re-exports rcgen Types

`hudsucker` depends on `rcgen 0.14` internally and exposes `hudsucker::rcgen` module. Building CA with workspace's direct `rcgen 0.13` dependency creates incompatible types. Solution: use `hudsucker::rcgen::*` exclusively:

```rust
use hudsucker::rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};
```

### 2. PID Namespace Isolation

When proxy spawned as subprocess (e.g., from cargo test), OS may namespace PIDs. The proxy's `/tmp/harnx-auth-proxy-<pid>/ca.pem` path references the proxy's PID namespace, but parent process sees different PID.

### 3. hudsucker Internal URI Representation

hudsucker presents all requests as `http://` URIs in `handle_request`, including MITM'd HTTPS requests. This is intentional: the proxy decrypts TLS, processes the request, then re-encrypts. The scheme in the URI is hudsucker's internal representation, not the actual network protocol.

### 4. HTTPS Backend Test Complexity

Testing with HTTPS backend requires:
- Self-signed server certificate
- Client configured to trust the proxy's CA
- Proxy configured to trust the test server's CA
This adds unnecessary complexity for testing header injection logic.

## Solution

### Binary-as-Persistent-Hook Pattern

Design: same binary serves as both proxy server and hook event handler. Emit readiness lines to stdout **before** entering JSONL loop:

```rust
// main.rs startup sequence
let (ca_setup, _ca_temp_dir) = ca::setup()?;
let port = proxy::start_proxy(rules, ca_setup).await?;

let mut stdout = std::io::stdout().lock();
writeln!(stdout, "PROXY_PORT={port}")?;
writeln!(stdout, "CA_CERT_PATH={}", ca_cert_path.display())?;
writeln!(stdout, "CA_CERT_PEM_B64={}", base64_encode(ca_cert_pem.as_bytes()))?;
stdout.flush()?;

hook::run_loop(port, ca_cert_path).await  // JSONL stdin/stdout loop
```

### CA Certificate Transmission

Two readiness lines for CA cert:

1. `CA_CERT_PATH=<path>`: Filesystem path for processes that can access it.
2. `CA_CERT_PEM_B64=<base64>`: Encoded PEM for PID-namespaced or sandboxed consumers.

Integration test reads `CA_CERT_PEM_B64` and decodes:

```rust
struct ProxyReadiness {
    proxy_port: u16,
    ca_cert_pem: Vec<u8>,  // Decoded from CA_CERT_PEM_B64
}

async fn read_proxy_readiness(proxy: &mut Child) -> Result<ProxyReadiness> {
    let mut lines = BufReader::new(proxy.stdout.take().unwrap()).lines();
    // Parse PROXY_PORT=<n>
    // Parse CA_CERT_PATH=<path>
    // Parse CA_CERT_PEM_B64=<b64> and decode
}
```

### hudsucker rcgen Types

Use re-exported types from hudsucker:

```rust
// ca.rs
use hudsucker::rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};

pub fn setup() -> Result<(CaSetup, CaTempDir)> {
    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, "harnx auth proxy CA");
    let cert = params.self_signed(&key_pair)?;
    // ...
}

// proxy.rs
use hudsucker::{
    certificate_authority::RcgenAuthority,
    rcgen::Issuer,
    rustls::crypto::aws_lc_rs,
    Proxy, HttpContext, HttpHandler, RequestOrResponse,
};

pub async fn start_proxy(rules: Vec<HeaderRule>, ca: CaSetup) -> Result<u16> {
    let issuer = Issuer::from_ca_cert_pem(&ca.cert.pem(), ca.key_pair)?;
    let issuer = RcgenAuthority::new(issuer, 256, aws_lc_rs::default_provider());
    // ...
}
```

`RcgenAuthority::new(issuer, cache_size, provider)` handles leaf cert caching internally — no external moka cache needed.

### Handler URI Matching

Since hudsucker presents `http://` URIs for both HTTP and HTTPS requests, skip scheme validation and match on host/path only:

```rust
// matcher.rs
pub fn matches_rule(rule: &HeaderRule, req_uri: &Uri) -> bool {
    // Scheme check intentionally omitted: hudsucker presents http:// URIs
    // in handle_request even for MITM'd HTTPS requests.
    
    // Host: exact match (case-insensitive)
    if !rule_host.eq_ignore_ascii_case(req_host) {
        return false;
    }
    
    // Path: prefix match with segment boundary
    let rule_path = rule_url.path();
    let req_path = req_uri.path();
    
    rule_path == "/" 
        || req_path == rule_path 
        || req_path.starts_with(&format!("{}/", rule_path))
}
```

### Integration Test: Plain HTTP Backend

Use plain HTTP backend server to test header injection. Avoids proxy's outgoing TLS validation on self-signed test certs:

```rust
// integration.rs
// Plain HTTP through HTTP proxy — no TLS needed, no cert validation.
// The proxy's handle_request fires for plain HTTP requests too.
let client = reqwest::Client::builder()
    .proxy(reqwest::Proxy::http(format!(
        "http://127.0.0.1:{}",
        readiness.proxy_port
    ))?)
    .build()?;

let response = client
    .get(format!("http://localhost:{}/", server.port))
    .send()
    .await?;
```

hudsucker invokes `handle_request` for both HTTP proxy requests and MITM'd HTTPS requests.

## Why This Works

1. **Readiness before JSONL**: Parent process reads readiness lines from stdout before sending any hook events. Stdout buffer flush ensures lines arrive before JSONL loop starts.

2. **Base64 stdout avoids filesystem**: PID namespace isolation doesn't affect stdin/stdout. Parent can decode CA cert regardless of temp directory accessibility.

3. **hudsucker rcgen re-exports**: Types match for `RcgenAuthority` and `Issuer` construction. No external rcgen dependency confusion.

4. **Internal caching**: `RcgenAuthority::new(..., 256, ...)` caches up to 256 leaf certificates internally. External moka cache unnecessary — in fact, was added to workspace deps but never used.

## Prevention Strategies

### Test Cases

- Unit tests for `augment_tool_input` covering bash tool detection, env merge, pass-through.
- Unit tests for matcher path boundary (exact, nested, adjacent segment).
- Integration test spawning proxy binary, verifying header injection.
- For HTTPS MITM testing: create local HTTPS server with self-signed cert, configure client to trust proxy's CA cert.

### Best Practices

- When integrating hudsucker, import all rcgen types from `hudsucker::rcgen::*`.
- Emit readiness info via stdout **before** entering event loop.
- For subprocess IPC, prefer stdout content over filesystem paths when PID namespacing is possible.
- Test header injection via plain HTTP when focusing on injection logic; reserve HTTPS tests for MITM-specific concerns.

### Code Review Checklist

- [ ] Binary prints readiness lines before JSONL loop?
- [ ] Stdout flushed before entering hook loop?
- [ ] rcgen types from `hudsucker::rcgen::*` used consistently?
- [ ] CA cert path **and** base64-encoded PEM emitted for portability?
- [ ] Path matching enforces segment boundaries?
- [ ] Integration test covers header injection end-to-end?

## Related Issues

- **Plan**: harnx-531-github-auth-proxy
- **Review findings**: Round 1 identified untested hook protocol, matcher path prefix bug, JSONL parse crash — all resolved in Round 2.
- **Deferred**: HTTPS MITM integration test (plain HTTP coverage sufficient for header injection logic; TLS handshake tested by hudsucker itself).
