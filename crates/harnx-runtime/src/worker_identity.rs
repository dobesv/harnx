//! Identity advertised by local workers for stale-process detection.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const IDENTITY_PROTOCOL: u8 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkerIdentity {
    protocol: u8,
    pub(crate) worker_id: String,
    pub(crate) pid: u32,
    pub(crate) build: String,
    pub(crate) executable_fingerprint: String,
    pub(crate) config_fingerprint: String,
}

impl WorkerIdentity {
    pub(crate) async fn current(worker_id: impl Into<String>) -> Result<Self> {
        let worker_id = worker_id.into();
        tokio::task::spawn_blocking(move || {
            Ok(Self {
                protocol: IDENTITY_PROTOCOL,
                worker_id,
                pid: std::process::id(),
                build: current_build().to_string(),
                executable_fingerprint: executable_fingerprint(
                    &std::env::current_exe().context(
                        "resolve the current worker executable for identity verification",
                    )?,
                )?,
                config_fingerprint: config_fingerprint()?,
            })
        })
        .await
        .context("join worker identity fingerprint task")?
    }

    pub(crate) fn from_payload(payload: &[u8]) -> Result<Self> {
        let identity: Self = serde_json::from_slice(payload)
            .context("local worker sent a legacy or invalid readiness marker")?;
        anyhow::ensure!(
            identity.protocol == IDENTITY_PROTOCOL,
            "local worker uses unsupported readiness protocol {}",
            identity.protocol
        );
        Ok(identity)
    }

    pub(crate) fn payload(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("serialize local worker identity")
    }
}

pub(crate) const fn current_build() -> &'static str {
    env!("HARNX_BUILD_SHA")
}

pub(crate) fn short_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

pub(crate) fn executable_fingerprint(path: &Path) -> Result<String> {
    let normalized = normalize(path);
    let metadata = fs::metadata(path)
        .with_context(|| format!("inspect worker executable '{}'", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("read worker executable mtime '{}'", path.display()))?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"harnx-worker-executable-v1\0");
    hasher.update(normalized.as_os_str().as_encoded_bytes());
    hasher.update(metadata.len().to_le_bytes());
    hasher.update(modified.as_secs().to_le_bytes());
    hasher.update(modified.subsec_nanos().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
    }
    Ok(hex_digest(hasher.finalize()))
}

/// Hash every file under the active config roots, plus an explicitly
/// redirected dotenv file. Absolute roots are part of the hash so two
/// frontends sharing a broker but using different config directories cannot
/// accidentally share one worker.
pub(crate) fn config_fingerprint() -> Result<String> {
    retry_transient_config_io(config_fingerprint_once)
}

fn config_fingerprint_once() -> Result<String> {
    let mut roots = vec![
        harnx_core::config_paths::config_dir(),
        harnx_core::config_paths::config_dir_path(),
    ];
    roots.sort();
    roots.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"harnx-local-worker-config-v1\0");
    for root in roots {
        hash_tree(&mut hasher, &root)?;
    }
    hash_external_file(
        &mut hasher,
        "env-file",
        &harnx_core::config_paths::env_file(),
    )?;
    Ok(hex_digest(hasher.finalize()))
}

fn retry_transient_config_io<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..MAX_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_transient_config_io_error(&error) => {
                log::debug!(
                    "config changed while fingerprinting; retrying ({attempt}/{MAX_ATTEMPTS})"
                );
                std::thread::yield_now();
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

fn is_transient_config_io_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::Interrupted
            )
        })
    })
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let digest = digest.as_ref();
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    fingerprint
}

fn hash_tree(hasher: &mut Sha256, root: &Path) -> Result<()> {
    let normalized = normalize(root);
    hasher.update(b"root\0");
    hasher.update(normalized.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    if !root.exists() {
        hasher.update(b"missing\0");
        return Ok(());
    }
    let mut visited = HashSet::new();
    hash_path(hasher, root, root, &mut visited)
}

fn hash_path(
    hasher: &mut Sha256,
    root: &Path,
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    hash_path_name(hasher, root, path);
    let symlink_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect config path '{}'", path.display()))?;
    let is_symlink = symlink_metadata.file_type().is_symlink();
    if is_symlink {
        hash_symlink_target(hasher, path)?;
    }
    let Some(metadata) = followed_metadata(path, is_symlink)? else {
        return Ok(());
    };
    if metadata.is_dir() {
        return hash_directory(hasher, root, path, visited);
    }
    if metadata.is_file() {
        hash_regular_file(hasher, path)?;
    }
    Ok(())
}

fn hash_path_name(hasher: &mut Sha256, root: &Path, path: &Path) {
    let relative = path.strip_prefix(root).unwrap_or(path);
    hasher.update(b"path\0");
    hasher.update(relative.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
}

fn hash_symlink_target(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let target =
        fs::read_link(path).with_context(|| format!("read config symlink '{}'", path.display()))?;
    hasher.update(b"symlink\0");
    hasher.update(target.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    Ok(())
}

fn followed_metadata(path: &Path, is_symlink: bool) -> Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(_) if is_symlink => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspect config path '{}'", path.display()))
        }
    }
}

