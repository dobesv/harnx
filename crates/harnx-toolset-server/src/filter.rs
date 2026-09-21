//! Tool filtering via glob patterns.
//!
//! Provides [`compile_enable_globs`] to build a [`globset::GlobSet`] from pattern strings,
//! and [`FilteredToolset`] to wrap a [`harnx_toolset::Toolset`] with filtering.

use anyhow::anyhow;
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// CLI flag for enabling specific tools by glob pattern.
pub const ENABLE_TOOL_FLAG: &str = "--enable-tool";

/// Compile a list of glob patterns into a `GlobSet`.
///
/// - Empty slice returns `Ok(None)`.
/// - Non-empty slice compiles each pattern and returns `Ok(Some(set))`.
/// - Invalid glob patterns return an `Err` describing the bad pattern.
/// - An empty string pattern `""` returns an `Err` (fail loud, never silent).
pub fn compile_enable_globs(patterns: &[String]) -> anyhow::Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() {
            return Err(anyhow!("empty glob pattern is not allowed"));
        }
        let glob =
            Glob::new(pattern).map_err(|e| anyhow!("invalid glob pattern {:?}: {}", pattern, e))?;
        builder.add(glob);
    }

    let set = builder.build()?;
    Ok(Some(set))
}

fn parse_separate_flag(args: &mut impl Iterator<Item = OsString>) -> anyhow::Result<String> {
    let value = args
        .next()
        .ok_or_else(|| anyhow!("{} requires a glob pattern argument", ENABLE_TOOL_FLAG))?;
    let value = value
        .to_str()
        .ok_or_else(|| anyhow!("{} argument is not valid UTF-8", ENABLE_TOOL_FLAG))?;
    if value.is_empty() || value.starts_with("--") {
        anyhow::bail!("{ENABLE_TOOL_FLAG} requires a non-empty glob pattern");
    }
    Ok(value.to_owned())
}

fn parse_inline_flag(value: &str) -> anyhow::Result<String> {
    if value.is_empty() {
        anyhow::bail!("{ENABLE_TOOL_FLAG} requires a non-empty glob pattern");
    }
    Ok(value.to_owned())
}

/// Extract one enable-tool glob pattern from a command-line argument.
fn parse_enable_tool_arg(
    arg: &OsStr,
    args: &mut impl Iterator<Item = OsString>,
) -> anyhow::Result<Option<String>> {
    let Some(arg) = arg.to_str() else {
        return Ok(None);
    };
    if arg == ENABLE_TOOL_FLAG {
        return parse_separate_flag(args).map(Some);
    }
    arg.strip_prefix("--enable-tool=")
        .map(parse_inline_flag)
        .transpose()
}

pub fn enable_tools_from_args(args: &[OsString]) -> anyhow::Result<Vec<String>> {
    let mut patterns = Vec::new();
    let mut args = args.iter().cloned();
    while let Some(arg) = args.next() {
        if let Some(pattern) = parse_enable_tool_arg(&arg, &mut args)? {
            patterns.push(pattern);
        }
    }
    Ok(patterns)
}

/// Check whether a tool name is enabled by the given `GlobSet`.
///
/// Raw tool names (e.g., `exec`, `read`) are matched directly against the patterns.
#[inline]
pub fn tool_enabled(set: &GlobSet, name: &str) -> bool {
    set.is_match(name)
}

/// Wrapper that filters a toolset by a allowlist of glob patterns.
///
/// Implements [`Toolset`] by delegating to the inner toolset, but:
/// - `tools()` returns only the tools whose names match the filter.
/// - `invoke` and `replay` reject filtered-out tools with a "not available" error.
pub struct FilteredToolset<T: Toolset> {
    inner: T,
    filter: Arc<GlobSet>,
}

/// Wrapper that allows `FilteredToolset` to own an `Arc<dyn Toolset>`.
pub struct ArcToolsetWrapper(pub Arc<dyn Toolset>);

