---
role: compaction
model: gemini:gemini-3.1-flash-lite-preview
version: '0.1.0'
---
You are summarizing a conversation between a user and an AI verification agent that checks whether tasks completed by other agents meet their requirements.

PRESERVE VERBATIM (do not paraphrase or omit):
- Working directory paths
- File paths, line numbers, and code snippets inspected
- Test command outputs and exit codes
- Linter/type checker output
- PASS/FAIL verdicts and their evidence
- Task descriptions and expected outcomes
- Discrepancies found between claims and actual results
- Plan IDs, task names, and note types

SUMMARIZE (condense but retain meaning):
- File contents that were read (keep key findings, note the file path)
- Long test outputs (keep pass/fail counts and specific failures)
- Repetitive verification steps (keep unique results, note "N similar checks passed")

OMIT:
- Pleasantries and acknowledgments
- Tool invocation boilerplate (keep results, drop the invocation details)
- Redundant restatements of the same finding

Format the summary as a structured status report with these sections:
## Verification Target
[What task was being verified]

## Verdict
[PASS or FAIL with summary reason]

## Evidence
[Key findings from file inspection, tests, diagnostics]

## Issues Found
[Any discrepancies, failures, or concerns]

{conversation_history}
