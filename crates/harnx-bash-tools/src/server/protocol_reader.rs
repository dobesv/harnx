//! Reads a protocol-mode template's stdout: every byte goes to the log file,
//! and each line that carries a `progress` object is forwarded as a live
//! progress update. Stdout never reaches the tool result.

use harnx_toolset::{ToolProgressHandle, ToolProgressPatch};
use serde_json::Value;
use tokio::fs::File as TokioFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub(crate) const MAX_PROTOCOL_LINE_BYTES: usize = 128 * 1024;

/// Turns one stdout line into a progress patch. Malformed lines are logged and
/// skipped so a chatty or buggy template cannot fail the call.
pub(crate) fn parse_progress_line(line: &[u8]) -> Option<ToolProgressPatch> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    let Ok(text) = std::str::from_utf8(line) else {
        log::warn!("skipping protocol line: not valid UTF-8");
        return None;
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            log::warn!("skipping protocol line: invalid JSON: {err}");
            return None;
        }
    };
    let Some(progress) = value.get("progress").and_then(Value::as_object) else {
        log::warn!("skipping protocol line: no `progress` object");
        return None;
    };
    let field = |name: &str| {
        progress
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let patch = ToolProgressPatch {
        title: field("title"),
        markdown: field("markdown"),
        ..Default::default()
    };
    (patch.title.is_some() || patch.markdown.is_some()).then_some(patch)
}

/// Splits a byte stream into lines without ever holding more than
/// `MAX_PROTOCOL_LINE_BYTES` of one line.
#[derive(Default)]
struct LineSplitter {
    pending: Vec<u8>,
    discarding: bool,
}

impl LineSplitter {
    fn feed(&mut self, mut chunk: &[u8], progress: &ToolProgressHandle) {
        while let Some(newline) = chunk.iter().position(|&b| b == b'\n') {
            self.push(&chunk[..newline]);
            self.finish_line(progress);
            chunk = &chunk[newline + 1..];
        }
        self.push(chunk);
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.discarding {
            return;
        }
        if self.pending.len() + bytes.len() > MAX_PROTOCOL_LINE_BYTES {
            log::warn!("skipping protocol line longer than {MAX_PROTOCOL_LINE_BYTES} bytes");
            self.pending = Vec::new();
            self.discarding = true;
            return;
        }
        self.pending.extend_from_slice(bytes);
    }

    fn finish_line(&mut self, progress: &ToolProgressHandle) {
        if !self.discarding {
            if let Some(patch) = parse_progress_line(&self.pending) {
                progress.update(patch);
            }
        }
        self.pending.clear();
        self.discarding = false;
    }

    fn finish(&mut self, progress: &ToolProgressHandle) {
        if !self.pending.is_empty() {
            self.finish_line(progress);
        }
    }
}

/// Copies `reader` to `writer` while forwarding progress lines. Returns an
/// empty buffer because protocol-mode stdout is not part of the result.
pub(crate) async fn read_protocol_pipe_to_file<R>(
    mut reader: R,
    mut writer: TokioFile,
    progress: ToolProgressHandle,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut splitter = LineSplitter::default();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read]).await?;
        splitter.feed(&buffer[..read], &progress);
    }
    splitter.finish(&progress);
    writer.flush().await?;
    writer.sync_all().await?;
    Ok(Vec::new())
}

#[cfg(test)]
pub(crate) mod test_support {
    use harnx_toolset::{ToolProgress, ToolProgressPatch};

    /// Progress sink that records every patch it receives.
    #[derive(Default)]
    pub(crate) struct Recorder(std::sync::Mutex<Vec<ToolProgressPatch>>);

    impl Recorder {
        pub(crate) fn patches(&self) -> Vec<ToolProgressPatch> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ToolProgress for Recorder {
        fn update(&self, patch: ToolProgressPatch) {
            self.0.lock().unwrap().push(patch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Recorder;
    use super::*;
    use std::sync::Arc;

    async fn read_all(input: &[u8]) -> (Vec<ToolProgressPatch>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let file = TokioFile::create(&path).await.unwrap();
        let recorder = Arc::new(Recorder::default());
        let handle = ToolProgressHandle::new(recorder.clone());
        let out = read_protocol_pipe_to_file(input, file, handle)
            .await
            .unwrap();
        assert!(out.is_empty());
        (recorder.patches(), std::fs::read(&path).unwrap())
    }

    fn titled(title: &str) -> ToolProgressPatch {
        ToolProgressPatch {
            title: Some(title.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn progress_line_sets_title_and_markdown() {
        let input = b"{\"progress\":{\"title\":\"T\",\"markdown\":\"[x](https://e/x)\"}}\n";
        let (patches, log) = read_all(input).await;
        assert_eq!(
            patches,
            vec![ToolProgressPatch {
                title: Some("T".into()),
                markdown: Some("[x](https://e/x)".into()),
                ..Default::default()
            }]
        );
        assert_eq!(log, input);
    }

    #[tokio::test]
    async fn progress_line_with_one_field_leaves_the_other_unset() {
        let (patches, _) = read_all(b"{\"progress\":{\"title\":\"T\"}}\n").await;
        assert_eq!(patches, vec![titled("T")]);
        assert!(patches[0].markdown.is_none());
    }

    #[tokio::test]
    async fn extra_progress_fields_are_ignored() {
        let input =
            b"{\"progress\":{\"title\":\"T\",\"status\":\"completed\",\"kind\":\"read\"}}\n";
        let (patches, _) = read_all(input).await;
        assert_eq!(patches, vec![titled("T")]);
    }

    #[tokio::test]
    async fn invalid_lines_are_skipped_and_later_lines_apply() {
        let input = b"not json\n[1,2]\n{\"notice\":{\"message\":\"m\"}}\n{\"progress\":\"x\"}\n{\"progress\":{\"title\":\"ok\"}}\n";
        let (patches, log) = read_all(input).await;
        assert_eq!(patches, vec![titled("ok")]);
        assert_eq!(log, input);
    }

    #[tokio::test]
    async fn final_line_without_newline_is_applied() {
        let (patches, _) = read_all(b"{\"progress\":{\"title\":\"last\"}}").await;
        assert_eq!(patches, vec![titled("last")]);
    }

    #[tokio::test]
    async fn crlf_lines_are_accepted() {
        let (patches, _) = read_all(b"{\"progress\":{\"title\":\"T\"}}\r\n").await;
        assert_eq!(patches, vec![titled("T")]);
    }

    #[tokio::test]
    async fn non_utf8_line_is_skipped() {
        let mut input = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(b"{\"progress\":{\"title\":\"ok\"}}\n");
        let (patches, log) = read_all(&input).await;
        assert_eq!(patches, vec![titled("ok")]);
        assert_eq!(log, input);
    }

    #[tokio::test]
    async fn overlong_line_is_skipped_and_reading_continues() {
        let mut input = b"{\"progress\":{\"title\":\"".to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_PROTOCOL_LINE_BYTES));
        input.extend_from_slice(b"\"}}\n{\"progress\":{\"title\":\"ok\"}}\n");
        let (patches, log) = read_all(&input).await;
        assert_eq!(patches, vec![titled("ok")]);
        assert_eq!(log, input);
    }
}
