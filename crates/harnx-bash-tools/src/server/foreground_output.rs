//! Pipe joins keep completed streams across a cancelled collection attempt.
use super::*;
use tokio::task::JoinHandle;

type PipeTask = JoinHandle<std::io::Result<Vec<u8>>>;
type PipeResult = Result<Vec<u8>, ErrorData>;

const ERROR_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) struct ForegroundOutput {
    stdout_task: PipeTask,
    stderr_task: PipeTask,
    stdout: Option<PipeResult>,
    stderr: Option<PipeResult>,
}

impl ForegroundOutput {
    pub fn new(stdout_task: PipeTask, stderr_task: PipeTask) -> Self {
        Self {
            stdout_task,
            stderr_task,
            stdout: None,
            stderr: None,
        }
    }

    pub async fn finish(&mut self) {
        collect_pipe(&mut self.stdout_task, &mut self.stdout, "stdout").await;
        collect_pipe(&mut self.stderr_task, &mut self.stderr, "stderr").await;
    }

    pub async fn finish_bounded(mut self) -> (String, String) {
        // Poll both streams so a stuck stdout doesn't hide completed stderr.
        let drain = async {
            tokio::join!(
                collect_pipe(&mut self.stdout_task, &mut self.stdout, "stdout"),
                collect_pipe(&mut self.stderr_task, &mut self.stderr, "stderr"),
            );
        };
        let _ = tokio::time::timeout(ERROR_DRAIN_TIMEOUT, drain).await;
        // Unfinished readers keep writing their log files. Don't let their EOF
        // or an I/O error delay or replace the original cleanup error.
        let collected = |pipe: Option<PipeResult>| {
            String::from_utf8_lossy(&pipe.and_then(Result::ok).unwrap_or_default()).into_owned()
        };
        (collected(self.stdout), collected(self.stderr))
    }

    pub fn into_strings(self) -> Result<(String, String), ErrorData> {
        let stdout = self.stdout.expect("stdout collected")?;
        let stderr = self.stderr.expect("stderr collected")?;
        Ok((
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        ))
    }
}

async fn collect_pipe(task: &mut PipeTask, bytes: &mut Option<PipeResult>, name: &str) {
    if bytes.is_none() {
        // Cache errors too: a completed JoinHandle cannot be polled again.
        *bytes = Some(join_pipe(task, name).await);
    }
}

#[cfg(test)]
mod tests;
