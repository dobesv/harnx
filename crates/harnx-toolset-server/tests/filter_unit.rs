mod tests {
    use async_trait::async_trait;
    use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
    use harnx_toolset_server::filter::{
        compile_enable_globs, enable_tools_from_args, tool_enabled, FilteredToolset,
    };
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// A minimal fake Toolset for testing FilteredToolset behavior.
    struct FakeToolset {
        tools: Vec<ToolSpec>,
        invoke_count: Arc<std::sync::atomic::AtomicUsize>,
        replay_count: Arc<std::sync::atomic::AtomicUsize>,
        cancel_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeToolset {
        fn new(tool_names: &[&str]) -> Self {
            Self {
                tools: tool_names
                    .iter()
                    .map(|name| ToolSpec {
                        cancellation_guarantee: Default::default(),
                        name: name.to_string(),
                        description: format!("{name} tool"),
                        input_schema: serde_json::json!({ "type": "object" }),
                        idempotent_hint: false,
                        read_only_hint: false,
                        timeout_secs: None,
                        meta: None,
                    })
                    .collect(),
                invoke_count: Arc::default(),
                replay_count: Arc::default(),
                cancel_count: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl Toolset for FakeToolset {
        fn name(&self) -> &str {
            "fake"
        }

        fn tools(&self) -> Vec<ToolSpec> {
            self.tools.clone()
        }

        async fn invoke(
            &self,
            tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            self.invoke_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "invoked": tool }))
        }

        fn can_replay(&self, tool: &str) -> bool {
            self.tools.iter().any(|t| t.name == tool)
        }

        async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
            self.replay_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({ "replayed": invocation.tool }))
        }

        async fn cancel(&self, _invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
            self.cancel_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn compile_enable_globs_matching() {
        let empty: Vec<String> = vec![];
        assert!(compile_enable_globs(&empty).unwrap().is_none());

        let patterns = vec!["exec".to_string(), "read".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "read"));
        assert!(!tool_enabled(&set, "write"));

        let patterns = vec!["read*".to_string(), "*exec_log".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        for name in ["read", "read_file", "read_exec_log", "get_exec_log"] {
            assert!(tool_enabled(&set, name));
        }
        for name in ["exec", "write"] {
            assert!(!tool_enabled(&set, name));
        }

        let patterns = vec!["exec".to_string(), "read*".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "read"));
        assert!(tool_enabled(&set, "read_file"));
        assert!(!tool_enabled(&set, "write"));
        assert!(!tool_enabled(&set, "delete"));
    }

    #[test]
    fn glob_patterns_work() {
        let patterns = vec!["*".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "read"));
        assert!(tool_enabled(&set, "anything"));
    }

    #[test]
    fn invalid_glob_returns_error() {
        let patterns = vec!["[invalid".to_string()];
        let result = compile_enable_globs(&patterns);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("[invalid"));
    }

    #[test]
    fn empty_string_pattern_returns_error() {
        let patterns = vec!["".to_string()];
        let result = compile_enable_globs(&patterns);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("empty glob pattern"));
    }

    fn invocation(tool: &str) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        }
    }

    fn assert_unavailable<T: std::fmt::Debug>(result: Result<T, ToolInvokeError>) {
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolInvokeError::Recoverable(msg) if msg.contains("is not available on this server"))
        );
    }

    // Tests for FilteredToolset
    #[test]
    fn filtered_toolset_tools_listing() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);
        let tools = filtered.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "exec");

        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let filter = compile_enable_globs(&["*_exec_log".to_string(), "tmpl_*".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);
        let tools = filtered.tools();
        let names: BTreeSet<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains("read_exec_log"));
        assert!(names.contains("tmpl_deploy"));
        assert!(!names.contains("exec"));
    }

    #[tokio::test]
    async fn filtered_toolset_execution_behavior() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let invoke_count = inner.invoke_count.clone();
        let replay_count = inner.replay_count.clone();
        let cancel_count = inner.cancel_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        assert_unavailable(
            filtered
                .invoke("read_exec_log", Value::Null, CancellationToken::new())
                .await,
        );
        assert_unavailable(
            filtered
                .invoke_with_context(invocation("read_exec_log"))
                .await,
        );
        assert_unavailable(filtered.replay(invocation("read_exec_log")).await);
        assert_unavailable(filtered.cancel(invocation("read_exec_log")).await);
        assert!(!filtered.can_replay("read_exec_log"));
        assert!(filtered.can_replay("exec"));

        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(replay_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(cancel_count.load(std::sync::atomic::Ordering::SeqCst), 0);

        assert!(filtered
            .invoke("exec", Value::Null, CancellationToken::new())
            .await
            .is_ok());
        assert!(filtered
            .invoke_with_context(invocation("exec"))
            .await
            .is_ok());
        assert!(filtered.replay(invocation("exec")).await.is_ok());
        assert!(filtered.cancel(invocation("exec")).await.is_ok());
        assert!(filtered.can_replay("exec"));

        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(replay_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(cancel_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // Tests for enable_tools_from_args
    #[test]
    fn enable_tools_from_args_parsing() {
        let args: Vec<std::ffi::OsString> =
            vec!["program".into(), "--other-flag".into(), "value".into()];
        assert!(enable_tools_from_args(&args).unwrap().is_empty());

        let args: Vec<std::ffi::OsString> =
            vec!["program".into(), "--enable-tool".into(), "exec".into()];
        assert_eq!(enable_tools_from_args(&args).unwrap(), vec!["exec"]);

        let args: Vec<std::ffi::OsString> = vec!["program".into(), "--enable-tool=read".into()];
        assert_eq!(enable_tools_from_args(&args).unwrap(), vec!["read"]);

        let args: Vec<std::ffi::OsString> = vec![
            "program".into(),
            "--enable-tool".into(),
            "exec".into(),
            "--enable-tool=read*".into(),
        ];
        assert_eq!(
            enable_tools_from_args(&args).unwrap(),
            vec!["exec", "read*"]
        );

        let args: Vec<std::ffi::OsString> = vec![
            "program".into(),
            "--enable-tool".into(),
            "a".into(),
            "--enable-tool=b*".into(),
            "--enable-tool".into(),
            "c".into(),
            "--other-flag".into(),
        ];
        assert_eq!(enable_tools_from_args(&args).unwrap(), vec!["a", "b*", "c"]);
    }

    #[test]
    fn enable_tools_from_args_errors() {
        let args: Vec<std::ffi::OsString> = vec!["program".into(), "--enable-tool".into()];
        let result = enable_tools_from_args(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("requires a glob pattern argument"));

        for value in ["", "--help"] {
            let args: Vec<std::ffi::OsString> =
                vec!["program".into(), "--enable-tool".into(), value.into()];
            let error = enable_tools_from_args(&args).unwrap_err();
            assert!(error
                .to_string()
                .contains("requires a non-empty glob pattern"));
        }
        let args: Vec<std::ffi::OsString> = vec!["program".into(), "--enable-tool=".into()];
        let error = enable_tools_from_args(&args).unwrap_err();
        assert!(error
            .to_string()
            .contains("requires a non-empty glob pattern"));
    }
}
