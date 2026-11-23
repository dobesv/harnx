---
harnx: minor
pantheon: minor
coding: minor
---
Return MCP image blocks from `harnx-fetch-tools` (`fetch_html`, `fetch_markdown`, `fetch_txt`, `fetch_json`, `fetch_readable`) for PNG, JPEG, GIF, and WebP responses up to 5 MiB. Unsupported image formats (such as SVG and AVIF) and images over 5 MiB return clear errors, while existing SSRF protections and `fetch_youtube_transcript` behavior remain unchanged.

When a successful native NATS tool reply exceeds the completion budget because of inline images, the toolset server now stores those images as session-owned `cid:media:` attachments and replaces them with `resource_link` blocks so the model still receives them. This covers `fs_read`, `fetch_*`, `attachment_read`, and the Kubernetes sandbox gateway.

Also allow `erato` in `pantheon` to call `fetch_fetch_markdown` for public image URLs without a sandbox, and update the `pantheon` and `coding` fetch tool configs and READMEs for image support.
