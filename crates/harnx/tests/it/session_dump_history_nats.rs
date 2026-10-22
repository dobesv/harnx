use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::process::{Command, Output};

struct DumpFixture {
    root: tempfile::TempDir,
    log: NatsSessionLog,
    entries: Vec<(u64, SessionLogEntry)>,
}

impl DumpFixture {
    async fn new(url: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let servers = root.path().join("config/nats_servers");
        std::fs::create_dir_all(&servers)?;
        std::fs::write(servers.join("fixture.yaml"), format!("url: {url}\n"))?;
        let client = async_nats::connect(url).await?;
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadata::new(
            "history",
            SessionInitializer::named("plain", Default::default()),
        );
        SessionMetadataStore::ensure(&jetstream, 1)
            .await?
            .create(&metadata)
            .await?;
        let log = NatsSessionLog::new_with_replicas(jetstream, metadata.storage_key(), 1);
        let entries: Vec<(u64, SessionLogEntry)> = serde_json::from_str(include_str!(
            "../../../harnx-core/tests/fixtures/compaction-history.json"
        ))?;
        for (seq, entry) in &entries {
            assert_eq!(log.append_event_async(entry).await?, *seq);
        }
        assert_eq!(log.load_events_at_least_async(23).await?, entries);
        Ok(Self { root, log, entries })
    }

    fn run(&self, args: &[String]) -> Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_harnx"))
            .args(args)
            .env("HARNX_CONFIG_DIR", self.root.path().join("config"))
            .env("HARNX_DATA_DIR", self.root.path().join("data"))
            .env("HARNX_STATE_DIR", self.root.path().join("state"))
            .env("HARNX_NATS_SERVER", "fixture")
            .env("NO_COLOR", "1")
            .env_remove("HARNX_NATS_URL")
            .env_remove("HARNX_NATS_TOKEN")
            .output()
            .context("run harnx dump session")
    }

    fn dump(&self, extra: &[&str]) -> Result<Output> {
        let args = ["dump", "session", "plain@fixture", "history"]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        self.run(&args)
    }

    fn follow_hint(&self, output: &Output, seq: u64) -> Result<Output> {
        let stdout = std::str::from_utf8(&output.stdout)?;
        let command = format!("harnx dump session plain@fixture history --before-compaction {seq}");
        assert_eq!(
            stdout.lines().next(),
            Some(format!("── Compaction {seq}: {command} ──").as_str())
        );
        let (_, printed) = stdout.lines().next().unwrap().split_once(": ").unwrap();
        let args = shell_words::split(printed.strip_suffix(" ──").unwrap())?;
        Ok(successful_output(self.run(&args[1..])?))
    }

    async fn assert_read_only(&self) -> Result<()> {
        assert_eq!(self.log.load_events_async().await?, self.entries);
        Ok(())
    }
}

fn successful_output(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_first_window(output: &Output) {
    // Keep existing CLI channels: summary notices go to stdout; user/final text
    // goes to stderr. The in-process snapshots capture their terminal ordering.
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"Inspect the session log format.\nTool calls and results are separate log entries.\n"
    );
}

fn assert_same_transcript(actual: Output, expected: &Output) {
    let actual = successful_output(actual);
    assert_eq!(
        (&actual.stdout, &actual.stderr),
        (&expected.stdout, &expected.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_walks_back_with_printed_commands_from_the_shared_fixture() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let fixture = DumpFixture::new(server.url()).await?;
    let current = successful_output(fixture.dump(&[])?);
    let previous = fixture.follow_hint(&current, 15)?;
    let first = fixture.follow_hint(&previous, 5)?;
    assert_first_window(&first);
    for (selector, expected) in [("1 back", &previous), ("2 back", &first)] {
        assert_same_transcript(fixture.dump(&["--before-compaction", selector])?, expected);
    }
    fixture.assert_read_only().await
}

fn assert_selector_error(fixture: &DumpFixture, case: (&str, &str)) -> Result<()> {
    let (selector, expected) = case;
    let output = fixture.dump(&["--before-compaction", selector])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "stderr: {:?}",
        output.stderr
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_unknown_compaction_and_excess_depth_fail_without_output() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let fixture = DumpFixture::new(server.url()).await?;
    for case in [
        ("99", "unknown compaction sequence 99"),
        ("7", "sequence 7 is not a compaction"),
        (
            "3 back",
            "cannot go 3 compactions back: only 2 available from the current view",
        ),
    ] {
        assert_selector_error(&fixture, case)?;
    }
    fixture.assert_read_only().await
}
