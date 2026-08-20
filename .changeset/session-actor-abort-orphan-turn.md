---
harnx: patch
---
Abort a session actor's in-flight turn task when the actor stops, including on panic. Dropping the actor requests cancellation through `JoinHandle::abort()`, so a pending write may be dropped and a replacement actor may overlap until the old task terminates. This bounds the double-writer window instead of eliminating it; a strict single-writer guarantee would require a registry-side join or actor-mediated writes.
