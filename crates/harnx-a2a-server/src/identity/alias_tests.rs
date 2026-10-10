use super::*;

fn aliases() -> Arc<UserAliases> {
    Arc::new(UserAliases::from_yaml("- name: Display\n  identities: [alice, bob, bob]\n- name: Later\n  identities: [bob, carol]\n- name: Empty\n  identities: ['', empty-alias]\n").unwrap())
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
