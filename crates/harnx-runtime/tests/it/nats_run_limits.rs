use crate::common::spawn_nats_server;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use harnx_core::config_data::{RunLimitsConfig, RunLimitsTimeout};
use harnx_runtime::{
    config::{agent, Config},
    nats_session_metadata::{
        CallTimeoutOverride, InvocationEdgeKind, InvocationIdentity, RunIdentity,
        RunLimitsPolicySource, RunLimitsRecord, SessionMetadataStore, SESSION_METADATA_BUCKET,
    },
};
use std::{ffi::OsString, num::NonZeroU64, time::Duration};

const STORAGE: &str = "test-agent-scoped-storage-key";

fn time() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn root(secs: u64) -> Result<RunLimitsRecord> {
    Ok(RunLimitsRecord::admit_root(
        RunIdentity::new(),
        InvocationIdentity::new(),
        time(),
        RunLimitsConfig {
            timeout_secs: RunLimitsTimeout::Finite(NonZeroU64::new(secs).unwrap()),
        },
        None,
        CallTimeoutOverride::Omitted,
    )?)
}

// Nextest isolates environment changes per process. Restore on panic as well.
struct ConfigFiles {
    dir: tempfile::TempDir,
    previous: Option<OsString>,
}

impl ConfigFiles {
    fn new() -> Result<Self> {
        harnx_core::require_nextest();
        let dir = tempfile::tempdir()?;
        let previous = std::env::var_os("HARNX_CONFIG_DIR");
        unsafe { std::env::set_var("HARNX_CONFIG_DIR", dir.path()) };
        Ok(Self { dir, previous })
    }

    fn load(&self, yaml: &str) -> Result<Config> {
        let path = self.dir.path().join("config.yaml");
        std::fs::write(&path, yaml)?;
        Config::load_from_file(&path)
    }

    fn target(&self, timeout: Option<&str>) -> Result<agent::Agent> {
        let path = self.dir.path().join("agents/target.md");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let frontmatter = timeout
            .map(|value| format!("run_limits:\n  timeout_secs: {value}\n"))
            .unwrap_or_default();
        std::fs::write(&path, format!("---\n{frontmatter}---\nTarget worker"))?;
        agent::load(&path)
    }

    fn package_target(&self) -> Result<agent::Agent> {
        let path = self.dir.path().join("packages/limits/agents/target.md");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            "---\nrun_limits:\n  timeout_secs: 30\n---\nTarget worker",
        )?;
        agent::load_with_qualified_name(&path, "limits/target")
    }

    fn patch(&self, expression: &str) -> Result<()> {
        let path = self.dir.path().join("packages/limits.patch.yaml");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, format!("agents:\n  - '{expression}'\n"))?;
        Ok(())
    }
}

impl Drop for ConfigFiles {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var("HARNX_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("HARNX_CONFIG_DIR") },
        }
    }
}

#[test]
fn real_loader_frontmatter_and_call_inherit_values_use_finite_policy() -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Call {
        #[serde(
            default,
            deserialize_with = "harnx_core::config_data::deserialize_timeout_override"
        )]
        timeout_secs: Option<u64>,
    }
    let files = ConfigFiles::new()?;
    for (yaml, global_secs) in [
        ("{}", 86400),
        ("run_limits:\n  timeout_secs: null", 86400),
        ("run_limits:\n  timeout_secs: 0", 86400),
        ("run_limits:\n  timeout_secs: -1", 86400),
        ("run_limits:\n  timeout_secs: -9223372036854775808", 86400),
        ("run_limits:\n  timeout_secs: 80", 80),
    ] {
        let config = files.load(yaml)?;
        for timeout in [
            None,
            Some("null"),
            Some("0"),
            Some("-1"),
            Some("-9223372036854775808"),
            Some("40"),
            Some("604800"),
            Some("2592000"),
        ] {
            let target = files.target(timeout)?;
            let expected = timeout
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(global_secs);
            let resolved = config.resolve_run_deadline(
                Some(&target),
                CallTimeoutOverride::Omitted,
                None,
                time(),
            )?;
            assert_eq!(
                resolved.deadline,
                Some(time() + chrono::Duration::seconds(expected)),
                "{yaml}/{timeout:?}"
            );
            assert_eq!(
                config.resolve_run_limits(Some(&target)).timeout_secs.get(),
                expected as u64
            );
            let source =
                if timeout.is_some_and(|value| ["40", "604800", "2592000"].contains(&value)) {
                    RunLimitsPolicySource::TargetAgent {
                        agent_name: "target".into(),
                    }
                } else {
                    RunLimitsPolicySource::GlobalDefault
                };
            assert_eq!(resolved.source, source);
            for json in [
                "{}",
                "{\"timeout_secs\":null}",
                "{\"timeout_secs\":0}",
                "{\"timeout_secs\":-1}",
                "{\"timeout_secs\":-9223372036854775808}",
            ] {
                let call: Call = serde_json::from_str(json)?;
                let intent = CallTimeoutOverride::from_optional(call.timeout_secs);
                assert_eq!(
                    config.resolve_run_deadline(Some(&target), intent, None, time())?,
                    resolved,
                    "{json}"
                );
                let parent = root(7)?;
                assert_eq!(
                    config
                        .resolve_run_deadline(Some(&target), intent, Some(&parent), time())?
                        .deadline,
                    parent.deadline,
                    "{json}: inherited clamp"
                );
            }
            let positive = config.resolve_run_deadline(
                Some(&target),
                CallTimeoutOverride::from_optional(Some(10)),
                None,
                time(),
            )?;
            assert_eq!(
                positive.deadline,
                Some(time() + chrono::Duration::seconds(10))
            );
            assert_eq!(positive.source, RunLimitsPolicySource::ExplicitOverride);
        }
    }
    for value in ["unlimited", "\"80\"", "1.5", "18446744073709551615"] {
        assert!(files
            .load(&format!("run_limits:\n  timeout_secs: {value}"))
            .is_err());
        assert!(files.target(Some(value)).is_err());
    }
    Ok(())
}

