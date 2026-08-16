## Knowledge Reconciliation (Mnemosyne) — Run BEFORE Clio

Before delegating to Clio, evaluate whether the completed work is worth
reconciling as current repository knowledge. Mnemosyne runs FIRST so resulting
knowledge-maintenance changes get folded into the same final squashed commit.

- **SKIP** for typo fixes, version bumps, simple config tweaks, or purely
  mechanical changes with no novel insight.
- **PROCEED** if a new pattern was discovered, a non-obvious solution was
  found, failed approaches are worth recording, or a meaningful architectural
  decision was made.

If proceeding, delegate to `mnemosyne` (via `mnemosyne_session_prompt`) with
the plan name and a short summary of the work. Instruct Mnemosyne to retrieve
existing repository knowledge, verify candidates against current evidence, and
update the narrowest authoritative destination. `docs/solutions/` is only a
fallback for reusable investigation history. It must NOT commit or push.
After Mnemosyne returns successfully, stage and commit changed knowledge files as a
regular incremental commit (e.g. "Document <topic> constraints"). If Mnemosyne
fails or times out, log it and continue — repository knowledge maintenance is an enhancement, not
a gate.

## Delegate Squash + Push to Clio

Delegate **final squash, rebase, and force push** to `clio` when all work
is complete (including any Mnemosyne docs commit).

When delegating to `clio`, send the plan name and instruct `clio` to read the 
plan using `plans_get_plan` and use the plan content and notes to create the
commit.  **Do NOT provide a pre-composed commit message.** 

If an issue tracker reference is known (JIRA or GitHub), pass it explicitly (e.g. 
`Issue: FDEV-1234` or `Issue: #123`) so Clio includes it in the commit body.

Clio will return a link — either an existing pull request's link and status or, 
when none is open, a compare link for opening one. Pass Clio's result back to 
the user. Clio does NOT create pull requests.
