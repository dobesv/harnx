//! Attachment tools in `bash` scripts: clean paging output through the real
//! NATS tool path, and the MCP bridge's result limit, which still rejects a
//! full 128 KiB structured page (#2984) until the caller pages smaller.

use super::*;
use harnx_blob_store::media::{ensure_attachments_bucket, put_media, AttachmentsBucket};
use harnx_blob_store::media_cid_url;

/// How the attachment server is reached: registered directly as a NATS tool
/// server, or wrapped by the MCP bridge like a non-native MCP server.
enum Transport {
    DirectNats,
    McpBridge,
}

/// A broker, a real `harnx-attachment-tools` server registered as
/// `attachments`, and script mode over its tools. Seeding goes through the
/// same object store the server reads.
struct AttachmentsServer {
    instance_id: ServerScope,
    config: Arc<ConfigLock>,
    store: AttachmentsBucket,
    _supervisor: ToolServerSupervisor,
    _env: EnvGuard,
    _broker: common::NatsServerHandle,
}

/// Content that forces multibyte page cuts: 2- and 4-byte characters, and no
/// page boundary that lands on a line end.
fn multibyte_body() -> String {
    (0..40)
        .map(|n| format!("line {n}: 日本語 tést 🌍\n"))
        .collect()
}

/// The server, with `content` already stored as `mime`, under `transport`.
/// `None` when the environment has no `nats-server`.
async fn seeded_server(
    transport: Transport,
    content: &str,
    mime: &str,
) -> Result<Option<(AttachmentsServer, String)>> {
    let Some(server) = AttachmentsServer::start(transport).await? else {
        return Ok(None);
    };
    let url = server.seed(content, mime).await?;
    Ok(Some((server, url)))
}

/// Run `build_script(&url)` against a server seeded with `content`.
/// `None` when the environment has no `nats-server`.
async fn run_script(
    transport: Transport,
    content: &str,
    mime: &str,
    build_script: impl FnOnce(&str) -> String,
) -> Result<Option<serde_json::Value>> {
    let Some((server, url)) = seeded_server(transport, content, mime).await? else {
        return Ok(None);
    };
    let output = run_bash_script(&server.config, &server.instance_id, &build_script(&url)).await?;
    Ok(Some(output))
}

/// The script ran, and what it printed is `expected` reassembled exactly —
/// the property every paging test asserts.
fn assert_reassembled(output: &serde_json::Value, expected: &str) {
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(output), expected.trim_end(), "{output}");
}

impl AttachmentsServer {
    async fn start(transport: Transport) -> Result<Option<Self>> {
        // The bridge strips HARNX_NATS_* from its child and the attachment
        // server reads its endpoint from the environment, so a bridged
        // server needs an unauthenticated broker and `env` to restore the
        // URL for the child process.
        let auth_token = match transport {
            Transport::DirectNats => Some(TOKEN.to_string()),
            Transport::McpBridge => None,
        };
        let Some(broker) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
            auth_token: auth_token.clone(),
        })
        .await?
        else {
            return Ok(None);
        };
        let mut options = async_nats::ConnectOptions::new();
        if auth_token.is_some() {
            options = options.token(TOKEN.to_string());
        }
        let client = options.connect(broker.url()).await?;
        let store = ensure_attachments_bucket(&async_nats::jetstream::new(client), 1).await?;
        let env = EnvGuard::install(&[
            (HARNX_NATS_URL_ENV, broker.url()),
            (HARNX_NATS_TOKEN_ENV, TOKEN),
        ]);
        let mut server = match transport {
            Transport::DirectNats => {
                time_server_config(&workspace_binary("harnx-attachment-tools")?.to_string_lossy())
            }
            Transport::McpBridge => {
                let bridge = workspace_binary("harnx-mcp-bridge")?;
                let attachments = workspace_binary("harnx-attachment-tools")?;
                let mut server = time_server_config(&bridge.to_string_lossy());
                server.args = ["--name", "attachments", "--"]
                    .into_iter()
                    .map(str::to_string)
                    .chain([
                        "/usr/bin/env".to_string(),
                        format!("HARNX_NATS_URL={}", broker.url()),
                        attachments.to_string_lossy().into_owned(),
                        "--mcp-stdio".to_string(),
                    ])
                    .collect();
                server
            }
        };
        server.name = "attachments".to_string();
        let (instance_id, supervisor, config) = start_script_server(
            broker.url(),
            server,
            "__attachments__attachments",
            &[
                "attachments_attachment_read",
                "attachments_attachment_create",
            ],
        )
        .await?;
        // A real caller has a session; the create tool stores under the
        // invoking session's identity, so the round config needs one for
        // inner calls to carry parent identity.
        config.write().session = Some(harnx_core::session::Session {
            id: "attEST".to_string(),
            session_id: Some("attEST".to_string()),
            agent_name: Some("test-agent".to_string()),
            ..Default::default()
        });
        Ok(Some(Self {
            instance_id,
            config,
            store,
            _supervisor: supervisor,
            _env: env,
            _broker: broker,
        }))
    }

    /// Store `content` and return its `cid:media:` URL.
    async fn seed(&self, content: &str, mime: &str) -> Result<String> {
        let session = harnx_core::cid_url::SessionRef::new(None, "attEST".to_string())?;
        let hash = harnx_core::crypto::sha256(content);
        let url = media_cid_url(&session, &hash);
        put_media(&self.store, &url, content.as_bytes(), mime).await?;
        Ok(url.to_string())
    }
}

