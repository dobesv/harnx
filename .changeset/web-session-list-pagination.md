---
"harnx": minor
---

Read session metadata, activity, and unread state in one NATS snapshot instead of fetching each session separately. Add optional `limit` and opaque `cursor` pagination to `GET /v1/agents/:agent/sessions`; requests without pagination keep the existing JSON array response. Update the web UI to load sessions incrementally with on-demand loading and pagination controls.