#[test]
fn real_target_patch_controls_effective_policy_and_diagnostics() -> Result<()> {
    let files = ConfigFiles::new()?;
    let config = files.load("run_limits:\n  timeout_secs: 80\n")?;
    let original = files.package_target()?;
    assert_eq!(
        config
            .resolve_run_limits(Some(&original))
            .timeout_secs
            .get(),
        30
    );
    for (expr, expected) in [
        ("45", 45),
        ("604800", 604800),
        ("2592000", 2592000),
        ("null", 80),
        ("0", 80),
        ("-1", 80),
        ("-9223372036854775808", 80),
    ] {
        files.patch(&format!(
            "if .name == \"target\" then .run_limits.timeout_secs = {expr} end"
        ))?;
        let effective = files.package_target()?;
        let resolved = config.resolve_run_deadline(
            Some(&effective),
            CallTimeoutOverride::Omitted,
            None,
            time(),
        )?;
        assert_eq!(
            resolved.deadline,
            Some(time() + chrono::Duration::seconds(expected)),
            "{expr}"
        );
        assert_eq!(
            resolved.source,
            if expected == 80 {
                RunLimitsPolicySource::GlobalDefault
            } else {
                RunLimitsPolicySource::TargetAgent {
                    agent_name: "limits/target".into(),
                }
            }
        );
    }
    Ok(())
}

#[test]
fn real_loader_rejects_invalid_policy_and_patches() -> Result<()> {
    let files = ConfigFiles::new()?;
    for value in [
        "unlimited",
        "\"80\"",
        "1.5",
        "true",
        "invalid",
        "18446744073709551615",
    ] {
        assert!(
            files
                .load(&format!("run_limits:\n  timeout_secs: {value}\n"))
                .is_err(),
            "global value {value}"
        );
        assert!(files.target(Some(value)).is_err(), "target value {value}");
    }
    files.patch(".run_limits.timeout_secs = \"unlimited\"")?;
    assert!(files.package_target().is_err());
    Ok(())
}

#[tokio::test]
async fn immutable_broker_roundtrip_conflict_and_independent_runs() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    let first = root(60)?;
    assert!(store
        .get_run_limits(STORAGE, first.run_id.as_str())
        .await?
        .is_none());
    assert!(store
        .get_invocation_limits(STORAGE, first.invocation_id.as_str())
        .await?
        .is_none());
    let run_revision = store.put_run_limits(STORAGE, &first).await?;
    let invocation_revision = store.put_invocation_limits(STORAGE, &first).await?;
    assert_eq!(store.put_run_limits(STORAGE, &first).await?, run_revision);
    assert_eq!(
        store.put_invocation_limits(STORAGE, &first).await?,
        invocation_revision
    );
    let mut changed = first.clone();
    changed.admitted_at += chrono::Duration::seconds(50);
    changed.deadline = None;
    assert!(store.put_run_limits(STORAGE, &changed).await.is_err());
    assert!(store
        .put_invocation_limits(STORAGE, &changed)
        .await
        .is_err());
    assert_eq!(
        store.get_run_limits(STORAGE, first.run_id.as_str()).await?,
        Some(first.clone())
    );
    assert_eq!(
        store
            .get_invocation_limits(STORAGE, first.invocation_id.as_str())
            .await?,
        Some(first.clone())
    );
    let child = RunLimitsRecord::admit_child(
        &first,
        InvocationIdentity::new(),
        InvocationEdgeKind::Delegation,
        time() + chrono::Duration::seconds(50),
        RunLimitsConfig::default(),
        None,
        CallTimeoutOverride::from_optional(Some(0)),
    )?;
    store.put_invocation_limits(STORAGE, &child).await?;
    assert!(store.put_run_limits(STORAGE, &child).await.is_err());
    assert_eq!(
        store
            .get_invocation_limits(STORAGE, child.invocation_id.as_str())
            .await?,
        Some(child)
    );
    let independent = root(90)?;
    store.put_run_limits(STORAGE, &independent).await?;
    store.put_invocation_limits(STORAGE, &independent).await?;
    assert_eq!(
        store
            .get_run_limits(STORAGE, independent.run_id.as_str())
            .await?,
        Some(independent)
    );
    assert_eq!(
        store.get_run_limits(STORAGE, first.run_id.as_str()).await?,
        Some(first.clone())
    );
    assert_eq!(store.put_run_limits(STORAGE, &first).await?, run_revision);
    Ok(())
}

