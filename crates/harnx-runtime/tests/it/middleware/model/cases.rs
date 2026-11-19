//! Contract cases through the real runtime loop, NATS dispatch and a mock model.
use super::{context, fixture, mock_model_for, serve_probe, Env, Probe};
use crate::common;
use anyhow::{Context, Result};
use harnx_core::{
    abort::AbortSignal,
    instance::ServerScope,
    message::MessageContent,
    tool::{ToolCall, ToolResult},
};
use harnx_runtime::agent_loop::run_agent_loop;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

struct Harness {
    _server: common::NatsServerHandle,
    _env: Env,
    scope: ServerScope,
    probe: Arc<Probe>,
    task: tokio::task::JoinHandle<Result<()>>,
}
impl Harness {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let env = Env::set(&[
            ("HARNX_NATS_URL", server.url()),
            ("HARNX_NATS_TOKEN", "test-token"),
        ]);
        let scope = ServerScope::new();
        let probe = Arc::new(Probe::default());
        let task = serve_probe(server.url(), &scope, probe.clone()).await?;
        Ok(Self {
            _server: server,
            _env: env,
            scope,
            probe,
            task,
        })
    }

    async fn eval(&self, entries: Vec<Value>, call: ToolCall) -> Result<ToolResult> {
        self.eval_with_abort(entries, call, harnx_core::abort::create_abort_signal())
            .await
    }

    async fn eval_with_abort(
        &self,
        entries: Vec<Value>,
        call: ToolCall,
        abort: AbortSignal,
    ) -> Result<ToolResult> {
        self.probe.calls.lock().clear();
        let config = fixture::config(entries);
        let (model, seen) = mock_model_for(call);
        let mut ctx = context(config.clone(), self.scope.clone(), model);
        ctx.abort_signal = abort;
        let input = harnx_runtime::config::input::from_str(&config, "run probe", None);
        run_agent_loop(&ctx, input).await?;
        let result = seen
            .lock()
            .iter()
            .find_map(|message| match &message.content {
                MessageContent::ToolCalls(calls) => calls.tool_results.first().cloned(),
                _ => None,
            });
        result.context("mock model didn't receive a tool result")
    }

    fn count(&self) -> usize {
        self.probe.calls.lock().len()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_denial_defaults_and_nesting() -> Result<()> {
    let h = Harness::new().await?;
    for entry in [
        fixture::bash("deny 'blocked'"),
        fixture::python("deny('blocked')"),
    ] {
        assert_eq!(
            h.eval(vec![entry], fixture::call(json!({}))).await?.output["is_error"],
            true
        );
        assert_eq!(h.count(), 0);
    }
    for entry in [fixture::bash(":"), fixture::python("pass")] {
        assert_eq!(
            h.eval(vec![entry], fixture::call(json!({"value":"original"})))
                .await?
                .output,
            json!({"value":"original"})
        );
        assert_eq!(h.count(), 1);
    }
    let mut outer = fixture::python(
        "r = call_next({'value':'outer'})\nr['output']['value'] += '-outer'\nrespond(r)",
    );
    outer["name"] = json!("outer");
    let inner = fixture::python("args = tool_input()['arguments']\nargs['value'] += '-inner'\nr = call_next(args)\nr['output']['value'] += '-inner'\nrespond(r)");
    let result = h.eval(vec![outer, inner], fixture::call(json!({}))).await?;
    assert_eq!(result.output["value"], "outer-inner-inner-outer");
    assert_eq!(h.count(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_error_normalization_and_recovery() -> Result<()> {
    let h = Harness::new().await?;
    for (source, expected) in [
        ("r = call_next({'recoverable':True})\nassert r['is_error']\nrespond({'output':{'recovered':True},'is_error':False})", json!({"recovered":true})),
        ("r = call_next({'isError':True})\nassert r['is_error']\nr['is_error'] = False\nrespond(r)", json!({"isError":true})),
        ("respond({'output':'failure','is_error':True})", json!({"is_error":true,"error":"failure"})),
    ] {
        let result = h.eval(vec![fixture::python(source)], fixture::call(json!({}))).await?;
        assert_eq!(result.output, expected);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_allowlist_and_bash_inner_call() -> Result<()> {
    let h = Harness::new().await?;
    let mut allowed = fixture::python(
        "r = call_tool('probe_aux', {'value':'helper'})\nrespond({'output':r,'is_error':False})",
    );
    allowed["tools"] = json!(["probe_aux"]);
    allowed["match"] = json!("^probe_.*$");
    assert_eq!(
        h.eval(vec![allowed], fixture::call(json!({})))
            .await?
            .output,
        json!({"value":"helper"})
    );
    assert_eq!(h.count(), 1);
    assert_eq!(h.probe.calls.lock()[0].0, "probe_aux");
    let denied = fixture::python("call_tool('probe_aux', {})");
    assert_eq!(
        h.eval(vec![denied], fixture::call(json!({}))).await?.output["is_error"],
        true
    );
    assert_eq!(h.count(), 0);
    let call = ToolCall::new(
        "bash".into(),
        json!({"script":"probe_run --json '{}'"}),
        Some("bash-root".into()),
        None,
    );
    let result = h
        .eval(vec![fixture::bash("deny 'inner blocked'")], call)
        .await?;
    assert!(
        result.output.to_string().contains("inner blocked"),
        "{}",
        result.output
    );
    assert_eq!(h.count(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_failure_policies_and_concurrent_next() -> Result<()> {
    let h = Harness::new().await?;
    for policy in ["allow", "deny"] {
        for script in ["exit 9", "next; exit 9"] {
            let mut entry = fixture::bash(script);
            entry["on_error"] = json!(policy);
            let result = h
                .eval(vec![entry], fixture::call(json!({"value":"original"})))
                .await?;
            if policy == "allow" {
                assert_eq!(result.output, json!({"value":"original"}));
            } else {
                assert_eq!(result.output["is_error"], true);
            }
            assert_eq!(
                h.count(),
                usize::from(policy == "allow" || script.starts_with("next"))
            );
        }
    }
    let entry = fixture::bash("next '{\"delay_ms\":100}' & next '{\"delay_ms\":100}' & wait");
    let result = h.eval(vec![entry], fixture::call(json!({}))).await?;
    assert!(
        result.output.to_string().contains("only one outstanding"),
        "{}",
        result.output
    );
    assert!(h.count() <= 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_timeout_never_redispatches() -> Result<()> {
    let h = Harness::new().await?;
    for policy in ["allow", "deny"] {
        let mut entry = fixture::bash("next; next '{\"hang\":true}'");
        entry["timeout"] = json!("300ms");
        entry["on_error"] = json!(policy);
        let result = h.eval(vec![entry], fixture::call(json!({}))).await?;
        assert_eq!(result.output["is_error"], true);
        if policy == "allow" {
            assert!(result.output.to_string().contains("outcome is unknown"));
        }
        assert_eq!(h.count(), 2);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_fatal_and_turn_cancellation_bypass_allow() -> Result<()> {
    let h = Harness::new().await?;
    let mut entry = fixture::python("try:\n    call_next({'fatal':True})\nexcept:\n    respond({'output':'swallowed','is_error':False})");
    entry["on_error"] = json!("allow");
    assert!(h.eval(vec![entry], fixture::call(json!({}))).await.is_err());
    assert_eq!(h.count(), 1);
    let abort = harnx_core::abort::create_abort_signal();
    let mut entry = fixture::bash("next '{\"hang\":true}'; next");
    entry["on_error"] = json!("allow");
    let run = h.eval_with_abort(vec![entry], fixture::call(json!({})), abort.clone());
    tokio::pin!(run);
    // Drain the fatal call's notification before waiting for the new dispatch.
    h.probe.started.notified().await;
    tokio::select! {
        result = &mut run => panic!("finished before cancellation: {result:?}"),
        _ = h.probe.started.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("dispatch never started"),
    }
    abort.set_ctrlc();
    assert!(tokio::time::timeout(Duration::from_secs(5), run)
        .await?
        .is_err());
    assert_eq!(h.count(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_mock_model_second_ref_or_no_ref_selects_media() -> Result<()> {
    let h = Harness::new().await?;
    for (response, expected_media) in [
        ("r['output'] = {'value':'selected'}\nrespond(r)", true),
        (
            "respond({'output':{'value':'selected'},'is_error':False})",
            false,
        ),
    ] {
        let source = format!("call_next({{'media':True,'value':'first'}})\nr = call_next({{'media':True,'value':'second'}})\n{response}");
        let result = h
            .eval(vec![fixture::python(&source)], fixture::call(json!({})))
            .await?;
        assert_eq!(result.output, json!({"value":"selected"}));
        assert_eq!(!result.content.is_empty(), expected_media);
        assert_eq!(h.count(), 2);
    }
    Ok(())
}
