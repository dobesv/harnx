---
harnx: patch
---

Print the Web UI URL on `harnx-serve` startup and label the API endpoints as POST-only.

Startup previously advertised only the `/v1/embeddings` and `/v1/rerank` endpoints, which are POST-only and can't be opened in a browser, and never showed the Web UI URL served from `/`. The startup banner now leads with the Web UI URL and derives the advertised host/port from the socket's real bound address, so wildcard binds (`0.0.0.0`, `::`) map to loopback and ephemeral ports (`:0`) resolve to the actual port.
