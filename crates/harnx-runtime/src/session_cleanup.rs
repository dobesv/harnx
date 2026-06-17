use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::fs;

use crate::config::{attachments_dir_for, GlobalConfig};

/// Convert byte count to human-readable string (e.g., "1.5 MB", "820 KB").
/// Uses 1024-based scaling while keeping KB/MB/GB labels.
pub fn humanize_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupStats {
    pub sessions_removed: u64,
    pub bytes_freed: u64,
}

pub async fn run_cleanup(config: &GlobalConfig, days: u64) -> CleanupStats {
    if days == 0 {
        return CleanupStats::default();
    }

    let threshold = Duration::from_secs(days.saturating_mul(86_400));
    let now = SystemTime::now();
    let (sessions_dir, sessions) = {
        let config = config.read();
        (config.sessions_dir(), config.list_sessions_with_meta())
    };

    let mut stats = CleanupStats::default();

    for session in sessions {
        let Some(modified) = session.modified else {
            continue;
        };

        let Ok(age) = now.duration_since(modified) else {
            debug!("session cleanup skipped entry with future timestamp");
            continue;
        };

        if age <= threshold {
            continue;
        }

        let session_yaml_path = sessions_dir.join(format!("{}.yaml", session.id));
        let attachments_dir = attachments_dir_for(&session_yaml_path);
        let measured =
            measure_path_size(&session_yaml_path).await + measure_path_size(&attachments_dir).await;

        let attachments_removed =
            remove_dir_best_effort(&attachments_dir, "attachments directory").await;
        let session_removed =
            remove_file_best_effort(&session_yaml_path, "session transcript").await;

        if attachments_removed && session_removed {
            stats.sessions_removed = stats.sessions_removed.saturating_add(1);
            stats.bytes_freed = stats.bytes_freed.saturating_add(measured);
        }
    }

    stats
}

async fn measure_path_size(path: &Path) -> u64 {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return 0,
        Err(err) => {
            debug!("session cleanup failed to read metadata during size measurement: {err}");
            return 0;
        }
    };

    if metadata.is_file() {
        return metadata.len();
    }

    if metadata.is_dir() {
        return measure_dir_size(path.to_path_buf()).await;
    }

    0
}

async fn measure_dir_size(root: PathBuf) -> u64 {
    let mut total = 0_u64;
    let mut stack = vec![root];

    while let Some(dir) = stack.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                debug!("session cleanup failed to read directory during size measurement: {err}");
                continue;
            }
        };

        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    total = total.saturating_add(measure_dir_entry(entry, &mut stack).await);
                }
                Ok(None) => break,
                Err(err) if err.kind() == ErrorKind::NotFound => break,
                Err(err) => {
                    debug!("session cleanup failed while iterating directory during size measurement: {err}");
                    break;
                }
            }
        }
    }

    total
}

async fn measure_dir_entry(entry: fs::DirEntry, stack: &mut Vec<PathBuf>) -> u64 {
    let file_type = match entry.file_type().await {
        Ok(file_type) => file_type,
        Err(err) if err.kind() == ErrorKind::NotFound => return 0,
        Err(err) => {
            debug!("session cleanup failed to read entry type during size measurement: {err}");
            return 0;
        }
    };

    if file_type.is_dir() {
        stack.push(entry.path());
        return 0;
    }

    if !file_type.is_file() {
        return 0;
    }

    match entry.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == ErrorKind::NotFound => 0,
        Err(err) => {
            debug!("session cleanup failed to read entry metadata during size measurement: {err}");
            0
        }
    }
}

async fn remove_dir_best_effort(path: &Path, kind: &str) -> bool {
    match fs::remove_dir_all(path).await {
        Ok(()) => true,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            debug!("session cleanup skipped missing {kind}");
            true
        }
        Err(err) => {
            warn!("session cleanup failed to remove {kind}: {err}");
            false
        }
    }
}

