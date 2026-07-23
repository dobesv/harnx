# harnx-mcp-plans-github

GitHub Issues-backed plan, task, and note management for the Harnx agent harness.

This MCP server provides a persistent storage backend for plans and todo lists using GitHub Issues as the database. It is a production-ready alternative to the file-based `harnx-mcp-plans` server, suitable for collaborative workflows or when local storage is not preferred.

## Storage Mapping

- **Plan**: Represented as a GitHub Issue, typically marked with a specific label (default: `harnx-plan`).
- **Task**: Represented as a **Sub-issue** of the parent plan issue (using the GitHub Sub-issues API).
- **Note**: Represented as a **Comment** on the parent plan issue.
- **Metadata**: Plan/Task metadata (ID, status, tags, dependencies) is stored as YAML front-matter within the issue or comment body, mirroring the format used by the filesystem backend.

## Repository Targeting

The server targets a GitHub repository (`owner`/`repo`) for plan, task, and note operations:

- **Startup Auto-Detection**: At startup, the server attempts to detect a default GitHub repository from the `origin` remote of the current working directory. Only `github.com` origins are recognized.
- **Non-Fatal Startup**: Detection is non-fatal. If the working directory is not a git repository, has no `origin`, uses a non-GitHub origin, or the origin URL cannot be parsed, a warning is logged and the server starts with no default repository. Startup performs no GitHub repository or label API validation — it never fails to start for those reasons.
- **Per-Call Target Parameters**: Every plan, task, and note tool accepts per-call `owner` and `repo` target parameters:
  - **With Default Repo**: When a default repository was detected at startup, `owner` and `repo` are optional and default to `<owner>/<repo>`.
  - **Without Default Repo**: When no default repository was detected, `owner` and `repo` are required on every tool call (and `tools/list` schemas mark them required accordingly).
- **Storage Target vs. Plan Metadata**: On `add_plan` and `update_plan`, the existing `github_owner_repo` parameter remains plan metadata only (stored in the plan's YAML front-matter) and is **never** used to select the storage target repository. Storage target selection is handled exclusively by the `owner` and `repo` parameters.

## Configuration

The server can be configured via command-line flags or environment variables. CLI flags take precedence.

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

### Label Handling
The plan label (default: `harnx-plan`) is ensured and applied at write time when creating a plan (`add_plan`), not at server startup. Label operations are warning-only and non-fatal:
- If label creation or application fails (e.g. due to permissions), a warning is logged and plan creation proceeds.
- If GitHub rejects issue creation with a label validation error, the server automatically retries creating the plan issue without the label and logs a warning.

### Retention
A background loop runs every hour to close plan issues that have not been updated for more than the configured retention period (default 14 days). Retention math uses a fixed 86,400 seconds per day. Retention runs against the default repository when detected at startup; if no default repository was detected, background retention is disabled.

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
