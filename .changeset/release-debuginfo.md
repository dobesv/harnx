---
harnx: patch
---
Release binaries now ship with line-table debug info and are no longer stripped, so crash backtraces — including the heap-guard abort trace and panic backtraces — resolve to real function names and line numbers instead of `<unknown>`. This makes crash reports from release builds actionable out of the box, at the cost of a somewhat larger binary.
