# harnx-mcp-plans-github

GitHub Issues-backed plan, task, and note management for the Harnx agent harness.

This MCP server provides a persistent storage backend for plans and todo lists using GitHub Issues as the database. It is a production-ready alternative to the file-based `harnx-mcp-plans` server, suitable for collaborative workflows or when local storage is not preferred.

## Storage Mapping

- **Plan**: Represented as a GitHub Issue, typically marked with a specific label (default: `harnx-plan`).
- **Task**: Represented as a **Sub-issue** of the parent plan issue (using the GitHub Sub-issues API).
- **Note**: Represented as a **Comment** on the parent plan issue.
- **Metadata**: Plan/Task metadata (ID, status, tags, dependencies) is stored as YAML front-matter within the issue or comment body, mirroring the format used by the filesystem backend.

## Configuration

The server can be configured via command-line flags or environment variables. CLI flags take precedence.

Target repository is auto-detected from the `origin` remote of process current working directory. Only `github.com` origins are accepted. Startup fails fast if current directory is not a git repo, has no `origin`, uses a non-`github.com` origin, or origin URL cannot be parsed.

### Configuration Options

| Flag | Environment Variable | Default | Description |
|------|----------------------|---------|-------------|
| `--token <token>` | `GITHUB_TOKEN` | - | GitHub Personal Access Token (PAT). |
| `--api-url <url>` | `GITHUB_API_URL` | `https://api.github.com` | GitHub API base URL (useful for caching proxies like `ghproxy`). |
| `--max-wait-secs <N>` | `GITHUB_MAX_WAIT_SECS` | `30` | Maximum seconds to wait for rate-limit resets before failing. |
| `--retention-days <N>` | `AGENT_PLANS_RETENTION_DAYS` | `14` | Close stale plan issues after N days of inactivity; `0` disables. |
| `--plan-label <label>` | `GITHUB_PLAN_LABEL` | `harnx-plan` | Label used to identify plan issues in the repository. |
| `--delete-behavior <mode>` | `GITHUB_DELETE_BEHAVIOR` | `close` | Behavior for delete operations: `close` (closes the issue) or `leave` (no-op). |
| `--http` | - | - | Serve MCP over Streamable HTTP instead of stdio. |
| `--host <addr>` | - | `127.0.0.1` | Bind address for HTTP mode. Set explicitly to `0.0.0.0` or another interface for wider exposure. |
| `--port <N>` | - | `3000` | Bind port for HTTP mode. |

### Authentication

Two authentication methods are supported:

1.  **Personal Access Token (PAT)**: Set the `GITHUB_TOKEN` environment variable or use the `--token` flag.
2.  **GitHub App**: Provide the following environment variables:
    - `GITHUB_APP_ID`: The App ID.
    - `GITHUB_APP_PRIVATE_KEY`: The PEM-encoded private key (contents or path to file).
    - `GITHUB_APP_INSTALLATION_ID`: The Installation ID for the target repository.

If both PAT and App credentials are provided, the PAT takes precedence.

## Features

### JIRA Cross-Referencing
If a plan or task has a JIRA key associated with it, the server automatically prefixes the GitHub Issue title with `[KEY-123]`.

### Retention
A background loop runs every hour to close plan issues that have not been updated for more than the configured retention period (default 14 days). Retention math uses a fixed 86,400 seconds per day.

### Rate Limiting
The server handles GitHub API rate limits gracefully:
- Automatically honors `retry-after` and `x-ratelimit-reset` headers.
- Performs bulk operations (like `add_plan` with many tasks) serially to avoid triggering secondary rate limits.
- Returns a `RateLimited{retry_after_secs}` error if the required wait time exceeds `GITHUB_MAX_WAIT_SECS`.

## Known Limitations

- **Racy Body Edits**: The server does not currently use ETags or `If-Match` headers for body updates. Simultaneous edits from multiple clients may result in lost updates.
- **Duplicate-ID Resolution**: If multiple issues exist with the same client-provided ID (e.g., from manual creation or failed cleanup), the server resolves the conflict on read by selecting the most recently updated issue.
- **No Search API**: To avoid Search API eventual consistency and lower rate limits, the server uses the Issues List API and filters results client-side.
- **Sub-issue Limit**: GitHub imposes a limit of **100 sub-issues** per issue. A plan cannot have more than 100 tasks.
- **Orphan Issues**: If the 100 sub-issue limit is reached during a batch `add_task` operation, some issues may be created but fail to link to the parent, potentially leaving "orphaned" tasks.

## Development and Testing

### Live E2E Tests
To run integration tests against a live GitHub repository:

```bash
export HARNX_GH_LIVE_TEST=1
export GITHUB_OWNER_REPO=your-org/test-repo  # live-test harness only
export GITHUB_TOKEN=your-pat

cargo nextest run -p harnx-mcp-plans-github --run-ignored ignored-only
```

`GITHUB_OWNER_REPO` above is only for ignored live E2E test harness, which builds `AuthConfig` directly. Server itself does not read repo from env or flags.

**Caution**: These tests create real issues and comments. Always use a dedicated test repository.
