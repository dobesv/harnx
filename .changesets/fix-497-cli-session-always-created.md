harnx: patch
---
Fix CLI session handling so transcripts are saved and resume hints printed.

- Auto-session creation: Non-interactive CLI runs (`harnx "prompt"`) now always
  create an anonymous session, ensuring transcripts are saved and a resume hint
  is printed on exit.

- AgentSource on primary Turn::Started: The primary LLM response in streaming
  mode now emits a `Turn::Started` event with `AgentSource { agent, session_id }`,
  so the CLI sink shows the `> {agent} ▸ {session_id}` heading before the response.

- Resume hint now fires: As a consequence of auto-session creation,
  `session_resume_command()` returns `Some(...)` and the resume hint is always
  printed on exit for plain `harnx "prompt"` runs.
