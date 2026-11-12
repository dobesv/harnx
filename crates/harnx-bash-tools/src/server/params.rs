// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

#[cfg(test)]
mod tests;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ExecCommandParams {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) env: Option<HashMap<String, String>>,
}

impl JsonSchema for ExecCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ExecCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        let env = generator.subschema_for::<Option<HashMap<String, String>>>();
        object_schema_with_desc(
            vec![
                ("command", "Bash command to execute. Avoid shell pipes like | head, | tail, | grep — use head_lines, tail_lines, max_output_bytes instead. For multi-line Python/Node/Ruby/etc. scripts, start the command with a shebang line (e.g. #!/usr/bin/env python3) and write the script body on subsequent lines — the correct interpreter will be used automatically.", command),
                ("working_dir", "Working directory for the command. Defaults to the project root.", working_dir),
                ("timeout_secs", "Kill the command after this many seconds, 1 to 86400. Defaults to 86400 (24 hours).", timeout_secs),
                ("head_lines", "Return only the first N lines of combined output. Prefer this over `| head -N` in the command.", head_lines),
                ("tail_lines", "Return only the last N lines of combined output. Prefer this over `| tail -N` in the command.", tail_lines),
                ("max_output_bytes", "Truncate output to this many bytes. Prefer this over `| head -c N` in the command.", max_output_bytes),
                ("env", "Additional environment variables for the command. Merged on top of the server's environment; per-call overrides only.", env),
            ],
            &["command"],
        )
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadExecLogParams {
    pub(crate) execution_id: String,
    pub(crate) stream: String,
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    #[serde(default)]
    pub(crate) tail: Option<usize>,
    #[serde(default)]
    pub(crate) grep: Option<String>,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
}

impl JsonSchema for ReadExecLogParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadExecLogParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let stream = generator.subschema_for::<String>();
        let offset = generator.subschema_for::<Option<usize>>();
        let limit = generator.subschema_for::<Option<usize>>();
        let tail = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema_with_desc(
            vec![
                (
                    "execution_id",
                    "The execution_id returned by exec or spawn.",
                    execution_id,
                ),
                ("stream", "'stdout' or 'stderr'.", stream),
                (
                    "offset",
                    "Skip the first N lines of the log (1-indexed).",
                    offset,
                ),
                ("limit", "Return at most N lines.", limit),
                ("tail", "Return only the last N lines. Can be combined with offset to tail from a starting line (skip to offset, then take the last N lines after it).", tail),
                ("grep", "Filter lines by regex before truncating.", grep),
                ("head_lines", "Return only the first N lines.", head_lines),
                ("tail_lines", "Return only the last N lines.", tail_lines),
                (
                    "max_output_bytes",
                    "Truncate output to this many bytes.",
                    max_output_bytes,
                ),
            ],
            &["execution_id", "stream"],
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct SpawnCommandParams {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,
    #[serde(default)]
    pub(crate) wait: Option<WaitOptions>,
}

impl JsonSchema for SpawnCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SpawnCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        let env = generator.subschema_for::<Option<HashMap<String, String>>>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let wait = generator.subschema_for::<Option<WaitOptions>>();
        object_schema_with_desc(
            vec![
                ("command", "Bash command to run in the background. Supports shebang lines: start the command with #!/usr/bin/env python3 (or node, ruby, etc.) to run a multi-line script with the correct interpreter instead of wrapping it in bash -c.", command),
                (
                    "working_dir",
                    "Working directory. Defaults to the project root.",
                    working_dir,
                ),
                (
                    "env",
                    "Additional environment variables for the command. Merged on top of the server's environment; per-call overrides only.",
                    env,
                ),
                ("timeout_secs", "Kill the process after this many seconds, 1 to 86400. Defaults to 86400 (24 hours). This is the process's own deadline, not a wait limit.", timeout_secs),
                ("wait", "Wait for completion or return partial output without stopping the process. Omit for immediate spawn-only behavior; {} waits up to 240 seconds by default. Use wait.forever to wait until the process exits.", wait),
            ],
            &["command"],
        )
    }
}

/// How long a wait may last. A wait never stops the process; it only bounds
/// how long the caller is held before getting a status snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitLimit {
    Seconds(u64),
    Forever,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitOptions {
    #[serde(default)]
    pub(crate) seconds: Option<u64>,
    #[serde(default)]
    pub(crate) forever: Option<bool>,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) grep: Option<String>,
}

/// Validate `seconds`/`forever`, naming `hint` in the out-of-range error.
fn wait_limit(
    seconds: Option<u64>,
    forever: Option<bool>,
    hint: &str,
) -> Result<WaitLimit, ErrorData> {
    match (seconds, forever == Some(true)) {
        (Some(_), true) => Err(ErrorData::invalid_params(
            "seconds and forever cannot be combined; pass only one",
            None,
        )),
        (None, true) => Ok(WaitLimit::Forever),
        (None, false) => Ok(WaitLimit::Seconds(WAIT_DEFAULT_SECS)),
        (Some(seconds), false) if (1..=WAIT_MAX_SECS).contains(&seconds) => {
            Ok(WaitLimit::Seconds(seconds))
        }
        (Some(_), false) => Err(ErrorData::invalid_params(hint.to_owned(), None)),
    }
}

