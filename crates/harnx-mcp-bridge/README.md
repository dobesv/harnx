# harnx-mcp-bridge

`harnx-mcp-bridge` bridges external Model Context Protocol (MCP) servers communicating over stdio to NATS, making them available as native Harnx tool servers within a `HARNX_SERVER_SCOPE`.

In addition to serving over NATS, `harnx-mcp-bridge` provides standalone commands to inspect tools and invoke tools directly over stdio without connecting to NATS.

## Modes of Operation

### 1. Serving over NATS

When deployed with `--name` and standard NATS environment variables (`HARNX_NATS_URL`, `HARNX_SERVER_SCOPE`), the bridge starts the wrapped child process, completes the MCP initialization handshake, and registers advertised tools on NATS:

```sh
harnx-mcp-bridge --name exa -- npx -y exa-mcp-server
```

Options:
- `--name <NAME>`: Server name used for NATS tool registration (required when serving).
- `--enable-tool <GLOB>`: Restrict advertised tools to matching glob patterns (repeatable).
- `--metrics-addr <ADDR>`: Prometheus metrics endpoint (e.g. `:8456`).
- `--healthz-addr <ADDR>`: Healthz readiness check endpoint (e.g. `:8457`).

### 2. Standalone Tool Listing (`--list-tools`)

Diagnose child server startup, handshake, and tool discovery without connecting to NATS:

```sh
harnx-mcp-bridge --list-tools -- npx -y @modelcontextprotocol/server-everything
```

The bridge launches the child, initializes the MCP session, queries `tools/list`, prints advertised tools with descriptions and hints to stdout, shuts down the child cleanly, and exits with code 0.

### 3. Standalone Direct Tool Invocation (`--call-tool`)

Execute a specific tool directly on the wrapped MCP server:

```sh
harnx-mcp-bridge --call-tool echo --tool-args '{"message": "hello"}' -- npx -y @modelcontextprotocol/server-everything
```

- `--call-tool <NAME>`: Exact tool name to invoke (incompatible with `--list-tools`).
- `--tool-args <JSON_OBJECT>`: JSON object passed as tool arguments. Defaults to `{}` if omitted. Requires `--call-tool`.
- The child MCP command and arguments must follow the explicit `--` separator. Bridge flags must appear before `--`.

## Argument Validation and Quoting

Tool arguments passed to `--tool-args` must be a valid JSON object (`{...}`):
- Non-object JSON (such as arrays, strings, numbers, booleans, or null) and malformed JSON are rejected before spawning the child process.
- The argument must be quoted to prevent shell word splitting.

## Output and Exit Status

- **Success**: Emits the bounded MCP `CallToolResult` as compact JSON to stdout and exits with code `0`. Results within the limit retain all content and fields unchanged.
- **Tool Error (`isError: true`)**: Emits the bounded result to stdout, logs `error: tool '<name>' reported isError: true` to stderr, and exits with code `1`. Size errors also use this status; see result limits below.
- **Incomplete Result**: If a tool returns a `resultType` other than `"complete"` (such as `"partial"`), the bridge exits with code `1`.
- **Protocol & Transport Errors**: Initialization failures, protocol violations, unknown tool names, and transport disconnects print diagnostics to stderr and exit with code `1`. No fabricated tool result is emitted to stdout.

## Tool Filtering

The repeatable `--enable-tool <GLOB>` option applies across all modes:
- In NATS serving mode, only matching tools are registered.
- In `--list-tools` mode, only matching tools are displayed.
- In `--call-tool` mode, targeting an excluded tool is rejected before invocation: `error: tool '<name>' is excluded by --enable-tool`.

## Child Environment and Isolation

- **NATS Environment Stripping**: `harnx-mcp-bridge` strips `HARNX_SERVER_SCOPE`, `HARNX_NATS_URL`, and `HARNX_NATS_TOKEN` from the child process environment before spawning. If the child process itself requires NATS connectivity (e.g., a native Harnx toolset server running in stdio mode), pass those variables explicitly to the child command:
  ```sh
  harnx-mcp-bridge --call-tool get_current_time -- env HARNX_NATS_URL=nats://127.0.0.1:4222 harnx-time-tools --mcp-stdio
  ```
- **Session Identity**: Tools requiring caller session identity (such as attachment creation tools) fail clearly when called via stdio bridge, because standalone stdio invocations carry no session identity context.
- **Process Lifecycle**: The bridge uses `ChildProcessManager` to manage child processes. On completion or error, the bridge explicitly stops and reaps the child process. `kill_on_drop` provides fallback protection against process leaks.

## Result Limits and Original Attachments

Every result is limited to **32,768 bytes** of compact serialized `CallToolResult`
JSON by default. This is a per-result limit, not a limit shared across calls in a
round. JSON escaping, all content parts, annotations, `structuredContent`, `_meta`
and envelope fields count. Direct CLI stdout adds a trailing newline outside the
JSON object.

Set `HARNX_MCP_MAX_RESULT_BYTES` or pass `--max-result-bytes`; the CLI option wins
over the environment. Values below 1,024 bytes are rejected before child startup.
For example:

```sh
harnx-mcp-bridge --name external --max-result-bytes 65536 -- external-mcp-server
HARNX_MCP_MAX_RESULT_BYTES=65536 harnx-mcp-bridge --call-tool list_items -- external-mcp-server
```

Fitting results pass through unchanged. With a trusted invoking session and the
existing NATS serving connection, oversized parts are handled in order:

- Text is stored as exact UTF-8 bytes before producing a middle-truncated preview,
  its `cid:media:` URL and instructions to narrow the request or page the original.
- Images, audio and embedded binary resources are decoded and stored without
  cutting base64. Their replacements are MCP `resource_link` blocks with `uri`,
  `name`, original `mimeType` and decoded byte `size`.
- Unaffected siblings and representable annotations, metadata, structured content
  and original `isError` status remain unchanged. Embedded resources retain their
  original MIME, including parameters. Plain MCP text defaults to `text/plain`.

Attachment CIDs hash the full data URI, including MIME and base64 data, so equal
bytes with different MIME types don't overwrite each other's metadata. Images
use the same data-URI hash convention as worker attachment caches. Storage uses
`harnx_attachments` with the invoking session as owner and existing session
retention/deletion rules. No connection is opened per invocation.

If many individually fitting parts exceed the aggregate limit, the full original
result is stored as `application/json` and replaced by an explicit aggregate
reference, keeping representable top-level fields and error status. Oversized
schema-sensitive fields or metadata that cannot be represented get an explicit
small `isError: true` result with the original JSON reference, not an invented
`structuredContent` schema. Malformed binary data also uses this JSON fallback.
The final result is reserialized to enforce the limit after all substitutions.

Use `attachment_read` with `byte_offset` and `byte_limit` to read large text/JSON
originals in pages; line selectors also work when lines are reasonably sized.
Attachment reads currently download the whole stored object before paging.

Direct `--call-tool` and calls without trusted session identity cannot retain
oversized originals. They return bounded errors saying the original wasn't
retained and side effects may already have occurred. Direct CLI exits nonzero.
Do not retry side-effecting tools blindly. Real storage failures remain fatal
infrastructure errors rather than being reported as successful truncation.
