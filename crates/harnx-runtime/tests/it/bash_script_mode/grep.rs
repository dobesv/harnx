//! Script-mode `grep_query`: the structured report pipes through `jq`, Python
//! sees a dict, and `help` lists the report's fields.
//!
//! The production binary has no base-url override and the sandbox's network is
//! loopback-only, so the toolset runs in-process against wiremock, served over
//! NATS the same way the spawned binaries register. The agent-visible command
//! is `grep_grep_query`: the runtime prefixes the server name onto the tool.

use super::{run_bash_script, script_mode_config, stdout_block, EnvGuard, TOKEN};
use crate::common;
use anyhow::{Context as _, Result};
use harnx_core::instance::ServerScope;
use harnx_grep_tools::GrepToolset;
use harnx_runtime::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use harnx_toolset_server::serve_over_nats;
use std::time::{Duration, Instant};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

const COMMAND: &str = "grep_grep_query";

/// One repository, one file, one facet bucket per kind, a minimal hit
/// response the document builder turns into a full search report.
const HITS_RESPONSE: &str = r#"{
  "hits": { "hits": [{
    "repo": { "raw": "owner/repo" },
    "path": { "raw": "src/main.py" },
    "content": { "snippet": "<span data-line=\"3\">import fastapi</span>" },
    "total_matches": { "raw": "1" }
  }] },
  "facets": {
    "count": 1,
    "lang": { "buckets": [{ "val": "Python", "count": 1 }] },
    "repo": { "buckets": [{ "val": "owner/repo", "count": 1 }] }
  }
}"#;

const NOT_FOUND_MESSAGE: &str = "No results found for this query";

/// A broker, a wiremock grep.app, and the grep toolset registered in-process.
struct GrepServer {
    instance_id: ServerScope,
    config: std::sync::Arc<harnx_runtime::config::ConfigLock>,
    _env: EnvGuard,
    _broker: common::NatsServerHandle,
    /// Kept so the mock's expectations stay owned by this fixture until
    /// teardown, not verified early when the handle is dropped.
    _mock: MockServer,
}

impl GrepServer {
    /// `None` when the environment has no `nats-server`.
    async fn start() -> Result<Option<Self>> {
        let Some(broker) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
            auth_token: Some(TOKEN.to_string()),
        })
        .await?
        else {
            return Ok(None);
        };
        let mock = MockServer::start().await;
        for (query, response) in [
            (
                "FastAPI",
                ResponseTemplate::new(200).set_body_string(HITS_RESPONSE),
            ),
            ("missing", ResponseTemplate::new(404)),
        ] {
            Mock::given(method("GET"))
                .and(path("/api/search"))
                .and(query_param("q", query))
                .respond_with(response)
                .mount(&mock)
                .await;
        }

        let env = EnvGuard::install(&[
            (HARNX_NATS_URL_ENV, broker.url()),
            (HARNX_NATS_TOKEN_ENV, TOKEN),
        ]);
        let instance_id = ServerScope::new();
        let toolset = GrepToolset::with_base_url(format!("{}/api/search", mock.uri()));
        let server_url = broker.url().to_owned();
        let scope = instance_id.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_over_nats(toolset, scope, &server_url, TOKEN).await {
                panic!("in-process grep server stopped: {error}");
            }
        });

        let config = script_mode_config(&["grep_*"]);
        wait_for_grep_declaration(&config, &instance_id).await?;
        Ok(Some(Self {
            instance_id,
            config,
            _env: env,
            _broker: broker,
            _mock: mock,
        }))
    }
}

/// Poll the registry until the in-process server's registration appears, then
/// poll the declaration cache, running the same refresh the agent loop runs
/// before a tool round.
async fn wait_for_grep_declaration(
    config: &std::sync::Arc<harnx_runtime::config::ConfigLock>,
    instance_id: &ServerScope,
) -> Result<()> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(std::env::var("HARNX_NATS_URL").context("nats url")?)
        .await?;
    let jetstream = async_nats::jetstream::new(client);
    let key = harnx_toolset_server::registration_key(instance_id, "____grep");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let registered = match jetstream
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await
        {
            Ok(store) => store.get(&key).await?.is_some(),
            Err(_) => false,
        };
        if registered {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "grep toolset never registered under {key}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        harnx_runtime::tool_context::refresh_nats_tool_declarations(config, instance_id).await;
        if config
            .read()
            .bash_inner_tool_declarations()
            .iter()
            .any(|declaration| declaration.name == COMMAND)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "{COMMAND} never reached the declaration cache"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The whole report is the command's output in every format, so a script pipes
/// it to `jq`; Python gets the same document as a dict.
#[tokio::test(flavor = "multi_thread")]
async fn grep_query_pipes_the_report_through_jq_and_python() -> Result<()> {
    let Some(grep) = GrepServer::start().await? else {
        return Ok(());
    };

    let script = "set -o pipefail\n\
grep_grep_query --query FastAPI | jq -r '.query'\n\
grep_grep_query --query FastAPI -o text | jq -r '.summary.total_results'\n\
grep_grep_query --query FastAPI -o json | jq -r '.results_by_repository[0].repository'\n\
grep_grep_query --query missing -o json | jq -r '.summary.message'\n\
python3 - <<'PY'\n\
report = tools.grep_grep_query(query='FastAPI')\n\
print(report['summary']['total_results'])\n\
print(report['results_by_repository'][0]['files'][0]['language'])\n\
empty = tools.grep_grep_query(query='missing')\n\
print(empty['summary']['message'])\n\
print(empty['results'])\n\
PY\n";

    let output = run_bash_script(&grep.config, &grep.instance_id, script).await?;
    assert_eq!(output["status"], "ok", "{output}");
    let expected = [
        "FastAPI",
        "1",
        "owner/repo",
        NOT_FOUND_MESSAGE,
        "1",
        "python",
        NOT_FOUND_MESSAGE,
        "[]",
    ]
    .join("\n");
    assert_eq!(stdout_block(&output), expected, "{output}");
    Ok(())
}

/// `help grep_grep_query` documents the whole-object output and lists the
/// report's fields, both result shapes included.
#[tokio::test(flavor = "multi_thread")]
async fn help_grep_query_documents_the_report_fields() -> Result<()> {
    let Some(grep) = GrepServer::start().await? else {
        return Ok(());
    };

    let script = format!("help {COMMAND}");
    let output = run_bash_script(&grep.config, &grep.instance_id, &script).await?;
    assert_eq!(output["status"], "ok", "{output}");
    let help = stdout_block(&output);
    assert!(help.contains("OUTPUT"), "{help}");
    assert!(
        help.contains("the search report as JSON; pipe it to jq"),
        "{help}"
    );
    assert!(help.contains("FIELDS (-o json)"), "{help}");
    let rows: Vec<(String, String)> = help
        .lines()
        .skip_while(|line| *line != "FIELDS (-o json)")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|row| {
            let row = row.trim();
            let (name, rest) = row.split_once("  ").expect("field name column");
            let kind = rest
                .trim_start()
                .split_once("  ")
                .map_or_else(|| rest.trim_start(), |(kind, _)| kind);
            (name.to_string(), kind.to_string())
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("query".to_string(), "string".to_string()),
            ("summary".to_string(), "object".to_string()),
            ("results_by_repository".to_string(), "array".to_string()),
            ("results".to_string(), "array".to_string()),
        ],
        "{help}"
    );
    Ok(())
}