async fn remove_file_best_effort(path: &Path, kind: &str) -> bool {
    match fs::remove_file(path).await {
        Ok(()) => true,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            debug!("session cleanup skipped missing {kind}");
            true
        }
        Err(err) => {
            warn!("session cleanup failed to remove {kind}: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs as stdfs;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use filetime::{set_file_mtime, FileTime};
    use parking_lot::RwLock;
    use tempfile::TempDir;

    use super::*;
    use crate::config::Config;

    const SESSION_HEADER: &str =
        "type: header\nmodel: test-model\nsession_id: sess-123\nworking_dir: /tmp/work\n";

    #[test]
    fn humanize_bytes_boundaries() {
        // Bytes
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(1), "1 B");
        assert_eq!(humanize_bytes(1023), "1023 B");

        // KB boundaries
        assert_eq!(humanize_bytes(1024), "1 KB");
        assert_eq!(humanize_bytes(1536), "2 KB"); // 1.5 KB rounds to 2 KB
        assert_eq!(humanize_bytes(1024 * 1023), "1023 KB");

        // MB boundaries
        assert_eq!(humanize_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(humanize_bytes(1024 * 1024 * 3 + 1024 * 512), "3.5 MB");
        assert_eq!(humanize_bytes(1024 * 1024 * 1023), "1023.0 MB");

        // GB boundaries
        assert_eq!(humanize_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(humanize_bytes(1024 * 1024 * 1024 * 5), "5.0 GB");
    }

    #[tokio::test]
    async fn cleanup_disabled_when_days_zero_leaves_stale_sessions_on_disk() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let session =
            create_session_fixture(tmp.path(), "stale-disabled", b"attachment-bytes").unwrap();
        backdate_file_mtime(&session.yaml_path, Duration::from_secs(10 * 86_400)).unwrap();

        let stats = run_cleanup(&config, 0).await;

        assert_eq!(stats, CleanupStats::default());
        assert!(session.yaml_path.exists());
        assert!(session.attachments_dir.exists());
        assert!(session.attachment_file.exists());
    }

    #[tokio::test]
    async fn cleanup_removes_stale_session_yaml_and_attachments() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let days = 1_u64;
        let session = create_session_fixture(tmp.path(), "stale", b"attached payload").unwrap();
        backdate_file_mtime(&session.yaml_path, Duration::from_secs(days * 86_400 + 600)).unwrap();

        let stats = run_cleanup(&config, days).await;

        assert_eq!(stats.sessions_removed, 1);
        assert!(stats.bytes_freed > 0);
        assert!(!session.yaml_path.exists());
        assert!(!session.attachments_dir.exists());
        assert!(!session.attachment_file.exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_fresh_session() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let session = create_session_fixture(tmp.path(), "fresh", b"fresh attachment").unwrap();

        let stats = run_cleanup(&config, 1).await;

        assert_eq!(stats, CleanupStats::default());
        assert!(session.yaml_path.exists());
        assert!(session.attachments_dir.exists());
        assert!(session.attachment_file.exists());
    }

    #[tokio::test]
    async fn cleanup_uses_real_mtime_boundary_for_old_vs_recent_sessions() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let days = 1_u64;
        let threshold = Duration::from_secs(days * 86_400);
        let stale = create_session_fixture(tmp.path(), "stale-boundary", b"stale-bytes").unwrap();
        let fresh = create_session_fixture(tmp.path(), "fresh-boundary", b"fresh-bytes").unwrap();

        backdate_file_mtime(&stale.yaml_path, threshold + Duration::from_secs(180)).unwrap();
        backdate_file_mtime(&fresh.yaml_path, threshold - Duration::from_secs(180)).unwrap();

        let stats = run_cleanup(&config, days).await;

        assert_eq!(stats.sessions_removed, 1);
        assert!(stats.bytes_freed > 0);
        assert!(!stale.yaml_path.exists());
        assert!(!stale.attachments_dir.exists());
        assert!(fresh.yaml_path.exists());
        assert!(fresh.attachments_dir.exists());
        assert!(fresh.attachment_file.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_does_not_follow_symlinked_attachments_contents() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let days = 1_u64;
        let session = create_session_fixture(tmp.path(), "symlinked-attachments", b"tiny").unwrap();
        let outside_dir = tmp.path().join("outside");
        stdfs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("outside.bin");
        stdfs::write(&outside_file, vec![7_u8; 4096]).unwrap();
        stdfs::remove_file(&session.attachment_file).unwrap();
        symlink(&outside_file, &session.attachment_file).unwrap();
        backdate_file_mtime(&session.yaml_path, Duration::from_secs(days * 86_400 + 600)).unwrap();

        let expected = stdfs::metadata(&session.yaml_path).unwrap().len();
        let stats = run_cleanup(&config, days).await;

        assert_eq!(stats.sessions_removed, 1);
        assert_eq!(stats.bytes_freed, expected);
        assert!(!session.yaml_path.exists());
        assert!(!session.attachments_dir.exists());
        assert!(outside_file.exists());
    }

    #[tokio::test]
    async fn cleanup_missing_sessions_dir_returns_empty_stats() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("missing-sessions-dir");
        let config = test_config(&sessions_dir);

        let stats = run_cleanup(&config, 1).await;

        assert_eq!(stats, CleanupStats::default());
        assert!(!sessions_dir.exists());
    }

    #[tokio::test]
    async fn cleanup_tolerates_missing_attachments_dir_for_stale_session() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let days = 1_u64;
        let session =
            create_session_fixture(tmp.path(), "missing-attachments", b"to-delete").unwrap();
        stdfs::remove_dir_all(&session.attachments_dir).unwrap();
        backdate_file_mtime(&session.yaml_path, Duration::from_secs(days * 86_400 + 600)).unwrap();

        let stats = run_cleanup(&config, days).await;

        assert_eq!(stats.sessions_removed, 1);
        assert!(!session.yaml_path.exists());
        assert!(!session.attachments_dir.exists());
    }

    fn test_config(sessions_dir: &Path) -> GlobalConfig {
        let config = Config {
            sessions_dir_override: Some(sessions_dir.to_path_buf()),
            ..Config::default()
        };
        Arc::new(RwLock::new(config))
    }

    fn backdate_file_mtime(path: &Path, age: Duration) -> std::io::Result<()> {
        let modified = SystemTime::now() - age;
        set_file_mtime(path, FileTime::from_system_time(modified))
    }

    fn create_session_fixture(
        sessions_dir: &Path,
        id: &str,
        attachment_contents: &[u8],
    ) -> std::io::Result<SessionFixture> {
        stdfs::create_dir_all(sessions_dir)?;

        let yaml_path = sessions_dir.join(format!("{id}.yaml"));
        stdfs::write(&yaml_path, SESSION_HEADER)?;

        let attachments_dir = attachments_dir_for(&yaml_path);
        stdfs::create_dir_all(&attachments_dir)?;
        let attachment_file = attachments_dir.join("blob.bin");
        stdfs::write(&attachment_file, attachment_contents)?;

        Ok(SessionFixture {
            yaml_path,
            attachments_dir,
            attachment_file,
        })
    }

    struct SessionFixture {
        yaml_path: PathBuf,
        attachments_dir: PathBuf,
        attachment_file: PathBuf,
    }
}
