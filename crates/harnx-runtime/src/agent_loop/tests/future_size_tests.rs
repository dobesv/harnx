use super::*;

#[test]
fn tool_round_futures_keep_checkpoint_state_out_of_callers() {
    let config = Arc::new(ConfigLock::new(crate::config::Config::default()));
    let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("future is not polled"));
    let ctx = make_test_context(config.clone(), call_fn, handoff_on_tool_round());
    let input = crate::config::input::from_str(&config, "user prompt", None);
    let fresh = crate::tool::execute_tool_round(
        ctx.tool_round_params(
            &config,
            &input,
            CompletionText {
                output: "",
                thought: None,
            },
        ),
        Vec::new(),
    );
    let fresh_bytes = std::mem::size_of_val(&fresh);
    // An inline durable round used 17 KiB here and grew every enclosing future,
    // even when middleware wasn't configured. Leave room for small new fields.
    assert!(
        fresh_bytes <= 1024,
        "new-round future embeds dispatch state: {fresh_bytes} bytes"
    );
    drop(fresh);

    let resumed = continue_agent_loop_from_tool_round(
        &ctx,
        input,
        PendingToolRound {
            seq: 1,
            output: String::new(),
            thought: None,
            calls: Vec::new(),
        },
        Vec::new(),
        Default::default(),
    );
    let resumed_bytes = std::mem::size_of_val(&resumed);
    assert!(
        resumed_bytes <= 12 * 1024,
        "resumed-round future embeds dispatch state: {resumed_bytes} bytes"
    );
}
