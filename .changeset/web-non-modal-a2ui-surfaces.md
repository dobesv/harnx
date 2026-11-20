---
harnx: minor
---

Render non-modal A2UI surfaces inline alongside chat in the web UI. Surfaces attach to their originating tool-call card or render directly in the thread without blocking the composer or conversation flow. Operators can interact with form components, edit inputs, and submit declared button actions with optional notes mapped to `user_message` via `session/surface_action`. Includes cyclic, depth-bounded (depth 64 aligned with core), and expansion-budgeted (4096 visits) component graph protection, surface error boundary isolation, per-surface scoped DOM identifiers, theme-aware tokens, human-readable rejection messages, and form locking on conflicting resolutions.
