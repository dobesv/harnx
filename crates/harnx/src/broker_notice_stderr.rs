//! Best-effort stderr for broker startup, separate from command output.

use harnx_runtime::nats_local_server::{BrokerNoticeSink, BrokerWaitNotice};
use std::fs::File;
use std::io::{self, Write};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

pub(super) fn sink() -> BrokerNoticeSink {
    let writer = duplicate_stderr()
        .and_then(|mut stderr| start_writer(move |notice| write_notice(&mut stderr, notice)));
    match writer {
        Ok((tx, _thread)) => Arc::new(move |notice| {
            let _ = tx.try_send(notice);
        }),
        Err(_) => Arc::new(|_| {}),
    }
}

#[cfg(unix)]
fn duplicate_stderr() -> io::Result<File> {
    use std::os::fd::AsFd;
    // A separate handle avoids taking Rust's global stderr lock around I/O.
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(File::from)
}

#[cfg(windows)]
fn duplicate_stderr() -> io::Result<File> {
    use std::os::windows::io::AsHandle;
    std::io::stderr()
        .as_handle()
        .try_clone_to_owned()
        .map(File::from)
}

fn start_writer<T: Send + 'static>(
    mut write: impl FnMut(T) -> io::Result<()> + Send + 'static,
) -> io::Result<(mpsc::SyncSender<T>, JoinHandle<()>)> {
    let (tx, rx) = mpsc::sync_channel(0);
    // No queue: a switch to the TUI cannot replay pending stderr notices. A
    // slow write can occupy only this one thread, never a Tokio worker or a
    // blocking-pool task that runtime shutdown would have to join. Dropping
    // the last sender stops the writer as soon as any in-flight write returns.
    let thread = thread::Builder::new()
        .name("harnx-broker-notices".into())
        .spawn(move || {
            while let Ok(notice) = rx.recv() {
                if let Err(error) = write(notice) {
                    // Later cadence ticks may try a temporarily full pipe
                    // again; a broken output stays disabled.
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        break;
                    }
                }
            }
        })?;
    Ok((tx, thread))
}

#[cfg(unix)]
// qual:allow(complexity, unsafe) reason: "poll(2) is required to skip known-full stderr pipes without changing inherited descriptor flags"
fn stderr_writable(stderr: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: stderr.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: fd is a live pollfd for the file we own. Timeout zero never waits.
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::ErrorKind::BrokenPipe.into());
    }
    if fd.revents & libc::POLLOUT == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    Ok(())
}

fn write_notice(stderr: &mut File, notice: BrokerWaitNotice) -> io::Result<()> {
    // Skip a known-full pipe without changing descriptor flags inherited from
    // the shell. Another writer can race poll; that is why I/O stays on the
    // dedicated thread even here.
    #[cfg(unix)]
    stderr_writable(stderr)?;
    stderr.write_all(format!("{notice}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn blocked_writer_drops_notices_and_exits_when_released() {
        let budget = Duration::from_secs(5);
        let (entered, wait_for_write) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (tx, writer) = start_writer(move |notice| {
            entered.send(notice).unwrap();
            blocked.recv_timeout(budget).unwrap();
            Ok(())
        })
        .unwrap();

        let deadline = Instant::now() + budget;
        while let Err(error) = tx.try_send(1) {
            assert_eq!(error, mpsc::TrySendError::Full(1));
            assert!(Instant::now() < deadline, "writer did not start");
            thread::yield_now();
        }
        assert_eq!(wait_for_write.recv_timeout(budget).unwrap(), 1);
        for next in 2..=100 {
            assert_eq!(tx.try_send(next), Err(mpsc::TrySendError::Full(next)));
        }
        drop(tx);
        release.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(
            wait_for_write.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        );
    }
}
