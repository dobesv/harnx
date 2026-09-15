use super::*;
use process_wrap::tokio::{CommandWrap, KillOnDrop, ProcessGroup};
use std::future::Future;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug)]
struct StubChild {
    term_error: Option<i32>,
    kill_result: io::Result<()>,
    wait_result: io::Result<Option<ExitStatus>>,
    calls: Mutex<Vec<&'static str>>,
}

impl StubChild {
    fn new(kill_result: io::Result<()>) -> Self {
        Self {
            term_error: None,
            kill_result,
            wait_result: Ok(None),
            calls: Mutex::default(),
        }
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }

    fn assert_calls(&self, expected: &[&str]) {
        assert_eq!(self.calls.lock().unwrap().as_slice(), expected);
    }
}

impl ChildWrapper for StubChild {
    fn inner(&self) -> &dyn ChildWrapper {
        self
    }

    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self
    }

    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        self
    }

    fn id(&self) -> Option<u32> {
        // Supply a stable Linux start time without sending any real signals.
        Some(std::process::id())
    }

    fn signal(&self, signal: i32) -> io::Result<()> {
        assert_eq!(signal, libc::SIGTERM);
        self.record("TERM");
        self.term_error
            .map_or(Ok(()), |code| Err(io::Error::from_raw_os_error(code)))
    }

    fn start_kill(&mut self) -> io::Result<()> {
        self.record("KILL");
        std::mem::replace(&mut self.kill_result, Ok(()))
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        panic!("cleanup must reap through wait, not group try_wait");
    }

    fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
        self.record("wait");
        let result = std::mem::replace(&mut self.wait_result, Ok(None));
        Box::pin(async move {
            match result? {
                Some(status) => Ok(status),
                None => std::future::pending().await,
            }
        })
    }
}

#[tokio::test]
async fn term_failure_still_escalates_without_reaping() -> anyhow::Result<()> {
    for code in [libc::EPERM, libc::ESRCH, libc::EIO] {
        let mut child = StubChild::new(Ok(()));
        child.term_error = Some(code);
        ProcessIdentity::capture(&child)
            .terminate(&mut child)
            .await?;
        child.assert_calls(&["TERM", "KILL"]);
    }
    Ok(())
}

#[tokio::test]
async fn already_dead_kill_errors_allow_owner_to_wait() -> anyhow::Result<()> {
    for error in [
        io::Error::from_raw_os_error(libc::ESRCH),
        io::Error::from_raw_os_error(libc::ECHILD),
        io::Error::from(io::ErrorKind::InvalidInput),
    ] {
        let mut child = StubChild::new(Err(error));
        ProcessIdentity::capture(&child)
            .terminate(&mut child)
            .await?;
        child.assert_calls(&["TERM", "KILL"]);
    }
    Ok(())
}

#[tokio::test]
async fn permission_error_is_benign_after_confirmed_exit() -> anyhow::Result<()> {
    for term_error in [None, Some(libc::EPERM)] {
        let mut child = StubChild::new(Err(io::Error::from_raw_os_error(libc::EPERM)));
        child.term_error = term_error;
        child.wait_result = Ok(Some(ExitStatus::from_raw(0)));
        ProcessIdentity::capture(&child)
            .terminate(&mut child)
            .await?;
        child.assert_calls(&["TERM", "KILL", "wait"]);
    }
    Ok(())
}

#[tokio::test]
async fn kill_errors_without_exit_evidence_are_not_hidden() {
    for code in [libc::EPERM, libc::EIO] {
        let mut child = StubChild::new(Err(io::Error::from_raw_os_error(code)));
        let error = ProcessIdentity::capture(&child)
            .terminate(&mut child)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(code)
        );
        child.assert_calls(&["TERM", "KILL", "wait"]);
    }
}

#[tokio::test]
async fn failed_exit_check_preserves_kill_error() {
    let mut child = StubChild::new(Err(io::Error::from_raw_os_error(libc::EPERM)));
    child.wait_result = Err(io::Error::from_raw_os_error(libc::EIO));
    let error = ProcessIdentity::capture(&child)
        .terminate(&mut child)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
        Some(libc::EPERM)
    );
    child.assert_calls(&["TERM", "KILL", "wait"]);
}

#[tokio::test]
async fn changed_identity_is_rejected_before_signalling() {
    let mut child = StubChild::new(Ok(()));
    let mut identity = ProcessIdentity::capture(&child);
    identity.pid = Some(std::process::id() + 1);
    assert!(identity.terminate(&mut child).await.is_err());
    child.assert_calls(&[]);
}

#[tokio::test]
async fn escalates_owned_group_and_rejects_changed_identity() -> anyhow::Result<()> {
    let mut command = CommandWrap::with_new("bash", |command| {
        command
            .args([
                "-c",
                "trap '' TERM; bash -c 'trap \"\" TERM; echo ready; read -r line'; :",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    });
    command.wrap(KillOnDrop).wrap(ProcessGroup::leader());
    let mut child = command.spawn()?;
    let identity = ProcessIdentity::capture(child.as_ref());
    let mut output = BufReader::new(child.stdout().take().unwrap()).lines();
    // The child has installed its TERM handler before this barrier.
    assert_eq!(output.next_line().await?.as_deref(), Some("ready"));
    #[cfg(target_os = "linux")]
    {
        let changed = ProcessIdentity {
            pid: identity.pid,
            started: Some("different-start-time".into()),
        };
        assert!(changed.terminate(child.as_mut()).await.is_err());
    }
    assert!(child.try_wait()?.is_none());
    tokio::time::timeout(Duration::from_secs(2), identity.terminate(child.as_mut())).await??;
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await??;
    assert!(!status.success());
    assert!(
        identity.terminate(child.as_mut()).await.is_err(),
        "reaped group IDs must never be reused for cleanup"
    );
    Ok(())
}

#[derive(Debug)]
struct FailedGroupKill(Box<dyn ChildWrapper>);

impl ChildWrapper for FailedGroupKill {
    fn inner(&self) -> &dyn ChildWrapper {
        self.0.as_ref()
    }

    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self.0.as_mut()
    }

    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        self.0
    }

    fn start_kill(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }
}

#[tokio::test]
async fn failed_group_kill_reaps_through_native_child() -> anyhow::Result<()> {
    let mut command = CommandWrap::with_new("bash", |command| {
        command
            .args(["-c", "echo ready; read -r line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    });
    // No KillOnDrop: this regression intentionally checks for a stale native PID.
    command.wrap(ProcessGroup::leader());
    let mut child = FailedGroupKill(command.spawn()?);
    let identity = ProcessIdentity::capture(&child);
    let mut output = BufReader::new(child.stdout().take().unwrap()).lines();
    assert_eq!(output.next_line().await?.as_deref(), Some("ready"));
    tokio::time::timeout(Duration::from_secs(2), identity.terminate(&mut child)).await??;
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await??;
    assert!(!status.success());
    assert_eq!(child.id(), None, "native child must also record the exit");
    Ok(())
}
