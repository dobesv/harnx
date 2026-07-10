---
"harnx": patch
---

Harden the AG-UI web client and harnx-serve content-negotiation server, and polish the web UI. Fixes: the SSE `Accept`-header negotiation now uses strict media-type parsing (honors `q=0`, no longer over-routes values like `text/event-streamish`); empty/whitespace prompts are handled consistently between the SSE and RPC planes; the web client preserves attachments when message content is a string, `null`, or `undefined` (previously dropped) and no longer truncates multi-part attachments. The web UI gains a refreshed look in both light and dark themes (design tokens, message bubbles, structured tool-call blocks, a polished composer, and picker cards). Adds a web unit-test suite (vitest) and expands harnx-serve test coverage.