#[tokio::test]
async fn concurrent_broker_creators_have_one_immutable_winner() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    let first = root(60)?;
    let mut competing = first.clone();
    competing.deadline = None;
    let (left, right) = tokio::join!(
        store.put_run_limits(STORAGE, &first),
        store.put_run_limits(STORAGE, &competing)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let saved = store
        .get_run_limits(STORAGE, first.run_id.as_str())
        .await?
        .context("winner")?;
    assert_eq!(
        saved,
        if left.is_ok() {
            first.clone()
        } else {
            competing.clone()
        }
    );
    let (left, right) = tokio::join!(
        store.put_invocation_limits(STORAGE, &first),
        store.put_invocation_limits(STORAGE, &competing)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let saved = store
        .get_invocation_limits(STORAGE, first.invocation_id.as_str())
        .await?
        .context("invocation winner")?;
    assert_eq!(saved, if left.is_ok() { first } else { competing });
    let next = root(20)?;
    let mut next_competing = next.clone();
    next_competing.deadline = None;
    let (left, right) = tokio::join!(
        store.load_or_create_run_limits(STORAGE, next.run_id.as_str(), || Ok(next.clone())),
        store.load_or_create_run_limits(STORAGE, next.run_id.as_str(), || Ok(
            next_competing.clone()
        ))
    );
    let frozen_root = left?;
    assert_eq!(frozen_root, right?);
    assert!(frozen_root == next || frozen_root == next_competing);
    let (left, right) = tokio::join!(
        store.load_or_create_invocation_limits(
            STORAGE,
            next.run_id.as_str(),
            next.invocation_id.as_str(),
            || Ok(next.clone())
        ),
        store.load_or_create_invocation_limits(
            STORAGE,
            next.run_id.as_str(),
            next.invocation_id.as_str(),
            || Ok(next_competing.clone())
        )
    );
    let frozen_invocation = left?;
    assert_eq!(frozen_invocation, right?);
    assert!(frozen_invocation == next || frozen_invocation == next_competing);
    Ok(())
}

#[tokio::test]
async fn replay_after_reload_reuses_frozen_broker_records() -> Result<()> {
    let files = ConfigFiles::new()?;
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    let config = files.load("run_limits:\n  timeout_secs: 80\n")?;
    let target = files.package_target()?;
    let run_id = RunIdentity::new();
    let invocation_id = InvocationIdentity::new();
    let first = store
        .load_or_create_run_limits(STORAGE, run_id.as_str(), || {
            Ok(RunLimitsRecord::admit_root(
                run_id.clone(),
                invocation_id.clone(),
                time(),
                config.data.run_limits,
                Some(&target),
                CallTimeoutOverride::Omitted,
            )?)
        })
        .await?;
    let admitted = store
        .load_or_create_invocation_limits(STORAGE, run_id.as_str(), invocation_id.as_str(), || {
            Ok(first.clone())
        })
        .await?;
    assert_eq!(first, admitted);
    // Change both global config and package patch. New admissions see the change.
    files.patch(".run_limits.timeout_secs = 900")?;
    let reloaded = files.load("run_limits:\n  timeout_secs: 2592000\n")?;
    let new_target = files.package_target()?;
    let later = time() + chrono::Duration::seconds(300);
    assert_eq!(
        reloaded
            .resolve_run_deadline(Some(&new_target), CallTimeoutOverride::Omitted, None, later)?
            .deadline,
        Some(later + chrono::Duration::seconds(900))
    );
    drop(store);
    // Fresh client/store models replay ownership after failover, not an in-memory cache.
    let replay_client = async_nats::connect(server.url()).await?;
    let replay =
        SessionMetadataStore::ensure(&async_nats::jetstream::new(replay_client), 1).await?;
    let frozen = replay
        .load_or_create_run_limits(STORAGE, run_id.as_str(), || {
            panic!("replay must not resolve new policy")
        })
        .await?;
    assert_eq!(frozen, first);
    assert!(frozen.is_expired_at(later));
    assert_eq!(
        replay
            .load_or_create_invocation_limits(
                STORAGE,
                run_id.as_str(),
                invocation_id.as_str(),
                || panic!("replay must not renew admission")
            )
            .await?,
        first
    );
    let independent = RunLimitsRecord::admit_root(
        RunIdentity::new(),
        InvocationIdentity::new(),
        later,
        reloaded.data.run_limits,
        Some(&new_target),
        CallTimeoutOverride::Omitted,
    )?;
    replay.put_run_limits(STORAGE, &independent).await?;
    assert_eq!(
        replay.get_run_limits(STORAGE, run_id.as_str()).await?,
        Some(first)
    );
    assert_eq!(
        replay
            .get_run_limits(STORAGE, independent.run_id.as_str())
            .await?,
        Some(independent)
    );
    Ok(())
}

#[tokio::test]
async fn lost_create_ack_is_read_back_for_run_and_invocation_records() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let mut js = async_nats::jetstream::new(client.clone());
    js.set_timeout(Duration::from_secs(1));
    let real = SessionMetadataStore::ensure(&js, 1).await?;
    let mut fault_kv = real.kv_store().clone();
    fault_kv.put_prefix = Some("fault.".into());
    let fault = SessionMetadataStore::from_store(fault_kv, client.clone());
    let record = root(60)?;
    for is_run in [true, false] {
        let key = if is_run {
            format!("sessions/{STORAGE}/runs/{}", record.run_id.as_str())
        } else {
            format!(
                "sessions/{STORAGE}/invocations/{}",
                record.invocation_id.as_str()
            )
        };
        let mut requests = client.subscribe(format!("fault.{key}")).await?;
        client.flush().await?;
        let actual_js = js.clone();
        let actual_key = key.clone();
        let proxy = tokio::spawn(async move {
            let request = requests.next().await.context("create request")?;
            let ack = actual_js
                .publish_with_headers(
                    format!("$KV.{SESSION_METADATA_BUCKET}.{actual_key}"),
                    request.headers.unwrap_or_default(),
                    request.payload,
                )
                .await?
                .await?;
            // Commit to the real broker, but withhold PubAck from the original caller.
            assert!(
                tokio::time::timeout(Duration::from_millis(1500), requests.next())
                    .await
                    .is_err(),
                "create must not be resent after ambiguous acknowledgement"
            );
            Ok::<_, anyhow::Error>(ack.sequence)
        });
        let observed = if is_run {
            fault.put_run_limits(STORAGE, &record).await?
        } else {
            fault.put_invocation_limits(STORAGE, &record).await?
        };
        assert_eq!(observed, proxy.await??);
        let entry = real
            .kv_store()
            .entry(&key)
            .await?
            .context("durable admission")?;
        assert_eq!(entry.revision, observed);
        assert_eq!(
            serde_json::from_slice::<RunLimitsRecord>(&entry.value)?,
            record
        );
        let retry = if is_run {
            real.put_run_limits(STORAGE, &record).await?
        } else {
            real.put_invocation_limits(STORAGE, &record).await?
        };
        assert_eq!(retry, observed);
    }
    Ok(())
}

