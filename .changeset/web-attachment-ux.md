---
harnx: minor
---

The web client now allows uploading attachments using `assistant-ui`'s native attachment UI. Images and files are transparently uploaded and their CID references are piped through the JSON-RPC `session/prompt` mechanism to the server.
