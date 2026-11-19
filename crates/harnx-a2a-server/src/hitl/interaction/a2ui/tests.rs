use super::*;
use serde_json::json;

fn cases() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../harnx-core/tests/fixtures/a2ui-build-data-cases.json"
    )))
    .unwrap()
}

fn envelope(payload: &Value, action: &str, values: Value) -> Value {
    let source = button_actions(payload)
        .into_iter()
        .find(|button| button.name == action)
        .map(|button| button.component_id)
        .unwrap_or_else(|| "root".to_owned());
    json!({"version": "1.0", "action": {
        "name": action, "surfaceId": payload["createSurface"]["surfaceId"],
        "sourceComponentId": source, "timestamp": "2026-06-30T12:00:00Z", "context": {},
        "metadata": {"extensions": {"harnx_response": {"data": values}}}
    }})
}

fn part(value: Value) -> Part {
    Part::data(value).with_media_type(MIME)
}

fn form() -> Value {
    let mut form = cases()[0]["surface"].clone();
    form["createSurface"]["surfaceId"] = json!("form");
    form
}

#[test]
fn a2ui_canonical_fixture_parity() {
    for case in cases().as_array().unwrap() {
        let mut payload = case["surface"].clone();
        payload["createSurface"]["surfaceId"] = json!("fixture");
        let message = envelope(
            &payload,
            case["action"].as_str().unwrap(),
            case["values"].clone(),
        );
        let response = parse(&payload, &[part(message)]).unwrap();
        assert_eq!(
            &response.data,
            case["expected"].as_object().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn a2ui_rejects_malformed_envelopes_and_sources() {
    let payload = form();
    let valid = envelope(
        &payload,
        "deploy",
        json!({"choice": ["prod"], "name": "two words"}),
    );
    assert_eq!(
        parse(&payload, &[part(valid.clone())]).unwrap().data["name"],
        "two words"
    );
    for (path, value) in [
        ("/version", json!("0.8")),
        ("/action/surfaceId", json!("other")),
        ("/action/sourceComponentId", json!("name")),
        ("/action/timestamp", json!("today")),
        ("/action/context", json!([])),
        ("/action/name", json!("undeclared")),
        ("/action/metadata/extensions/harnx_response/data", json!([])),
        (
            "/action/metadata/extensions/harnx_response/note",
            json!(false),
        ),
    ] {
        let mut invalid = valid.clone();
        // The optional note doesn't exist in the default envelope.
        if path.ends_with("/note") {
            invalid["action"]["metadata"]["extensions"]["harnx_response"]["note"] = value;
        } else {
            *invalid.pointer_mut(path).unwrap() = value;
        }
        assert!(
            parse(&payload, &[part(invalid)])
                .unwrap_err()
                .is::<InvalidReply>(),
            "{path}"
        );
    }
    for field in [
        "name",
        "surfaceId",
        "sourceComponentId",
        "timestamp",
        "context",
    ] {
        let mut invalid = valid.clone();
        invalid["action"].as_object_mut().unwrap().remove(field);
        assert!(parse(&payload, &[part(invalid)]).is_err(), "{field}");
    }
    let mut mixed_events = valid.clone();
    mixed_events["error"] = json!({"code": "failure", "message": "not an answer"});
    assert!(parse(&payload, &[part(mixed_events)]).is_err());
    assert!(parse(&payload, &[Part::data(valid.clone())]).is_err());
    assert!(parse(&payload, &[part(valid.clone()), Part::text("deny")]).is_err());
    assert!(parse(&payload, &[part(valid.clone()), part(valid)]).is_err());
}

#[test]
fn a2ui_text_defaults_and_deny_match_structured() {
    let payload = form();
    for text in ["deploy", "deploy choice=prod name=rel-1", "deny"] {
        let parsed = parse(&payload, &[Part::text(text)]).unwrap();
        let values = if text.contains("choice=") {
            json!({"choice": ["prod"], "name": "rel-1"})
        } else {
            json!({})
        };
        let structured = parse(
            &payload,
            &[part(envelope(&payload, &parsed.action, values))],
        )
        .unwrap();
        assert_eq!(parsed, structured);
    }
    for text in [
        "yesterday",
        "deploy name has spaces",
        "deploy choice=invalid",
        "deploy choice=prod,staging",
        "deploy unknown=value",
    ] {
        assert!(parse(&payload, &[Part::text(text)]).is_err(), "{text}");
    }
}
