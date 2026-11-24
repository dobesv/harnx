//! Focused coverage for the `url` tool-challenge branch — the `{url, message}` payload and summary built for frontends,
//! the D6 identity and checkpoint stamping, and the validation that must
//! refuse an unsafe link before the challenge becomes a durable interaction.
//! The provider turns the same refusal into a recoverable tool error
//! worker-side (`decode_input_required`), so these rejections never
//! surface as a suspension at any layer.
use super::*;

fn url_challenge(url: Option<&str>, message: Option<&str>) -> ToolInputRequired {
    ToolInputRequired {
        producer: "tool:jira/jira_connect".into(),
        key: "connect-round-1".into(),
        kind: ToolInputRequired::KIND_URL.into(),
        message: message.map(str::to_string),
        surface: None,
        url: url.map(str::to_string),
        summary: String::new(),
        request_state: json!({"challenge_id": "abc-123"}),
    }
}

fn context() -> ToolChallengeContext {
    ToolChallengeContext {
        session: "sess-1".into(),
        tool_round_seq: 7,
        tool_call_id: "call-9".into(),
        chain_version: "chain-1".into(),
        step_path: vec![InteractionStep {
            middleware: "prompt".into(),
            key: "next".into(),
        }],
    }
}

/// The D6 id the stamp helper must have minted from every identity field.
fn expected_interaction_id(request: &InteractionRequest, context: &ToolChallengeContext) -> String {
    interaction_id(&InteractionIdentity {
        session: &context.session,
        tool_round_seq: context.tool_round_seq,
        tool_call_id: &context.tool_call_id,
        chain_version: &context.chain_version,
        producer: &request.producer,
        step_path: &request.step_path,
        key: &request.key,
        kind: &request.kind,
        payload: &request.payload,
    })
}

/// A rejected challenge must fail the whole conversion, never suspend.
fn assert_rejected(input: &ToolInputRequired, context: &ToolChallengeContext, needle: &str) {
    let error = tool_challenge_outcome(input, context)
        .expect_err("an invalid url challenge must never suspend");
    assert!(
        error.to_string().contains(needle),
        "unexpected rejection: {error:#}"
    );
}

/// A valid `https` url challenge suspends with the `{url, message}` payload
/// frontends render, the message as default summary, the full D6 identity,
/// and a checkpoint that carries the tool's rerun state.
#[test]
fn valid_https_url_challenge_builds_payload_summary_identity_and_checkpoint() {
    let input = url_challenge(
        Some("https://connect.example.test/abc"),
        Some("Open the connect page"),
    );
    let context = context();
    let ToolMiddlewareOutcome::InputRequired {
        interaction,
        checkpoint,
    } = tool_challenge_outcome(&input, &context).expect("a valid url challenge suspends")
    else {
        panic!("a valid url challenge must suspend");
    };
    // Frontends read the link from the stored payload; the challenge itself
    // never carries credentials (decision 6).
    assert_eq!(
        interaction.payload,
        json!({"url": "https://connect.example.test/abc", "message": "Open the connect page"})
    );
    assert_eq!(interaction.summary, "Open the connect page");
    assert_eq!(interaction.kind, ToolInputRequired::KIND_URL);
    assert_eq!(interaction.producer, "tool:jira/jira_connect");
    assert_eq!(interaction.key, "connect-round-1");
    assert_eq!(interaction.chain_version, "chain-1");
    assert_eq!(interaction.step_path, context.step_path);
    // The id is stamped from every identity field, payload included.
    assert!(!interaction.interaction_id.is_empty());
    assert_eq!(
        interaction.interaction_id,
        expected_interaction_id(&interaction, &context)
    );
    // The checkpoint keeps the round facts and the tool's opaque state.
    assert_eq!(checkpoint.state["tool_request_state"], input.request_state);
    assert_eq!(checkpoint.state["session"], "sess-1");
    assert_eq!(checkpoint.state["tool_round_seq"], 7);
    assert_eq!(checkpoint.state["tool_call_id"], "call-9");
    assert_eq!(checkpoint.state["chain_version"], "chain-1");
}

