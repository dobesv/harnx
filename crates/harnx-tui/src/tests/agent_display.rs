//! Assistant discovery and picker selection consumers.

use super::*;
use harnx_runtime::config::NatsRouting;

// --- Display discovery consumers ---------------------------------------------

const DEFAULT_ROUTING_AGENTS: &[&str] = &[
    "agent-default@shared",
    "agent-foreign@other",
    "agent-local",
    "test-package/agent-package",
];
const CLUSTER_ROUTING_AGENTS: &[&str] = &["agent-default", "agent-foreign@other"];

fn agent_display_test_config(root: &std::path::Path, routing: NatsRouting) -> GlobalConfig {
    create_agent_stubs(&root.join("agents"), &["agent-local"]);
    create_agent_stubs(
        &root.join("packages/test-package/agents"),
        &["agent-package"],
    );
    std::fs::write(
        root.join("agents/agent-subagent.md"),
        "---\nrole: subagent\n---\nSubagent",
    )
    .unwrap();
    let servers_dir = root.join("nats_servers");
    std::fs::create_dir_all(&servers_dir).unwrap();
    std::fs::write(
        servers_dir.join("shared.yaml"),
        "url: nats://127.0.0.1:4222\nagents:\n  - name: agent-default\n    role: assistant\n  - name: agent-remote-subagent\n    role: subagent\n",
    )
    .unwrap();
    std::fs::write(
        servers_dir.join("other.yaml"),
        "url: nats://127.0.0.1:4223\nagents:\n  - name: agent-foreign\n    role: assistant\n",
    )
    .unwrap();

    // Keep the mock model and no active session: these consumers only read fixtures,
    // so neither Config::load_from_file nor a broker connection is needed.
    let config = test_config();
    {
        let mut guard = config.write();
        guard.nats_routing = routing;
        guard.nats_servers = Config::load_nats_servers_from_dir(&servers_dir).unwrap();
    }
    config
}

fn assert_agent_display_picker(modal: &Option<crate::types::ModalState>, expected: &[&str]) {
    let Some(crate::types::ModalState::AgentPicker {
        agents,
        selected,
        query,
    }) = modal
    else {
        panic!("expected AgentPicker, got {modal:?}");
    };
    assert_eq!(agents, expected);
    assert_eq!(*selected, 0);
    assert!(query.is_empty());
}

#[tokio::test]
async fn agent_display_startup_default_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), NatsRouting::Default);

    let modal = Tui::resolve_initial_modal(&config).await;
    assert_agent_display_picker(&modal, DEFAULT_ROUTING_AGENTS);
}

#[tokio::test]
async fn agent_display_startup_cluster_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), NatsRouting::Cluster("shared".into()));

    let modal = Tui::resolve_initial_modal(&config).await;
    assert_agent_display_picker(&modal, CLUSTER_ROUTING_AGENTS);
}

#[tokio::test]
async fn agent_display_open_picker_default_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), NatsRouting::Default);
    let mut tui = Tui::init(&config).await.unwrap();

    tui.open_agent_picker().await;
    assert_agent_display_picker(&tui.app.modal, DEFAULT_ROUTING_AGENTS);
}

#[tokio::test]
async fn agent_display_open_picker_cluster_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), NatsRouting::Cluster("shared".into()));
    let mut tui = Tui::init(&config).await.unwrap();

    tui.open_agent_picker().await;
    assert_agent_display_picker(&tui.app.modal, CLUSTER_ROUTING_AGENTS);
}

async fn assert_agent_completions(routing: NatsRouting, expected: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), routing);
    let tui = Tui::init(&config).await.unwrap();

    for line in [".agent agent", ".session "] {
        let mut agents: Vec<String> = tui
            .compute_completions(line, line.len())
            .await
            .into_iter()
            .map(|(agent, _)| agent)
            .collect();
        agents.sort();
        assert_eq!(agents, expected, "completion for {line:?}");
    }
}

#[tokio::test]
async fn agent_display_completion_default_routing() {
    assert_agent_completions(NatsRouting::Default, DEFAULT_ROUTING_AGENTS).await;
}

#[tokio::test]
async fn agent_display_completion_cluster_routing() {
    assert_agent_completions(
        NatsRouting::Cluster("shared".into()),
        CLUSTER_ROUTING_AGENTS,
    )
    .await;
}

