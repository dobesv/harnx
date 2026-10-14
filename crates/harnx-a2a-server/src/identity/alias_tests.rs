use super::*;

fn aliases() -> Arc<UserAliases> {
    Arc::new(UserAliases::from_yaml("- name: Display\n  identities: [alice, bob, bob]\n- name: Later\n  identities: [bob, carol]\n- name: Empty\n  identities: ['', empty-alias]\n").unwrap())
}

fn aliases_with_memberships() -> Arc<UserAliases> {
    Arc::new(UserAliases::from_yaml(
        "- name: Alice\n  identities: [alice, bob]\n  groups: [engineering, ops]\n  roles: [admin]\n- name: Carol\n  identities: [carol]\n  groups: [support]\n  roles: [viewer]\n",
    ).unwrap())
}

#[test]
fn aliases_request_and_service_params_expand_only_users() {
    harnx_core::require_nextest();
    let policy = Identity::with_memberships(
        &["x-user".into()],
        &["x-groups".into()],
        &["x-roles".into()],
    )
    .unwrap()
    .with_user_aliases(Some(aliases()));
    for (user, expected) in [
        ("alice", vec!["alice", "bob", "bob"]),
        ("bob", vec!["alice", "bob", "bob"]),
        ("carol", vec!["bob", "carol"]),
        ("unknown", vec!["unknown"]),
        ("Display", vec!["Display"]),
        ("Alice", vec!["Alice"]),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("x-user", user.parse().unwrap());
        headers.insert("x-groups", "alice, team".parse().unwrap());
        headers.insert("x-roles", "bob, operator".parse().unwrap());
        let params = ServiceParams::from([
            ("x-user".into(), vec![user.into()]),
            ("x-groups".into(), vec!["alice, team".into()]),
            ("x-roles".into(), vec!["bob, operator".into()]),
        ]);
        for request in [
            policy.resolve_request(&headers).unwrap(),
            policy.resolve_request_params(&params).unwrap(),
        ] {
            assert_eq!(request.principal.user_id(), Some(user));
            let caller = request.caller();
            let view = caller.view();
            assert_eq!(view.users, expected, "{user}");
            assert_eq!(view.groups, &["alice", "team"]);
            assert_eq!(view.roles, &["bob", "operator"]);
        }
    }
}

#[test]
fn aliases_anonymous_does_not_expand_empty_identity() {
    harnx_core::require_nextest();
    let policy = Identity::default().with_user_aliases(Some(aliases()));
    let request = policy.resolve_request(&HeaderMap::new()).unwrap();
    assert_eq!(request.principal, Principal::Anonymous);
    assert!(request.caller().view().users.is_empty());
    let empty = RequestIdentity::from(Principal::User(String::new()));
    assert_eq!(empty.caller().view().users, &[""]);
}

#[test]
fn aliases_configured_groups_and_roles_merge_with_headers() {
    harnx_core::require_nextest();
    let policy = Identity::with_memberships(
        &["x-user".into()],
        &["x-groups".into()],
        &["x-roles".into()],
    )
    .unwrap()
    .with_user_aliases(Some(aliases_with_memberships()));

    // Alice matches entry with groups [engineering, ops] and roles [admin]
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse().unwrap());
    headers.insert("x-groups", "team, ops".parse().unwrap()); // ops overlap
    headers.insert("x-roles", "viewer".parse().unwrap());

    let request = policy.resolve_request(&headers).unwrap();
    let caller = request.caller();
    let view = caller.view();
    // Header: [team, ops], Configured: [engineering, ops] -> [team, ops, engineering]
    assert_eq!(view.groups, &["team", "ops", "engineering"]);
    // Header: [viewer], Configured: [admin] -> [viewer, admin]
    assert_eq!(view.roles, &["viewer", "admin"]);
}

#[test]
fn aliases_configured_groups_and_roles_without_headers() {
    harnx_core::require_nextest();
    assert_configured_memberships_without_headers(
        "- name: Alice\n  identities: [alice]\n  groups: [engineering]\n  roles: [admin]\n",
        "alice",
        &["engineering"],
        &["admin"],
    );
}

