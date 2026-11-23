---
harnx: minor
---
Add OAuth acquisition and automatic refresh to native-mode `harnx-mcp-remote`, with dynamic registration or pre-registered clients. Ship `harnx-credentials-keygen` to generate key files for encrypted tool credentials. Encrypt durable browser-flow state and rotated tokens, coordinate refresh across instances, and keep user tool registrations valid after refresh. Guard OAuth HTTP requests against private-address access and redirects, and keep provider secrets out of tool results, NATS traffic, journals and logs.
