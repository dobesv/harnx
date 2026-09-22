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
