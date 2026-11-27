//! The structured-result request over native upstream `tools/call` (#3003):
//! `call_params` forwards `harnx:structured_result` only when the caller
//! asked, the mock upstream switches its result shape on it, and a BashKit
//! run over the real `RemoteCredentialsToolset` maps the structured
//! `structuredContent` the way the `bash` tool's inner calls see it.

#![cfg(test)]

use std::sync::Arc;

use anyhow::{anyhow, Result};
use harnx_mcp_remote::native::registrations::map_upstream_tool;
use harnx_mcp_remote::native::toolset::RemoteCredentialsToolset;
use harnx_script::commands::{tool_commands, CallOutcome, InnerExecutor};
use harnx_script::{
    bind_python_call, python_value, tools_prelude, BashKitBackend, ExcType, ExtFunctionResult,
    MontyException, MontyObject, ObjectRef, ScriptBackend, ScriptHostContext, ScriptLimits,
    ScriptPythonFunctions, ScriptPythonHandler, ScriptRequest, ScriptRun, ScriptStatus,
    PYTHON_CALL_FUNCTION,
};
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolInvokeError, Toolset};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken as ScriptToken;

use super::fixture::*;
use crate::native_common::*;

/// The structured `structuredContent` the mock upstream answers with when
/// it saw the request, shared by the plain and script-facing assertions.
fn expected_structured() -> Value {
    json!({
        "stdout": "upstream stdout\n",
        "stderr": "upstream stderr",
        "exit_code": 3,
        "items": ["alpha", "beta"],
        "count": 2,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_meta_reflects_the_structured_result_request() {
    harnx_core::require_nextest();
    let upstream = spawn_structured_upstream("authorization")
        .await
        .expect("start mock upstream");
    let (_keys, fixture) = alice_fixture(&upstream).await;
    let toolset = &fixture.server.toolset;
    wait_for_catalog_tool(
        &fixture.server.lifecycle,
        &alice_principal(),
        "structured_echo",
    )
    .await;

    // Without the capability the upstream sees no request and answers in
    // model style.
    let plain = invoke_with_capabilities(toolset, "structured_echo", Some("alice"), &[])
        .await
        .expect("model-style call succeeds");
    assert_eq!(plain["content"][0]["text"], json!("model-style text"));
    assert!(
        plain.get("structuredContent").is_none(),
        "unasked call must not carry structuredContent: {plain}"
    );

    // With the capability the upstream sees `_meta[harnx:structured_result]
    // == true`, and the within-cap structuredContent passes the actual
    // upstream ResultLimits normalization intact.
    let structured = invoke_with_capabilities(
        toolset,
        "structured_echo",
        Some("alice"),
        &[harnx_toolset::CAPABILITY_STRUCTURED_RESULT],
    )
    .await
    .expect("structured call succeeds");
    assert_eq!(
        structured["structuredContent"],
        expected_structured(),
        "normalized result: {structured}"
    );
    assert_eq!(
        upstream.structured_requests(),
        vec![None, Some(json!(true))],
        "the upstream must see exactly the caller's two requests: the key \
         absent on the model-style call, `true` on the structured one"
    );
}

/// Calls the toolset the way a `bash` script's inner calls do: an invocation
/// context whose capabilities say whether the result is wanted as
/// `structuredContent`.
struct RemoteScriptExecutor {
    toolset: Arc<RemoteCredentialsToolset>,
    user_id: String,
    structured: bool,
}

impl RemoteScriptExecutor {
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: ScriptToken,
    ) -> Result<Value, ToolInvokeError> {
        let mut capabilities = std::collections::BTreeSet::new();
        if self.structured {
            capabilities.insert(harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_string());
        }
        let context = ToolInvocationContext {
            user_id: Some(self.user_id.clone()),
            capabilities,
            ..Default::default()
        };
        self.toolset
            .invoke_with_context(ToolInvocation {
                tool: tool.to_string(),
                args,
                context,
                cancel,
            })
            .await
    }
}

#[async_trait::async_trait]
impl InnerExecutor for RemoteScriptExecutor {
    async fn call(&self, name: &str, args: Value, cancel: ScriptToken) -> CallOutcome {
        match self.invoke(name, args, cancel).await {
            Ok(value) => CallOutcome::Ok(value),
            Err(ToolInvokeError::Recoverable(message)) => CallOutcome::ToolError {
                message,
                partial_result: None,
            },
            Err(error) => CallOutcome::Fatal {
                message: error.to_string(),
            },
        }
    }
}

/// `tools.<name>()` for the script's Python, over the same executor the Bash
/// commands use (the runtime's `with_tools` shape, scoped to one tool).
struct PythonToolsHandler {
    executor: Arc<RemoteScriptExecutor>,
    schema: Value,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for PythonToolsHandler {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        _ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        if name != PYTHON_CALL_FUNCTION {
            return ExtFunctionResult::Error(MontyException::new(
                ExcType::NameError,
                Some(format!("unregistered Python host function: {name}")),
            ));
        }
        let (tool, call_args, call_kwargs) = match call_parts(args, kwargs) {
            Ok(parts) => parts,
            Err(error) => return ExtFunctionResult::Error(error),
        };
        let bound = match bind_python_call(&tool, &self.schema, call_args, call_kwargs) {
            Ok(bound) => bound,
            Err(error) => {
                return ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some(error.0),
                ))
            }
        };
        match self.executor.invoke(&tool, bound, ScriptToken::new()).await {
            Ok(value) => match monty_from_json(python_value(&value)) {
                Ok(object) => ExtFunctionResult::Return(object),
                Err(error) => raise(format!("{tool}: {error:#}")),
            },
            Err(error) => raise(format!("{tool}: {error}")),
        }
    }
}

