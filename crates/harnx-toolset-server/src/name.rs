//! Toolset registration name overrides.

use anyhow::anyhow;
use async_trait::async_trait;
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use tokio_util::sync::CancellationToken;

/// CLI flag for overriding a toolset's registration name.
pub const NAME_FLAG: &str = "--name";

/// Validate a toolset name used as one NATS subject token and an agent tool prefix.
///
/// `label` identifies the CLI flag, environment variable, or API field that supplied the name.
pub fn validate_toolset_name(name: &str, label: &str) -> anyhow::Result<()> {
    let starts_with_alphanumeric = name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric);
    let contains_only_valid_bytes = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !starts_with_alphanumeric || !contains_only_valid_bytes {
        anyhow::bail!(
            "invalid {label} {name:?}: start with an ASCII letter or digit and use only ASCII letters, digits, '-' or '_'"
        );
    }
    Ok(())
}

fn parse_separate_flag(args: &mut impl Iterator<Item = OsString>) -> anyhow::Result<String> {
    let value = args
        .next()
        .ok_or_else(|| anyhow!("{NAME_FLAG} requires a name argument"))?;
    let value = value
        .to_str()
        .ok_or_else(|| anyhow!("{NAME_FLAG} argument is not valid UTF-8"))?;
    if value.starts_with('-') {
        anyhow::bail!("{NAME_FLAG} requires a name argument; found option-like value {value:?}");
    }
    validate_toolset_name(value, NAME_FLAG)?;
    Ok(value.to_owned())
}

fn parse_inline_flag(value: &str) -> anyhow::Result<String> {
    if value.starts_with('-') {
        anyhow::bail!("{NAME_FLAG} requires a name argument; found option-like value {value:?}");
    }
    validate_toolset_name(value, NAME_FLAG)?;
    Ok(value.to_owned())
}

fn parse_name_arg(
    arg: &OsStr,
    args: &mut impl Iterator<Item = OsString>,
) -> anyhow::Result<Option<String>> {
    let Some(arg) = arg.to_str() else {
        return Ok(None);
    };
    if arg == NAME_FLAG {
        return parse_separate_flag(args).map(Some);
    }
    arg.strip_prefix("--name=")
        .map(parse_inline_flag)
        .transpose()
}

/// Extract an optional toolset name override from command-line arguments.
pub fn toolset_name_from_args(args: &[OsString]) -> anyhow::Result<Option<String>> {
    let mut name = None;
    let mut args = args.iter().cloned();
    while let Some(arg) = args.next() {
        if let Some(value) = parse_name_arg(&arg, &mut args)? {
            if name.replace(value).is_some() {
                anyhow::bail!("{NAME_FLAG} may only be specified once");
            }
        }
    }
    Ok(name)
}

/// Wrapper that changes a toolset's registration name and delegates all other behavior.
pub struct NamedToolset<T: Toolset> {
    inner: T,
    name: String,
}

impl<T: Toolset> NamedToolset<T> {
    /// Create a wrapper that registers `inner` under `name`.
    pub fn new(inner: T, name: String) -> anyhow::Result<Self> {
        validate_toolset_name(&name, "toolset name")?;
        Ok(Self { inner, name })
    }
}

#[async_trait]
impl<T: Toolset> Toolset for NamedToolset<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn default_mcp_http_port(&self) -> u16 {
        self.inner.default_mcp_http_port()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.inner.tools()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.inner.invoke(tool, args, cancel).await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        self.inner.invoke_with_context(invocation).await
    }

    fn can_replay(&self, tool: &str) -> bool {
        self.inner.can_replay(tool)
    }

    async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        self.inner.replay(invocation).await
    }

    async fn cancel(&self, invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        self.inner.cancel(invocation).await
    }
}
