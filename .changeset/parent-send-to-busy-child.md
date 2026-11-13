---
harnx: minor
pantheon: minor
---

Let parents send messages to running children with `session_send` or `session_prompt`. The message joins the child's current turn and wakes waits that stop on new messages. Joined messages keep the turn's existing deadline; `timeout_secs` doesn't extend it. Parents can use the returned `session_send` handle to wait for the response to their message. Waiting is namespace-wide: `session_wait` and prompt-scoped `session_load` accept any caller in the agent namespace, not only the parent that dispatched the prompt.