fn raise(message: String) -> ExtFunctionResult {
    ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError, Some(message)))
}

/// The tool name, positional values and keyword pairs a `tools.NAME(...)`
/// call carries.
type CallParts = (String, Vec<Value>, Vec<(String, Value)>);

fn call_parts(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> std::result::Result<CallParts, MontyException> {
    let shape = || {
        MontyException::new(
            ExcType::TypeError,
            Some(format!(
                "{PYTHON_CALL_FUNCTION}() takes a tool name, a list and a dict"
            )),
        )
    };
    let mut parts = args.into_iter();
    let (name, list, dict) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(name), Some(list), Some(dict), None) => (name, list, dict),
        _ => return Err(shape()),
    };
    let right_shape = kwargs.is_empty()
        && name.as_ref().type_name() == "str"
        && list.as_ref().type_name() == "list"
        && dict.as_ref().type_name() == "dict";
    if !right_shape {
        return Err(shape());
    }
    match (
        json_from_monty(&name)?,
        json_from_monty(&list)?,
        json_from_monty(&dict)?,
    ) {
        (Value::String(name), Value::Array(list), Value::Object(dict)) => {
            Ok((name, list, dict.into_iter().collect()))
        }
        _ => Err(shape()),
    }
}

/// JSON for the Monty values a `tools.NAME(...)` call can pass (the
/// runtime's `from_python`, scoped to the JSON-compatible shapes).
fn json_from_monty(value: &MontyObject) -> std::result::Result<Value, MontyException> {
    json_from_ref(value.as_ref())
}

fn json_from_ref(item: ObjectRef<'_>) -> std::result::Result<Value, MontyException> {
    let invalid = |what: &str| {
        MontyException::new(
            ExcType::TypeError,
            Some(format!("{what} is not JSON-compatible")),
        )
    };
    Ok(match item.type_name() {
        "NoneType" => Value::Null,
        "bool" => Value::Bool(item.as_bool().ok_or_else(|| invalid("bool"))?),
        "int" => item
            .py_repr()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| invalid("int"))?,
        "str" => Value::String(item.as_str().ok_or_else(|| invalid("str"))?.to_owned()),
        "list" | "tuple" => {
            let items = item
                .items()
                .ok_or_else(|| invalid("sequence"))?
                .into_iter()
                .map(json_from_ref)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Value::Array(items)
        }
        "dict" => {
            let mut map = serde_json::Map::new();
            for (key, value) in item.pairs().ok_or_else(|| invalid("dict"))? {
                let key = key.as_str().ok_or_else(|| invalid("dict key"))?.to_owned();
                map.insert(key, json_from_ref(value)?);
            }
            Value::Object(map)
        }
        other => return Err(invalid(other)),
    })
}

