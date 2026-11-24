const MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    reject_http_transport(std::env::args_os().skip(1))?;
    // Keep debug-build transport futures off Windows' 1 MiB main-thread stack.
    std::thread::Builder::new()
        .name("claude-hook-tools".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(Box::pin(harnx_toolset_server::run_toolset_main(
                    harnx_claude_hook_tools::ClaudeHookToolset::default(),
                )))
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("Claude hook server thread panicked"))?
}

fn reject_http_transport(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> anyhow::Result<()> {
    if args
        .into_iter()
        .any(|arg| arg.as_ref() == std::ffi::OsStr::new("--mcp-http"))
    {
        anyhow::bail!(
            "--mcp-http is disabled for harnx-claude-hook-tools; use NATS or MCP stdio only"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reject_http_transport;

    #[test]
    fn rejects_http_transport() {
        let error = reject_http_transport(["--mcp-http"]).unwrap_err();
        assert!(error.to_string().contains("NATS or MCP stdio only"));
    }

    #[test]
    fn allows_nats_and_stdio_transports() {
        assert!(reject_http_transport(["--name", "claude_hook"]).is_ok());
        assert!(reject_http_transport(["--mcp-stdio"]).is_ok());
    }
}