#[tokio::test]
async fn malformed_deleted_and_wrong_identity_records_fail_closed() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    let record = root(60)?;
    let wrong_run = RunIdentity::new();
    assert!(store
        .load_or_create_invocation_limits(
            STORAGE,
            wrong_run.as_str(),
            record.invocation_id.as_str(),
            || Ok(record.clone())
        )
        .await
        .is_err());
    assert!(store
        .get_invocation_limits(STORAGE, record.invocation_id.as_str())
        .await?
        .is_none());
    store.put_run_limits(STORAGE, &record).await?;
    let key = format!("sessions/{STORAGE}/runs/{}", record.run_id.as_str());
    store.kv_store().delete(&key).await?;
    assert!(store
        .get_run_limits(STORAGE, record.run_id.as_str())
        .await
        .is_err());
    assert!(store.put_run_limits(STORAGE, &record).await.is_err());
    assert!(store
        .load_or_create_run_limits(STORAGE, record.run_id.as_str(), || panic!(
            "deleted identity must not be renewed"
        ))
        .await
        .is_err());
    let invocation_key = format!(
        "sessions/{STORAGE}/invocations/{}",
        record.invocation_id.as_str()
    );
    store
        .kv_store()
        .put(&invocation_key, "not json".into())
        .await?;
    assert!(store
        .get_invocation_limits(STORAGE, record.invocation_id.as_str())
        .await
        .is_err());
    assert!(store
        .load_or_create_invocation_limits(
            STORAGE,
            record.run_id.as_str(),
            record.invocation_id.as_str(),
            || panic!("corrupt identity must not be renewed")
        )
        .await
        .is_err());
    Ok(())
}