/// Monty for the JSON a call returns (the runtime's `to_python`, scoped to
/// the same shapes).
fn monty_from_json(value: Value) -> Result<MontyObject> {
    Ok(match value {
        Value::Null => MontyObject::none(),
        Value::Bool(value) => MontyObject::bool(value),
        Value::String(value) => MontyObject::string(value),
        Value::Number(value) => value
            .as_i64()
            .map(MontyObject::int)
            .or_else(|| value.as_f64().map(MontyObject::float))
            .ok_or_else(|| anyhow!("JSON number cannot be represented in Python"))?,
        Value::Array(values) => {
            let items = values
                .into_iter()
                .map(monty_from_json)
                .collect::<Result<Vec<_>>>()?;
            MontyObject::list(items)
        }
        Value::Object(values) => {
            let entries = values
                .into_iter()
                .map(|(key, value)| Ok((MontyObject::string(key), monty_from_json(value)?)))
                .collect::<Result<Vec<_>>>()?;
            MontyObject::dict(entries)
        }
    })
}

/// The `bash`-tool view of one `structured_echo` tool: the upstream's
/// advertised mapping, mapped through the native server's own
/// `map_upstream_tool`.
struct ScriptHarness {
    _keys: tempfile::TempDir,
    _fixture: NativeFixture,
    upstream: UpstreamHandle,
    toolset: Arc<RemoteCredentialsToolset>,
    spec: harnx_script::commands::ToolSpec,
}

impl ScriptHarness {
    fn executor(&self, structured: bool) -> Arc<RemoteScriptExecutor> {
        Arc::new(RemoteScriptExecutor {
            toolset: Arc::clone(&self.toolset),
            user_id: "alice".to_string(),
            structured,
        })
    }

    async fn run_bash(&self, script: &str, structured: bool) -> ScriptRun {
        let commands = tool_commands(vec![self.spec.clone()], self.executor(structured))
            .expect("register tool commands");
        BashKitBackend
            .run(ScriptRequest {
                script,
                python_functions: None,
                limits: ScriptLimits {
                    timeout_secs: 30,
                    ..ScriptLimits::default()
                },
                commands,
                fs_mounts: vec![],
                cancel: ScriptToken::new(),
            })
            .await
    }

    async fn run_python(&self, code: &str, structured: bool) -> ScriptRun {
        let names = vec![self.spec.name.clone()];
        let functions = ScriptPythonFunctions {
            names: vec![PYTHON_CALL_FUNCTION.to_string()],
            handler: Arc::new(PythonToolsHandler {
                executor: self.executor(structured),
                schema: self.spec.schema.clone(),
            }),
            prelude: Some(tools_prelude(&names)),
        };
        let commands = tool_commands(vec![self.spec.clone()], self.executor(structured))
            .expect("register tool commands");
        let script = format!("python3 <<'PY'\n{code}\nPY\n");
        BashKitBackend
            .run(ScriptRequest {
                script: &script,
                python_functions: Some(functions),
                limits: ScriptLimits {
                    timeout_secs: 30,
                    ..ScriptLimits::default()
                },
                commands,
                fs_mounts: vec![],
                cancel: ScriptToken::new(),
            })
            .await
    }
}

