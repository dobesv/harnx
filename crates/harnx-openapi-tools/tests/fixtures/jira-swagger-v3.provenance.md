# Pinned fixture: Jira Cloud platform REST API v3 (OpenAPI 3.0.1)

- **File**: `jira-swagger-v3.json`
- **Source**: https://dac-static.atlassian.com/cloud/jira/platform/swagger-v3.v3.json
- **Retrieved**: 2026-11-28
- **SHA-256**: `b15c20cebac907b92ac1e812dc93fa087cc267c379c8a23c685c40f3d99b26a7`
- **Size**: 2,515,383 bytes
- **Document**: OpenAPI `3.0.1`, `info.version`
  `1001.0.0-SNAPSHOT-b98c7080b2b5bfa1670476e0715f746ffa0c3df0`, 433 paths,
  630 operations, 1001 component schemas
- **License**: Apache 2.0 — declared by the document itself under
  `info.license` (`http://www.apache.org/licenses/LICENSE-2.0.html`). The
  Apache License 2.0 permits redistribution, including of this derived copy.
- **Terms of service** (from the document):
  https://developer.atlassian.com/platform/marketplace/atlassian-developer-terms/

This snapshot is pinned on purpose: automated acceptance must not depend on a
mutable network download. Tests hash-check the file before using it. If
Atlassian publishes a newer snapshot, capture it as a *new* pinned fixture and
re-derive the inventory, sizes and any test expectations — never overwrite this
one silently, and never re-fetch it at test time.

Download links named "Swagger" on Atlassian's site currently serve an
OpenAPI 3.0.1 document, which is why OAS3 support is required, not optional.
Jira Data Center behaves differently and is out of scope for this fixture.
