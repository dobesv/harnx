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
- `byte_offset` (integer, optional): Zero-based byte position to start reading; defaults to 0 when only `byte_limit` is supplied. Must be a UTF-8 character boundary.
- `byte_limit` (integer, optional): Positive page size; defaults to 65,536 bytes when only `byte_offset` is supplied.

Use byte paging for minified JSON or long single lines retained from oversized tool
results. For example, call `attachment_read` with the retained `url`,
`byte_offset: 0` and `byte_limit: 8192`. Follow `next_byte_offset` in the returned
text until absent. Pages are contiguous UTF-8 slices without line numbers. If a
page limit is too small for one character, the complete character is returned
(at most 4 bytes), so paging always advances.

Byte selectors cannot be combined with line/grep selectors. `max_output_bytes`
is a line-mode option, not a byte-page limit. A user-audience text block reports
bytes read, start/end offsets and total size, plus `next_byte_offset` when more
content remains or `end_of_content: true` at the end. These page-body limits
aren't serialized JSON result limits; formatting and escaping add overhead.

MIME types with parameters (e.g., `text/plain; charset=utf-8`) are normalized for dispatch while preserving the original MIME type in the resource link.

**Memory consideration:** Whole objects are downloaded into memory before paging
is applied. Byte paging bounds the returned text, not download or peak memory
usage. Stored text above 5 MiB can be read; the 5 MiB limit applies to
`attachment_create`, not reads.

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
- `<hash>` is a 64-character lowercase SHA-256 digest. `attachment_create` hashes
  the text bytes; bridge offloads and worker media caches hash the complete
  MIME-bearing data URI. Treat returned CIDs as opaque references rather than
  deriving them from downloaded bytes.

Reads and writes through this server touch the owning session's activity timestamp (`SessionActivity.last_activity_at`), debounced to at most once per hour. This resets the session retention clock while stored media remains in active use.
