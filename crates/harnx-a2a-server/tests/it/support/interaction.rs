//! Real middleware fixtures, separate from the legacy PreToolUse approval agents.
use super::*;

pub(super) fn agent(script: Script, dir: &std::path::Path) -> Result<Option<String>> {
    if matches!(
        script,
        Script::A2uiTool | Script::A2uiSerial | Script::A2uiSameRound
    ) {
        return super::a2ui::agent(script, dir).map(Some);
    }
    if script == Script::ShippedDaedalus {
        return shipped_daedalus(dir).map(Some);
    }
    let (tool, body) = match script {
        Script::ConfirmTool | Script::ConfirmHeld => ("bash_exec", "if confirm --id first 'Run counted command?'; then next; else deny 'Denied by user'; fi"),
        Script::ConfirmSerial => ("bash_exec", "if confirm --id first 'First confirmation?' && confirm --id second 'Second confirmation?'; then next; else deny 'Denied by user'; fi"),
        Script::ConfirmSameRound => {
            write_same_round_tools_agent(dir)?;
            ("time_get_current_time", "if confirm --id first \"Approve $(tool_input | jq -r .tool_call_id)?\"; then next; else deny 'Denied by user'; fi")
        }
        _ => return Ok(None),
    };
    let middleware = serde_json::json!([{
        "name": "confirmation", "match": format!("^{tool}$"), "bash": body,
    }]);
    Ok(Some(format!("---\nmodel: mock:test\nuse_tools: [{tool}]\nmiddleware: {middleware}\n---\nTest middleware confirmations\n")))
}

fn shipped_daedalus(dir: &std::path::Path) -> Result<String> {
    let agents = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/pantheon/agents");
    let source = std::fs::read_to_string(agents.join("daedalus.md"))?;
    let loaded = harnx_runtime::config::agent::load(&agents.join("daedalus.md"))?;
    assert!(loaded.hooks().is_none());
    // Keep the shipped policy and prompt; only route model requests to the stub.
    let model_line = source
        .lines()
        .find(|line| line.starts_with("model: "))
        .context("shipped model")?;
    let source = source.replacen(model_line, "model: mock:test", 1);
    std::fs::create_dir_all(dir.join("agents/shared"))?;
    for entry in std::fs::read_dir(agents.join("shared"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(
                entry.path(),
                dir.join("agents/shared").join(entry.file_name()),
            )?;
        }
    }
    Ok(source)
}
