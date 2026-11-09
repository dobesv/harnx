use super::*;
use harnx_runtime::nats_worker::tool_reservation::ToolReservationView;
use harnx_runtime::operator_tools::{BashToolSelection, OperatorToolCommand, OperatorToolReply};
use harnx_runtime::tool_reservation_client::ToolReservationHandle;

fn bash_path(path: &std::path::Path) -> String {
    shell_words::join([path.to_string_lossy().as_ref()])
}

#[test]
fn bash_path_quote_preserves_backslashes_and_spaces() {
    let path = std::path::Path::new(r"C:\Users\Runner Admin\AppData\Local\Temp\sample.txt");
    let quoted = bash_path(path);
    assert_eq!(
        shell_words::split(&quoted).unwrap(),
        [r"C:\Users\Runner Admin\AppData\Local\Temp\sample.txt"]
    );
}

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
    let sample_dir = h.dir.path().join("fixture dir with spaces");
    std::fs::create_dir_all(&sample_dir)?;
    let sample = sample_dir.join("sample.txt");
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
        bash_path(sample)
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
            format!("fs_read --path {}", bash_path(sample)),
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
            format!("fs_read --path {}", bash_path(sample)),
            BashToolSelection::Explicit {
                selectors: vec!["fs_*".into()],
            },
        )
        .await?;
    assert!(allowed.error.is_none(), "{allowed:?}");
    Ok(())
}

/// CodeRabbit #2655 finding D (4515991652): explicit agentless Bash must
/// admit Resolve(E) ∩ reservation view, never the full view. The CLI derives
/// the view from the same command, so these rows pin the public
/// `run_reserved_tool_command` contract for reservation reuse with an
/// independently chosen command selection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_agentless_explicit_admission_is_confined_to_command_selectors() -> Result<()> {
    let Some(h) = Harness::start(true, false).await? else {
        return Ok(());
    };
    let sample_dir = h.dir.path().join("confined dir");
    std::fs::create_dir_all(&sample_dir)?;
    let sample = sample_dir.join("sample.txt");
    std::fs::write(&sample, "confined content\n")?;
    let mut config = h.config.read().clone();
    config.nats_routing = NatsRouting::Cluster("local".into());

    // Broader reservation than the command's selectors: the view admits
    // every fs_* tool, but the explicit command names only fs_read.
    let mut broad = BashReservation::open(config.clone(), &["fs_*"]).await?;
    assert_explicit_confined_to_view(&broad, &sample, &sample_dir).await?;
    assert_explicit_empty_admits_nothing(&broad, &sample).await?;
    // Keep the fs server warm through the broad reservation, then prove a
    // narrower reservation cannot admit warm inventory outside its own view.
    let mut read_only = BashReservation::open(config, &["fs_read"]).await?;
    assert_read_only_view_confines_selectors(&read_only, &sample, &sample_dir).await?;
    read_only.close().await?;
    broad.close().await?;
    Ok(())
}

/// Rows against the broad fs_* view: the selected tool runs, a view tool the
/// selectors did not name stays out, and the script's shell part still runs.
async fn assert_explicit_confined_to_view(
    broad: &BashReservation,
    sample: &std::path::Path,
    sample_dir: &std::path::Path,
) -> Result<()> {
    let narrowed = broad
        .run(
            format!("fs_read --path {}", bash_path(sample)),
            BashToolSelection::Explicit {
                selectors: vec!["fs_read".into()],
            },
        )
        .await?;
    assert!(
        narrowed.error.is_none(),
        "selected tool must run: {narrowed:?}"
    );

    let not_selected = broad
        .run(
            format!("echo ok; fs_ls --path {}", bash_path(sample_dir)),
            BashToolSelection::Explicit {
                selectors: vec!["fs_read".into()],
            },
        )
        .await?;
    assert!(
        not_selected.error.is_some(),
        "view tool the selectors did not name must not dispatch: {not_selected:?}"
    );
    assert!(
        not_selected.output.contains("ok"),
        "shell part must still run: {not_selected:?}"
    );
    assert!(
        !not_selected.output.contains("confined dir")
            && !not_selected.output.contains("sample.txt"),
        "unselected fs_ls must not run: {not_selected:?}"
    );
    Ok(())
}

/// Explicit empty is still explicit: with a non-empty view it admits nothing —
/// pure shell only.
async fn assert_explicit_empty_admits_nothing(
    broad: &BashReservation,
    sample: &std::path::Path,
) -> Result<()> {
    let empty = broad
        .run(
            format!("echo empty; fs_read --path {}", bash_path(sample)),
            BashToolSelection::Explicit {
                selectors: Vec::new(),
            },
        )
        .await?;
    assert!(
        empty.error.is_some(),
        "explicit empty must not admit the reservation view: {empty:?}"
    );
    assert!(
        empty.output.contains("empty"),
        "explicit empty must still run pure shell: {empty:?}"
    );
    assert!(
        !empty.output.contains("confined content"),
        "fs_read must not dispatch on explicit empty: {empty:?}"
    );
    Ok(())
}

