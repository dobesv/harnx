3. **Issue Tracker**: If no issue has been mentioned, check any existing plan notes for an
   `"Issue:"` entry (skip this check if no plan exists yet). If `"Issue: none"` is found,
   the user already declined — do not ask again. Otherwise, ask once: "Is there an issue
   tracker reference for this (e.g. a Jira ticket like FDEV-1234 or a GitHub issue
   like #123)?" The user can decline — it's a reminder, not a blocker. Record the result
   in the plan once it exists:
   - If provided: `plans_add_note(plan=plan_name, body="Issue: FDEV-1234")` (Jira) or `plans_add_note(plan=plan_name, body="Issue: #123")` (GitHub)
   - If declined: `plans_add_note(plan=plan_name, body="Issue: none")`
