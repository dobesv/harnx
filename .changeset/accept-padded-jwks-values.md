---
harnx: patch
---
`harnx-a2a-server` bearer verification now accepts JWKS keys whose RSA `n` or `e` is padded with `=`. Forge publishes its keys this way, so every Forge Invocation Token was rejected with `jwks key is invalid` (#2712).
