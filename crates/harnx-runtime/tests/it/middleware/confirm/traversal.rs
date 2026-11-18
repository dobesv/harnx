use super::*;

async fn skipped_next(explicit: bool, language: &str) {
    let (first, second) = if explicit {
        ("first", "second")
    } else {
        ("1", "2")
    };
    let mut outer = if language == "bash" {
        let first_id = if explicit { "--id first" } else { "" };
        let second_id = if explicit { "--id second" } else { "" };
        bash(&format!(
            r#"
if saved skip > /tmp/skip; then :; else
  next {first_id} '{{"stage":"first"}}' > /tmp/first
fi
save skip true
next {second_id} '{{"stage":"second"}}'
"#
        ))
    } else {
        let first_id = if explicit { ", id='first'" } else { "" };
        let second_id = if explicit { ", id='second'" } else { "" };
        python(&format!("if saved('skip') is None:\n    call_next({{'stage':'first'}}{first_id})\nsave('skip', True)\ncall_next({{'stage':'second'}}{second_id})"))
    };
    outer["name"] = json!("outer");
    let mut inner =
        bash("if confirm --id gate 'Same question?'; then next; else deny 'not approved'; fi");
    inner["name"] = json!("inner");
    let mut harness = Harness::new(vec![outer, inner]).await;
    let one = pending(harness.attempt(json!({})).await);
    assert_eq!(one.interaction.step_path[0].key, first);
    harness.accept(&one, "deny");
    let two = pending(harness.attempt(json!({})).await);
    assert_eq!(two.interaction.step_path[0].key, second);
    assert_ne!(
        one.interaction.interaction_id,
        two.interaction.interaction_id
    );
    assert!(harness.fixture.calls().is_empty());
    harness.accept(&two, "approve");
    let result = completed(harness.attempt(json!({})).await);
    if explicit {
        assert_eq!(result.output["stage"], "second");
        assert_eq!(harness.fixture.calls().len(), 1);
    } else {
        // Traversal changed: ordinal 1 now labels the second logical operation,
        // so it finds the first operation's denial. Explicit IDs avoid this.
        assert_eq!(result.output["error"], "not approved");
        assert!(harness.fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_explicit_next_keys_survive_skipped_first_bash() {
    skipped_next(true, "bash").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_ordinal_next_keys_change_meaning_when_first_skipped_bash() {
    skipped_next(false, "bash").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_explicit_next_keys_survive_skipped_first_python() {
    skipped_next(true, "python").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_ordinal_next_keys_change_meaning_when_first_skipped_python() {
    skipped_next(false, "python").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_bare_next_runs_fresh_after_resume() {
    let mut harness = Harness::new(vec![bash("next; confirm --id keep 'Keep?' ")]).await;
    let request = pending(harness.attempt(json!({"value":"first"})).await);
    harness.accept(&request, "approve");
    let result = completed(harness.attempt(json!({"value":"second"})).await);
    assert_eq!(result.output["value"], "second");
    assert_eq!(harness.fixture.calls().len(), 2);
}
