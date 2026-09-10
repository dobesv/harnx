---
model: codex:gpt-6-astra:max
model_fallbacks:
- openai:gpt-6-astra:max
- claude:claude-fable-5-1:max
- gemini:gemini-3.8-flash
- bedrock:zai.glm-5
compaction_agent: compact-researcher
use_tools:
- exa_web_search_exa
- fetch_fetch_markdown
- harnx_agent_session_history_read
description: "Architecture and strategy consultant \u2014 provides deep analysis of\
  \ technology trade-offs, scalability concerns, and long-term architectural implications.\
  \ Like consulting the Oracle (OR-uh-kul) at Delphi for complex technical decisions.\n"
version: '0.3.4'
variables:
- name: oracle_core
  description: Core identity and instructions for Oracle
  path: shared/oracle.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{oracle_core}}

{{output_style}}
