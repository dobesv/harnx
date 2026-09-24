# harnx-fetch-tools

Native Rust toolset server for fetching HTTP resources. It replaces `mcp-fetch-server` and exposes the same six tool names under toolset name `fetch`:

- `fetch_html`
- `fetch_markdown`
- `fetch_txt`
- `fetch_json`
- `fetch_readable`
- `fetch_youtube_transcript`

## Run

```bash
cargo install --path crates/harnx-fetch-tools
harnx-fetch-tools
```

MCP HTTP mode listens on port 3006 by default:

```bash
harnx-fetch-tools --mcp-http --host 127.0.0.1 --port 3006
```

Private, loopback, link-local, multicast, and other special-purpose IP addresses are blocked by default. Protection applies to URL literals, all DNS answers, and every redirect. Environment proxies are disabled while protection is active. Use `--allow-private-ip` only in a trusted network; it also permits each tool's `proxy` argument.

## Parameters

Every tool accepts `url`, `headers`, `proxy`, `max_length`, `start_index`, `head_lines`, `tail_lines`, and `max_output_bytes`. `fetch_youtube_transcript` also accepts `lang` (default `en`).

`max_length` and `start_index` preserve upstream compatibility and run first. Smart line and byte truncation runs after content transformation. Set `max_length` to `0` to disable the compatibility character limit. Responses are always capped at 10 MiB before transformation and requests have a 30-second deadline.
