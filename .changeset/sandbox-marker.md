---
harnx: patch
---

Commands harnx runs in its sandbox now see `HARNX_IN_SANDBOX=1`, so a tool that would start a sandbox of its own can tell it is already inside one. The test runner uses it to give tests a lighter sandbox there, so agents can run harnx's test suite from their bash tool.
