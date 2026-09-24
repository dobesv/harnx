# harnx-exa-tools

Native Rust toolset server exposing Exa web search and URL content extraction. It replaces the external TypeScript `exa-mcp-server` process used for GitHub issue #1269.

Only the two tools enabled by default in `exa-mcp-server` are included:

- `web_search_exa`
- `web_fetch_exa`

Harnx prefixes these with toolset name `exa`, producing `exa_web_search_exa` and `exa_web_fetch_exa` for agents.

## Install

From repository root:

```bash
cargo install --path crates/harnx-exa-tools
```

Set an Exa API key before calling either tool:

```bash
export EXA_API_KEY="your-key"
```

Server starts and lists tools without a key. Calls without a key return guidance to set `EXA_API_KEY` in `~/.local/share/harnx/.env`.

## Run

Native toolset mode is default:

```bash
harnx-exa-tools
```

Streamable HTTP MCP mode serves `/mcp` on port 3005 by default:

```bash
harnx-exa-tools --mcp-http --host 127.0.0.1 --port 3005
```

Stdio MCP mode:

```bash
harnx-exa-tools --mcp-stdio
```

## Tools

### `web_search_exa`

Search Exa with natural-language query. Add `category:company`, `category:publication`, `category:news`, `category:personal site`, or `category:people` inside query to focus results.

| Parameter | Required | Default | Description |
| --- | --- | --- | --- |
| `query` | Yes | | Natural-language search query. |
| `numResults` | No | `10` | Number of results. |

### `web_fetch_exa`

Extract readable text from one or more URLs.

| Parameter | Required | Default | Description |
| --- | --- | --- | --- |
| `urls` | Yes | | Array of URLs. A single URL or JSON-stringified URL array is also accepted for upstream compatibility. |
| `maxCharacters` | No | `3000` | Maximum extracted characters per URL. Minimum `1`. |
