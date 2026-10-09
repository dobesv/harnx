use super::*;
use crate::{
    cli::Args,
    test_support::{EnvGuard, TestConfigSandbox},
};
use clap::Parser;

fn policy() -> Identity {
    harnx_core::require_nextest();
    Identity::with_memberships(
        &["X-User".into()],
        &["X-Groups".into(), "X-Other-Groups".into()],
        &["X-Roles".into()],
    )
    .unwrap()
}

#[test]
fn memberships_header_and_service_params_agree_without_flattening_users() {
    let policy = policy();
    let params = ServiceParams::from([
        (
            "x-user".into(),
            vec!["alice, ignored-user".into(), "ignored-too".into()],
        ),
        (
            "x-groups".into(),
            vec![" , engineering, ops ,".into(), "support,engineering".into()],
        ),
        ("x-other-groups".into(), vec!["extra".into()]),
        ("x-roles".into(), vec!["reviewer, ,".into(), "admin".into()]),
        ("x-unconfigured".into(), vec!["spoofed".into()]),
    ]);
    let mut headers = HeaderMap::new();
    for (name, values) in &params {
        for value in values {
            headers.append(name.parse::<HeaderName>().unwrap(), value.parse().unwrap());
        }
    }
    for request in [
        policy.resolve_request(&headers).unwrap(),
        policy.resolve_request_params(&params).unwrap(),
    ] {
        assert_eq!(request.principal, Principal::User("alice".into()));
        assert_eq!(
            request.groups,
            ["engineering", "ops", "support", "engineering", "extra"]
        );
        assert_eq!(request.roles, ["reviewer", "admin"]);
        let caller = request.caller();
        assert_eq!(caller.view().users, ["alice"]);
        assert_eq!(caller.view().groups.len(), 5);
        assert_eq!(caller.view().roles.len(), 2);
    }
}

#[test]
fn memberships_service_params_reject_malformed_later_occurrences_without_echo() {
    let policy = policy();
    for name in ["x-groups", "x-other-groups", "x-roles"] {
        let params = ServiceParams::from([
            ("x-user".into(), vec!["alice".into()]),
            (name.into(), vec!["valid".into(), "secret\ninvalid".into()]),
        ]);
        let error = policy.resolve_request_params(&params).unwrap_err();
        assert_eq!(error.code, MISSING_IDENTITY_CODE);
        assert!(!error.message.contains("secret"));
    }
}

#[test]
fn memberships_never_replace_user_or_authentication() {
    let policy = policy();
    for authorization in ["Basic YWxpY2U6c2VjcmV0", "Bearer secret"] {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", authorization.parse().unwrap());
        headers.insert("x-groups", "alice, engineering".parse().unwrap());
        headers.insert("x-roles", "admin".parse().unwrap());
        assert_eq!(
            policy.resolve_request(&headers).unwrap_err().code,
            MISSING_IDENTITY_CODE
        );
        let anonymous =
            Identity::with_memberships(&[], &["x-groups".into()], &["x-roles".into()]).unwrap();
        assert!(anonymous.validate_access_rules(true).is_err());
        let request = anonymous.resolve_request(&headers).unwrap();
        assert_eq!(request.principal, Principal::Anonymous);
        assert!(request.caller().view().users.is_empty());
    }
}

#[test]
fn memberships_absent_empty_and_unconfigured_values_are_not_grants() {
    let policy = policy();
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse().unwrap());
    headers.insert("x-spoofed", "admin".parse().unwrap());
    for value in ["", " , , \t"] {
        headers.insert("x-groups", value.parse().unwrap());
        headers.insert("x-roles", value.parse().unwrap());
        let request = policy.resolve_request(&headers).unwrap();
        assert!(request.groups.is_empty());
        assert!(request.roles.is_empty());
    }
    let legacy = Identity::new(&["x-user".into()]).unwrap();
    headers.insert(
        "x-groups",
        axum::http::HeaderValue::from_bytes(b"secret\xff").unwrap(),
    );
    assert!(legacy.resolve_request(&headers).unwrap().groups.is_empty());
}

#[tokio::test]
async fn memberships_invalid_config_fails_before_exports_with_or_without_rules() {
    let sandbox = TestConfigSandbox::new();
    let _env = EnvGuard::set("HARNX_ACCESS_RULES", None);
    let rules = sandbox.config_dir().join("rules.yaml");
    std::fs::write(&rules, "rules: []\n").unwrap();
    for enabled in [false, true] {
        for flag in ["--group-header", "--role-header"] {
            for name in [
                "invalid name",
                "header:x-members",
                "cookie:members",
                "x-good,x-other",
            ] {
                let mut args = Args::try_parse_from([
                    "harnx-a2a-server",
                    "--agent",
                    "missing-agent",
                    "--user-id-header",
                    "x-user",
                    flag,
                    name,
                ])
                .unwrap();
                args.config_dir = Some(sandbox.config_dir().to_owned());
                args.access_rules = enabled.then(|| rules.clone());
                let error = crate::run(args).await.unwrap_err().to_string();
                assert!(error.contains(flag.trim_start_matches("--")), "{error}");
                assert!(!error.contains("resolve agent exports"), "{error}");
            }
        }
    }
}
