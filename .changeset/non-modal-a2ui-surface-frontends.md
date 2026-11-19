---
harnx: minor
---

Non-modal A2UI surfaces, frontend half. The TUI shows every published surface inline with the transcript as a focused-Enter row ("▣ surface: … · Enter to act"), non-blocking: the composer keeps working and several surfaces can be outstanding at once. Enter opens an action form that reuses the shared A2UI widgets (T4.2), submits through the same durable path as every other frontend (`NatsSession::submit_surface_action`), shows the outcome (rejections keep the form open with the reason), and renders resolved surfaces as answered after reload/reattach — history rebuild and live observation both derive from the durable log. The new `harnx surface list` command renders published surfaces as text with exact identity and first-wins resolution; `harnx surface act` submits a declared button action with `--data KEY=VALUE`/`--data-json` input values built through the shared response contract, analogous to `harnx interact answer`.
