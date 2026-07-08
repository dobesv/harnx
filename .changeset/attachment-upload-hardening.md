---
"harnx": patch
---

fix(serve): harden multipart attachment upload against OOM/DoS

**B4 — early size enforcement**
- Add Content-Length header pre-check: reject payloads > MAX_UPLOAD_BYTES before reading body
- Stream body with cumulative size check: reject as soon as cumulative bytes exceed 20 MiB limit
- Prevents OOM from malicious/huge payloads that previously buffered fully before checking size

**B5 — executable-path upload tests**
- Add 6 tests exercising the multipart upload handler:
  - `upload_attachments_success_returns_cid_refs`: valid multipart produces cid refs, stores files
  - `upload_attachments_malformed_multipart_returns_400`: malformed multipart rejected
  - `upload_attachments_no_parts_returns_400`: no attachment fields returns 400
  - `upload_attachments_oversized_returns_413`: payload > 20 MiB returns 413
  - `upload_attachments_oversized_content_length_header_returns_413_early`: oversized Content-Length header rejected early
  - `upload_attachments_unsupported_content_type_returns_415`: unsupported MIME type returns 415
