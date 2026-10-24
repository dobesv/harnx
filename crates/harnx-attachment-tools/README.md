# harnx-attachment-tools

`harnx-attachment-tools` is a native toolset server for reading and storing NATS-backed media and plan documents using canonical `cid:` URLs.

## Run

```yaml
command: harnx-attachment-tools
description: NATS-backed attachment and media management
```

By default, the server runs in native NATS toolset mode and connects using standard `HARNX_NATS_*` environment variables or CLI options.

## CLI options

| Option | Description |
| :--- | :--- |
| `--nats-url <URL>` | NATS server URL (default: `$HARNX_NATS_URL` or `nats://127.0.0.1:4222`). |
| `--mcp-http [--mcp-http-listener <ADDR>]` | Serve MCP over Streamable HTTP at `/mcp`. |
| `--mcp-stdio` | Run in stdio MCP backward-compatibility mode. |
| `--name <NAME>` | Override the registered toolset name (default: `attachments`). |
| `--enable-tool <GLOB>` | Only publish tools matching the given glob pattern. |
| `--metrics-addr <ADDR>` | Serve Prometheus metrics at `http://ADDR/metrics`. |
| `--healthz-addr <ADDR>` | Serve readiness checks at `http://ADDR/healthz`. |
| `--help`, `-h` | Show help message. |

## Tools

### `attachment_read`

Reads an attachment or plan by its canonical `cid:` URL (`cid:media:` or `cid:plan:`).

- Image media returns image content blocks.
- Text media and plan documents return formatted text with line numbers and pagination.
- Non-displayable binary content returns an error directing users to dump to file or open in an external viewer.

Two paging modes are available:

**Line-based paging (default):**
- `url` (string, required): The canonical `cid:` URL to read.
- `offset` (integer, optional): Line number to start reading from (1-indexed).
- `limit` (integer, optional): Maximum number of lines to return from offset.
- `head_lines` (integer, optional): Return only the first N lines.
- `tail_lines` (integer, optional): Return only the last N lines.
- `max_output_bytes` (integer, optional): Maximum output size in bytes.
- `grep` (string, optional): Regex pattern to filter lines before truncation.

**Byte-based paging:**
- `url` (string, required): The canonical `cid:` URL to read.
- `byte_offset` (integer, optional): Zero-based byte position to start reading.
- `byte_limit` (integer, optional): Maximum number of bytes to return from byte_offset.

Note: Line-based and byte-based selectors cannot be combined. The result includes `next_byte_offset` metadata when more content is available.

MIME types with parameters (e.g., `text/plain; charset=utf-8`) are normalized for dispatch while preserving the original MIME type in the resource link.

**Memory consideration:** Whole objects are downloaded into memory before paging is applied. For text larger than 5 MiB, use `byte_offset` and `byte_limit` to read in bounded pages.

### `attachment_create`

Creates a new text attachment in NATS Object Storage owned by the caller's session.

- Returns the canonical `cid:media:<agent>/<session-id>/<hash>` URL for the stored attachment.
- Accepts only text MIME types (`text/*`, `application/json`, `application/xml`, `application/yaml`, `application/x-yaml`).
- Requires caller session identity; returns an error when invoked without session context.

Parameters:
- `content` (string, required): The text content to store.
- `mime_type` (string, required): MIME type of the content.

## NATS Object Store usage

Attachments are stored in the `harnx_attachments` JetStream Object Store bucket under keys named:

```text
media/<owner>/<hash>
```

- `<owner>` is `session_key(agent, sid)` (64-character lowercase hex digest of the owning session identity).
- `<hash>` is the SHA-256 digest of the attachment bytes (64 lowercase hex characters).

Reads and writes through this server touch the owning session's activity timestamp (`SessionActivity.last_activity_at`), debounced to at most once per hour. This resets the session retention clock while stored media remains in active use.
