use super::*;
use harnx_core::message::{ImageUrl, MessageContentPart};

fn malformed_output() -> Value {
    json!({"content": [
        {"type": "text", "text": "original diagnostic"},
        {"type": "image", "mimeType": "image/jpeg", "data": "Zm9v"},
        {"type": "resource_link", "uri": "cid:media:invalid", "mimeType": "image/png"}
    ], "isError": false, "_meta": {"original": true}})
}

fn assert_malformed_result(result: &ToolResult) {
    let original = malformed_output();
    let diagnostic = crate::media::extract_image_parts(&original).unwrap_err();
    assert_eq!(
        result.output,
        json!({
            "is_error": true, "error": format!("{diagnostic:#}"), "original_output": original
        })
    );
    assert!(result.content.is_empty());
}

#[tokio::test]
async fn malformed_image_is_recoverable_and_keeps_valid_batch_siblings() {
    for names in [["bad", "good"], ["good", "bad"]] {
        let ctx = test_context(
            vec![
                Arc::new(MockToolProvider::ok(
                    "bad",
                    Duration::ZERO,
                    malformed_output(),
                )),
                Arc::new(MockToolProvider::ok(
                    "good",
                    Duration::ZERO,
                    json!({
                        "content": [{"type": "image", "mimeType": "image/jpeg", "data": "Zm9v"}]
                    }),
                )),
            ],
            |_| continue_hook_outcome(),
        );
        let results = eval_tool_calls(
            &ctx,
            names.into_iter().map(test_call).collect(),
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 2);
        assert_malformed_result(results.iter().find(|r| r.call.name == "bad").unwrap());
        let good = results.iter().find(|r| r.call.name == "good").unwrap();
        assert_eq!(
            good.content,
            vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/jpeg;base64,Zm9v".into()
                }
            }]
        );
    }
}

#[tokio::test]
async fn malformed_image_replay_preserves_diagnostic_without_hooks_or_dispatch() {
    let ctx = test_context(
        vec![Arc::new(MockToolProvider::ok(
            "replay_only",
            Duration::ZERO,
            malformed_output(),
        ))],
        |_| panic!("replay must not repeat hooks"),
    );
    let call = test_call("replay_only");
    let result = replay_tool_call(
        &ctx,
        harnx_core::tool::ToolReplay {
            session_id: "session",
            tool_round: 1,
            call: &call,
            worker_id: None,
            fence_token: None,
            authorization: None,
        },
        &create_abort_signal(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_malformed_result(&result);
}

#[tokio::test]
async fn malformed_image_operator_result_stays_lossless() {
    let original = malformed_output();
    let mut ctx = test_context(
        vec![Arc::new(MockToolProvider::ok(
            "bad",
            Duration::ZERO,
            original.clone(),
        ))],
        |_| continue_hook_outcome(),
    );
    ctx.allowed_tool_names.insert("bad".into());
    let result = eval_operator_tool_call(ctx, test_call("bad"), &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(result.output, original);
    assert!(result.content.is_empty());
}
