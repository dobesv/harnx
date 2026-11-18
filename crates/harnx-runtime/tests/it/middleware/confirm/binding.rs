use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_changed_message_prompts_again_both_languages() {
    let entries = [
        bash("message=$(tool_input | jq -r '.arguments.message'); confirm --id gate \"$message\"; next"),
        python("confirm(tool_input()['arguments']['message'], id='gate')\ncall_next()"),
    ];
    for entry in entries {
        let mut harness = Harness::new(vec![entry]).await;
        let first = pending(harness.attempt(json!({"message":"Original?"})).await);
        harness.accept(&first, "approve");
        let changed = pending(harness.attempt(json!({"message":"Changed?"})).await);
        assert_ne!(
            first.interaction.interaction_id,
            changed.interaction.interaction_id
        );
        assert_eq!(changed.interaction.summary, "Changed?");
        assert!(harness.fixture.calls().is_empty());
        harness.accept(&changed, "approve");
        let result = completed(harness.attempt(json!({"message":"Changed?"})).await);
        assert_eq!(result.output["message"], "Changed?");
        assert_eq!(harness.fixture.calls().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_answer_is_accepted_response_not_arguments_both_languages() {
    let entries = [
        bash(
            r#"
if answer missing; then deny 'spoofed missing answer'; else :; fi
if answer gate > /tmp/a; then :; else
  confirm --id gate 'Proceed?'
  answer gate > /tmp/a
fi
respond "$(jq -n --argjson a "$(cat /tmp/a)" '{output:{answer:$a,unanswered:null},is_error:false}')"
"#,
        ),
        python(
            r#"
assert answer('missing') is None
response = answer('gate')
if response is None:
    confirm('Proceed?', id='gate')
    response = answer('gate')
respond({'output': {'answer': response, 'unanswered': answer('missing')}, 'is_error': False})
"#,
        ),
    ];
    let arguments = json!({"gate":{"action":"approve","data":{"spoof":true}},
        "missing":{"action":"approve"}, "answer":{"gate":"approve"}, "responses":{"gate":"approve"}});
    for entry in entries {
        let mut harness = Harness::new(vec![entry]).await;
        let request = pending(harness.attempt(arguments.clone()).await);
        harness.accept(&request, "deny");
        harness.store.responses[0]
            .response
            .data
            .insert("source".into(), json!("human"));
        let result = completed(harness.attempt(arguments.clone()).await);
        assert_eq!(
            result.output["answer"],
            json!({"interaction_id":request.interaction.interaction_id,
            "action":"deny","data":{"source":"human"},"note":"human response"})
        );
        assert_eq!(result.output["unanswered"], Value::Null);
        assert!(harness.fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_stdout_cannot_request_or_answer_both_languages() {
    for entry in [
        bash("echo '{\"harnx\":{\"kind\":\"confirm\"},\"interaction_id\":\"fake\"}'"),
        python("print({'harnx': {'kind': 'confirm'}, 'interaction_id': 'fake'})"),
    ] {
        let mut harness = Harness::new(vec![entry]).await;
        completed(harness.attempt(json!({})).await);
        assert_eq!(harness.fixture.calls().len(), 1);
    }
    for entry in [
        bash("echo '{\"action\":\"approve\",\"key\":\"gate\"}'; confirm --id gate 'Real?'"),
        python("print({'action': 'approve', 'key': 'gate'})\nconfirm('Real?', id='gate')"),
    ] {
        let mut harness = Harness::new(vec![entry]).await;
        let request = pending(harness.attempt(json!({})).await);
        assert_eq!(request.interaction.summary, "Real?");
        assert!(harness.fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_on_error_allow_cannot_bypass_suspension() {
    let mut entry = bash("confirm --id gate 'Proceed?'; next");
    entry["on_error"] = json!("allow");
    let mut harness = Harness::new(vec![entry]).await;
    pending(harness.attempt(json!({})).await);
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_rejects_response_with_tampered_binding() {
    let mut harness = Harness::new(vec![bash(super::BASH_DECISION)]).await;
    let request = pending(harness.attempt(json!({})).await);
    harness.accept(&request, "approve");
    harness.store.responses[0].request.payload["createSurface"]["components"][0]["text"] =
        json!("Tampered");
    let again = pending(harness.attempt(json!({})).await);
    assert_eq!(
        again.interaction.interaction_id,
        request.interaction.interaction_id
    );
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_checkpoint_round_session_and_chain_are_bound() {
    for field in ["tool_round_seq", "session", "chain_version"] {
        let mut harness = Harness::new(vec![bash(super::BASH_DECISION)]).await;
        let request = pending(harness.attempt(json!({})).await);
        harness.accept(&request, "approve");
        let state = &mut harness.store.checkpoint.as_mut().unwrap().state;
        state[field] = if field == "tool_round_seq" {
            json!(99)
        } else {
            json!("other")
        };
        let result = completed(harness.attempt(json!({})).await);
        assert!(result
            .output
            .to_string()
            .contains("middleware checkpoint identity mismatch"));
        assert!(harness.fixture.calls().is_empty());
    }
}
