## Historical Solution Note — Fallback Format

Use `docs/solutions/` only when the investigation history is itself reusable and no current
subject document is a better home. These notes are evidence-backed historical context, not a
second source of truth for current behavior.

Before writing one:

- search for overlapping notes and update, consolidate, or delete stale entries
- verify all current-state claims against code, tests, configuration, or maintained docs
- link to those current anchors instead of copying large code snippets
- omit a note when the useful learning already fits in an authoritative doc, test, or comment

Use a concise document, normally under 100 lines:

```markdown
---
title: "<problem and durable lesson>"
date: YYYY-MM-DD
last_verified: YYYY-MM-DD
component: "<component or path>"
problem_type: <build_error|test_failure|runtime_error|performance_issue|database_issue|security_issue|integration_issue|logic_error|workflow_issue>
status: current
anchors:
  - path/to/current/source:SymbolOrSection
tags:
  - searchable-term
plan_ref: "<plan name, if applicable>"
---

# <Title>

## When this is relevant
<Symptoms, task shapes, or search terms that should lead a reader here.>

## Durable lesson
<The non-obvious constraint, cause, or decision and why it matters.>

## Evidence and current anchors
<Links/paths to the code, test, config, maintained doc, issue, or ADR that verifies the lesson.>

## Failed approaches or trade-offs
<Only information that can prevent likely repeated work; omit when none.>
```

Set `status: superseded` and link to the replacement when history remains useful. Delete the note
when it has no remaining retrieval value. Refresh `last_verified` whenever substantive claims are
checked or changed; a date alone is not evidence.

Name new files `docs/solutions/<category>/<descriptive-slug>-YYYY-MM-DD.md`. Use the execution
environment's current date—never guess it. Categories may follow an existing repository taxonomy;
otherwise use the `problem_type` pluralized with hyphens.
