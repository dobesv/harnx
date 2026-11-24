//! `mrtr` mapping unit tests. Filter: `mrtr`.
// rmcp 3.8 marks sampling/roots request types deprecated (SEP-2577), but the
// MRTR spec still allows servers to request them; these tests cover the
// harnx-side refusal of those kinds.
#![allow(deprecated)]
use super::schema::empty_requested_schema;
use super::*;
use harnx_toolset::{ToolInputRequired, ToolInputResponse};
use rmcp::model::{
    ElicitRequest, ElicitRequestParams, ElicitationSchema, InputRequest, InputRequests,
    InputRequiredResult, InputResponses,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

fn confirm_challenge() -> ToolInputRequired {
    ToolInputRequired::confirm("ask-key", "Proceed?", json!({"round": 1}))
}

fn a2ui_challenge() -> ToolInputRequired {
    ToolInputRequired::a2ui(
        "form",
        json!({
            "version": "1.0",
            "createSurface": {
                "dataModel": {"choice": [], "name": ""},
                "components": [
                    {"id": "root", "component": "Card", "child": "col"},
                    {"id": "col", "component": "Column", "children": ["title", "choice", "name", "submit"]},
                    {"id": "title", "component": "Text", "text": "Pick"},
                    {"id": "choice", "component": "ChoicePicker",
                     "options": [{"label": "First", "value": "a"}, {"label": "Second", "value": "b"}],
                     "value": {"@path": "/choice"}},
                    {"id": "name", "component": "TextField", "label": "Name", "value": {"@path": "/name"}},
                    {"id": "submit", "component": "Button", "child": "title",
                     "action": {"event": {"name": "submit"}}}
                ]
            }
        }),
        "fill the form",
        json!({"round": 2}),
    )
}

#[test]
// qual:allow(test_quality, no_sut) reason: "Pins the cross-crate contract
// MAX_TOOL_INPUT_ROUNDS == rmcp DEFAULT_MRTR_MAX_ROUNDS == 10 by value; the
// runtime's round-cap behavior is covered by tool_input_required_round_cap_gives_up_after_ten_challenges."
fn mrtr_round_cap_matches_rmcp_default() {
    assert_eq!(
        harnx_toolset::MAX_TOOL_INPUT_ROUNDS,
        rmcp::model::DEFAULT_MRTR_MAX_ROUNDS
    );
    assert_eq!(harnx_toolset::MAX_TOOL_INPUT_ROUNDS, 10);
}

#[test]
fn mrtr_confirm_challenge_round_trips_with_fidelity_meta() {
    let input = confirm_challenge();
    let result = challenge_to_mrtr(&input).expect("map confirm challenge");
    assert_eq!(result.request_state.as_deref(), Some("{\"round\":1}"));
    let (key, request) = result
        .input_requests
        .as_ref()
        .and_then(|requests| requests.iter().next())
        .expect("one input request");
    assert_eq!(key, "ask-key");
    let InputRequest::Elicitation(elicit) = request else {
        panic!("confirm challenge maps to elicitation");
    };
    let ElicitRequestParams::FormElicitationParams {
        message,
        requested_schema,
        ..
    } = &elicit.params
    else {
        panic!("confirm challenge maps to form elicitation");
    };
    assert_eq!(message, "Proceed?");
    assert!(requested_schema.properties.is_empty());

    let decoded = challenge_from_mrtr(&result).expect("decode back");
    // The _meta copy is preferred, so the challenge survives exactly.
    assert_eq!(decoded, input);
}

#[test]
fn mrtr_a2ui_challenge_carries_surface_and_derives_schema() {
    let input = a2ui_challenge();
    let result = challenge_to_mrtr(&input).expect("map a2ui challenge");
    let decoded = challenge_from_mrtr(&result).expect("decode back");
    assert_eq!(decoded, input);

    // Strip the harnx _meta copy to check what a third-party client sees.
    let mut third_party = result.clone();
    third_party.meta = None;
    let generic = challenge_from_mrtr(&third_party).expect("generic decode");
    assert_eq!(generic.kind, ToolInputRequired::KIND_A2UI);
    let surface = generic.surface.expect("derived surface");
    assert_eq!(
        surface["createSurface"]["components"][1]["children"],
        json!([
            "prompt",
            "field-choice",
            "field-name",
            "submit_label",
            "submit"
        ]),
        "derived surface layout: {surface}"
    );
    assert_eq!(surface["createSurface"]["dataModel"]["choice"], json!([]));
    assert_eq!(surface["createSurface"]["dataModel"]["name"], json!(""));
}

#[test]
fn mrtr_decode_rejects_unsupported_request_kinds() {
    let sampling_params: rmcp::model::CreateMessageRequestParams = serde_json::from_value(json!({
        "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}],
        "maxTokens": 10
    }))
    .expect("sampling params");
    let mut requests = InputRequests::new();
    requests.insert(
        "s".to_string(),
        InputRequest::CreateMessage(rmcp::model::CreateMessageRequest::new(sampling_params)),
    );
    let result = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&result).expect_err("sampling is unsupported");
    assert!(error.contains("sampling"), "{error}");

    let mut requests = InputRequests::new();
    requests.insert(
        "r".to_string(),
        InputRequest::ListRoots(rmcp::model::ListRootsRequest {
            method: Default::default(),
            extensions: Default::default(),
        }),
    );
    let result = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&result).expect_err("roots are unsupported");
    assert!(error.contains("roots"), "{error}");

    let url_elicit: InputRequest = serde_json::from_value(json!({
        "method": "elicitation/create",
        "params": {"mode": "url", "message": "sign in", "url": "https://example.com", "elicitationId": "1"}
    }))
    .expect("url elicitation");
    let mut requests = InputRequests::new();
    requests.insert("u".to_string(), url_elicit);
    let result = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&result).expect_err("url elicitation is unsupported");
    assert!(error.contains("URL elicitation"), "{error}");
}

