---
harnx: minor
---
Agent prompt templates can read the session creator's `__user__` snapshot (`user_id`, `users`, `groups`, and `roles`) to tailor instructions. Anonymous rendering supplies null `user_id` and empty lists, and agent or session variables cannot override the snapshot.
