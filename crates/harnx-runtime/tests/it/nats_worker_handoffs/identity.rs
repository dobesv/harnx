//! User identity inheritance across generated and explicit handoff destinations.

use super::*;

struct IdentityInheritanceCase {
    case: &'static str,
    source_user: Option<&'static str>,
    inherit: bool,
    expected: &'static str,
}

fn identity_inheritance_cases() -> [IdentityInheritanceCase; 5] {
    [
        IdentityInheritanceCase {
            case: "owned",
            source_user: Some("source-owner"),
            inherit: true,
            expected: "source-owner",
        },
        IdentityInheritanceCase {
            case: "anonymous",
            source_user: None,
            inherit: true,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "blank",
            source_user: Some(" \t"),
            inherit: true,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "private",
            source_user: Some("private-owner"),
            inherit: false,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "existing",
            source_user: Some("source-owner"),
            inherit: true,
            expected: "existing-owner",
        },
    ]
}

async fn seed_existing_target_metadata(
    store: &SessionMetadataStore,
    target_id: &str,
) -> Result<()> {
    store
        .create(&harnx_runtime::nats_session_metadata::SessionMetadata::new(
            target_id,
            SessionInitializer::named("delegate-agent", Default::default())
                .with_user_id("existing-owner"),
        ))
        .await?
        .expect("seed existing target");
    Ok(())
}

async fn setup_identity_source_session(
    fixture: &HandoffFixture,
    source_id: &str,
    source_user: Option<&str>,
    inherit: bool,
) -> Result<NatsSession> {
    let mut source_config = session_config("source-agent", Some(source_id));
    if let Some(user) = source_user {
        source_config.initializer =
            source_config
                .initializer
                .with_properties(serde_json::from_value(json!({
                    "user_id": {"value": user, "inherit": inherit},
                    "git_branch": {"value": "source-only", "inherit": true}
                }))?);
    }
    // Raw creation preserves legacy blank properties so the inheritance
    // path, not just with_user_id(), must normalize them before defaults.
    NatsSession::new(
        source_config,
        fixture.client.clone(),
        fixture.jetstream.clone(),
        create_abort_signal(),
    )
    .await
}

async fn execute_identity_handoff_turn(
    fixture: &HandoffFixture,
    source: NatsSession,
    explicit: bool,
    target_id: &str,
) -> Result<String> {
    let stream = fixture.source_stream(&source).await?;
    let prompt = if explicit {
        format!("identity-explicit:{target_id}")
    } else {
        "generated handoff".to_string()
    };
    source
        .with_external_admission()
        .run_turn(&prompt, Arc::new(NullSink), None)
        .await?;
    let observed = observe_source_handoff(stream).await?;
    let (_, committed_id, _) = observed.committed.expect("handoff committed");
    if explicit {
        assert_eq!(committed_id, target_id);
    }
    Ok(committed_id)
}

struct HandoffTarget<'a> {
    committed_id: &'a str,
    expected: &'a str,
    case: &'a str,
    explicit: bool,
}

async fn assert_identity_handoff_target(
    fixture: &HandoffFixture,
    store: &SessionMetadataStore,
    target: HandoffTarget<'_>,
) -> Result<()> {
    let HandoffTarget {
        committed_id,
        expected,
        case,
        explicit,
    } = target;
    let target_log =
        NatsSessionLog::for_agent(fixture.jetstream.clone(), "delegate-agent", committed_id);
    wait_for_handoff_target(&target_log, "finish generated work").await?;
    let record = store
        .get_for_agent(committed_id, "delegate-agent")
        .await?
        .expect("target metadata");
    let properties = harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?;
    assert_eq!(
        properties.text("user_id"),
        Some(expected),
        "case={case}, explicit={explicit}"
    );
    assert_ne!(properties.text("git_branch"), Some("source-only"));
    Ok(())
}

async fn run_identity_inheritance_case(
    fixture: &HandoffFixture,
    store: &SessionMetadataStore,
    explicit: bool,
    test_case: &IdentityInheritanceCase,
) -> Result<()> {
    let source_id = format!("identity-{}-{explicit}", test_case.case);
    let target_id = format!("target-{}-{explicit}", test_case.case);
    if test_case.case == "existing" {
        seed_existing_target_metadata(store, &target_id).await?;
    }
    let source = setup_identity_source_session(
        fixture,
        &source_id,
        test_case.source_user,
        test_case.inherit,
    )
    .await?;
    let committed_id = execute_identity_handoff_turn(fixture, source, explicit, &target_id).await?;
    assert_identity_handoff_target(
        fixture,
        store,
        HandoffTarget {
            committed_id: &committed_id,
            expected: test_case.expected,
            case: test_case.case,
            explicit,
        },
    )
    .await
}

pub(super) async fn run_scenario() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    let store = SessionMetadataStore::ensure(&fixture.jetstream, 1).await?;
    for explicit in [false, true] {
        for test_case in identity_inheritance_cases() {
            if test_case.case == "existing" && !explicit {
                continue;
            }
            run_identity_inheritance_case(&fixture, &store, explicit, &test_case).await?;
        }
    }
    Ok(())
}
