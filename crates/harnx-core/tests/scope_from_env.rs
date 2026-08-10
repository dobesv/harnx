use harnx_core::instance::{scope_from_env, StandaloneMode, HARNX_SERVER_SCOPE};

#[test]
fn rejects_a_missing_scope() {
    harnx_core::require_nextest();
    std::env::remove_var(HARNX_SERVER_SCOPE);
    let error = scope_from_env(StandaloneMode::McpStdio).expect_err("missing scope must fail");
    assert!(
        error.to_string().contains(HARNX_SERVER_SCOPE),
        "got: {error}"
    );
}

/// `std::env::var` returns `Ok("")` for a variable that's set but empty.
/// `ServerScope::from_string("")` would strip no prefix from any subject or
/// registration key, so the server would start and register under an empty
/// scope no worker configured for a real scope can ever find. That must fail
/// the same way an absent variable does, not silently succeed.
#[test]
fn rejects_an_empty_scope_the_same_as_a_missing_one() {
    harnx_core::require_nextest();
    std::env::set_var(HARNX_SERVER_SCOPE, "");
    let error = scope_from_env(StandaloneMode::WorkerLaunched).expect_err("empty scope must fail");
    assert!(
        error.to_string().contains(HARNX_SERVER_SCOPE),
        "got: {error}"
    );
    std::env::remove_var(HARNX_SERVER_SCOPE);
}

#[test]
fn accepts_a_non_empty_scope() {
    harnx_core::require_nextest();
    std::env::set_var(HARNX_SERVER_SCOPE, "shared");
    let scope = scope_from_env(StandaloneMode::ListTools).expect("non-empty scope must succeed");
    assert_eq!(scope.as_str(), "shared");
    std::env::remove_var(HARNX_SERVER_SCOPE);
}
