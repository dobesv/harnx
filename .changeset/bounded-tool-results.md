---
harnx: patch
---
Bound MCP bridge results to 32 KiB of serialized JSON by default, configurable
with `--max-result-bytes` or `HARNX_MCP_MAX_RESULT_BYTES`. Preserve oversized
parts as MIME-aware session attachments with previews or resource links, and
hydrate attached images before the next model request. Add UTF-8 byte paging
for large text/JSON originals. Bound native and bridged terminal replies before
journal storage and NATS delivery without losing successful sibling calls;
real infrastructure failures remain fatal. Fixes [#2431](https://github.com/dobesv/harnx/issues/2431).
