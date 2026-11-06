use super::*;
use harnx_runtime::nats_worker::tool_reservation::ToolReservationView;
use harnx_runtime::operator_tools::{BashToolSelection, OperatorToolCommand, OperatorToolReply};
use harnx_runtime::tool_reservation_client::ToolReservationHandle;

struct BashReservation {
    handle: ToolReservationHandle,
    view: ToolReservationView,
}

impl BashReservation {
    async fn open(config: Config, selectors: &[&str]) -> Result<Self> {
        let view = ToolReservationView {
            package: None,
            use_tools: selectors.iter().map(|s| (*s).to_owned()).collect(),
        };
        let handle = ToolReservationHandle::open(
            config,
            harnx_runtime::SessionActivationRoute::ClusterShared,
            view.clone(),
        )
        .await?;
        Ok(Self { handle, view })
    }

    async fn run(&self, script: String, selection: BashToolSelection) -> Result<OperatorToolReply> {
        let command = OperatorToolCommand::Bash { script, selection };
        let abort = create_abort_signal();
        harnx_runtime::operator_tools::cli::run_reserved_tool_command(
            &self.handle,
            &self.view,
            harnx_runtime::operator_tools::cli::ToolCommandExecution {
                command: &command,
                json: true,
                abort: &abort,
                bash_timeout_secs: None,
            },
        )
        .await
    }

    async fn close(&mut self) -> Result<()> {
        self.handle.close().await
    }
}

/// #2630 Task 4: the two agentless rows of the selection matrix against a
/// real managed fs fixture server. Cold = the reservation claim starts the
/// server; warm = the same server is already running for later commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_agentless_bash_matrix_claims_admission_cold_and_warm() -> Result<()> {
    let Some(h) = Harness::start(true, false).await? else {
        return Ok(());
    };
    let sample = h.dir.path().join("sample.txt");
    std::fs::write(&sample, "reserved content\n")?;
    let mut config = h.config.read().clone();
    config.nats_routing = NatsRouting::Cluster("local".into());

    let mut cold = BashReservation::open(config.clone(), &["fs_read"]).await?;
    assert_cold_inferred(&cold, &sample).await?;
    // Keep both reservations alive: warm discovery must not widen admission.
    let mut warm = BashReservation::open(config.clone(), &["fs_*"]).await?;
    assert_warm_selection(&warm, &sample).await?;
    assert_unmatched_explicit(config.clone()).await?;
    assert_pure_shell(config).await?;
    warm.close().await?;
    cold.close().await?;
    Ok(())
}

async fn assert_cold_inferred(cold: &BashReservation, sample: &std::path::Path) -> Result<()> {
    // Claims and admission use exact literal names, never selector expansion.
    let script = format!(
        "result=$(fs_read --path {}); echo \"$result\"",
        sample.display()
    );
    let reply = cold
        .run(
            script,
            BashToolSelection::Inferred {
                names: vec!["fs_read".into()],
            },
        )
        .await?;
    assert!(reply.error.is_none(), "cold inferred run failed: {reply:?}");
    let result: Value = serde_json::from_str(&reply.output).unwrap_or(Value::Null);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["trace"][0]["name"], "fs_read");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved content"),
        "{reply:?}"
    );
    Ok(())
}

async fn assert_warm_selection(warm: &BashReservation, sample: &std::path::Path) -> Result<()> {
    let denied = warm
        .run(
            format!("fs_read --path {}", sample.display()),
            BashToolSelection::Inferred {
                names: vec!["fs_*".into()],
            },
        )
        .await?;
    assert!(
        denied.error.is_some(),
        "inferred literal must not expand selectors, warm or not: {denied:?}"
    );
    // Explicit selectors resolve through the normal path, so the same glob admits fs_read.
    let allowed = warm
        .run(
            format!("fs_read --path {}", sample.display()),
            BashToolSelection::Explicit {
                selectors: vec!["fs_*".into()],
            },
        )
        .await?;
    assert!(allowed.error.is_none(), "{allowed:?}");
    Ok(())
}

async fn assert_unmatched_explicit(config: Config) -> Result<()> {
    // The CLI's unmatched explicit selectors also become reservation claims.
    let mut reservation = BashReservation::open(config, &["no_such_tool"]).await?;
    let unmatched = reservation
        .run(
            "echo ok; fs_read --path x".into(),
            BashToolSelection::Explicit {
                selectors: vec!["no_such_tool".into()],
            },
        )
        .await?;
    assert!(
        unmatched.error.is_some(),
        "unmatched explicit must fail the tool call: {unmatched:?}"
    );
    assert!(
        unmatched.output.contains("ok"),
        "the script's shell part must still run: {unmatched:?}"
    );
    assert!(
        !unmatched.output.contains("Cannot resolve path"),
        "fs_read must not dispatch on unmatched explicit selectors: {unmatched:?}"
    );
    reservation.close().await?;
    Ok(())
}

async fn assert_pure_shell(config: Config) -> Result<()> {
    let mut reservation = BashReservation::open(config, &[]).await?;
    let shell = reservation
        .run(
            "echo hello".into(),
            BashToolSelection::Inferred { names: Vec::new() },
        )
        .await?;
    assert!(shell.error.is_none(), "{shell:?}");
    assert!(shell.output.contains("hello"), "{shell:?}");
    reservation.close().await?;
    Ok(())
}
