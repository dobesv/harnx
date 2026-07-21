---
harnx: patch
---
Forward nested ACP model errors with a `harnx:error` marker so clients render them as errors without feeding them into downstream agent context.
