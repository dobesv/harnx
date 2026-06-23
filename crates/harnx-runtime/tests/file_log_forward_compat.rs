use anyhow::Result;
use harnx_core::{require_nextest, session::SessionLogEntry};

#[test]
fn file_log_yaml_round_trips_byte_identical_without_fence_tokens() -> Result<()> {
    require_nextest();

    let yaml = concat!(
        "type: header\n",
        "model: test-model\n",
        "save_session: true\n",
        "session_id: sess-1\n",
        "agent_instructions: You are Oracle\n",
        "---\n",
        "type: message\n",
        "role: user\n",
        "content: hello\n",
        "---\n",
        "type: message\n",
        "role: assistant\n",
        "content: hi there\n",
        "---\n",
        "type: tool_calls\n",
        "text: checking\n",
        "calls:\n",
        "- name: Bash\n",
        "  arguments:\n",
        "    command: pwd\n",
        "  id: call-1\n",
        "---\n",
        "type: tool_results\n",
        "results:\n",
        "- id: call-1\n",
        "  name: Bash\n",
        "  output:\n",
        "    stdout: /tmp\n",
    );

    let docs = yaml
        .split("---\n")
        .map(serde_yaml::from_str::<SessionLogEntry>)
        .collect::<Result<Vec<_>, _>>()?;
    let reserialized = docs
        .iter()
        .map(serde_yaml::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("---\n");

    assert_eq!(reserialized, yaml);
    assert!(!reserialized.contains("fence_token"));
    assert!(!reserialized.contains("null"));
    Ok(())
}

#[test]
fn unknown_session_log_entry_type_is_tolerated() -> Result<()> {
    require_nextest();

    let yaml = "type: future_variant\nnew_field: true\n";
    let entry: SessionLogEntry = serde_yaml::from_str(yaml)?;
    assert!(matches!(entry, SessionLogEntry::Unknown));
    Ok(())
}