async fn script_harness() -> ScriptHarness {
    let upstream = spawn_structured_upstream("authorization")
        .await
        .expect("start mock upstream");
    let (keys, fixture) = alice_fixture(&upstream).await;
    let catalog = wait_for_catalog_tool(
        &fixture.server.lifecycle,
        &alice_principal(),
        "structured_echo",
    )
    .await;
    let tool = catalog
        .record
        .tools
        .iter()
        .find(|tool| &*tool.name == "structured_echo")
        .expect("catalog holds structured_echo")
        .clone();
    // The script-facing spec derives from the mapped harnx ToolSpec, the
    // runtime's ScriptShape path: the upstream's `harnx:script_output` meta
    // survives the mapping and becomes the command's output mapping.
    let mapped = map_upstream_tool(&tool, false);
    let output = mapped.script_output().and_then(|output| {
        serde_json::from_value(serde_json::to_value(output).expect("script output serializes")).ok()
    });
    let output_schema = mapped.output_schema().cloned();
    let spec = harnx_script::commands::ToolSpec {
        name: mapped.name,
        description: mapped.description,
        schema: mapped.input_schema,
        output,
        output_schema,
    };
    let toolset = Arc::new(RemoteCredentialsToolset::new(
        Arc::clone(&fixture.server.lifecycle),
        fixture.server.base_url.clone(),
    ));
    ScriptHarness {
        _keys: keys,
        _fixture: fixture,
        upstream,
        toolset,
        spec,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_command_maps_structured_streams_and_exit_status() {
    harnx_core::require_nextest();
    let harness = script_harness().await;
    let run = harness.run_bash("structured_echo", true).await;
    assert_eq!(run.status, ScriptStatus::Error, "{run:?}");
    assert_eq!(run.exit_code, 3, "{run:?}");
    assert_eq!(run.stdout, "upstream stdout\n", "{run:?}");
    assert_eq!(run.stderr, "structured_echo: upstream stderr\n", "{run:?}");

    let run = harness.run_bash("structured_echo -o json", true).await;
    assert_eq!(run.status, ScriptStatus::Error, "{run:?}");
    assert_eq!(run.exit_code, 3, "{run:?}");
    let printed: Value = serde_json::from_str(run.stdout.trim_end())
        .unwrap_or_else(|error| panic!("stdout {error}: {run:?}"));
    assert_eq!(printed, expected_structured(), "{run:?}");
    assert_eq!(
        harness.upstream.structured_requests(),
        vec![Some(json!(true)), Some(json!(true))],
        "both script calls asked for the structured result"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bash_command_without_the_request_stays_model_style() {
    harnx_core::require_nextest();
    let harness = script_harness().await;
    let run = harness.run_bash("structured_echo", false).await;
    assert_eq!(run.status, ScriptStatus::Ok, "{run:?}");
    assert_eq!(run.exit_code, 0, "{run:?}");
    assert_eq!(run.stdout, "model-style text\n", "{run:?}");
    assert_eq!(run.stderr, "", "{run:?}");

    let run = harness.run_bash("structured_echo -o json", false).await;
    assert_eq!(run.status, ScriptStatus::Ok, "{run:?}");
    let printed: Value = serde_json::from_str(run.stdout.trim_end())
        .unwrap_or_else(|error| panic!("stdout {error}: {run:?}"));
    assert_eq!(
        printed["content"][0]["text"],
        json!("model-style text"),
        "{run:?}"
    );
    assert!(
        printed.get("structuredContent").is_none(),
        "no structuredContent without the request: {printed}"
    );
    assert_eq!(
        harness.upstream.structured_requests(),
        vec![None, None],
        "neither script call asked for the structured result"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_tools_returns_the_structured_dict_only_when_asked() {
    harnx_core::require_nextest();
    let harness = script_harness().await;
    let run = harness
        .run_python(
            "result = tools.structured_echo()\nprint(result['stdout'], end='')\nprint(result['count'])",
            true,
        )
        .await;
    assert_eq!(run.status, ScriptStatus::Ok, "{run:?}");
    assert_eq!(run.stdout, "upstream stdout\n2\n", "{run:?}");

    let run = harness
        .run_python(
            "result = tools.structured_echo()\nprint(result, end='')",
            false,
        )
        .await;
    assert_eq!(run.status, ScriptStatus::Ok, "{run:?}");
    assert_eq!(run.stdout, "model-style text\n", "{run:?}");
    assert_eq!(
        harness.upstream.structured_requests(),
        vec![Some(json!(true)), None],
        "Python sees the same request switching as the Bash commands"
    );
}
