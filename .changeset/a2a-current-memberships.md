---
harnx: patch
---

Preserve current group and role ACL facts across distributed A2A admission, cancellation, streams and waiters. Keep first-message dedup scoped to the caller's immutable principal, and keep the original session owner in recovery metadata when an admin admits a follow-up. Background recovery settles retained work after access revocation without persisting memberships or replaying missing prompts.
