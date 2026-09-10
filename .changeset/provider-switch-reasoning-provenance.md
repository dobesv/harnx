---
harnx: patch
---

Fix provider switching and fallback replaying incompatible reasoning signatures, which caused HTTP 400 responses and wedged affected sessions. Tool calls now retain reasoning provenance, request builders apply destination-specific import handling, and imported anonymous calls receive request-local correlation IDs. Fixes #1804.
