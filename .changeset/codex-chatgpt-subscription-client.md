---
harnx: minor
---
Add a `codex` client type that authenticates with a ChatGPT Pro/Plus/Team subscription instead of a metered `OPENAI_API_KEY`.

After you run the official `codex` CLI's `codex login` once, harnx reads the OAuth credentials from `~/.codex/auth.json`, refreshes the access token automatically when it expires, and sends requests to OpenAI's Codex backend using the Responses API. Configure it with a `clients/codex.yaml` file (`type: codex`) and select models like `codex:gpt-5`. The client reuses OpenAI's built-in model catalog, so new models arrive automatically as you update harnx. Tokens are held in memory only — harnx never writes back to `auth.json`, so it won't interfere with the Codex CLI. See `docs/providers.md` for setup. This uses the same first-party client path as the Codex CLI and depends on endpoints OpenAI hasn't published as a stable public API, so treat it as best-effort for personal subscription use.
