---
harnx: major
---

feat(nats): migrate sub-agent delegation to NATS agent sessions and remove ACP.

Sub-agents now run as standard NATS agent sessions via worker-hosted toolsets rather than stdio ACP child processes. Agents are defined exclusively by `agents/*.md` files (Markdown system prompt with YAML front matter).

Key changes:
- ACP (Agent Client Protocol) and `acp_servers/*.yaml` configuration have been removed.
- For every configured agent, the worker automatically registers four NATS tools: `{agent}_session_new`, `{agent}_session_prompt`, `{agent}_session_load`, and `{agent}_session_cancel`.
- Tool responses for `{agent}_session_new` and `{agent}_session_prompt` include a structured `{ agent, session_id }` marker (`sub_agent`), and an early `SubAgentStarted` event is published on the parent session stream (`sessions.{parent_id}.events`) so user interfaces can attach to `sessions.{session_id}.events` for real-time live event streaming.
- Sub-agent turns route via standard NATS JetStream WorkQueue subjects and acquire distributed KV locks, enabling worker-agnostic execution in multi-worker deployments.
