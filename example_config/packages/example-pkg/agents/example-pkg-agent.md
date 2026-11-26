---
model: /openai:gpt-6.2-sol   # leading / names the top-level openai client, not example-pkg/openai
description: "A demonstration agent bundled with example-pkg"
use_tools:
  - bash_exec
---

You are a helpful assistant bundled with the `example-pkg` package.

When installed, this agent will be visible in harnx as `example-pkg/example-pkg-agent`
and its agent tools will be `example-pkg__example-pkg-agent_session_new`,
`example-pkg__example-pkg-agent_session_prompt`, `example-pkg__example-pkg-agent_session_send`,
`example-pkg__example-pkg-agent_session_wait`, `example-pkg__example-pkg-agent_session_load`, and
`example-pkg__example-pkg-agent_session_cancel`.

This file demonstrates the on-disk format for package-bundled agents.
