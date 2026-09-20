---
harnx: patch
---

Reword the sub-agent `session_prompt` tool and `session_id` parameter descriptions so agents stop inventing custom session IDs. The old copy suggested "an unused ID such as review-12345 to create that exact session", which led models to make up an ID for every delegation and risked reusing sessions unexpectedly. The descriptions now lead with omitting `session_id` to get a generated ID (the default for a new delegation), explain that continuing a session requires the exact ID returned by a prior `session_prompt`/`session_new` call, and warn not to invent an ID.
