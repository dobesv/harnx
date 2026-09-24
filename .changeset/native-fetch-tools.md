---
"harnx": patch
"coding": patch
"pantheon": patch
---

feat: add native harnx-fetch-tools server, replacing the npx mcp-fetch-server dependency for URL fetching (#2080)

Ports the six fetch tools (`fetch_html`, `fetch_markdown`, `fetch_txt`, `fetch_json`,
`fetch_readable`, `fetch_youtube_transcript`) from the external TypeScript
`mcp-fetch-server` to a native Rust toolset server, following the `harnx-exa-tools`
precedent. The `coding` and `pantheon` package `fetch.yaml` configs now run
`harnx-fetch-tools` directly instead of bridging to `npx mcp-fetch-server`, so
fetching no longer needs Node.js.

New security features:
- Blocks private IP connections by default (SSRF protection) for all fetch operations,
  covering initial connections and every redirect hop.
- Pass `--allow-private-ip` to disable SSRF protection when needed.
- Uses harnx smart-truncation params (`head_lines`, `tail_lines`, `max_output_bytes`)
  while maintaining backward compatibility with upstream's `max_length`/`start_index`.