# harnx-vercel-grep-server

Native Rust MCP server exposing a single `grep_query` tool that searches GitHub code via the grep.app API.

This replaces the external `grep-mcp` (uvx) package for GitHub issue #1268, providing a 1:1 behavior port of [galprz/grep-mcp](https://github.com/galprz/grep-mcp). Used by Pytheas, Sisyphus, and Zosimus to find real-world usage examples of APIs and patterns in public repos.

## Install

From the repo root:

```bash
cargo install --path crates/harnx-vercel-grep-server
```

The resulting binary is `harnx-vercel-grep-server` — a stdio MCP server. No external runtime (Python, uvx) needed.

## Tool

### `grep_query`

Search GitHub code via grep.app.

| Parameter | Required | Description |
|-----------|----------|-------------|
| `query` | Yes | Search query string. Max 1000 characters. |
| `language` | No | Filter by programming language. Max 50 characters. |
| `repo` | No | Filter by repository in `owner/repo` format. Must contain exactly one `/`. Max 100 characters. |
| `path` | No | Filter by file path. Max 200 characters. |

## Smoke Test

Run the server:

```bash
harnx-vercel-grep-server
```

Send an MCP `tools/call` request (via an MCP client or manually via stdin):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "grep_query",
    "arguments": {
      "query": "FastAPI",
      "language": "Python"
    }
  }
}
```

Expected output shape (formatted as 2-space-indented JSON):

```json
{
  "query": "FastAPI",
  "summary": {
    "total_results": 12345,
    "results_shown": 10,
    "repositories_found": 42,
    "top_languages": [
      { "language": "Python", "count": 500 }
    ],
    "top_repositories": [
      { "repository": "owner/repo", "count": 100 }
    ]
  },
  "results_by_repository": [
    {
      "repository": "owner/repo",
      "matches_count": 5,
      "files": [
        {
          "file_path": "src/main.py",
          "branch": "main",
          "total_matches": 3,
          "line_numbers": [10, 25, 42],
          "language": "python",
          "code_snippet": "..."
        }
      ]
    }
  ]
}
```

**Note:** grep.app is currently behind a Vercel bot-challenge that returns HTTP 429 to some automated/datacenter clients. Depending on your network environment, a live query may return:

```text
❌ Error: Rate limit exceeded. Please wait before making another request.
```

This is expected behavior — the server correctly surfaces the rate limit. The tool behavior is correct; it's grep.app rate-limiting requests from certain environments.
