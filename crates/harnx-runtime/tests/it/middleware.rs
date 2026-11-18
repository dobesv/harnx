//! Runtime configuration → script runner → engine/provider integration.
mod confirm;
mod diagnostics;
mod documentation;
mod fetch;
mod fixture;
mod model;
mod policy;
mod provenance;
mod shipped;
mod timing;

use fixture::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_bash_rewrites_input_and_result() {
    let fixture = Fixture::new(vec![bash(
        r#"
args=$(tool_input | jq '.arguments | .value = "changed"')
r=$(next --id run "$args")
respond "$(echo "$r" | jq '.output.value = (.output.value + "-wrapped")')"
"#,
    )])
    .await;
    let result = fixture.eval(json!({"value": "original"})).await.unwrap();
    assert_eq!(result.output["value"], "changed-wrapped");
    assert_eq!(fixture.calls()[0].1["value"], "changed");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_python_rewrites_input_and_result() {
    let fixture = Fixture::new(vec![python(
        r#"
args = tool_input()["arguments"]
args["value"] = "changed"
r = call_next(args, id="run")
r["output"]["value"] += "-wrapped"
respond(r)
"#,
    )])
    .await;
    let result = fixture.eval(json!({"value": "original"})).await.unwrap();
    assert_eq!(result.output["value"], "changed-wrapped");
    assert_eq!(fixture.calls()[0].1["value"], "changed");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_denial_skips_tool_in_both_languages() {
    for entry in [
        bash("deny 'not permitted'"),
        python("deny('not permitted')"),
    ] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        assert_eq!(result.output["is_error"], true);
        assert_eq!(result.output["error"], "not permitted");
        assert_eq!(result.output["blocked_by_middleware"], "test");
        assert!(fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_exit_zero_defaults_to_pass_through_or_last_next() {
    for script in [":", "next '{\"value\":1}'; next '{\"value\":2}'"] {
        let fixture = Fixture::new(vec![bash(script)]).await;
        let result = fixture.eval(json!({"value": 2})).await.unwrap();
        assert_eq!(result.output["value"], 2);
        assert_eq!(fixture.calls().len(), if script == ":" { 1 } else { 2 });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_entries_nest_outermost_first() {
    let outer = json!({"name":"outer", "match":"^probe_run$", "python":r#"
a = tool_input()["arguments"]
a["value"] += "-outer-before"
r = call_next(a)
r["output"]["value"] += "-outer-after"
respond(r)
"#});
    let inner = json!({"name":"inner", "match":"^probe_run$", "python":r#"
a = tool_input()["arguments"]
a["value"] += "-inner-before"
r = call_next(a)
r["output"]["value"] += "-inner-after"
respond(r)
"#});
    let fixture = Fixture::new(vec![outer, inner]).await;
    let result = fixture.eval(json!({"value":"start"})).await.unwrap();
    assert_eq!(
        result.output["value"],
        "start-outer-before-inner-before-inner-after-outer-after"
    );
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_allowlisted_tools_bypass_chain_in_both_languages() {
    for mut entry in [
        bash("probe_aux --json '{\"value\":\"helper\"}'; next"),
        python("call_tool('probe_aux', {'value':'helper'})\ncall_next()"),
    ] {
        entry["tools"] = json!(["probe_aux"]);
        entry["match"] = json!("^probe_");
        let fixture = Fixture::new(vec![entry]).await;
        fixture.eval(json!({})).await.unwrap();
        assert_eq!(
            fixture
                .calls()
                .iter()
                .map(|c| c.0.as_str())
                .collect::<Vec<_>>(),
            ["probe_aux", "probe_run"]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_non_allowlisted_tool_fails_in_both_languages() {
    for entry in [
        bash("probe_aux --json '{}'"),
        python("call_tool('probe_aux', {})"),
    ] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        assert_eq!(result.output["is_error"], true, "{}", result.output);
        assert!(result.output.to_string().contains("test"));
        assert!(fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_operator_call_uses_runtime_wiring() {
    let fixture = Fixture::new(vec![bash("deny 'operator denied'")]).await;
    let result = harnx_engine::tool::eval_operator_tool_call(
        fixture.ctx,
        call(json!({})),
        &harnx_core::abort::create_abort_signal(),
    )
    .await
    .unwrap();
    assert_eq!(result.output["error"], "operator denied");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_bash_tool_inner_call_cannot_bypass_chain() {
    let fixture = Fixture::new(vec![bash("deny 'inner denied'")]).await;
    let results = harnx_engine::tool::eval_tool_calls(
        &fixture.ctx,
        vec![harnx_core::tool::ToolCall::new(
            "bash".into(),
            json!({"script":"probe_run --json '{}'"}),
            Some("bash-root".into()),
            None,
        )],
        &harnx_core::abort::create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(
        results[0].output.to_string().contains("inner denied"),
        "{}",
        results[0].output
    );
    assert!(fixture.calls().is_empty());
}