async fn assert_escape_reopens_picker(routing: NatsRouting, expected: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), routing);
    let mut tui = Tui::init(&config).await.unwrap();
    // Match the startup SessionPicker origin without fetching sessions over NATS.
    tui.app.modal = Some(crate::types::ModalState::SessionPicker {
        sessions: vec![],
        selected: 0,
        origin_agent: None,
        origin_session: None,
        error: None,
    });

    tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_agent_display_picker(&tui.app.modal, expected);
    assert!(config.read().session.is_none());
    assert!(!tui.app.should_quit);
}

#[tokio::test]
async fn agent_display_escape_reopens_picker_default_routing() {
    assert_escape_reopens_picker(NatsRouting::Default, DEFAULT_ROUTING_AGENTS).await;
}

#[tokio::test]
async fn agent_display_escape_reopens_picker_cluster_routing() {
    assert_escape_reopens_picker(
        NatsRouting::Cluster("shared".into()),
        CLUSTER_ROUTING_AGENTS,
    )
    .await;
}

async fn assert_remote_picker_selection(
    routing: NatsRouting,
    selection: &str,
    expected_target: (&str, &str),
    local_collision: bool,
) {
    let (expected_agent, expected_cluster) = expected_target;
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());
    let config = agent_display_test_config(tmp.path(), routing);
    if local_collision {
        create_agent_stubs(&tmp.path().join("agents"), &[expected_agent]);
    }
    // Session lookup should fail at URL parsing, never dial a broker. Selection
    // itself only validates the catalog and must still advance to SessionPicker.
    for server in &mut config.write().nats_servers {
        server.url = "invalid://picker-test".into();
    }
    let mut tui = Tui::init(&config).await.unwrap();
    tui.open_agent_picker().await;
    let Some(crate::types::ModalState::AgentPicker {
        agents, selected, ..
    }) = &mut tui.app.modal
    else {
        panic!("expected AgentPicker");
    };
    *selected = agents.iter().position(|name| name == selection).unwrap();

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(
        config.read().remote_agent,
        Some((expected_agent.into(), expected_cluster.into()))
    );
    assert!(
        config.read().agent.is_none(),
        "must not activate a local agent"
    );
    let Some(crate::types::ModalState::SessionPicker {
        sessions, error, ..
    }) = &tui.app.modal
    else {
        panic!("expected SessionPicker, got {:?}", tui.app.modal);
    };
    assert!(sessions.is_empty());
    assert!(error.is_some(), "invalid URL must fail session lookup");
}

#[tokio::test]
async fn agent_display_enter_selects_bare_remote_without_local_agent() {
    assert_remote_picker_selection(
        NatsRouting::Cluster("shared".into()),
        "agent-default",
        ("agent-default", "shared"),
        false,
    )
    .await;
}

#[tokio::test]
async fn agent_display_enter_prefers_default_remote_over_local_collision() {
    assert_remote_picker_selection(
        NatsRouting::Cluster("shared".into()),
        "agent-default",
        ("agent-default", "shared"),
        true,
    )
    .await;
}

#[tokio::test]
async fn agent_display_enter_preserves_explicit_remote_in_cluster_mode() {
    assert_remote_picker_selection(
        NatsRouting::Cluster("shared".into()),
        "agent-foreign@other",
        ("agent-foreign", "other"),
        false,
    )
    .await;
}

#[tokio::test]
async fn agent_display_enter_preserves_explicit_remote_in_default_mode() {
    assert_remote_picker_selection(
        NatsRouting::Default,
        "agent-default@shared",
        ("agent-default", "shared"),
        false,
    )
    .await;
}

#[tokio::test]
async fn agent_picker_enter_activates_agent_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(tmp.path());

    // Create a real agent .md file so use_agent_by_name succeeds.
    let agents_dir = tmp.path().join("agents");
    create_agent_stubs(&agents_dir, &["hermes"]);

    let config = picker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.app.modal = Some(crate::types::ModalState::AgentPicker {
        agents: vec!["hermes".into()],
        selected: 0,
        query: String::new(),
    });

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    assert!(config.read().remote_agent.is_none());
    // Agent should be activated immediately on config.
    let agent_name = config.read().agent.as_ref().map(|a| a.name().to_string());
    assert_eq!(
        agent_name.as_deref(),
        Some("hermes"),
        "agent must be set immediately on Enter"
    );
}
