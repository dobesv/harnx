# harnx-fetch-tools

Native Rust toolset server for fetching HTTP resources. It replaces `mcp-fetch-server` and exposes the same six tool names under toolset name `fetch`:

- `fetch_html`
- `fetch_markdown`
- `fetch_txt`
- `fetch_json`
- `fetch_readable`
- `fetch_youtube_transcript`

## Run

```bash
cargo install --path crates/harnx-fetch-tools
harnx-fetch-tools
```

Pass `--name <NAME>` to override the registered toolset name. MCP HTTP mode listens on port 3006 by default:

```bash
harnx-fetch-tools --mcp-http --host 127.0.0.1 --port 3006
```

Private, loopback, link-local, multicast, and other special-purpose IP addresses are blocked by default. Protection applies to URL literals, all DNS answers, and every redirect. Environment proxies are disabled while protection is active. Use `--allow-private-ip` only in a trusted network; it also permits each tool's `proxy` argument.

## Image responses

Use any of the five page fetchers to fetch PNG, JPEG, GIF (including animated GIF), or WebP images up to 5 MiB. They return an MCP image block and a text summary with the final URL, detected format, and byte count. Text truncation parameters do not apply to images. `fetch_youtube_transcript` returns text only.

For `image/*` responses, the file signature determines the format. An unsupported signature returns an error (including for SVG and AVIF). A response without a `Content-Type`, or with `application/octet-stream`, is also checked for a supported image signature. Other content types keep their usual text handling.

## Output in `bash` scripts

In a `bash` tool script, all six fetch tools print the page text on stdout, so `fetch_markdown --url "$u" | grep -n TODO` works as it would with `curl`. A direct call is unchanged.

- Notes go to stderr as `<tool>: <note>`: how many characters remain after the printed text and the `start_index` that continues exactly where it stopped (counted after the byte cap, so it follows what the script received, whether `max_length` or the byte cap cut it), the byte cap that cut the text (`max_output_bytes`, default 131072, whole lines from the start), and for `fetch_json` a warning that a cut preview is not valid JSON. `head_lines` and `tail_lines` are ignored in a script, with a note saying so (`fetch_markdown: tail_lines is ignored in a script; pipe to tail`).
- A non-2xx status, a blocked address and the other failures are errors: nothing on stdout, the message on stderr, exit 1.
- A fetched image becomes a line `[attachment: <cid> (<mime>, <size>)] path=/attachments/<cid>`, and the bytes are readable at that path.
- `-o json` prints `text`, `notes`, `url`, `final_url` and `content_type`.

## Parameters

Every tool accepts `url`, `headers`, `proxy`, `max_length`, `start_index`, `head_lines`, `tail_lines`, and `max_output_bytes`. `fetch_youtube_transcript` also accepts `lang` (default `en`).

`max_length` (default 131072 characters) and `start_index` preserve upstream compatibility and run first. Smart truncation then keeps 250 head lines and 750 tail lines within a 131072-byte (128 KiB) budget, with per-line clipping off by default. Set `max_length` to `0` to disable the compatibility character limit; the smart truncation limits still apply. Responses are always capped at 10 MiB before transformation and requests have a 30-second deadline.
