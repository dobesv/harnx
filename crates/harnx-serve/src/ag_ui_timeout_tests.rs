use super::*;
use serde_json::json;

#[test]
fn ag_ui_timeout_secs_parsing() {
    let omitted: RelaxedRunAgentInput = serde_json::from_value(json!({"messages": []})).unwrap();
    assert_eq!(omitted.timeout_secs, None);
    for (value, expected) in [
        (json!(null), None),
        (json!(0), None),
        (json!(-1), None),
        (json!(23), Some(23)),
    ] {
        let input: RelaxedRunAgentInput =
            serde_json::from_value(json!({"messages": [], "timeout_secs": value})).unwrap();
        assert_eq!(input.timeout_secs, expected, "input: {value}");
    }
    for value in [
        json!(1.5),
        json!("23"),
        json!(true),
        json!([]),
        json!({}),
        json!(u64::MAX),
    ] {
        assert!(
            serde_json::from_value::<RelaxedRunAgentInput>(
                json!({"messages": [], "timeout_secs": value})
            )
            .is_err(),
            "invalid timeout accepted: {value}"
        );
    }
}