#[test]
fn mrtr_decode_rejects_multiple_and_missing_requests() {
    let form_elicit = |_key: &str, message: &str| {
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: message.to_string(),
                requested_schema: empty_requested_schema(),
            },
        ))
    };
    let mut requests = InputRequests::new();
    requests.insert("a".to_string(), form_elicit("a", "one"));
    requests.insert("b".to_string(), form_elicit("b", "two"));
    let multi = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&multi).expect_err("one interaction at a time");
    assert!(error.contains("2 input requests"), "{error}");

    let none = InputRequiredResult::new(None, Some("state".to_string()));
    let error = challenge_from_mrtr(&none).expect_err("no request to show");
    assert!(error.contains("without inputRequests"), "{error}");
}

#[test]
fn mrtr_empty_schema_elicit_maps_to_confirm() {
    let mut requests = InputRequests::new();
    requests.insert(
        "k".to_string(),
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: "Continue?".to_string(),
                requested_schema: empty_requested_schema(),
            },
        )),
    );
    let result = InputRequiredResult::new(Some(requests), Some("opaque".to_string()));
    let decoded = challenge_from_mrtr(&result).expect("decode");
    assert_eq!(decoded.kind, ToolInputRequired::KIND_CONFIRM);
    assert_eq!(decoded.message.as_deref(), Some("Continue?"));
    assert_eq!(decoded.request_state, json!("opaque"));
}

#[test]
fn mrtr_form_elicit_maps_to_a2ui_subset_and_rejects_other_types() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "env": {"type": "string", "enum": ["staging", "prod"]}
        }
    });
    let requested_schema: ElicitationSchema =
        serde_json::from_value(schema).expect("valid elicitation schema");
    // Both probes share the elicit shape; only the schema differs.
    let form = |requested_schema: ElicitationSchema| {
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: "Deploy".to_string(),
                requested_schema,
            },
        ))
    };
    let mut requests = InputRequests::new();
    requests.insert("form".to_string(), form(requested_schema));
    let result = InputRequiredResult::new(Some(requests), None);
    let decoded = challenge_from_mrtr(&result).expect("form decodes");
    assert_eq!(decoded.kind, ToolInputRequired::KIND_A2UI);
    let components = &decoded.surface.expect("surface")["createSurface"]["components"];
    let names: Vec<&str> = components
        .as_array()
        .expect("components")
        .iter()
        .map(|component| component["component"].as_str().expect("component kind"))
        .collect();
    assert_eq!(
        names,
        vec![
            "Card",
            "Column",
            "Text",
            "ChoicePicker",
            "TextField",
            "Text",
            "Button"
        ]
    );

    let bad_schema = json!({
        "type": "object",
        "properties": {"age": {"type": "integer"}}
    });
    let requested_schema: ElicitationSchema =
        serde_json::from_value(bad_schema).expect("valid schema shape");
    let mut requests = InputRequests::new();
    requests.insert("form".to_string(), form(requested_schema));
    let result = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&result).expect_err("integer is unsupported");
    assert!(error.contains("'age'"), "{error}");
}

#[test]
fn mrtr_request_state_round_trips_and_passes_through_opaque_text() {
    let input = ToolInputRequired::confirm("k", "m", json!({"round": 3, "blob": "x".repeat(2048)}));
    let result = challenge_to_mrtr(&input).expect("map");
    let decoded = challenge_from_mrtr(&result).expect("decode");
    assert_eq!(decoded.request_state, input.request_state);

    // A third-party client echoing arbitrary text must not break the decode.
    let mut mangled = result;
    mangled.request_state = Some("not json".to_string());
    let decoded = challenge_from_mrtr(&mangled).expect("opaque state stays decodable");
    assert_eq!(decoded.request_state, json!("not json"));
}