#[test]
fn aliases_unmatched_caller_gets_no_configured_memberships() {
    harnx_core::require_nextest();
    let policy = Identity::with_memberships(
        &["x-user".into()],
        &["x-groups".into()],
        &["x-roles".into()],
    )
    .unwrap()
    .with_user_aliases(Some(aliases_with_memberships()));

    let mut headers = HeaderMap::new();
    headers.insert("x-user", "unknown".parse().unwrap());
    headers.insert("x-groups", "team".parse().unwrap());
    headers.insert("x-roles", "viewer".parse().unwrap());

    let request = policy.resolve_request(&headers).unwrap();
    let caller = request.caller();
    let view = caller.view();
    // Unmatched caller: header values only, no configured memberships
    assert_eq!(view.groups, &["team"]);
    assert_eq!(view.roles, &["viewer"]);
}

#[test]
fn aliases_anonymous_gets_no_configured_memberships() {
    harnx_core::require_nextest();
    let policy = Identity::default().with_user_aliases(Some(aliases_with_memberships()));
    let request = policy.resolve_request(&HeaderMap::new()).unwrap();
    assert_eq!(request.principal, Principal::Anonymous);
    let caller = request.caller();
    let view = caller.view();
    assert!(view.users.is_empty());
    assert!(view.groups.is_empty());
    assert!(view.roles.is_empty());
}

#[test]
fn aliases_configured_memberships_do_not_mask_malformed_headers() {
    // Malformed membership headers must fail closed even when caller has configured memberships
    harnx_core::require_nextest();
    let policy = Identity::with_memberships(
        &["x-user".into()],
        &["x-groups".into()],
        &["x-roles".into()],
    )
    .unwrap()
    .with_user_aliases(Some(Arc::new(
        UserAliases::from_yaml(
            "- name: Alice\n  identities: [alice]\n  groups: [eng]\n  roles: [admin]\n",
        )
        .unwrap(),
    )));

    // Test resolve_request with malformed group header
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse().unwrap());
    headers.append("x-groups", "eng".parse().unwrap());
    headers.append("x-roles", "admin".parse().unwrap());
    // Add invalid UTF-8 byte to group header
    headers.append(
        "x-groups",
        axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    assert!(policy.resolve_request(&headers).is_err());

    // Test resolve_request with malformed role header
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse().unwrap());
    headers.append("x-groups", "eng".parse().unwrap());
    headers.append("x-roles", "admin".parse().unwrap());
    headers.append(
        "x-roles",
        axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    assert!(policy.resolve_request(&headers).is_err());

    // Test resolve_request_params with malformed group header in ServiceParams
    let params = a2a_server_lf::middleware::ServiceParams::from([
        ("x-user".into(), vec!["alice".into()]),
        ("x-groups".into(), vec!["eng".into(), "\u{ffff}".into()]),
        ("x-roles".into(), vec!["admin".into()]),
    ]);
    assert!(policy.resolve_request_params(&params).is_err());

    // Test resolve_request_params with malformed role header in ServiceParams
    let params = a2a_server_lf::middleware::ServiceParams::from([
        ("x-user".into(), vec!["alice".into()]),
        ("x-groups".into(), vec!["eng".into()]),
        ("x-roles".into(), vec!["admin".into(), "\u{ffff}".into()]),
    ]);
    assert!(policy.resolve_request_params(&params).is_err());
}

#[test]
fn aliases_configured_memberships_normalized_like_headers() {
    harnx_core::require_nextest();
    assert_configured_memberships_without_headers(
        "- name: Test\n  identities: [test]\n  groups: [' eng , ops ', '']\n  roles: [' admin , admin ', 'viewer']\n",
        "test",
        &["eng", "ops"],
        &["admin", "viewer"],
    );
}

fn assert_configured_memberships_without_headers(
    yaml: &str,
    user: &str,
    expected_groups: &[&str],
    expected_roles: &[&str],
) {
    let policy = Identity::with_memberships(&["x-user".into()], &[], &[])
        .unwrap()
        .with_user_aliases(Some(Arc::new(UserAliases::from_yaml(yaml).unwrap())));
    let mut headers = HeaderMap::new();
    headers.insert("x-user", user.parse().unwrap());
    let params = ServiceParams::from([("x-user".into(), vec![user.into()])]);
    for request in [
        policy.resolve_request(&headers).unwrap(),
        policy.resolve_request_params(&params).unwrap(),
    ] {
        assert_eq!(request.principal.user_id(), Some(user));
        let caller = request.caller();
        let view = caller.view();
        assert_eq!(view.users, &[user]);
        assert_eq!(view.groups, expected_groups);
        assert_eq!(view.roles, expected_roles);
    }
}
