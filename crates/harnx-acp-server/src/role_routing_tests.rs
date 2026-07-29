use super::{parse_remote_agent, should_run_local_turn, AcpExecutionRole};

#[test]
fn backend_role_with_broker_credentials_routes_local_refs_in_process() {
    harnx_core::require_nextest();

    // Nextest gives this process exclusive ownership of environment mutation.
    unsafe {
        std::env::set_var(
            harnx_acp::ACP_EXECUTION_ROLE_ENV,
            harnx_acp::ACP_BACKEND_ROLE,
        );
        std::env::set_var("HARNX_NATS_URL", "nats://127.0.0.1:4222");
        std::env::set_var("HARNX_NATS_TOKEN", "test-token");
    }

    assert!(std::env::var_os("HARNX_NATS_URL").is_some());
    assert!(std::env::var_os("HARNX_NATS_TOKEN").is_some());
    let role = AcpExecutionRole::from_env();
    assert_eq!(role, AcpExecutionRole::Backend);

    // Disable the test fallback so the production backend guard drives routing.
    assert!(should_run_local_turn(None, role, false));
    let remote_agent = parse_remote_agent("researcher@cluster-a").expect("remote agent ref");
    assert!(!should_run_local_turn(Some(&remote_agent), role, false));
}