/// Both paging e2es reassemble the same multibyte content exactly; only the
/// paging loop differs (bash tool calls vs Python `tools`). Seeded with a
/// charset parameter so the read path also normalizes the MIME type.
async fn assert_paging_reassembles(build_script: impl FnOnce(&str) -> String) -> Result<()> {
    let original = multibyte_body();
    let Some(output) = run_script(
        Transport::DirectNats,
        &original,
        "text/plain; charset=utf-8",
        build_script,
    )
    .await?
    else {
        return Ok(());
    };
    assert_reassembled(&output, &original);
    Ok(())
}

/// A `bash` script pages a multibyte attachment through byte mode, follows
/// the structured continuation fields, and reassembles the content exactly.
#[tokio::test(flavor = "multi_thread")]
async fn bash_pages_an_attachment_byte_by_byte() -> Result<()> {
    assert_paging_reassembles(|url| {
        format!(
            r#"url='{url}'
: > /tmp/pages
offset=0
for i in $(seq 0 2000); do
  json=$(attachments_attachment_read --url "$url" --byte-offset "$offset" --byte-limit 7 -o json)
  printf '%s' "$json" | jq -j .text >> /tmp/pages
  if [ "$(printf '%s' "$json" | jq -r .end_of_content)" = "true" ]; then
    break
  fi
  offset=$(printf '%s' "$json" | jq -r .next_byte_offset)
done
cat /tmp/pages"#
        )
    })
    .await
}

/// One page's stdout is clean data and the continuation hint lands on stderr:
/// nothing about `next_byte_offset` leaks into piped output. The full default
/// 128 KiB page also survives the NATS tool path.
#[tokio::test(flavor = "multi_thread")]
async fn bash_attachment_read_keeps_stdout_clean_and_notes_on_stderr() -> Result<()> {
    let body = "héllo 世界, this is line one\nsecond line\nthird line\nfourth line\nfinal line\n";
    let Some(output) = run_script(Transport::DirectNats, body, "text/plain", |url| {
        format!(
            r#"url='{url}'
attachments_attachment_read --url "$url" --byte-offset 0 --byte-limit 5
echo "---"
attachments_attachment_read --url "$url" --head-lines 2 --tail-lines 1"#
        )
    })
    .await?
    else {
        return Ok(());
    };
    assert_eq!(output["status"], "ok", "{output}");
    // The byte page prints byte-for-byte with no added newline, so `echo`
    // continues the same line — the property that lets a script reassemble
    // pages exactly. Byte 5 is the `o` of héllo.
    assert_eq!(
        stdout_block(&output),
        "héll---\nhéllo 世界, this is line one\nsecond line\nfinal line",
        "{output}"
    );
    let stderr = stderr_block(&output);
    assert!(stderr.contains("Use byte_offset=5 to continue"), "{output}");
    assert!(stderr.contains("2 lines left out"), "{output}");
    assert!(!stderr.contains("next_byte_offset: 5"), "{output}");

    // The NATS tool path carries a full default 128 KiB page in one piece.
    let big = "x".repeat(200_000);
    let Some(output) = run_script(Transport::DirectNats, &big, "text/plain", |url| {
        format!(r#"attachments_attachment_read --url '{url}' --byte-offset 0 | wc -c"#)
    })
    .await?
    else {
        return Ok(());
    };
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(&output), "131072", "{output}");
    Ok(())
}

