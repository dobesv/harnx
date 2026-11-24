//! Connect-page parsing and escaping checks.

use super::gate::csrf_matches;
use super::pages::{escape_html, parse_expiry, sanitize_message};
use chrono::Utc;
use harnx_credentials::{FlowStatus, PendingFlow, Principal, Secret};

#[test]
fn csrf_compare_is_exact_and_constant_time_shaped() {
    let mut flow = base_flow();
    assert!(csrf_matches(&flow, "csrf-token"));
    assert!(!csrf_matches(&flow, "csrf-token2"));
    assert!(!csrf_matches(&flow, ""));
    flow.csrf_token = Secret::new("other".to_string());
    assert!(!csrf_matches(&flow, "csrf-token"));
}

#[test]
fn expiry_parsing_accepts_dates_and_rfc3339_and_rejects_past() {
    let future = (Utc::now() + chrono::Duration::days(10))
        .format("%Y-%m-%d")
        .to_string();
    assert!(parse_expiry(None).unwrap().is_none());
    assert!(parse_expiry(Some(&future)).unwrap().is_some());
    let rfc3339 = (Utc::now() + chrono::Duration::days(10)).to_rfc3339();
    assert!(parse_expiry(Some(&rfc3339)).unwrap().is_some());
    let past = "2020-01-01";
    assert!(parse_expiry(Some(past)).is_err());
    assert!(parse_expiry(Some("not-a-date")).is_err());
    assert!(parse_expiry(Some("")).unwrap().is_none());
}

#[test]
fn sanitize_strips_control_characters_and_caps_length() {
    assert_eq!(sanitize_message("line\nbreak"), "line break");
    let long = "x".repeat(400);
    assert_eq!(sanitize_message(&long).chars().count(), 300);
}

#[test]
fn html_escaping_covers_injection_characters() {
    assert_eq!(
        escape_html("<script>&\"'"),
        "&lt;script&gt;&amp;&quot;&#39;"
    );
}

fn base_flow() -> PendingFlow {
    use chrono::TimeDelta;
    let now = Utc::now();
    PendingFlow {
        owner: Principal::Ownerless,
        server: "server".to_string(),
        challenge_id: Secret::new("challenge".to_string()),
        created_at: now,
        expires_at: now + TimeDelta::minutes(10),
        status: FlowStatus::Pending,
        outcome: None,
        csrf_token: Secret::new("csrf-token".to_string()),
        oauth_state: Secret::new("state".to_string()),
        pkce_verifier: None,
        redirect_uri: "https://example.test/callback".to_string(),
        scopes: vec![],
        oauth_client: None,
        authorization_state: None,
        oauth_context: None,
    }
}