fn hash_directory(
    hasher: &mut Sha256,
    root: &Path,
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    if !visited.insert(normalize(path)) {
        hasher.update(b"directory-cycle\0");
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read config directory '{}'", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        hash_path(hasher, root, &entry.path(), visited)?;
    }
    Ok(())
}

fn hash_regular_file(hasher: &mut Sha256, path: &Path) -> Result<()> {
    hasher.update(b"file\0");
    hasher
        .update(fs::read(path).with_context(|| format!("read config file '{}'", path.display()))?);
    hasher.update(b"\0");
    Ok(())
}

fn hash_external_file(hasher: &mut Sha256, label: &str, path: &Path) -> Result<()> {
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    let normalized = normalize(path);
    hasher.update(normalized.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    match fs::read(path) {
        Ok(contents) => hasher.update(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => hasher.update(b"missing"),
        Err(error) => {
            return Err(error).with_context(|| format!("read config file '{}'", path.display()))
        }
    }
    hasher.update(b"\0");
    Ok(())
}

fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_environment::{env_lock, EnvGuard};
    use std::cell::Cell;

    #[test]
    fn config_fingerprint_changes_with_client_config() {
        let _lock = env_lock();
        let root = tempfile::tempdir().expect("create config root");
        let data = tempfile::tempdir().expect("create data root");
        let _config = EnvGuard::new("HARNX_CONFIG_DIR", root.path());
        let _data = EnvGuard::new("HARNX_DATA_DIR", data.path());
        fs::create_dir(root.path().join("clients")).expect("create clients directory");
        let client = root.path().join("clients/openai.yaml");
        fs::write(&client, "type: openai\napi_key: first\n").expect("write client config");
        let first = config_fingerprint().expect("fingerprint initial config");

        fs::write(&client, "type: openai\napi_key: second\n").expect("update client config");
        let second = config_fingerprint().expect("fingerprint updated config");

        assert_ne!(first, second);
    }

    #[test]
    fn executable_fingerprint_tracks_file_metadata() {
        let directory = tempfile::tempdir().expect("create executable fixture directory");
        let executable = directory.path().join("worker");
        fs::write(&executable, b"first build").expect("write first executable fixture");
        let first = executable_fingerprint(&executable).expect("fingerprint first fixture");

        fs::write(&executable, b"second build").expect("write second executable fixture");
        let second = executable_fingerprint(&executable).expect("fingerprint second fixture");

        assert_ne!(first, second);
    }

    #[test]
    fn config_fingerprint_retries_transient_io_errors() {
        let attempts = Cell::new(0);
        let fingerprint = retry_transient_config_io(|| {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            if attempt < 3 {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
            }
            Ok("fingerprint")
        })
        .expect("transient config errors should be retried");

        assert_eq!(fingerprint, "fingerprint");
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn worker_identity_rejects_invalid_readiness_payload() {
        let error = WorkerIdentity::from_payload(b"local")
            .expect_err("legacy readiness marker must be rejected");

        assert!(
            format!("{error:#}").contains("legacy or invalid readiness marker"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn worker_identity_rejects_unsupported_protocol() {
        let identity = WorkerIdentity {
            protocol: IDENTITY_PROTOCOL + 1,
            worker_id: "local".to_string(),
            pid: 42,
            build: "test-build".to_string(),
            executable_fingerprint: "executable".to_string(),
            config_fingerprint: "config".to_string(),
        };
        let payload = serde_json::to_vec(&identity).expect("serialize test identity");
        let error = WorkerIdentity::from_payload(&payload)
            .expect_err("unsupported readiness protocol must be rejected");

        assert!(
            error
                .to_string()
                .contains("unsupported readiness protocol 2"),
            "unexpected error: {error:#}"
        );
    }
}
