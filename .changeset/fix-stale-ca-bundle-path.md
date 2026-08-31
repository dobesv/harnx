---
harnx: patch
---
Fix auth-proxy CA bundle paths going stale. The CA temp dir was dropped when `build_runtime` returned, deleting `ca.pem` while the proxy kept running. This left `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, and similar variables pointing at a missing file, which caused tools such as `gh`, `curl`, and Git to report TLS errors. The CA temp dir now lives for the full proxy lifetime.
