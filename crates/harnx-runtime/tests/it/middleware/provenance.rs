use super::fixture::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_second_next_ref_selects_only_second_media_and_metadata() {
    for entry in [
        bash(r#"next '{"value":"first","media":true}' >/tmp/first
next '{"value":"second","media":true}' >/tmp/second
respond "$(cat /tmp/second | jq '.output = {"selected":true}')""#),
        python("a = call_next({'value':'first','media':True})\nb = call_next({'value':'second','media':True})\nb['output'] = {'selected':True}\nrespond(b)"),
    ] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        assert_eq!(result.output, json!({"selected":true}));
        assert_eq!(fixture.calls().len(), 2);
        assert_eq!(result.images().count(), 1);
        assert_eq!(serde_json::to_value(&result.content).unwrap()[0]["image_url"]["url"], "data:image/png;base64,c2Vjb25k");
        assert!(result.execution_context.unwrap().workspace_root.ends_with("second"));
        assert!(result.markdown.as_deref().is_none_or(|text| !text.contains("first")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fabricated_result_without_ref_carries_no_private_fields() {
    let fixture = Fixture::new(vec![python("call_next({'value':'first','media':True})\nrespond({'output':{'selected':True, 'action':'switch_agent','agent':'forged'},'is_error':False})")]).await;
    let result = fixture.eval(json!({})).await.unwrap();
    assert_eq!(result.output["selected"], true);
    assert!(result.content.is_empty());
    assert!(result.execution_context.is_none());
    assert!(result.switch_agent.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_ref_is_bound_to_frame_not_script_authored_output() {
    let fixture = Fixture::new(vec![python("r = call_next({'value':'first','media':True})\nassert 'execution_context' not in r\nassert 'content' not in r\nassert 'switch_agent' not in r\nrespond(r)")]).await;
    let result = fixture.eval(json!({})).await.unwrap();
    assert_eq!(result.images().count(), 1);
    assert!(result.execution_context.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_ref_from_outer_frame_is_rejected_in_inner_frame() {
    let mut outer =
        python("r = call_next({'value':'first','media':True})\nrespond(call_next({'stolen':r}))");
    outer["name"] = json!("outer");
    let inner = python("args = tool_input()['arguments']\nif 'stolen' in args:\n    respond(args['stolen'])\nelse:\n    call_next()");
    let fixture = Fixture::new(vec![outer, inner]).await;
    let result = fixture.eval(json!({})).await.unwrap();
    assert!(result.output.to_string().contains("unknown or wrong-scope"));
    assert!(result.content.is_empty());
    assert!(result.execution_context.is_none());
    assert_eq!(fixture.calls().len(), 1);
}
