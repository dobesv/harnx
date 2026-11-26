//! The `tools` object a script's Python uses to call tools.
//!
//! Each tool is an attribute built by a prelude; calling it reaches the
//! hidden host function `_harnx_call(name, args, kwargs)`. This module binds
//! those arguments to the tool's input schema and hands the bound call to the
//! context's runner, so every context shares one binding and one set of
//! argument errors and contributes only how a call runs.

use std::{collections::HashMap, sync::Arc};

use harnx_core::tool::ToolDeclaration;
use harnx_script::{
    bind_python_call, tools_prelude, ExcType, ExtFunctionResult, MontyException, MontyObject,
    ScriptHostContext, ScriptPythonFunctions, ScriptPythonHandler, PYTHON_CALL_FUNCTION,
};
use serde_json::Value;

use super::conversion::from_python;

/// How one context runs a tool for its Python, once the call is bound.
#[async_trait::async_trait]
pub(crate) trait PythonToolRunner: Send + Sync {
    async fn run_tool(
        &self,
        name: String,
        arguments: Value,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult;
}

/// Adds `tools` to a script's Python: each tool is an attribute that binds
/// its arguments against the tool's schema and hands the call to `runner`.
/// A context with no tools gets no `tools` object and no prelude, so its
/// Python behaves as if this had never been called.
pub(crate) fn with_tools(
    functions: ScriptPythonFunctions,
    tools: &[ToolDeclaration],
    runner: Arc<dyn PythonToolRunner>,
) -> ScriptPythonFunctions {
    let unchanged = functions.clone();
    (!tools.is_empty())
        .then_some(())
        .map_or(unchanged, |()| add_tools(functions, tools, runner))
}

fn add_tools(
    functions: ScriptPythonFunctions,
    tools: &[ToolDeclaration],
    runner: Arc<dyn PythonToolRunner>,
) -> ScriptPythonFunctions {
    let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let schemas = tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.parameters.as_value().clone()))
        .collect();
    let mut all_names = functions.names;
    all_names.push(PYTHON_CALL_FUNCTION.to_string());
    ScriptPythonFunctions {
        names: all_names,
        handler: Arc::new(ToolsHandler {
            inner: functions.handler,
            schemas,
            runner,
        }),
        prelude: Some(tools_prelude(&names)),
    }
}

struct ToolsHandler {
    inner: Arc<dyn ScriptPythonHandler>,
    schemas: HashMap<String, Value>,
    runner: Arc<dyn PythonToolRunner>,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for ToolsHandler {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        if name != PYTHON_CALL_FUNCTION {
            return self.inner.call(name, args, kwargs, ctx).await;
        }
        match self.bind(args, kwargs) {
            Ok((tool, arguments)) => self.runner.run_tool(tool, arguments, ctx).await,
            Err(error) => ExtFunctionResult::Error(error),
        }
    }
}

impl ToolsHandler {
    fn bind(
        &self,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
    ) -> Result<(String, Value), MontyException> {
        let (name, call_args, call_kwargs) = call_parts(args, kwargs)?;
        let schema = self.schemas.get(&name).ok_or_else(|| {
            MontyException::new(
                ExcType::AttributeError,
                Some(format!("'tools' has no tool '{name}'")),
            )
        })?;
        bind_python_call(&name, schema, call_args, call_kwargs)
            .map(|bound| (name, bound))
            .map_err(|error| MontyException::new(ExcType::TypeError, Some(error.0)))
    }
}

type CallParts = (String, Vec<Value>, Vec<(String, Value)>);

