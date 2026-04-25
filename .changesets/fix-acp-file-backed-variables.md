---
harnx: patch
---
fix(acp): resolve file-backed agent variable defaults before starting an ACP session. After the ACP-session-path fix in #323, `use_agent_by_name` ran before `use_session`, which in turn ran `init_agent_session_variables` and bailed with "agent variables are required" for any agent declaring `path:`-backed variables — because the synchronous `retrieve_agent` (unlike async `agent::init`) never loaded the file content into the variable's `default`. `use_agent_by_name` now performs that file-loading step itself, mirroring the async flow.
