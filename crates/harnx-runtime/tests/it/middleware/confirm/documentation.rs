use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_documentation_examples_execute_verbatim() {
    let examples = super::super::documentation::blocks();
    // Blocks 10 and 11 are the interact section's bash/python examples,
    // executed by middleware::interact.
    assert_eq!(examples.len(), 14);
    for (index, (language, source)) in examples.into_iter().take(10).enumerate().skip(6) {
        let entry = match language.as_str() {
            "bash" => bash(&source),
            "python" => python(&source),
            _ => panic!("uncovered confirmation example: {language}"),
        };
        for action in ["approve", "deny"] {
            let mut harness = Harness::new(vec![entry.clone()]).await;
            let args = json!({"value":"second","media":true});
            let request = pending(harness.attempt(args.clone()).await);
            let post_next = index >= 8;
            assert_eq!(harness.fixture.calls().len(), usize::from(post_next));
            harness.accept(&request, action);
            let result = completed(harness.attempt(args.clone()).await);
            if action == "approve" {
                assert_eq!(result.output["content"][0]["text"], "\"second\"");
                assert_eq!(result.images().count(), 1);
                assert!(result.execution_context.is_some());
                assert_eq!(harness.fixture.calls().len(), 1);
            } else {
                assert_eq!(
                    result.output["error"],
                    if post_next {
                        "Result discarded. The command already ran."
                    } else {
                        "not approved"
                    }
                );
                assert!(result.content.is_empty());
                assert_eq!(harness.fixture.calls().len(), usize::from(post_next));
            }
        }
    }
}
