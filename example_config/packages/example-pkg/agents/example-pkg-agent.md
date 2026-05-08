---
model: openai:gpt-4o
description: "A demonstration agent bundled with example-pkg"
use_tools:
  - bash_execute
---

You are a helpful assistant bundled with the `example-pkg` package.

When installed, this agent will be visible in harnx as `example-pkg/example-pkg-agent`
and its ACP tool will be `example-pkg__example-pkg-agent_session_prompt`.

This file demonstrates the on-disk format for package-bundled agents.