/// The tool name, positional values and keyword pairs a prelude call carries.
fn call_parts(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> Result<CallParts, MontyException> {
    let shape = || {
        type_error(format!(
            "{PYTHON_CALL_FUNCTION}() takes a tool name, a list and a dict"
        ))
    };
    let mut parts = args.into_iter();
    let (Some(name), Some(list), Some(dict), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(shape());
    };
    let right_shape = kwargs.is_empty()
        && name.as_ref().type_name() == "str"
        && list.as_ref().type_name() == "list"
        && dict.as_ref().type_name() == "dict";
    if !right_shape {
        return Err(shape());
    }
    match (convert(name)?, convert(list)?, convert(dict)?) {
        (Value::String(name), Value::Array(list), Value::Object(dict)) => {
            Ok((name, list, dict.into_iter().collect()))
        }
        _ => Err(shape()),
    }
}

fn convert(value: MontyObject) -> Result<Value, MontyException> {
    from_python(value).map_err(|error| type_error(format!("{error:#}")))
}

fn type_error(message: String) -> MontyException {
    MontyException::new(ExcType::TypeError, Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::{event::ToolKind, json_schema::JsonSchema};
    use harnx_script::{
        BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRequest, ScriptStatus,
    };
    use parking_lot::Mutex;
    use serde_json::json;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, Value)>>);

    #[async_trait::async_trait]
    impl PythonToolRunner for Recorder {
        async fn run_tool(
            &self,
            name: String,
            arguments: Value,
            _ctx: ScriptHostContext,
        ) -> ExtFunctionResult {
            self.0.lock().push((name, arguments));
            ExtFunctionResult::Return(MontyObject::string("ran"))
        }
    }

    struct Other;

    #[async_trait::async_trait]
    impl ScriptPythonHandler for Other {
        async fn call(
            &self,
            name: String,
            _args: Vec<MontyObject>,
            _kwargs: Vec<(MontyObject, MontyObject)>,
            _ctx: ScriptHostContext,
        ) -> ExtFunctionResult {
            ExtFunctionResult::Return(MontyObject::string(format!("other:{name}")))
        }
    }

    fn fs_read() -> ToolDeclaration {
        ToolDeclaration {
            name: "fs_read".into(),
            description: String::new(),
            parameters: JsonSchema::new(json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
                "required": ["path"],
            })),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: Some(ToolKind::Read),
        }
    }

    async fn run(python: &str) -> (String, Vec<(String, Value)>) {
        let recorder = Arc::new(Recorder::default());
        let inner = ScriptPythonFunctions {
            names: vec!["other_fn".into()],
            handler: Arc::new(Other),
            prelude: None,
        };
        let functions = with_tools(inner, &[fs_read()], recorder.clone());
        let result = BashKitBackend
            .run(ScriptRequest {
                script: &format!("python3 <<'PY'\n{python}\nPY\n"),
                limits: ScriptLimits::default(),
                commands: vec![],
                python_functions: Some(functions),
                fs_mounts: vec![],
                cancel: CancellationToken::new(),
            })
            .await;
        assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
        let calls = recorder.0.lock().clone();
        (result.stdout, calls)
    }

    #[tokio::test]
    async fn tools_hands_a_bound_call_to_the_runner() {
        let (stdout, calls) = run("print(tools.fs_read('a.txt', limit=3))").await;
        assert_eq!(stdout.trim(), "ran");
        assert_eq!(
            calls,
            vec![("fs_read".into(), json!({"path": "a.txt", "limit": 3}))]
        );
    }

    #[tokio::test]
    async fn a_bad_argument_never_reaches_the_runner() {
        let python = "try:\n    tools.fs_read(bogus=1)\nexcept TypeError as e:\n    print(e)\n\
                      try:\n    tools.fs_read()\nexcept TypeError as e:\n    print(e)";
        let (stdout, calls) = run(python).await;
        assert_eq!(
            stdout,
            "fs_read() got an unexpected keyword argument 'bogus'\n\
             fs_read() missing 1 required argument: 'path'\n"
        );
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_attribute_error() {
        let python =
            "try:\n    _harnx_call('nope', [], {})\nexcept AttributeError as e:\n    print(e)";
        let (stdout, calls) = run(python).await;
        assert_eq!(stdout.trim(), "'tools' has no tool 'nope'");
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn a_malformed_internal_call_is_a_type_error() {
        let python = "for a in [(1, [], {}), ('fs_read', {}, {}), ('fs_read', [], [])]:\n    \
                      try:\n        _harnx_call(*a)\n    except TypeError as e:\n        print(e)\n\
                      try:\n    _harnx_call('fs_read', [], {}, x=1)\nexcept TypeError as e:\n    print(e)";
        let (stdout, calls) = run(python).await;
        let shape = "_harnx_call() takes a tool name, a list and a dict\n";
        assert_eq!(stdout, shape.repeat(4));
        assert!(calls.is_empty());
    }

    #[test]
    fn no_tools_leaves_the_functions_unchanged() {
        let inner = ScriptPythonFunctions {
            names: vec!["other_fn".into()],
            handler: Arc::new(Other),
            prelude: None,
        };
        let functions = with_tools(inner, &[], Arc::new(Recorder::default()));
        assert_eq!(functions.names, vec!["other_fn".to_string()]);
        assert!(functions.prelude.is_none());
    }

    #[tokio::test]
    async fn other_host_functions_pass_through() {
        let (stdout, calls) = run("print(other_fn())").await;
        assert_eq!(stdout.trim(), "other:other_fn");
        assert!(calls.is_empty());
    }
}
