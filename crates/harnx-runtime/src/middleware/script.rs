use super::{bash, frame::Frame, python, tools};
use anyhow::{anyhow, Result};
use harnx_core::middleware::{MiddlewareSource, ResolvedMiddleware};
use harnx_script::{
    BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRequest, ScriptStatus,
};
use std::sync::Arc;

pub(super) async fn execute(frame: &Arc<Frame>, entry: &ResolvedMiddleware) -> Result<()> {
    let mut commands = tools::commands(frame)?;
    anyhow::ensure!(
        !frame
            .tools
            .iter()
            .any(|tool| bash::OPERATIONS.contains(&tool.name.as_str())),
        "allowlisted tool collides with a middleware operation"
    );
    commands.extend(bash::commands(frame));
    let script = source(&entry.source);
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let backend = BashKitBackend;
    let run = backend.run(ScriptRequest {
        script: &script,
        limits: ScriptLimits {
            timeout_secs: entry.timeout.as_secs().saturating_add(1),
            ..Default::default()
        },
        commands,
        python_functions: Some(python::functions(frame)),
        fs_mounts: vec![],
        cancel: cancel.clone(),
    });
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result,
        () = tokio::time::sleep(entry.timeout) => {
            cancel.cancel();
            let _ = run.await;
            return Err(anyhow!("middleware timed out after {:?} (including downstream calls)", entry.timeout));
        }
        () = harnx_core::abort::wait_abort_signal(&frame.prepared.context.abort) => {
            cancel.cancel();
            let _ = run.await;
            frame.terminal.lock().get_or_insert(Err(harnx_core::tool::ToolError::Fatal(
                anyhow!("interrupted during middleware execution"),
            )));
            return Err(anyhow!("middleware interrupted"));
        }
    };
    if result.status == ScriptStatus::Ok {
        Ok(())
    } else {
        Err(anyhow!(
            "middleware script {:?}, exit {}: {}{}",
            result.status,
            result.exit_code,
            result.error.unwrap_or_default(),
            result.stderr
        ))
    }
}

fn source(source: &MiddlewareSource) -> String {
    match source {
        MiddlewareSource::Bash(source) => source.clone(),
        MiddlewareSource::Python(source) => {
            let delimiter = format!("HARNX_PYTHON_{}", uuid::Uuid::new_v4().simple());
            format!("python3 <<'{delimiter}'\n{source}\n{delimiter}\n")
        }
    }
}
