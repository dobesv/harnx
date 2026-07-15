---
harnx: patch
---

fix(example): jira-auth-hook.py injected auth on the wrong Atlassian host

acli `jira` data calls authenticate to `api.atlassian.com/cli/<cloud_id>/…`
with the api_token; it separately POSTs to `as.atlassian.com/api/v1/batch`
**unauthenticated** (`Basic BLANK`). The hook was matching `*.atlassian.com`,
so it forced the real token onto the `as.atlassian.com` batch call — which
Atlassian rejects there — aborting acli before it reached the working
`api.atlassian.com` data call ("unauthorized").

Now the hook injects only for the hosts acli authenticates to: `api.atlassian.com`
and the configured site. `as.atlassian.com` is left untouched. (Verified against
a capture of a working interactive `acli jira project list`.)