#[async_trait]
impl Toolset for ArcToolsetWrapper {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.0.tools()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.0.invoke(tool, args, cancel).await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        self.0.invoke_with_context(invocation).await
    }

    fn can_replay(&self, tool: &str) -> bool {
        self.0.can_replay(tool)
    }

    async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        self.0.replay(invocation).await
    }

    async fn cancel(&self, invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        self.0.cancel(invocation).await
    }
}

impl<T: Toolset> FilteredToolset<T> {
    /// Create a new filtered toolset.
    ///
    /// The `filter` glob set determines which tools are visible.
    pub fn new(inner: T, filter: GlobSet) -> Self {
        Self {
            inner,
            filter: Arc::new(filter),
        }
    }

    /// Check if a tool name passes the filter.
    fn is_enabled(&self, name: &str) -> bool {
        tool_enabled(&self.filter, name)
    }

    /// Return the standard "not available" error for a filtered-out tool.
    fn unavailable_error(name: &str) -> ToolInvokeError {
        ToolInvokeError::Recoverable(format!("tool '{}' is not available on this server", name))
    }
}

#[async_trait]
impl<T: Toolset> Toolset for FilteredToolset<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.inner
            .tools()
            .into_iter()
            .filter(|spec| self.is_enabled(&spec.name))
            .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if !self.is_enabled(tool) {
            return Err(Self::unavailable_error(tool));
        }
        self.inner.invoke(tool, args, cancel).await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        if !self.is_enabled(&invocation.tool) {
            return Err(Self::unavailable_error(&invocation.tool));
        }
        self.inner.invoke_with_context(invocation).await
    }

    fn can_replay(&self, tool: &str) -> bool {
        if !self.is_enabled(tool) {
            return false;
        }
        self.inner.can_replay(tool)
    }

    async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        if !self.is_enabled(&invocation.tool) {
            return Err(Self::unavailable_error(&invocation.tool));
        }
        self.inner.replay(invocation).await
    }

    async fn cancel(&self, invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        if !self.is_enabled(&invocation.tool) {
            return Err(Self::unavailable_error(&invocation.tool));
        }
        self.inner.cancel(invocation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
    fn empty_patterns_returns_none() {
        let patterns: Vec<String> = vec![];
        let result = compile_enable_globs(&patterns).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn valid_patterns_returns_some() {
        let patterns = vec!["exec".to_string(), "read".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "read"));
        assert!(!tool_enabled(&set, "write"));
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

    #[test]
    fn globset_matches_prefix_and_suffix() {
        let patterns = vec!["read*".to_string(), "*exec_log".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "read"));
        assert!(tool_enabled(&set, "read_file"));
        assert!(tool_enabled(&set, "read_exec_log"));
        assert!(!tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "get_exec_log"));
        assert!(tool_enabled(&set, "read_exec_log"));
        assert!(!tool_enabled(&set, "write"));
    }

    #[test]
    fn multiple_patterns_combined() {
        let patterns = vec!["exec".to_string(), "read*".to_string()];
        let set = compile_enable_globs(&patterns).unwrap().unwrap();
        assert!(tool_enabled(&set, "exec"));
        assert!(tool_enabled(&set, "read"));
        assert!(tool_enabled(&set, "read_file"));
        assert!(!tool_enabled(&set, "write"));
        assert!(!tool_enabled(&set, "delete"));
    }

    // Tests for FilteredToolset
    #[test]
    fn filtered_toolset_filters_tools_list() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);
        let tools = filtered.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "exec");
    }

    #[test]
    fn filtered_toolset_glob_patterns() {
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
    async fn filtered_toolset_invoke_rejected_on_filtered_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let invoke_count = inner.invoke_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let result = filtered
            .invoke("read_exec_log", Value::Null, CancellationToken::new())
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolInvokeError::Recoverable(msg) if msg.contains("is not available on this server"))
        );
        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn filtered_toolset_invoke_delegates_on_allowed_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let invoke_count = inner.invoke_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let result = filtered
            .invoke("exec", Value::Null, CancellationToken::new())
            .await;
        assert!(result.is_ok());
        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn filtered_toolset_invoke_with_context_rejected() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let invoke_count = inner.invoke_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "read_exec_log".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.invoke_with_context(invocation).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolInvokeError::Recoverable(msg) if msg.contains("is not available on this server"))
        );
        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn filtered_toolset_invoke_with_context_delegates() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let invoke_count = inner.invoke_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "exec".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.invoke_with_context(invocation).await;
        assert!(result.is_ok());
        assert_eq!(invoke_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn filtered_toolset_can_replay_false_on_filtered_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);
        assert!(!filtered.can_replay("read_exec_log"));
        assert!(filtered.can_replay("exec"));
    }

    #[tokio::test]
    async fn filtered_toolset_replay_rejected_on_filtered_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let replay_count = inner.replay_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "read_exec_log".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.replay(invocation).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolInvokeError::Recoverable(msg) if msg.contains("is not available on this server"))
        );
        assert_eq!(replay_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn filtered_toolset_replay_delegates_on_allowed_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let replay_count = inner.replay_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "exec".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.replay(invocation).await;
        assert!(result.is_ok());
        assert_eq!(replay_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn filtered_toolset_cancel_rejected_on_filtered_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let cancel_count = inner.cancel_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "read_exec_log".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.cancel(invocation).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolInvokeError::Recoverable(msg) if msg.contains("is not available on this server"))
        );
        assert_eq!(cancel_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn filtered_toolset_cancel_delegates_on_allowed_name() {
        let inner = FakeToolset::new(&["exec", "read_exec_log", "tmpl_deploy"]);
        let cancel_count = inner.cancel_count.clone();
        let filter = compile_enable_globs(&["exec".to_string()])
            .unwrap()
            .unwrap();
        let filtered = FilteredToolset::new(inner, filter);

        let invocation = ToolInvocation {
            tool: "exec".to_string(),
            args: Value::Null,
            cancel: CancellationToken::new(),
            context: harnx_toolset::ToolInvocationContext {
                call_id: "test".to_string(),
                invoking_session_id: Some("session".to_string()),
                capabilities: Default::default(),
                checkpoint: None,
                checkpoint_store: None,
            },
        };
        let result = filtered.cancel(invocation).await;
        assert!(result.is_ok());
        assert_eq!(cancel_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // Tests for enable_tools_from_args
    #[test]
    fn enable_tools_from_args_empty_when_absent() {
        let args: Vec<std::ffi::OsString> =
            vec!["program".into(), "--other-flag".into(), "value".into()];
        let result = enable_tools_from_args(&args).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn enable_tools_from_args_separate_form() {
        let args: Vec<std::ffi::OsString> =
            vec!["program".into(), "--enable-tool".into(), "exec".into()];
        let result = enable_tools_from_args(&args).unwrap();
        assert_eq!(result, vec!["exec"]);
    }

    #[test]
    fn enable_tools_from_args_equals_form() {
        let args: Vec<std::ffi::OsString> = vec!["program".into(), "--enable-tool=read".into()];
        let result = enable_tools_from_args(&args).unwrap();
        assert_eq!(result, vec!["read"]);
    }

    #[test]
    fn enable_tools_from_args_repeatable() {
        let args: Vec<std::ffi::OsString> = vec![
            "program".into(),
            "--enable-tool".into(),
            "exec".into(),
            "--enable-tool=read*".into(),
        ];
        let result = enable_tools_from_args(&args).unwrap();
        assert_eq!(result, vec!["exec", "read*"]);
    }

    #[test]
    fn enable_tools_from_args_mixed_forms() {
        let args: Vec<std::ffi::OsString> = vec![
            "program".into(),
            "--enable-tool".into(),
            "a".into(),
            "--enable-tool=b*".into(),
            "--enable-tool".into(),
            "c".into(),
            "--other-flag".into(),
        ];
        let result = enable_tools_from_args(&args).unwrap();
        assert_eq!(result, vec!["a", "b*", "c"]);
    }

    #[test]
    fn enable_tools_from_args_trailing_flag_errors() {
        let args: Vec<std::ffi::OsString> = vec!["program".into(), "--enable-tool".into()];
        let result = enable_tools_from_args(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("requires a glob pattern argument"));
    }

    #[test]
    fn enable_tools_from_args_rejects_empty_or_flag_values() {
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