impl WaitOptions {
    /// The limit of a wait nested in `spawn`.
    pub(crate) fn limit(&self) -> Result<WaitLimit, ErrorData> {
        wait_limit(
            self.seconds,
            self.forever,
            &format!(
                "wait.seconds must be between 1 and {WAIT_MAX_SECS}; omit wait to return immediately"
            ),
        )
    }

    fn schema_properties(
        generator: &mut SchemaGenerator,
    ) -> Vec<(&'static str, &'static str, Schema)> {
        let seconds = generator.subschema_for::<Option<u64>>();
        let forever = generator.subschema_for::<Option<bool>>();
        let mut properties = vec![
            ("seconds", "Seconds to wait before returning the current status and partial output without killing the process, 1 to 240. Defaults to 240. Cannot be combined with forever.", seconds),
            ("forever", "Wait until the process exits instead of for a fixed time. The process's own timeout_secs deadline still applies. Cannot be combined with seconds.", forever),
        ];
        properties.extend(Self::output_properties(generator));
        properties
    }

    /// The output-filtering properties shared by wait, spawn.wait and status.
    fn output_properties(
        generator: &mut SchemaGenerator,
    ) -> Vec<(&'static str, &'static str, Schema)> {
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        vec![
            ("head_lines", "Return only the first N lines of output. Prefer this over post-processing with head.", head_lines),
            ("tail_lines", "Return only the last N lines of output. Prefer this over post-processing with tail.", tail_lines),
            ("max_output_bytes", "Truncate output to this many bytes.", max_output_bytes),
            ("grep", "Filter output lines by regex.", grep),
        ]
    }
}

impl JsonSchema for WaitOptions {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WaitOptions")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        object_schema_with_desc(Self::schema_properties(generator), &[])
    }
}

/// `wait` declares its fields directly: serde cannot combine
/// `deny_unknown_fields` with `flatten`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitParams {
    pub(crate) execution_id: String,
    #[serde(default)]
    pub(crate) seconds: Option<u64>,
    #[serde(default)]
    pub(crate) forever: Option<bool>,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) grep: Option<String>,
}

impl WaitParams {
    pub(crate) fn options(&self) -> WaitOptions {
        WaitOptions {
            seconds: self.seconds,
            forever: self.forever,
            head_lines: self.head_lines,
            tail_lines: self.tail_lines,
            max_output_bytes: self.max_output_bytes,
            grep: self.grep.clone(),
        }
    }

    /// The limit of a `wait` call, whose error points at `bash_status`.
    pub(crate) fn limit(&self) -> Result<WaitLimit, ErrorData> {
        wait_limit(
            self.seconds,
            self.forever,
            &format!(
                "seconds must be between 1 and {WAIT_MAX_SECS}; call bash_status to check without waiting"
            ),
        )
    }
}

impl JsonSchema for WaitParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WaitParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let mut properties = vec![(
            "execution_id",
            "The execution_id returned by spawn.",
            execution_id,
        )];
        properties.extend(WaitOptions::schema_properties(generator));
        object_schema_with_desc(properties, &["execution_id"])
    }
}

/// `status` reports a spawned process's current state without waiting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusParams {
    pub(crate) execution_id: String,
    #[serde(default)]
    pub(crate) head_lines: Option<usize>,
    #[serde(default)]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default)]
    pub(crate) max_output_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) grep: Option<String>,
}

impl StatusParams {
    /// Output filters in the shape the wait path reads them.
    pub(crate) fn options(&self) -> WaitOptions {
        WaitOptions {
            head_lines: self.head_lines,
            tail_lines: self.tail_lines,
            max_output_bytes: self.max_output_bytes,
            grep: self.grep.clone(),
            ..WaitOptions::default()
        }
    }
}

impl JsonSchema for StatusParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("StatusParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let mut properties = vec![(
            "execution_id",
            "The execution_id returned by spawn.",
            execution_id,
        )];
        properties.extend(WaitOptions::output_properties(generator));
        object_schema_with_desc(properties, &["execution_id"])
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TerminateParams {
    pub(crate) execution_id: String,
    #[serde(default)]
    pub(crate) signal: Option<String>,
}

impl JsonSchema for TerminateParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("TerminateParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let signal = generator.subschema_for::<Option<String>>();
        object_schema_with_desc(
            vec![
                (
                    "execution_id",
                    "The execution_id returned by spawn.",
                    execution_id,
                ),
                (
                    "signal",
                    "Signal to send. One of: SIGTERM (default), SIGKILL, SIGINT, SIGHUP.",
                    signal,
                ),
            ],
            &["execution_id"],
        )
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RollbackParams {
    pub(crate) commit_id: String,
    pub(crate) repo_path: String,
}

impl JsonSchema for RollbackParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RollbackParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let commit_id = generator.subschema_for::<String>();
        let repo_path = generator.subschema_for::<String>();
        object_schema_with_desc(
            vec![
                (
                    "commit_id",
                    "The harnx snapshot commit SHA shown in a prior tool response diff header.",
                    commit_id,
                ),
                (
                    "repo_path",
                    "Absolute path to the git repository root to roll back.",
                    repo_path,
                ),
            ],
            &["commit_id", "repo_path"],
        )
    }
}
