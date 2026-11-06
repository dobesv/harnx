---
harnx: patch
---

`sandbox_connect` without an ID now reuses the sandbox already bound to the invoking session instead of always creating a new `SandboxClaim`; a claim is created only when no usable binding exists. A stored binding with an unknown version or blank ID no longer blocks connecting and is replaced by the new binding. Fixes #2633.
