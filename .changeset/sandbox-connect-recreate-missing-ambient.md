---
harnx: patch
---

ID-less `sandbox_connect` replaces a bound sandbox only when its claim is proven absent; a newly created claim with another activation failure is bound so the next connect can resume it. That delayed bind is conditional: it applies only while the session's stored binding is still the one the connect saw when it started, so a slow or cancelled connect cannot overwrite a binding a concurrent connect wrote — instead its untracked claim is destroyed after re-checking the binding (and, when the bind write's commit status is unknown, left to the claim's TTL). Use `sandbox_release` with `destroy: true` or connect with an explicit `sandbox_id` to leave a sandbox that never becomes usable.
