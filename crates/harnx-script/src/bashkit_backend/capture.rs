use bashkit::ExecResult;

use crate::ScriptLimits;

pub(super) struct CapturedStream {
    pub text: String,
    pub truncated: bool,
    cap: usize,
}

impl CapturedStream {
    fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            truncated: false,
            cap,
        }
    }

    fn append(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let remaining = self.cap.saturating_sub(self.text.len());
        let mut end = text.len().min(remaining);
        self.truncated |= end < text.len();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
    }

    fn replace(&mut self, text: &str, truncated: bool) {
        self.text.clear();
        self.truncated = false;
        self.append(text);
        self.truncated |= truncated;
    }
}

pub(super) struct Capture {
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    pub exit_code: i32,
    pub error: Option<String>,
}

impl Capture {
    pub fn new(limits: &ScriptLimits) -> Self {
        Self {
            stdout: CapturedStream::new(limits.max_stdout_bytes),
            stderr: CapturedStream::new(limits.max_stderr_bytes),
            exit_code: 0,
            error: None,
        }
    }

    pub fn record_error(&mut self, message: String) {
        self.append("", &format!("{message}\n"));
        self.error = Some(message);
    }

    pub fn output_limit(&self) -> Option<String> {
        (self.stdout.truncated || self.stderr.truncated)
            .then(|| "output capture limit exceeded".to_owned())
    }

    pub fn append(&mut self, stdout: &str, stderr: &str) {
        self.stdout.append(stdout);
        self.stderr.append(stderr);
    }

    pub fn replace(&mut self, result: ExecResult) {
        self.stdout
            .replace(&result.stdout.text_lossy(), result.stdout_truncated);
        self.stderr
            .replace(&result.stderr.text_lossy(), result.stderr_truncated);
        self.exit_code = result.exit_code;
    }
}

#[cfg(test)]
mod tests {
    use super::CapturedStream;

    #[test]
    fn capture_keeps_utf8_prefix_and_ignores_later_chunks_after_truncation() {
        let mut stream = CapturedStream::new(5);
        stream.append("ééé");
        stream.append("later");
        assert_eq!(stream.text, "éé");
        assert!(stream.truncated);
        stream.replace("final", true);
        assert_eq!(stream.text, "final");
        assert!(stream.truncated);
        stream.replace("ok", false);
        assert_eq!(stream.text, "ok");
        assert!(!stream.truncated);
    }

    #[test]
    fn zero_capture_cap_retains_nothing() {
        let mut stream = CapturedStream::new(0);
        stream.append("data");
        assert!(stream.text.is_empty());
        assert!(stream.truncated);
    }
}
