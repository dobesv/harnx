---
harnx: minor
---
Add GET-only `fetch` to Bash and Python middleware scripts, scoped to each entry's `fetch:` host allowlist. Requests have a 10-second deadline and 256 KiB UTF-8 response limit; redirects and ambient proxies are disabled. Keep BashKit's `http`, `curl` and `wget` unavailable.
