//! `interact` A2UI surfaces: request-time validation, durable suspension,
//! resume with accepted `{action, data}`.

use super::fixture::{bash, call, python, Fixture};
use super::interaction_harness::{completed, pending, Harness};
use harnx_core::abort::create_abort_signal;
use harnx_engine::tool::PendingToolInteraction;
use serde_json::{json, Value};

/// A valid single-message createSurface form: a mutually exclusive
/// ChoicePicker and a TextField, answered through a Button action.
const BASH_FORM: &str = r#"
surface='{"version":"1.0","createSurface":{"dataModel":{"choice":[],"name":""},"components":[{"id":"root","component":"Card","child":"col"},{"id":"col","component":"Column","children":["title","choice","name","submit_label","submit"]},{"id":"title","component":"Text","text":"Pick a choice"},{"id":"choice","component":"ChoicePicker","options":[{"label":"First","value":"a"},{"label":"Second","value":"b"}],"value":{"@path":"/choice"}},{"id":"name","component":"TextField","label":"Name","value":{"@path":"/name"}},{"id":"submit_label","component":"Text","text":"Submit"},{"id":"submit","component":"Button","child":"submit_label","action":{"event":{"name":"submit"}}}]}}'
response=$(interact --id form "$surface")
respond "$(echo "$response" | jq '{output: ., is_error: false}')"
"#;

const PYTHON_FORM: &str = r#"
surface = {
    "version": "1.0",
    "createSurface": {
        "dataModel": {"choice": [], "name": ""},
        "components": [
            {"id": "root", "component": "Card", "child": "col"},
            {"id": "col", "component": "Column",
             "children": ["title", "choice", "name", "submit_label", "submit"]},
            {"id": "title", "component": "Text", "text": "Pick a choice"},
            {"id": "choice", "component": "ChoicePicker",
             "options": [{"label": "First", "value": "a"}, {"label": "Second", "value": "b"}],
             "value": {"@path": "/choice"}},
            {"id": "name", "component": "TextField", "label": "Name",
             "value": {"@path": "/name"}},
            {"id": "submit_label", "component": "Text", "text": "Submit"},
            {"id": "submit", "component": "Button", "child": "submit_label",
             "action": {"event": {"name": "submit"}}}
        ]
    }
}
response = interact(surface, id="form")
looked = answer("form")
respond({"output": {"interact": response, "answer": looked}, "is_error": False})
"#;

