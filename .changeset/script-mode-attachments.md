---
harnx: minor
---
Read attachment bytes from script-mode bash through a read-only `/attachments` mount.

Use `/attachments/<full-media-cid>` without another URL-encoding layer. Reads are lazy and cached per invocation, with 10 MB per-file and 100 MB cache limits. Listings remain empty, mutations fail with a read-only error, and oversized blobs fail instead of returning truncated bytes. Runtime reads use the round's broker connection and match `attachment_read` access rules, including explicit cross-session CID references.

Externalize inline inner-tool images, audio and embedded resources before scripts receive results, including JSON output, structured content and partial results. Return attachment markers with readable `/attachments` paths in place of inline media blocks (data URIs inside plain text are left alone). Reads from `/attachments` follow `attachment_read`'s access rule but don't pass through tool selection or `PreToolUse` hooks. Uploads use the parent agent/session identity and finish before the command returns; storage failures stop the script rather than exposing inline data.
