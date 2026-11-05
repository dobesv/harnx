//! Exercise the generated command with native schemas, naming and real storage.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_attachment_tools::AttachmentToolset;
use harnx_core::{
    agent_config::AgentConfig,
    tool::{ToolDeclaration, Tools},
};
use harnx_script::{
    commands::{tool_commands, CallOutcome, InnerExecutor, ToolSpec},
    BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptStatus,
};
use harnx_toolset::{Registration, SessionRef, ToolInvocation, ToolInvocationContext, Toolset};
use serde_json::{json, Value};

use super::build_registered_tools;
use crate::config::Config;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn declarations() -> Vec<ToolDeclaration> {
    let mut declarations = Vec::new();
    for (package, active_package, server, expected_name) in [
        (None, None, "attachments", "attachments_attachment_create"),
        (
            Some("coding"),
            Some("coding"),
            "attachments",
            "attachments_attachment_create",
        ),
        (
            Some("coding"),
            Some("other"),
            "attachments",
            "coding__attachments_attachment_create",
        ),
        (None, None, "media-tools", "media-tools_attachment_create"),
    ] {
        let registration = Registration {
            package: package.map(str::to_owned),
            config: "attachments".into(),
            server: server.into(),
            tools: AttachmentToolset::new().tools(),
            schema_version: 1,
            proto_version: harnx_toolset_server::TOOL_PROTOCOL_VERSION,
        };
        let (routes, tools) = build_registered_tools(active_package, vec![registration]);
        let create = tools
            .into_iter()
            .find(|tool| tool.name == expected_name)
            .unwrap();
        assert_eq!(routes[expected_name].raw_name, "attachment_create");
        assert_eq!(routes["attachment_create"].raw_name, "attachment_create");
        declarations.push(create);
    }
    // The raw alias has the same schema, and remains routable for older calls.
    let mut raw = declarations[0].clone();
    raw.name = "attachment_create".into();
    declarations.insert(0, raw);
    declarations
}

fn description(tool: &ToolDeclaration) -> String {
    let config = Config {
        tools: Tools::init_from_mcp(Some(vec![tool.clone()])),
        ..Default::default()
    };
    let mut agent = AgentConfig::from_prompt("attachment encoding test");
    agent.set_bash_tools(Some(vec![tool.name.clone()]));
    config
        .select_tools(&agent)
        .unwrap()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .unwrap()
        .description
}

fn idiom(description: &str) -> &str {
    description
        .lines()
        .find_map(|line| {
            line.strip_prefix("- Create a PNG attachment from VFS bytes: `")?
                .strip_suffix("`.")
        })
        .expect("real attachment_create schema must enable the base64 idiom")
}

#[test]
fn real_attachment_declarations_enable_base64_description_idiom() {
    let mut commands = Vec::new();
    for tool in declarations() {
        let schema = tool.parameters.as_value();
        assert_eq!(schema["required"], json!(["content", "mime_type"]));
        assert_eq!(
            schema["properties"]["encoding"]["enum"],
            json!(["utf-8", "base64"])
        );
        let guide = description(&tool);
        let command = idiom(&guide);
        assert_eq!(
            command,
            format!(
                "{} --mime-type image/png --encoding base64 --content \"$(base64 -w0 /tmp/x.png)\"",
                tool.name
            )
        );
        commands.push(command.to_owned());
    }
    insta::assert_snapshot!("real_attachment_base64_idioms", commands.join("\n"));
}

struct NativeExecutor {
    toolset: AttachmentToolset,
    visible_name: String,
    args: Mutex<Vec<Value>>,
    results: Mutex<Vec<Value>>,
}

fn session() -> SessionRef {
    SessionRef {
        agent: Some("test-agent".into()),
        session_id: "abcDEF".into(),
    }
}

#[async_trait]
impl InnerExecutor for NativeExecutor {
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> CallOutcome {
        assert_eq!(name, self.visible_name);
        self.args.lock().unwrap().push(args.clone());
        let result = self
            .toolset
            .invoke_with_context(ToolInvocation {
                tool: "attachment_create".into(),
                args,
                context: ToolInvocationContext {
                    invoking_session: Some(session()),
                    ..Default::default()
                },
                cancel,
            })
            .await
            .expect("invoke native attachment_create");
        self.results.lock().unwrap().push(result.clone());
        CallOutcome::Ok(result)
    }
}

#[tokio::test]
async fn generated_attachment_idiom_roundtrips_png_through_native_toolset() {
    let Some(nats) = harnx_test_bins::spawn_nats_server().await.unwrap() else {
        return;
    };
    let png = STANDARD.decode(PNG).unwrap();
    for tool in declarations() {
        let guide = description(&tool);
        let command = idiom(&guide);
        let executor = Arc::new(NativeExecutor {
            toolset: AttachmentToolset::with_nats_url(nats.url()),
            visible_name: tool.name.clone(),
            args: Default::default(),
            results: Default::default(),
        });
        let commands = tool_commands(
            vec![ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.parameters.as_value().clone(),
            }],
            executor.clone(),
        )
        .unwrap();
        // Seed binary bytes into VFS; the generated idiom must read and encode
        // them itself, rather than passing our fixture's encoded string through.
        let script = format!("printf '%s' '{PNG}' | base64 -d >/tmp/x.png\n{command}");
        let result = BashKitBackend
            .run(harnx_script::ScriptRequest {
                script: &script,
                limits: ScriptLimits::default(),
                commands,
                fs_mounts: vec![],
                cancel: CancellationToken::new(),
            })
            .await;
        assert_eq!(result.status, ScriptStatus::Ok, "{}: {result:?}", tool.name);
        let args = executor.args.lock().unwrap().clone();
        assert_eq!(args.len(), 1);
        assert_eq!(
            args[0],
            json!({"mime_type":"image/png", "encoding":"base64", "content":PNG})
        );
        assert_eq!(
            STANDARD
                .decode(args[0]["content"].as_str().unwrap())
                .unwrap(),
            png
        );
        let created = executor.results.lock().unwrap()[0].clone();
        assert_eq!(created["isError"], false, "{created}");
        let cid = created["content"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|block| block["text"].as_str()?.strip_prefix("Created attachment: "))
            .unwrap();
        assert!(result.stdout.contains(cid), "{result:?}");
        assert_stored_png(&executor.toolset, cid, &png).await;
    }
}

async fn assert_stored_png(toolset: &AttachmentToolset, cid: &str, png: &[u8]) {
    let read = toolset
        .invoke_with_context(ToolInvocation {
            tool: "attachment_read".into(),
            args: json!({"url":cid}),
            context: ToolInvocationContext {
                invoking_session: Some(session()),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(read["isError"], false, "{read}");
    let image = read["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "image")
        .unwrap();
    assert_eq!(image["mimeType"], "image/png");
    assert_eq!(
        STANDARD.decode(image["data"].as_str().unwrap()).unwrap(),
        png
    );
}
