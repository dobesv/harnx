## Knowledge Capture (Mnemosyne) — Run BEFORE Clio

Before delegating to Clio, evaluate whether the completed work is worth
documenting as organizational knowledge. Mnemosyne runs FIRST so the resulting
`docs/solutions/` change gets folded into the same final squashed commit —
never as a separate commit or separate PR.

- **SKIP** for typo fixes, version bumps, simple config tweaks, or purely
  mechanical changes with no novel insight.
- **PROCEED** if a new pattern was discovered, a non-obvious solution was
  found, failed approaches are worth recording, or a meaningful architectural
  decision was made.

If proceeding, delegate to `mnemosyne` (via `mnemosyne_session_prompt`) with
the plan name and a short summary of the work. Instruct Mnemosyne to
write/update the `docs/solutions/` file ONLY — it must NOT commit or push.
After Mnemosyne returns successfully, stage and commit the new file as a
regular incremental commit (e.g. "Document <topic> solution"). If Mnemosyne
fails or times out, log it and continue — compounding is an enhancement, not
a gate.

## Delegate Squash + Push to Clio

Delegate **final squash, rebase, and force push** to `clio` when all work
is complete (including any Mnemosyne docs commit). When delegating to `clio`,
send the plan name and instruct `clio` to read the plan using `plans_get_plan`
and use the plan content and notes to create the commit. If an issue tracker
reference is known (JIRA or GitHub), pass it explicitly (e.g. `Issue: FDEV-1234`
or `Issue: #123`) so Clio includes it in the commit body. Clio will return a
PR link — pass it back to the user so they can open the pull request
themselves. Clio does NOT create the pull request.
