//! Unix group cleanup for launchers, including a leader that already exited.

use super::*;

struct LauncherFixture {
    base: Fixture,
    descendant_log: PathBuf,
    descendant_lock: PathBuf,
}

impl LauncherFixture {
    fn new(overrides: Value) -> Result<Self> {
        let base = Fixture::new(overrides)?;
        Ok(Self {
            descendant_log: base.dir.path().join("descendant.pid"),
            descendant_lock: base.dir.path().join("descendant.lock"),
            base,
        })
    }

    async fn run(&self, flags: &[&str], exit_launcher: bool) -> Result<Output> {
        let mut command = self.base.command(flags, true);
        command.arg("--launcher-dir").arg(self.base.dir.path());
        if exit_launcher {
            command.arg("--launcher-exit");
        }
        let mut bridge = command.spawn()?;
        let launcher = wait_pid(&self.base.spawn_log, &mut bridge).await?;
        let mut cleanup = GroupCleanup(Some(launcher));
        let descendant = wait_pid(&self.descendant_log, &mut bridge).await?;
        assert_ne!(launcher, bridge.id().context("bridge PID")?);
        assert_ne!(launcher, descendant);
        assert_ne!(launcher, std::process::id());
        // A descendant in another group would not exercise managed group cleanup.
        assert_eq!(
            unsafe { libc::getpgid(descendant as libc::pid_t) },
            launcher as libc::pid_t
        );
        assert_held(&self.descendant_lock, descendant)?;
        if exit_launcher {
            wait_released(&self.base.lifetime_lock, launcher).await?;
            // The server must still be alive after its launcher has exited.
            assert_held(&self.descendant_lock, descendant)?;
        } else {
            assert_held(&self.base.lifetime_lock, launcher)?;
        }
        std::fs::write(&self.base.gate, "start")?;
        let output = tokio::time::timeout(DEADLINE, bridge.wait_with_output()).await??;
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("child pid {launcher}")));
        wait_released(&self.descendant_lock, descendant).await?;
        wait_released(&self.base.lifetime_lock, launcher).await?;
        cleanup.0 = None;
        Ok(output)
    }
}

// Failure-only cleanup for this fixture's recorded group, never a kill-as-success
// assertion or a process-name scan. Disarmed only after both lifetime proofs pass.
struct GroupCleanup(Option<u32>);
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

async fn wait_pid(path: &Path, bridge: &mut tokio::process::Child) -> Result<u32> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    anyhow::ensure!(pid > 0, "invalid fixture PID");
                    return Ok(pid);
                }
            }
            anyhow::ensure!(
                bridge.try_wait()?.is_none(),
                "bridge exited before {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

fn assert_held(path: &Path, pid: u32) -> Result<()> {
    let lock = open_lock(path)?;
    assert!(
        matches!(lock.try_lock(), Err(TryLockError::WouldBlock)),
        "fixture {pid} was not alive"
    );
    Ok(())
}

async fn wait_released(path: &Path, pid: u32) -> Result<()> {
    let lock = open_lock(path)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match lock.try_lock() {
                Ok(()) => {
                    lock.unlock()?;
                    return Ok::<_, anyhow::Error>(());
                }
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    })
    .await
    .with_context(|| format!("fixture process {pid} survived retirement"))?
}

async fn assert_call_cleanup(is_error: bool) -> Result<()> {
    for exit_launcher in [false, true] {
        let expected = mixed_result(is_error);
        let fixture = LauncherFixture::new(json!({"call_result":expected}))?;
        let output = fixture
            .run(&["--call-tool", "mixed_tool"], exit_launcher)
            .await?;
        assert_json_result(&output, &expected, !is_error)?;
        fixture.base.assert_initialized_call(&json!({}))?;
    }
    Ok(())
}

#[tokio::test]
async fn launcher_and_descendant_retired_after_success_even_if_launcher_exited() -> Result<()> {
    assert_call_cleanup(false).await
}

#[tokio::test]
async fn launcher_and_descendant_retired_after_tool_error_even_if_launcher_exited() -> Result<()> {
    assert_call_cleanup(true).await
}

#[tokio::test]
async fn launcher_descendant_retired_on_constructor_error_listing_and_invocation_errors(
) -> Result<()> {
    for exit_launcher in [false, true] {
        for (overrides, flags, status) in [
            (json!({}), vec!["--list-tools"], 0),
            (
                json!({"initialize_error":{"code":-32602,"message":"initialize sentinel"}}),
                vec!["--call-tool", "mixed_tool"],
                1,
            ),
            (json!({}), vec!["--call-tool", "unknown_tool"], 1),
            (
                json!({"call_error":{"code":-32602,"message":"invocation sentinel"}}),
                vec!["--call-tool", "mixed_tool"],
                1,
            ),
        ] {
            let fixture = LauncherFixture::new(overrides)?;
            let output = fixture.run(&flags, exit_launcher).await?;
            assert_eq!(output.status.code(), Some(status));
            if status == 1 {
                assert!(output.stdout.is_empty(), "error fabricated a tool result");
            } else {
                assert!(String::from_utf8_lossy(&output.stdout).contains("mixed_tool"));
            }
        }
    }
    Ok(())
}
