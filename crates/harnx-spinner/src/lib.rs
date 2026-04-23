//! Terminal spinner + abortable-run helpers for harnx. Moved from
//! `crates/harnx/src/utils/spinner.rs` in Plan 44c (2026-04-22). See
//! `docs/superpowers/specs/2026-04-21-frontend-crate-splits-design.md`.

use harnx_core::abort::{wait_abort_signal, AbortSignal};

use anyhow::{bail, Result};
use crossterm::{cursor, queue, style, terminal};
use std::{
    future::Future,
    io::{stdout, Write},
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver},
        oneshot,
    },
    time::interval,
};

fn is_stdout_terminal() -> bool {
    use std::io::IsTerminal;
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::io::stdout().is_terminal())
}

fn poll_abort_signal(abort_signal: &AbortSignal) -> anyhow::Result<bool> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    if crossterm::event::poll(Duration::from_millis(25))? {
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                    abort_signal.set_ctrlc();
                    return Ok(true);
                }
                KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                    abort_signal.set_ctrld();
                    return Ok(true);
                }
                _ => {}
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Default)]
pub struct SpinnerInner {
    index: usize,
    message: String,
}

impl SpinnerInner {
    const DATA: [&'static str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    fn step(&mut self) -> Result<()> {
        if !is_stdout_terminal() || self.message.is_empty() {
            return Ok(());
        }
        let mut writer = stdout();
        let frame = Self::DATA[self.index % Self::DATA.len()];
        let line = format!("{frame}{}", self.message);
        queue!(writer, cursor::MoveToColumn(0), style::Print(line),)?;
        if self.index == 0 {
            queue!(writer, cursor::Hide)?;
        }
        writer.flush()?;
        self.index += 1;
        Ok(())
    }

    fn set_message(&mut self, message: String) -> Result<()> {
        self.clear_message()?;
        if !message.is_empty() {
            self.message = format!(" {message}");
        }
        Ok(())
    }

    fn clear_message(&mut self) -> Result<()> {
        if !is_stdout_terminal() || self.message.is_empty() {
            return Ok(());
        }
        self.message.clear();
        let mut writer = stdout();
        queue!(
            writer,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown),
            cursor::Show
        )?;
        writer.flush()?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Spinner(mpsc::UnboundedSender<SpinnerEvent>);

impl Spinner {
    pub fn create(message: &str) -> (Self, UnboundedReceiver<SpinnerEvent>) {
        let (tx, spinner_rx) = mpsc::unbounded_channel();
        let spinner = Spinner(tx);
        let _ = spinner.set_message(message.to_string());
        (spinner, spinner_rx)
    }

    pub fn set_message(&self, message: String) -> Result<()> {
        self.0.send(SpinnerEvent::SetMessage(message))?;
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.0.send(SpinnerEvent::Stop);
        std::thread::sleep(Duration::from_millis(10));
    }

    /// Clear the spinner display without terminating the background task.
    /// The spinner can be resumed later with `set_message()`.
    pub fn pause(&self) {
        let _ = self.0.send(SpinnerEvent::Pause);
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub enum SpinnerEvent {
    SetMessage(String),
    /// Clear spinner and terminate the background task.
    Stop,
    /// Clear spinner display but keep the background task alive.
    Pause,
}

pub fn spawn_spinner(message: &str) -> Spinner {
    let (spinner, mut spinner_rx) = Spinner::create(message);
    tokio::spawn(async move {
        let mut spinner = SpinnerInner::default();
        let mut interval = interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                evt = spinner_rx.recv() => {
                    if let Some(evt) = evt {
                        match evt {
                            SpinnerEvent::SetMessage(message) => {
                                spinner.set_message(message)?;
                            }
                            SpinnerEvent::Stop => {
                                spinner.clear_message()?;
                                break;
                            }
                            SpinnerEvent::Pause => {
                                spinner.clear_message()?;
                            }
                        }

                    }
                }
                _ = interval.tick() => {
                    let _ = spinner.step();
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    spinner
}

pub async fn abortable_run_with_spinner<F, T>(
    task: F,
    message: &str,
    abort_signal: AbortSignal,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let (_, spinner_rx) = Spinner::create(message);
    abortable_run_with_spinner_rx(task, spinner_rx, abort_signal).await
}

pub async fn abortable_run_with_spinner_rx<F, T>(
    task: F,
    spinner_rx: UnboundedReceiver<SpinnerEvent>,
    abort_signal: AbortSignal,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if is_stdout_terminal() {
        let (done_tx, done_rx) = oneshot::channel();
        let run_task = async {
            tokio::select! {
                ret = task => {
                    let _ = done_tx.send(());
                    ret
                }
                _ = tokio::signal::ctrl_c() => {
                    abort_signal.set_ctrlc();
                    let _ = done_tx.send(());
                    bail!("Aborted!")
                },
                _ = wait_abort_signal(&abort_signal) => {
                    let _ = done_tx.send(());
                    bail!("Aborted.");
                },
            }
        };
        let (task_ret, spinner_ret) = tokio::join!(
            run_task,
            run_abortable_spinner(spinner_rx, done_rx, abort_signal.clone())
        );
        spinner_ret?;
        task_ret
    } else {
        task.await
    }
}

async fn run_abortable_spinner(
    mut spinner_rx: UnboundedReceiver<SpinnerEvent>,
    mut done_rx: oneshot::Receiver<()>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let mut spinner = SpinnerInner::default();
    loop {
        if abort_signal.aborted() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;

        match done_rx.try_recv() {
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                break;
            }
            _ => {}
        }

        match spinner_rx.try_recv() {
            Ok(SpinnerEvent::SetMessage(message)) => {
                spinner.set_message(message)?;
            }
            Ok(SpinnerEvent::Stop) | Ok(SpinnerEvent::Pause) => {
                spinner.clear_message()?;
            }
            Err(_) => {}
        }

        if poll_abort_signal(&abort_signal)? {
            break;
        }

        spinner.step()?;
    }

    spinner.clear_message()?;
    Ok(())
}
