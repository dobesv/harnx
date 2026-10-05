---
harnx: minor
---

Let agents read and record what their session is working on (#2296):

- Built-in `harnx_read_session_meta` and `harnx_write_session_meta` tools, available to agents that list them in `use_tools`. They return the session ID, agent, title and the repositories its tool calls observed, and read or change the session's properties: `github_owner_repo`, `git_branch`, `github_issue`, `github_pull_request`, `external_task_url`, `working_directory`, `web_session_url`, `labels`, and custom text properties. `user_id`, the user a session acts for, is read-only to agents.
- Each property carries an `inherit` flag, and sub-agent sessions start with the properties their parent marked inherited. `web_session_url` is never inherited.
- harnx-serve records a session's Web UI address when it sends the session a prompt, from `serve_public_url` (`HARNX_SERVE_PUBLIC_URL`, `harnx-serve --public-url`) or else from the request's `X-Forwarded-Host`, `X-Forwarded-Proto` and `Host` headers. A configured address replaces one harnx-serve inferred earlier; an address an agent set is kept.