/// Loopback `http` links are the other valid shape (local connect pages).
#[test]
fn loopback_http_urls_are_valid_challenges() {
    let context = context();
    for url in [
        "http://127.0.0.1:8080/connect/abc",
        "http://localhost:9999/connect/abc",
        "http://[::1]:8080/connect/abc",
    ] {
        let input = url_challenge(Some(url), Some("Open the connect page"));
        let request = tool_challenge_request(&input, &context).expect("loopback http is valid");
        assert_eq!(request.payload["url"], json!(url), "{url}");
    }
}

/// A non-empty summary overrides the message as the summary line.
#[test]
fn url_challenge_keeps_an_explicit_summary() {
    let mut input = url_challenge(
        Some("https://connect.example.test/abc"),
        Some("Open the connect page"),
    );
    input.summary = "Connect Jira".into();
    let request = tool_challenge_request(&input, &context()).expect("valid challenge");
    assert_eq!(request.summary, "Connect Jira");
}

/// The url branch needs both a link and a message; either missing or blank
/// refuses the challenge outright.
#[test]
fn url_challenge_needs_both_url_and_message() {
    let context = context();
    assert_rejected(
        &url_challenge(None, Some("open it")),
        &context,
        "url tool challenge needs a url",
    );
    assert_rejected(
        &url_challenge(Some(""), Some("open it")),
        &context,
        "url tool challenge needs a url",
    );
    assert_rejected(
        &url_challenge(Some("https://connect.example.test/abc"), None),
        &context,
        "url tool challenge needs a message",
    );
    assert_rejected(
        &url_challenge(Some("https://connect.example.test/abc"), Some("")),
        &context,
        "url tool challenge needs a message",
    );
}

/// Remote `http`, script schemes and non-http(s) schemes are policy
/// violations (decision 6): the challenge is refused, never suspended.
#[test]
fn url_challenge_rejects_remote_http_and_script_schemes() {
    let context = context();
    for url in [
        "http://connect.example.test/abc",
        "javascript:alert(1)",
        "ftp://connect.example.test/abc",
    ] {
        assert_rejected(
            &url_challenge(Some(url), Some("open it")),
            &context,
            "must be https or loopback http",
        );
    }
    // An unparseable link fails the same way: refused, never suspended.
    assert_rejected(
        &url_challenge(Some("not a url"), Some("open it")),
        &context,
        "invalid url",
    );
}

/// A loopback-looking userinfo cannot smuggle a remote host past the
/// policy: `http://localhost@evil.test` parses with a remote domain host.
#[test]
fn url_challenge_rejects_userinfo_tricks() {
    let context = context();
    for url in [
        "http://localhost:8080@connect.example.test/abc",
        "http://127.0.0.1@connect.example.test/abc",
        "http://user:pass@connect.example.test/abc",
    ] {
        assert_rejected(
            &url_challenge(Some(url), Some("open it")),
            &context,
            "must be https or loopback http",
        );
    }
}

/// The identity bounds every challenge kind shares: producer shape,
/// non-empty key, request-state budget, labeled step path and a known kind.
#[test]
fn url_challenge_enforces_shared_identity_and_kind_bounds() {
    let labeled = context();
    let valid = || url_challenge(Some("https://connect.example.test/abc"), Some("open it"));
    let mut input = valid();
    input.producer = "jira/jira_connect".into();
    assert_rejected(&input, &labeled, "producer must be tool:<server>/<tool>");

    let mut input = valid();
    input.producer = "tool:/jira_connect".into();
    assert_rejected(&input, &labeled, "producer must be tool:<server>/<tool>");

    let mut input = valid();
    input.key = String::new();
    assert_rejected(&input, &labeled, "tool challenge key exceeds");

    let mut input = valid();
    input.request_state = json!("x".repeat(harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES));
    assert_rejected(&input, &labeled, "tool challenge request_state exceeds");

    let mut input = valid();
    input.kind = "phishing".into();
    assert_rejected(&input, &labeled, "unsupported tool challenge kind");

    let mut unlabeled = context();
    unlabeled.step_path = vec![InteractionStep {
        middleware: String::new(),
        key: "next".into(),
    }];
    assert_rejected(
        &valid(),
        &unlabeled,
        "tool challenge step path must label middleware next steps",
    );
}
