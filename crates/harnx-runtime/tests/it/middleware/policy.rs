use super::fixture::*;
use harnx_core::abort::create_abort_signal;
use harnx_engine::tool::eval_tool_calls;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_recoverable_error_can_be_replaced_with_success() {
    for entry in [
        bash(r#"r=$(next '{"recoverable":true}'); echo "$r" | jq -e '.is_error == true'; respond '{"output":{"recovered":true},"is_error":false}'"#),
        python("r = call_next({'recoverable':True})\nassert r['is_error']\nrespond({'output':{'recovered':True}, 'is_error':False})"),
    ] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        assert_eq!(result.output, json!({"recovered":true}));
        assert_eq!(fixture.calls().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_normalizes_mcp_and_script_error_markers() {
    for (script, expected) in [
        ("r = call_next({'isError':True, 'partial':42})\nassert r['is_error']\nr['is_error'] = False\nrespond(r)", json!({"isError":true,"partial":42})),
        ("respond({'output':{'partial':42}, 'is_error':True})", json!({"is_error":true,"error":{"partial":42}})),
        ("respond({'output':{'is_error':True}, 'is_error':False})", json!({"is_error":true})),
    ] {
        let fixture = Fixture::new(vec![python(script)]).await;
        assert_eq!(fixture.eval(json!({})).await.unwrap().output, expected);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fatal_downstream_bypasses_on_error_allow() {
    let mut entry = python("try:\n    call_next({'fatal':True})\nexcept:\n    respond({'output':'swallowed','is_error':False})");
    entry["on_error"] = json!("allow");
    let fixture = Fixture::new(vec![entry]).await;
    let error = fixture.eval(json!({})).await.err().unwrap();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_on_error_policies_before_and_after_next() {
    for policy in ["allow", "deny"] {
        for (script, expected, count) in [
            ("exit 7", json!({"value":"original"}), 1),
            (
                "next '{\"value\":\"last\"}'; exit 7",
                json!({"value":"last"}),
                1,
            ),
            (
                "probe_aux --json '{}'; exit 7",
                json!({"value":"original"}),
                2,
            ),
        ] {
            let mut entry = bash(script);
            entry["on_error"] = json!(policy);
            entry["tools"] = json!(["probe_aux"]);
            let fixture = Fixture::new(vec![entry]).await;
            let result = fixture.eval(json!({"value":"original"})).await.unwrap();
            if policy == "allow" {
                assert_eq!(result.output, expected);
                assert_eq!(fixture.calls().len(), count);
            } else {
                assert_eq!(result.output["is_error"], true);
                assert!(result.output["error"].as_str().unwrap().contains("test"));
                assert_eq!(fixture.calls().len(), usize::from(script != "exit 7"));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_timeout_during_next_never_redispatches_or_uses_earlier_result() {
    for policy in ["allow", "deny"] {
        for script in [
            "next '{\"hang\":true}'",
            "next; next '{\"hang\":true}'",
            "probe_aux --json '{\"hang\":true}'",
        ] {
            let mut entry = bash(script);
            entry["on_error"] = json!(policy);
            entry["timeout"] = json!("200ms");
            entry["tools"] = json!(["probe_aux"]);
            let fixture = Fixture::new(vec![entry]).await;
            let result = tokio::time::timeout(Duration::from_secs(5), fixture.eval(json!({})))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.output["is_error"], true, "{}", result.output);
            if policy == "allow" {
                assert!(result.output.to_string().contains("outcome is unknown"));
            }
            assert_eq!(
                fixture.calls().len(),
                if script.starts_with("next;") { 2 } else { 1 }
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_concurrent_next_is_contract_error() {
    let fixture = Fixture::new(vec![bash(
        "next '{\"delay_ms\":100}' & next '{\"delay_ms\":100}' & wait",
    )])
    .await;
    let result = fixture.eval(json!({})).await.unwrap();
    assert_eq!(result.output["is_error"], true, "{}", result.output);
    assert!(
        result.output.to_string().contains("only one outstanding"),
        "{}",
        result.output
    );
    assert!(fixture.calls().len() <= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_turn_cancellation_stops_interpreter_and_host_dispatch() {
    let mut entry = bash("next '{\"hang\":true}'; probe_aux --json '{}'");
    entry["on_error"] = json!("allow");
    entry["tools"] = json!(["probe_aux"]);
    let fixture = Fixture::new(vec![entry]).await;
    let abort = create_abort_signal();
    let calls = eval_tool_calls(&fixture.ctx, vec![call(json!({}))], &abort);
    tokio::pin!(calls);
    tokio::select! {
        _ = fixture.probe.started.notified() => {},
        result = &mut calls => panic!("finished before cancellation: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("dispatch never started"),
    }
    abort.set_ctrlc();
    assert!(tokio::time::timeout(Duration::from_secs(5), &mut calls)
        .await
        .unwrap()
        .is_err());
    tokio::time::timeout(Duration::from_secs(5), fixture.probe.dropped.notified())
        .await
        .unwrap();
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_invalid_ref_is_contract_error_obeying_policy() {
    for policy in ["allow", "deny"] {
        let mut entry = python("call_next({'value':'last'})\nrespond({'output':'fake','is_error':False,'ref':'forged'})");
        entry["on_error"] = json!(policy);
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        if policy == "allow" {
            assert_eq!(result.output, json!({"value":"last"}));
        } else {
            assert!(result.output.to_string().contains("unknown or wrong-scope"));
        }
        assert_eq!(fixture.calls().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_ancestor_allow_cannot_redispatch_after_inner_helper_timeout() {
    let mut outer = bash("next; exit 7");
    outer["name"] = json!("outer");
    outer["on_error"] = json!("allow");
    let mut inner = bash("probe_aux --json '{\"hang\":true}'");
    inner["tools"] = json!(["probe_aux"]);
    inner["timeout"] = json!("200ms");
    let fixture = Fixture::new(vec![outer, inner]).await;
    let result = fixture.eval(json!({})).await.unwrap();
    assert!(result.output.to_string().contains("outcome is unknown"));
    assert_eq!(fixture.calls().len(), 1);
    assert_eq!(fixture.calls()[0].0, "probe_aux");
}