/// Warm-inventory rows against the read-only view: E resolves fs_ls, but the
/// fs_read-only view keeps it out; and the view's own tool is no substitute
/// for the selectors.
async fn assert_read_only_view_confines_selectors(
    read_only: &BashReservation,
    sample: &std::path::Path,
    sample_dir: &std::path::Path,
) -> Result<()> {
    let outside_view = read_only
        .run(
            format!("echo warm; fs_ls --path {}", bash_path(sample_dir)),
            BashToolSelection::Explicit {
                selectors: vec!["fs_ls".into()],
            },
        )
        .await?;
    assert!(
        outside_view.error.is_some(),
        "selector outside the reservation view must not admit warm inventory: {outside_view:?}"
    );
    assert!(
        outside_view.output.contains("warm"),
        "shell part must still run: {outside_view:?}"
    );
    let view_only = read_only
        .run(
            format!("echo view; fs_read --path {}", bash_path(sample)),
            BashToolSelection::Explicit {
                selectors: vec!["fs_ls".into()],
            },
        )
        .await?;
    assert!(
        view_only.error.is_some(),
        "view tool not named by the selectors must not dispatch: {view_only:?}"
    );
    assert!(
        !view_only.output.contains("confined content"),
        "fs_read must not dispatch when selectors name only fs_ls: {view_only:?}"
    );
    Ok(())
}

/// #2655 cycle-2 F1 (completing finding A): agentless reserved Bash must
/// honor `config.tool_use` in every selection mode. Named-worker Bash,
/// agentless explicit Bash and direct `call tool` already gate on it;
/// inferred names bypassed the gate. With tool use disabled, neither
/// inferred literal names nor explicit selectors may admit or dispatch an
/// inner tool — only the synthetic `bash` keeps running as pure shell.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_agentless_bash_tool_use_disabled_admits_no_inner_tools() -> Result<()> {
    let Some(h) = Harness::start(true, false).await? else {
        return Ok(());
    };
    let sample_dir = h.dir.path().join("disabled dir");
    std::fs::create_dir_all(&sample_dir)?;
    let sample = sample_dir.join("sample.txt");
    std::fs::write(&sample, "disabled content\n")?;
    let mut config = h.config.read().clone();
    config.nats_routing = NatsRouting::Cluster("local".into());
    config.tool_use = false;

    let mut reservation = BashReservation::open(config, &["fs_read"]).await?;
    assert_disabled_selection_admits_nothing(
        &reservation,
        &sample,
        BashToolSelection::Inferred {
            names: vec!["fs_read".into()],
        },
        "inferred names",
    )
    .await?;
    assert_disabled_selection_admits_nothing(
        &reservation,
        &sample,
        BashToolSelection::Explicit {
            selectors: vec!["fs_read".into()],
        },
        "explicit selectors",
    )
    .await?;
    assert_disabled_pure_shell_still_runs(&reservation).await?;
    reservation.close().await
}

/// One disabled-tool-use row: the given selection must not admit or dispatch
/// fs_read, while the script's shell part still runs.
async fn assert_disabled_selection_admits_nothing(
    reservation: &BashReservation,
    sample: &std::path::Path,
    selection: BashToolSelection,
    source: &str,
) -> Result<()> {
    let reply = reservation
        .run(
            format!("echo ok; fs_read --path {}", bash_path(sample)),
            selection,
        )
        .await?;
    assert!(
        reply.error.is_some(),
        "{source} must not admit with tool use disabled: {reply:?}"
    );
    assert!(
        reply.output.contains("ok"),
        "shell part must still run: {reply:?}"
    );
    assert!(
        !reply.output.contains("disabled content"),
        "fs_read must not dispatch with tool use disabled: {reply:?}"
    );
    Ok(())
}

async fn assert_disabled_pure_shell_still_runs(reservation: &BashReservation) -> Result<()> {
    let shell = reservation
        .run(
            "echo pure-shell".into(),
            BashToolSelection::Inferred { names: Vec::new() },
        )
        .await?;
    assert!(
        shell.error.is_none(),
        "pure shell must still run with tool use disabled: {shell:?}"
    );
    assert!(shell.output.contains("pure-shell"), "{shell:?}");
    Ok(())
}

/// #2655 cycle-2 F2: inferred agentless Bash is bounded by the reservation
/// view's admitted set — the same decoupled-view gap finding D closed for
/// explicit commands. The CLI derives the view from the same command, so
/// only the public `run_reserved_tool_command` contract exercises this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_agentless_inferred_admission_is_confined_to_reservation_view() -> Result<()> {
    let Some(h) = Harness::start(true, false).await? else {
        return Ok(());
    };
    let sample_dir = h.dir.path().join("inferred dir");
    std::fs::create_dir_all(&sample_dir)?;
    std::fs::write(sample_dir.join("entry.txt"), "inferred entry\n")?;
    let mut config = h.config.read().clone();
    config.nats_routing = NatsRouting::Cluster("local".into());

    // The fs server stays warm through the broad reservation, but the
    // read-only view bounds the inferred command to fs_read: inferred fs_ls,
    // outside the view, must not admit warm inventory.
    let mut broad = BashReservation::open(config.clone(), &["fs_*"]).await?;
    let mut read_only = BashReservation::open(config, &["fs_read"]).await?;
    let outside = read_only
        .run(
            format!("echo warm; fs_ls --path {}", bash_path(&sample_dir)),
            BashToolSelection::Inferred {
                names: vec!["fs_ls".into()],
            },
        )
        .await?;
    assert!(
        outside.error.is_some(),
        "inferred name outside the reservation view must not admit warm inventory: {outside:?}"
    );
    assert!(
        outside.output.contains("warm"),
        "shell part must still run: {outside:?}"
    );
    assert!(
        !outside.output.contains("inferred entry"),
        "fs_ls must not dispatch outside the view: {outside:?}"
    );
    read_only.close().await?;
    broad.close().await?;
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
