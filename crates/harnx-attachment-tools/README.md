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
| `--list-tools [--format text\|json]` | Print the tools this server publishes and exit without connecting to NATS. See [Listing a Server's Tools](../../docs/configuration-guide.md#listing-a-servers-tools---list-tools). |
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
- `byte_limit` (integer, optional): Positive page size; defaults to 131,072 bytes (128 KiB) when only `byte_offset` is supplied.

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

Creates a text or binary attachment in NATS Object Storage owned by the caller's session.

- Returns the canonical `cid:media:<agent>/<session-id>/<hash>` URL for the stored attachment.
- Default `utf-8` encoding accepts only text MIME types (`text/*`, `application/json`, `application/xml`, `application/yaml`, `application/x-yaml`).
- `base64` encoding accepts strict standard base64 and any MIME type. Stored bytes are decoded before hashing. Text MIME types still require UTF-8 decoded content because `attachment_read` decodes text as UTF-8, regardless of charset parameters. To retain non-UTF-8 bytes unchanged, use a binary MIME type such as `application/octet-stream` (not displayable by `attachment_read`).
- The handler limits decoded payloads to 5 MiB. Oversized encoded input is rejected before decoding; the decoded-size check still handles the final base64 quantum.
- The complete JSON tool request must also fit NATS `max_payload` (default 1 MiB). Base64 occupies `4 * ceil(decoded_bytes / 3)` bytes, so the default broker permits less than 768 KiB of binary content after request/header overhead; script mode also reserves 1 KiB for the envelope. Exact capacity depends on the serialized request. Requests are not chunked. A 5 MiB binary attachment needs 6,990,508 base64 bytes plus overhead, so requires a higher broker limit (for example, 8 MiB).
- Requires caller session identity; returns an error when invoked without session context. Session identity is supplied by the runtime, not a tool argument.

Parameters:
- `content` (string, required): Text or base64-encoded bytes to store.
- `mime_type` (string, required): MIME type of the decoded content.
- `encoding` (string, optional): `utf-8` (default) or `base64`.

When this tool is in an agent's `bash_tools`, its generated `bash` description includes:

```bash
attachments_attachment_create --mime-type image/png --encoding base64 --content "$(base64 -w0 /tmp/x.png)"
```

`/tmp/x.png` must already exist in the script VFS. The command uses the agent-visible tool name; custom server names or cross-package tools can have different prefixes.

## Output in bash scripts

When an agent selects this toolset's tools in `bash_tools`, the inner calls ask
for structured results and the commands behave like file tools:

- `attachments_attachment_read` prints the selected text on stdout byte for
  byte, without truncation markers or paging notices. What was cut off is a
  note on stderr, and `-o json` (or Python's `tools.attachments_attachment_read(...)`)
  returns the fields:
  - Byte mode: `text`, `notes`, `url`, `content_type`, `byte_offset`,
    `next_byte_offset` (the offset to pass next; `null` at the end),
    `end_of_content`, `total_bytes`. The stderr note names the next
    `byte_offset` while content remains.
  - Line mode: `text`, `notes`, `url`, `content_type`. The byte cap keeps
    whole lines from the start and the note names the `offset` that
    continues; `head_lines`/`tail_lines` are honored as explicit caps and
    drop the middle with an "N lines left out" note instead of the model
    mode's silent 250/750-line window. `grep` matches the line without its
    LF/CRLF terminator and reports how many lines it left out; an invalid
    pattern leaves the text unfiltered with a stderr note saying so. A
    cap-cut note without `Use offset=` means the byte cap stopped inside a
    two-sided head/tail cut, whose kept blocks are disjoint, so no single
    continuation line exists — not that the content ended. Grep and
    window-omission notes never carry a cursor. `limit` truncates without
    a note, since the caller chose it.
  - A single line longer than `max_output_bytes` is cut at a character
    boundary, and the note names the `offset` that skips past it.
- Images still return image blocks (the script layer externalizes them to
  `/attachments/<cid>`), and non-displayable binary content is still a tool
  error that exits 1.
- `attachments_attachment_create` prints its confirmation line
  (`Created attachment: <cid>`, newline-terminated) on stdout; `-o json` and
  Python return `message`, `url` and `mime_type`, so a script never parses
  the line for the URL.

**MCP bridge limit**: the bridge counts the serialized envelope, not just the
page text, against its `--max-result-bytes` (default 128 KiB). A full default
128 KiB page from `attachment_read` is therefore rejected with an explicit
error until #2984 is fixed; scripts behind a bridge must pass a smaller
`--byte-limit` or `--max-output-bytes`, or the operator raises the bridge's
limit. Direct NATS tool servers have no such cap.

## NATS Object Store usage

Attachments are stored in the `harnx_attachments` JetStream Object Store bucket under keys named:

```text
media/<owner>/<hash>
```

- `<owner>` is `session_key(agent, sid)` (64-character lowercase hex digest of the owning session identity).
- `<hash>` is a 64-character lowercase SHA-256 digest. `attachment_create` hashes
  the decoded content bytes; bridge offloads and worker media caches hash the complete
  MIME-bearing data URI. Treat returned CIDs as opaque references rather than
  deriving them from downloaded bytes.

Writes through this server touch the owning session's activity timestamp (`SessionActivity.last_activity_at`), debounced to at most once per hour, which resets the session retention clock. Reads don't, so media whose session is only read expires with it.
