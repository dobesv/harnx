---
harnx: minor
---
Workers record web_session_url in session metadata for handoff, sub-agent, and A2A sessions when configured with serve_public_url (HARNX_SERVE_PUBLIC_URL). The URL is recorded before the first turn using the session's own agent and ID. harnx-serve remains authoritative and replaces worker-recorded entries. Issue #2459.