/// `help` shows the tool's OUTPUT and FIELDS, so a script author can find
/// the continuation fields without reading the source.
#[tokio::test(flavor = "multi_thread")]
async fn help_lists_attachment_read_output_fields() -> Result<()> {
    let Some(output) = run_script(Transport::DirectNats, "any body\n", "text/plain", |_| {
        "help attachments_attachment_read".to_string()
    })
    .await?
    else {
        return Ok(());
    };
    assert_eq!(output["status"], "ok", "{output}");
    let stdout = stdout_block(&output);
    assert!(stdout.contains("next_byte_offset"), "{stdout}");
    assert!(stdout.contains("FIELDS (-o json)"), "{stdout}");
    assert!(stdout.contains("end_of_content"), "{stdout}");
    Ok(())
}

/// Python reads the same attachment through `tools`: a dict per page, with
/// the continuation in fields, and the reassembly comes out exact.
#[tokio::test(flavor = "multi_thread")]
async fn python_reassembles_an_attachment_through_tools() -> Result<()> {
    assert_paging_reassembles(|url| {
        format!(
            r#"python3 - <<'EOF'
url = '{url}'
out = ''
offset = 0
while True:
    page = tools.attachments_attachment_read(url=url, byte_offset=offset, byte_limit=11)
    out += page['text']
    if page['end_of_content']:
        break
    offset = page['next_byte_offset']
print(out, end='')
EOF"#
        )
    })
    .await
}

/// The mapped create confirmation is a complete stdout line: a following
/// command starts on its own line. Composed output is the only way to catch
/// a missing trailing newline — command substitution strips it.
#[tokio::test(flavor = "multi_thread")]
async fn bash_create_confirmation_is_a_complete_line() -> Result<()> {
    let Some(output) = run_script(Transport::DirectNats, "seed\n", "text/plain", |_| {
        r#"attachments_attachment_create --content "from bash" --mime-type text/plain
echo SENTINEL"#
            .to_string()
    })
    .await?
    else {
        return Ok(());
    };
    assert_eq!(output["status"], "ok", "{output}");
    let stdout = stdout_block(&output);
    let (line, sentinel) = stdout.rsplit_once('\n').expect("sentinel on its own line");
    assert_eq!(sentinel, "SENTINEL", "{output}");
    assert!(
        line.starts_with("Created attachment: cid:media:"),
        "{stdout}"
    );
    Ok(())
}

/// The attachment server behind the MCP bridge: the structured-result request
/// crosses the bridge, a smaller page comes back clean, and a full default
/// 128 KiB page is still rejected by the bridge's result limit (#2984) — the
/// operator must pass a `--byte-limit` under the cap.
#[tokio::test(flavor = "multi_thread")]
async fn bridged_full_page_still_trips_the_result_limit() -> Result<()> {
    let big = "x".repeat(150_000);
    let Some(output) = run_script(Transport::McpBridge, &big, "text/plain", |url| {
        format!(
            r#"url='{url}'
if attachments_attachment_read --url "$url" --byte-offset 0; then
  echo "unexpected-success"
else
  echo "rejected"
fi
echo -n "$(attachments_attachment_read --url "$url" --byte-offset 0 --byte-limit 4096 -o json | jq -r .end_of_content)""#
        )
    })
    .await?
    else {
        return Ok(());
    };
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(&output), "rejected\nfalse", "{output}");
    let stderr = stderr_block(&output);
    assert!(stderr.contains("max-result-bytes"), "{output}");
    assert!(
        stderr.contains("--max-output-bytes") || stderr.contains("--limit"),
        "the error must tell a script how to page smaller: {output}"
    );
    Ok(())
}
