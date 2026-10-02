use crate::config::*;

#[test]
fn test_render_status_line() {
    let mut config = Config {
        model: harnx_client::Model::new("test", "test-model"),
        ..Default::default()
    };

    // When agent and session are missing:
    assert_eq!(config.render_status_line(true), "");

    let mut agent = Agent::new(AgentConfig::from_markdown("my-agent", "prompt").unwrap());
    agent.set_model(crate::client::Model::new("test", "agent-model"));
    config.agent = Some(agent);

    // Agent + Model (no session)
    assert_eq!(
        config.render_status_line(true),
        "🤖 my-agent ▸ test:agent-model"
    );
    assert_eq!(
        config.render_status_line(false),
        "my-agent ▸ test:agent-model"
    );

    let session = crate::config::session::new(&config, "my-session", None).unwrap();
    let session_id = session.id().to_string();
    config.session = Some(session);

    // Agent + Model + Session
    assert_eq!(
        config.render_status_line(true),
        format!("🤖 my-agent ▸ test:agent-model ▸ {}", session_id)
    );
    assert_eq!(
        config.render_status_line(false),
        format!("my-agent ▸ test:agent-model ▸ {}", session_id)
    );

    // Agent + Session (No Model ID)
    let mut config3 = Config::default();
    let mut agent3 = Agent::new(AgentConfig::from_markdown("agent3", "prompt").unwrap());
    agent3.set_model(crate::client::Model::new("", ""));
    config3.agent = Some(agent3);
    let session3 = crate::config::session::new(&config3, "session3", None).unwrap();
    let session_id3 = session3.id().to_string();
    config3.session = Some(session3);

    assert_eq!(
        config3.render_status_line(true),
        format!("🤖 agent3 ▸ {}", session_id3)
    );
    assert_eq!(
        config3.render_status_line(false),
        format!("agent3 ▸ {}", session_id3)
    );

    // Session only (create a session without an agent)
    let mut config2 = Config::default();
    let session_no_agent = crate::config::session::new(&config2, "my-session2", None).unwrap();
    let session_id2 = session_no_agent.id().to_string();
    config2.session = Some(session_no_agent);
    assert_eq!(
        config2.render_status_line(true),
        format!("💬 {}", session_id2)
    );
    assert_eq!(config2.render_status_line(false), session_id2);
}