fn form_request_checks(pending: &PendingToolInteraction) {
    assert_eq!(pending.interaction.key, "form");
    assert_eq!(pending.interaction.kind, "a2ui");
    assert_eq!(
        pending.interaction.payload["createSurface"]["metadata"]["extensions"]["harnx"]["kind"],
        "a2ui"
    );
    // Runtime-owned transport fields: a fresh surface id, never reused.
    let surface_id = pending.interaction.payload["createSurface"]["surfaceId"]
        .as_str()
        .expect("stamped surface id");
    assert!(!surface_id.is_empty());
    assert_eq!(
        pending.interaction.payload["createSurface"]["catalogId"],
        "https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json"
    );
    assert_eq!(pending.interaction.summary, "Pick a choice");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_bash_form_round_trips() {
    let mut harness = Harness::new(vec![bash(BASH_FORM)]).await;
    let request = pending(harness.attempt(json!({})).await);
    form_request_checks(&request);
    assert!(harness.fixture.calls().is_empty());
    harness.accept_data(&request, "submit", json!({"choice": ["b"], "name": "Ada"}));
    let result = completed(harness.attempt(json!({})).await);
    assert_eq!(result.output["action"], "submit");
    assert_eq!(result.output["data"]["choice"], json!(["b"]));
    assert_eq!(result.output["data"]["name"], "Ada");
    assert_eq!(result.output["note"], "human response");
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_python_form_round_trips() {
    let mut harness = Harness::new(vec![python(PYTHON_FORM)]).await;
    let request = pending(harness.attempt(json!({})).await);
    form_request_checks(&request);
    harness.accept_data(
        &request,
        "submit",
        json!({"choice": ["a"], "name": "Grace"}),
    );
    let result = completed(harness.attempt(json!({})).await);
    let interact = &result.output["interact"];
    assert_eq!(interact["action"], "submit");
    assert_eq!(interact["data"]["choice"], json!(["a"]));
    assert_eq!(interact["data"]["name"], "Grace");
    // `answer` sees the same accepted response, keyed by interaction id.
    let looked = &result.output["answer"];
    assert_eq!(looked["action"], "submit");
    assert_eq!(looked["data"]["name"], "Grace");
    assert_eq!(looked["interaction_id"], request.interaction.interaction_id);
    assert!(harness.fixture.calls().is_empty());
}

/// A script whose surface depends on tool input: the surface text is the
/// logical identity input, so a changed surface is a new prompt and a stale
/// answer is never applied.
const BASH_PARAMETRIC_FORM: &str = r#"
title=$(tool_input | jq -r '.arguments.title')
surface=$(jq -cn --arg t "$title" '{version:"1.0",createSurface:{dataModel:{choice:[],name:""},components:[{id:"root",component:"Card",child:"col"},{id:"col",component:"Column",children:["title","choice","name","submit_label","submit"]},{id:"title",component:"Text",text:$t},{id:"choice",component:"ChoicePicker",options:[{label:"First",value:"a"},{label:"Second",value:"b"}],value:{"@path":"/choice"}},{id:"name",component:"TextField",label:"Name",value:{"@path":"/name"}},{id:"submit_label",component:"Text",text:"Submit"},{id:"submit",component:"Button",child:"submit_label",action:{event:{name:"submit"}}}]}}')
response=$(interact --id form "$surface")
respond "$(echo "$response" | jq --arg t "$title" '{output: {action: .action, title: $t}, is_error: false}')"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_changed_surface_prompts_again() {
    let mut harness = Harness::new(vec![bash(BASH_PARAMETRIC_FORM)]).await;
    let first = pending(harness.attempt(json!({"title": "First prompt"})).await);
    assert_eq!(first.interaction.summary, "First prompt");
    harness.accept_data(&first, "submit", json!({}));
    let result = completed(harness.attempt(json!({"title": "First prompt"})).await);
    assert_eq!(result.output["action"], "submit");
    assert_eq!(result.output["title"], "First prompt");

    // The stored answer is bound to the first surface: changing the text
    // produces a new interaction id and suspends again instead of consuming it.
    let second = pending(harness.attempt(json!({"title": "Second prompt"})).await);
    assert_ne!(
        first.interaction.interaction_id,
        second.interaction.interaction_id
    );
    assert_eq!(second.interaction.summary, "Second prompt");
    harness.accept_data(&second, "deny", json!({}));
    let result = completed(harness.attempt(json!({"title": "Second prompt"})).await);
    assert_eq!(result.output["action"], "deny");
    assert!(harness.fixture.calls().is_empty());
}

async fn invalid_surface_fails(entry: Value, expected: &str) {
    let mut harness = Harness::new(vec![entry]).await;
    let result = completed(harness.attempt(json!({})).await);
    assert_eq!(result.output["is_error"], true, "{}", result.output);
    let error = result.output["error"].as_str().unwrap_or_default();
    assert!(error.contains(expected), "error was: {error}");
    assert!(harness.fixture.calls().is_empty());
    assert!(!harness.suspended(), "no suspension expected");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_bash_rejects_invalid_surfaces_at_request_time() {
    let surface = |components: &str| {
        format!(
            r#"surface='{{"version":"1.0","createSurface":{{"components":[{components}]}}}}'
response=$(interact "$surface")
respond "$(echo "$response" | jq '{{output: ., is_error: false}}')"#
        )
    };
    // Unknown component.
    invalid_surface_fails(
        bash(&surface(r#"{"id":"root","component":"Row","children":[]}"#)),
        "Basic Catalog subset",
    )
    .await;
    // Missing required TextField value.
    invalid_surface_fails(
        bash(&surface(
            r#"{"id":"root","component":"TextField","label":"Name"}"#,
        )),
        "missing required field 'value'",
    )
    .await;
    // Wrong version.
    invalid_surface_fails(
        bash(
            r#"surface='{"version":"0.9","createSurface":{"components":[]}}'
response=$(interact "$surface")
respond "$(echo "$response" | jq '{output: ., is_error: false}')"#,
        ),
        "version must be \"1.0\"",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_python_rejects_invalid_surfaces_at_request_time() {
    for (surface, expected) in [
        (
            json!({"version": "1.0", "createSurface": {"components": [
                {"id": "root", "component": "Slider", "min": 0, "max": 1,
                 "value": {"@path": "/v"}}
            ]}}),
            "Basic Catalog subset",
        ),
        (
            json!({"version": "1.0", "createSurface": {"components": [
                {"id": "root", "component": "TextField", "label": "Name"}
            ]}}),
            "missing required field 'value'",
        ),
        (
            json!({"version": "1.0", "updateComponents": {"surfaceId": "x", "components": []}}),
            "single createSurface",
        ),
    ] {
        let source = format!(
            "response = interact({surface}, id='form')\nrespond({{'output': response, 'is_error': False}})"
        );
        invalid_surface_fails(python(&source), expected).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_documentation_examples_execute_verbatim() {
    let examples: Vec<String> = super::documentation::blocks()
        .into_iter()
        .filter(|(language, _)| language == "bash" || language == "python")
        .filter(|(_, source)| {
            source.contains("interact --id deploy")
                || source.contains("interact(surface, id=\"deploy\")")
        })
        .map(|(_, source)| source)
        .collect();
    assert_eq!(
        examples.len(),
        2,
        "expected the guide's bash and python interact examples"
    );
    for source in examples {
        let entry = if source.contains("interact --id deploy") {
            bash(&source)
        } else {
            python(&source)
        };
        let mut harness = Harness::new(vec![entry]).await;
        let request = pending(harness.attempt(json!({})).await);
        assert_eq!(request.interaction.key, "deploy");
        harness.accept_data(
            &request,
            "deploy",
            json!({"choice": ["prod"], "name": "rel-1"}),
        );
        let result = completed(harness.attempt(json!({})).await);
        assert_eq!(result.output["action"], "deploy");
        assert_eq!(result.output["data"]["choice"], json!(["prod"]));
        assert_eq!(result.output["data"]["name"], "rel-1");
        assert!(harness.fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_inner_bash_tool_fails_even_on_error_allow() {
    // A valid inline surface, so the failure is the inner-origin rule and not
    // validation.
    let surface = r#"{"version":"1.0","createSurface":{"components":[{"id":"root","component":"Card","child":"body"},{"id":"body","component":"Column","children":["prompt","submit_label","submit"]},{"id":"prompt","component":"Text","text":"Inside a script?"},{"id":"submit_label","component":"Text","text":"Submit"},{"id":"submit","component":"Button","child":"submit_label","action":{"event":{"name":"submit"}}}]}}"#;
    let entry = json!({"name":"test", "match":"^probe_run$", "on_error":"allow",
        "bash": format!("interact '{surface}'\nnext")});
    let harness = Harness::new(vec![entry]).await;
    let results = harnx_engine::tool::eval_tool_calls(
        &harness.fixture.ctx,
        vec![harnx_core::tool::ToolCall::new(
            "bash".into(),
            json!({"script":"probe_run --json '{}'"}),
            Some("bash-root".into()),
            None,
        )],
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(results[0]
        .output
        .to_string()
        .contains("interaction required, can't be answered inside a script"));
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_interact_operator_call_fails_without_interaction_ui() {
    for entry in [bash(BASH_FORM), python(PYTHON_FORM)] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = harnx_engine::tool::eval_operator_tool_call(
            fixture.ctx.clone(),
            call(json!({})),
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert_eq!(result.output["is_error"], true, "{}", result.output);
        assert!(result
            .output
            .to_string()
            .contains("operator call (no interaction UI)"));
        assert!(fixture.calls().is_empty());
    }
}
