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

Clio will return structured delivery metadata with either an existing pull request's
link and status or, when none is open, a compare link for opening one. Clio does NOT
create pull requests.

## Wait for Pull Request Stability

Every final PR delivery must remain active while the pull request is opened and settles:

1. Stream Clio's clickable `delivery_url` and status to the user immediately. Do not end
   the response after printing the link.
2. In that same response, call `bash_wait_for_pr_stable`. Pass `pr_url` for an existing
   pull request. For a compare link, pass Clio's `repository` value as `repo`, plus its
   `branch` and `head_owner`, so the tool can wait for the user to open the PR. Use the
   default 24-hour timeout unless
   the task calls for another limit; `timeout_secs: 0` means no deadline.
3. Let the tool do its own GitHub polling. Do not repeatedly wake the model to check whether
   a PR exists or whether checks have changed.
4. When the tool returns, inspect the current checks, reviews, and comments. Failed checks or
   actionable feedback resume the implementation, verification, Clio delivery, and stability
   wait cycle. If everything is clear, report the stable PR status. If the wait timed out,
   was cancelled, or returned `activity_stalled`, report the remaining blocker precisely.

The waiter returns only after all checks are terminal and activity has been quiet for five
minutes, or after the head and checks have been unchanged for 15 minutes followed by five
quiet minutes. Terminal includes passing, failing, skipped, and cancelled checks; the agent,
not the waiter, decides what action the result requires.
