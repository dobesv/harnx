//! Execute documentation source, rather than maintaining parallel script copies.
use super::fixture::{bash, call, python, Fixture, Probe};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    config_data::ConfigData,
    tool::{ToolError, ToolProvider, ToolProviderOutput},
};
use serde_json::{json, Value};
use std::sync::Arc;

const GUIDE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/middleware-guide.md"
));
const EXAMPLE_CONFIG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../example_config/config.yaml"
));

pub(super) fn blocks() -> Vec<(&'static str, &'static str)> {
    GUIDE
        .split("```")
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, block)| block.split_once('\n').unwrap())
        .collect()
}

fn entries(yaml: &str) -> Vec<Value> {
    let config: ConfigData = serde_yaml::from_str(yaml).unwrap();
    serde_json::to_value(config.middleware)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

struct EchoBash(Arc<Probe>);

#[async_trait::async_trait]
impl ToolProvider for EchoBash {
    fn name(&self) -> &str {
        "documentation-echo"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "bash_exec"
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.0.invoke(name, args).await
    }
}

async fn assert_config_rewrite(yaml: &str) {
    let mut fixture = Fixture::new(entries(yaml)).await;
    fixture
        .ctx
        .providers
        .push(Arc::new(EchoBash(fixture.probe.clone())));
    fixture.ctx.allowed_tool_names.insert("bash_exec".into());
    let mut invocation = call(json!({"command":"echo hello", "env":{"KEEP":"yes"}}));
    invocation.name = "bash_exec".into();
    let results =
        harnx_engine::tool::eval_tool_calls(&fixture.ctx, vec![invocation], &create_abort_signal())
            .await
            .unwrap();
    assert_eq!(fixture.calls().len(), 1);
    assert_eq!(fixture.calls()[0].1["env"]["MIDDLEWARE_LABEL"], "reviewed");
    assert_eq!(results[0].output["env"]["KEEP"], "yes");
    assert_eq!(results[0].output["reviewed"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_documentation_examples_execute_verbatim() {
    let examples = blocks();
    assert_eq!(
        examples.iter().map(|(lang, _)| *lang).collect::<Vec<_>>(),
        [
            "yaml", "bash", "python", "python", "bash", "python", "bash", "python", "bash",
            "python", "yaml", "yaml"
        ],
        "update execution coverage when adding an example"
    );
    assert_config_rewrite(examples[0].1).await;
    for (index, (lang, source)) in examples.iter().take(6).enumerate().skip(1) {
        let entry = match *lang {
            "bash" => bash(source),
            "python" => python(source),
            _ => panic!("uncovered example language: {lang}"),
        };
        let fixture = Fixture::new(vec![entry]).await;
        let arguments = json!({"env":{"KEEP":"yes"}});
        let result = fixture.eval(arguments.clone()).await.unwrap();
        match index {
            1 | 2 => {
                assert_eq!(result.output["error"], "not permitted");
                assert!(fixture.calls().is_empty());
            }
            3 => {
                assert_eq!(result.output["env"]["MIDDLEWARE_LABEL"], "reviewed");
                assert_eq!(result.output["env"]["KEEP"], "yes");
                assert_eq!(result.output["reviewed"], true);
                assert_eq!(fixture.calls().len(), 1);
            }
            4 | 5 => {
                assert_eq!(result.output, arguments);
                assert_eq!(fixture.calls().len(), 1);
            }
            _ => panic!("uncovered example {index}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_example_config_parses_and_uncommented_example_executes() {
    let _: ConfigData = serde_yaml::from_str(EXAMPLE_CONFIG).unwrap();
    let (_, rest) = EXAMPLE_CONFIG
        .split_once("# BEGIN middleware example\n")
        .unwrap();
    let (commented, _) = rest.split_once("# END middleware example").unwrap();
    let yaml = commented
        .lines()
        .map(|line| line.strip_prefix("# ").unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    // Parse the entire config after enabling the example, not only its fragment.
    let enabled = EXAMPLE_CONFIG.replacen(commented, &format!("{yaml}\n"), 1);
    let _: ConfigData = serde_yaml::from_str(&enabled).unwrap();
    assert_config_rewrite(&yaml).await;
    assert_eq!(entries(&yaml), entries(blocks()[0].1));
}

#[test]
fn middleware_documentation_disable_override_resolves() {
    use harnx_runtime::config::{agent::Agent, AgentConfig, Config};
    let mut config = Config {
        data: serde_yaml::from_str(blocks()[0].1).unwrap(),
        ..Config::default()
    };
    assert_eq!(config.resolved_middleware().unwrap().entries.len(), 1);
    let disable = GUIDE
        .split('`')
        .find(|text| text.starts_with("{name: "))
        .unwrap();
    let mut agent = AgentConfig::from_prompt("Documentation override");
    agent.middleware = serde_yaml::from_str(&format!("[{disable}]")).unwrap();
    config.agent = Some(Agent::new(agent));
    assert!(config.resolved_middleware().unwrap().entries.is_empty());
}