#[test]
fn mrtr_oversized_state_and_key_fail_clearly() {
    let big = ToolInputRequired::confirm("k", "m", json!({"blob": "x".repeat(70 * 1024)}));
    let error = challenge_to_mrtr(&big).expect_err("oversized state rejected");
    assert!(error.contains("requestState exceeds"), "{error}");

    let mut requests = InputRequests::new();
    requests.insert(
        "x".repeat(300),
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: "m".to_string(),
                requested_schema: empty_requested_schema(),
            },
        )),
    );
    let result = InputRequiredResult::new(Some(requests), None);
    let error = challenge_from_mrtr(&result).expect_err("oversized key rejected");
    assert!(error.contains("key exceeds"), "{error}");
}

#[test]
fn mrtr_responses_encode_accept_decline_and_keep_harnx_meta() {
    // One construction site; each answer passes only what differs.
    let response =
        |action: &str, data: Map<String, Value>, note: Option<String>| ToolInputResponse {
            action: action.to_string(),
            data,
            note,
        };
    let mut responses = BTreeMap::new();
    responses.insert(
        "approve-key".to_string(),
        response("approve", Map::new(), None),
    );
    responses.insert(
        "submit-key".to_string(),
        response(
            "submit",
            [("name".to_string(), json!("Ada"))].into_iter().collect(),
            Some("operator note".to_string()),
        ),
    );
    responses.insert("deny-key".to_string(), response("deny", Map::new(), None));
    let encoded = responses_to_mrtr(&responses);
    assert_eq!(encoded["approve-key"]["action"], json!("accept"));
    assert_eq!(
        encoded["approve-key"]["_meta"]["harnx:action"],
        json!("approve")
    );
    assert_eq!(encoded["submit-key"]["action"], json!("accept"));
    assert_eq!(encoded["submit-key"]["content"], json!({"name": "Ada"}));
    assert_eq!(
        encoded["submit-key"]["_meta"]["harnx:action"],
        json!("submit")
    );
    assert_eq!(
        encoded["submit-key"]["_meta"]["harnx:note"],
        json!("operator note")
    );
    assert_eq!(encoded["deny-key"]["action"], json!("decline"));

    let decoded = responses_from_mrtr(&encoded).expect("responses decode");
    assert_eq!(decoded["approve-key"].action, "approve");
    assert_eq!(decoded["submit-key"].action, "submit");
    assert_eq!(decoded["submit-key"].note.as_deref(), Some("operator note"));
    assert_eq!(decoded["deny-key"].action, "deny");
}

#[test]
fn mrtr_third_party_elicit_result_decodes_without_harnx_meta() {
    let mut encoded = InputResponses::new();
    encoded.insert(
        "k".to_string(),
        json!({"action": "accept", "content": {"name": "Ada"}}),
    );
    encoded.insert("d".to_string(), json!({"action": "decline"}));
    encoded.insert("c".to_string(), json!({"action": "cancel"}));
    let decoded = responses_from_mrtr(&encoded).expect("ElicitResult shapes decode");
    assert_eq!(decoded["k"].action, "approve");
    assert_eq!(decoded["k"].data["name"], json!("Ada"));
    assert_eq!(decoded["d"].action, "deny");
    assert_eq!(decoded["c"].action, "deny");
}

#[test]
fn mrtr_harnx_shaped_responses_decode_verbatim() {
    let mut encoded = InputResponses::new();
    encoded.insert(
        "k".to_string(),
        json!({"action": "approve", "data": {"a": 1}, "note": "n"}),
    );
    let decoded = responses_from_mrtr(&encoded).expect("harnx shape decodes verbatim");
    assert_eq!(decoded["k"].action, "approve");
    assert_eq!(decoded["k"].data["a"], json!(1));
    assert_eq!(decoded["k"].note.as_deref(), Some("n"));
}

#[test]
fn mrtr_malformed_response_fails_naming_the_key() {
    let mut encoded = InputResponses::new();
    encoded.insert("bad".to_string(), json!("just a string"));
    let error = responses_from_mrtr(&encoded).expect_err("malformed response rejected");
    assert!(error.contains("'bad'"), "{error}");

    let mut encoded = InputResponses::new();
    encoded.insert("bad".to_string(), json!({"action": "maybe"}));
    let error = responses_from_mrtr(&encoded).expect_err("unknown action rejected");
    assert!(error.contains("'maybe'"), "{error}");
}

#[test]
fn mrtr_request_state_from_value_passes_strings_through_and_encodes_json() {
    assert_eq!(request_state_from_value(&Value::Null), Ok(None));
    // A string is the opaque text a third-party server may have sent: echo
    // it as-is, never JSON-quoted.
    assert_eq!(
        request_state_from_value(&json!("state")),
        Ok(Some("state".to_string()))
    );
    // A JSON value re-encodes compactly, matching what the server echoed.
    assert_eq!(
        request_state_from_value(&json!({"round": 1})),
        Ok(Some("{\"round\":1}".to_string()))
    );
    // The echoed state is bounded like the challenge state.
    let oversized = json!({"blob": "x".repeat(harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES)});
    assert!(request_state_from_value(&oversized).is_err());
}
