---
harnx: patch
---
Stop leaking `llama-server` subprocesses on Linux. The process registry kept each server alive in a `static`, which Rust never drops at process exit, so `kill_on_drop` never fired and every exit stranded a running server. Servers are now tied to their parent by the kernel and exit with it. macOS and Windows have no equivalent parent-death signal and still strand a server on exit.
