//! A Python prelude is prepended to the script as text, so BashKit reports
//! every line number too high by the prelude's length. This module wraps the
//! Python builtin and puts the numbers back.

use async_trait::async_trait;
use bashkit::{Builtin, BuiltinContext, ExecResult, ExecutionPlan};

/// Name shown for a traceback frame that lies inside the prelude.
const PRELUDE_FILE: &str = "<harnx tools>";

/// The Python builtin plus a prelude; failed runs report the script's own lines.
pub(super) struct PreludePython {
    inner: bashkit::Python,
    shift: usize,
}

/// The file name BashKit gives the main script's frames, from the builtin's
/// arguments: `-c` is `<string>`, a script path is the path as written, and
/// stdin (`-` or no argument) is `<stdin>`.
fn main_script_name(args: &[String]) -> String {
    match args.first().map(String::as_str) {
        Some("-c") => "<string>".to_owned(),
        Some(path) if path != "-" => path.to_owned(),
        _ => "<stdin>".to_owned(),
    }
}

impl PreludePython {
    pub(super) fn new(inner: bashkit::Python, prelude: &str) -> Self {
        Self {
            inner,
            // BashKit joins the prelude and the script with one newline, so
            // the script starts on the line after the prelude's last newline.
            shift: prelude.matches('\n').count() + 1,
        }
    }
}

#[async_trait]
impl Builtin for PreludePython {
    fn llm_hint(&self) -> Option<&'static str> {
        self.inner.llm_hint()
    }

    fn streams_io(&self) -> bool {
        self.inner.streams_io()
    }

    fn reset_session_state(&self) {
        self.inner.reset_session_state();
    }

    async fn execution_plan(
        &self,
        ctx: &BuiltinContext<'_>,
    ) -> bashkit::Result<Option<ExecutionPlan>> {
        self.inner.execution_plan(ctx).await
    }

    async fn execute(&self, ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        let script_name = main_script_name(ctx.args);
        let result = self.inner.execute(ctx).await?;
        Ok(with_own_lines(result, &script_name, self.shift))
    }
}

fn with_own_lines(mut result: ExecResult, script_name: &str, shift: usize) -> ExecResult {
    if result.exit_code != 0 {
        let rewritten = own_line_numbers(&result.stderr.text_lossy(), script_name, shift);
        result.stderr = with_future_hint(rewritten).into();
    }
    result
}

/// What the interpreter says when a `from __future__` import is not the first
/// statement of the module it parsed.
const FUTURE_IMPORT_ERROR: &str = "from __future__ imports must occur at the beginning of the file";

/// The line appended to that error: the prelude comes first, so a script that
/// can call tools cannot start with such an import.
const FUTURE_IMPORT_HINT: &str =
    "python: a script that can call tools cannot use \"from __future__\" imports; remove that line";

/// BashKit parses the prelude and the script as one module, so a script's
/// `from __future__` import is never first. The script's source cannot be
/// reordered from here, so the failure says what is wrong instead. The hint
/// is keyed on the interpreter's wording; if that changes, the hint stops
/// appearing and the interpreter's own message is all that is left.
fn with_future_hint(mut stderr: String) -> String {
    if stderr.contains(FUTURE_IMPORT_ERROR) {
        if !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(FUTURE_IMPORT_HINT);
        stderr.push('\n');
    }
    stderr
}

/// stderr text with the prelude's `shift` lines taken off every
/// `File "<script_name>", line N` (N > shift), and prelude frames renamed.
/// Frames from any other file, such as an `exec` snippet whose lines count
/// from its own start, pass through untouched.
///
/// With `python3 -c` the script and an `exec` snippet are both `<string>`,
/// so their frames cannot be told apart and the snippet's are rewritten too.
pub(super) fn own_line_numbers(stderr: &str, script_name: &str, shift: usize) -> String {
    stderr
        .split_inclusive('\n')
        .map(|line| rewrite_frame(line, script_name, shift).unwrap_or_else(|| line.to_owned()))
        .collect()
}

/// One traceback line split into the parts the rewrite keeps or changes.
struct FrameLine<'a> {
    indent: &'a str,
    file: &'a str,
    number: usize,
    /// Everything after the number: `, in name` and the line ending.
    tail: &'a str,
}

fn rewrite_frame(line: &str, script_name: &str, shift: usize) -> Option<String> {
    let frame = parse_frame(line).filter(|frame| frame.file == script_name)?;
    let (file, number) = if frame.number > shift {
        (frame.file, frame.number - shift)
    } else {
        (PRELUDE_FILE, frame.number)
    };
    Some(format!(
        "{}File \"{file}\", line {number}{}",
        frame.indent, frame.tail
    ))
}

fn parse_frame(line: &str) -> Option<FrameLine<'_>> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let rest = body.strip_prefix("File \"")?;
    let (file, rest) = rest.split_once("\", line ")?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, tail) = rest.split_at(digits);
    let name_and_ending = tail.trim_end_matches(['\r', '\n']);
    if !(name_and_ending.is_empty() || name_and_ending.starts_with(", in ")) {
        return None;
    }
    Some(FrameLine {
        indent,
        file,
        number: number.parse().ok()?,
        tail,
    })
}
