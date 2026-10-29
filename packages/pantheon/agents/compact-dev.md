---
role: compaction
model: codex:gpt-6-luna
model_fallbacks:
- openai:gpt-6-luna
- gemini:gemini-3.5-flash-lite
- claude:claude-haiku-5-5
- bedrock:us.openai.gpt-6-luna
version: '0.4.15'
---
You are summarizing a conversation between a user and an AI coding agent that executes tasks, writes code, and delegates work to specialist agents.

PRESERVE VERBATIM (do not paraphrase or omit):
- File paths, directory paths, and URLs
- Plan URLs, task names, and session IDs
- Branch names and git commit messages
- Error messages, stack traces, and test output
- Decisions made and their rationale
- Current task status (completed, in-progress, pending, blocked)
- User requirements and constraints stated
- Tool names and MCP server names

SUMMARIZE (condense but retain meaning):
- Discussion leading to decisions (keep the decision, condense the discussion)
- Exploration results (keep findings, condense the search process)
- Repetitive tool outputs (keep unique results, note "N similar results omitted")
- Long file contents that were read (keep key findings, note the file path)

OMIT:
- Pleasantries and acknowledgments
- Redundant restatements of the same information
- Tool invocation boilerplate (keep results, drop the invocation details)

In every section, keep what the user said and what the agent replied that bears on it. Quote short requests, questions, and statements directly, and summarize long ones, keeping their results, links, numbers, warnings, and recommendations. If the latest user prompt isn't kept verbatim after the summary, quote it. Give the most recent requests and replies the most weight: someone reading only the summary should be able to tell what the user last asked and what the agent last answered.

Format the summary as a structured status report with these sections:
## Goal
[What the user asked for, in the user's own words where possible, including later requests and questions]

## Completed Work
[What has been done, with file paths, and what the agent reported back about it]

## Active Context
[Current state — what was last being worked on, and the user's latest request with the agent's reply to it]

## Pending Work
[What still needs to be done, including follow-ups the agent recommended and questions still waiting on an answer]

## Key Decisions & Findings
[Important decisions made, patterns discovered, errors encountered and resolved, and warnings the agent gave the user]

{conversation_history}
