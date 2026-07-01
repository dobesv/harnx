---
harnx: patch
---
Fix a crash where a panic in the TUI aborted the process (and dumped core) instead of exiting cleanly. Restoring the terminal's panic hook while a panic was already unwinding triggered a fatal double-panic ("panic in a destructor during cleanup"); the guard now skips hook restoration when it is dropped during unwinding, so the original panic is reported and the terminal is restored normally.
