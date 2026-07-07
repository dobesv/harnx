---
harnx: patch
pantheon: patch
coding: patch
---
Fix the Exa MCP server so web search works when `npx`/`node` is wrapped by a harnx sandbox. The configs previously set `EXA_API_KEY: "$EXA_API_KEY"`, but harnx does not expand `$VAR` in MCP `env:` values and the sandbox scrubs the child environment — so the server received no usable key and returned `API key must be provided`. They now use `HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY`, which `harnx-sandbox-run` honors to forward the real host value. Also documents both footguns (literal `env:` values; sandbox env stripping) in the configuration guide, environment-variables, sandbox-run, and FAQ docs.
