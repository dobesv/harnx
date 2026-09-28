---
"harnx": patch
"coding": patch
"pantheon": patch
---

feat: add native harnx-attachment-tools server for cid: URL read/create

Implements PR4 of the NATS attachments & plans architecture:

- New `harnx-attachment-tools` crate with two tools:
  - `attachment_read(url, ...)`: Read media blobs with truncation params
    matching fs.read (head_lines, tail_lines, offset, limit, max_output_bytes,
    grep). Returns image blocks for image MIME types, truncated text for text
    types, and is_error for non-displayable binary content.
  - `attachment_create(content, mime_type)`: Create text attachments in the
    NATS object store. Requires caller session identity; returns is_error
    when invoked via MCP stdio/HTTP bridges without context.

- Both tools touch session activity on the attachment owner.
- NATS-only access: no filesystem, no network fetch.
- Port 3007 for MCP HTTP listener (next in sequence after fetch's 3006).

Tests cover attachment creation, cross-session reading, image reading with
truncation, and missing-context error handling.
