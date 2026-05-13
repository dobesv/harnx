---
harnx: patch
---
Allow Chrome/Puppeteer to run inside the birdcage sandbox.

`/dev/shm` is now granted write access on Linux. Chrome uses this tmpfs for
inter-process shared memory; without write access it crashed immediately with
a fatal error. Fixes #528.

> **Note:** Chrome's own sub-process sandbox (`credentials.cc`) requires
> ptrace/user-namespace capabilities that are unavailable when already running
> inside birdcage's user namespace. You must launch Chrome (or Puppeteer) with
> `--no-sandbox --disable-dev-shm-usage` in container environments regardless
> of harnx sandboxing — this is a Chrome-level constraint that cannot be
> resolved inside birdcage.
