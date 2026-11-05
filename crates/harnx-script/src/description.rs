//! Model-facing script guide generated from the same catalog as command help.

use std::sync::OnceLock;

use crate::{commands::ToolSpec, help::synopsis, BashKitBackend, FsLimits, ScriptLimits};

mod examples;

const PURPOSE: &str = "Chain this agent's tool commands and process their results in one script/round trip. This is an in-process shell, not host Bash. Only tool commands can reach host services, through their normal permissions and hooks.\n\n";

const DIALECT: &str = "Dialect: Bash-compatible BashKit. Fresh VFS per run, cwd /tmp; /attachments is read-only at /attachments/<full media CID> (keep existing CID encoding). No direct host filesystem, network or subprocess access. Pipes, redirects, variables, loops and background jobs (& / wait) work. set -e is on by default, with POSIX-style exceptions for conditions, ! and non-final &&/|| commands. Pipelines use the last command's status unless you enable set -o pipefail. Use set +e to handle ordinary failures; fatal errors still stop the script.\n\n";

const PYTHON: &str = "Python: python and python3 are the same embedded Monty subset, not CPython. Use -c, a VFS script file or stdin/heredoc. open()/pathlib use the VFS. Simple classes work; inheritance, metaclasses, re and third-party imports don't. Python cannot call tools or start subprocesses/network requests; call tools in Bash and pass results via /tmp. A shared work budget caps scripts at ~95 Python calls; batch loops into one invocation.\n\n";

const ARGUMENTS: &str = "Tool arguments/discovery:\n\
- Property names become --kebab-case flags; strings/numbers take values, booleans use --flag / --no-flag, scalar arrays repeat flags, objects/complex values take quoted JSON. No positional arguments.\n\
- --json '<object>' supplies original property keys; --json - reads stdin. Flags override base keys regardless of order. Stdin is otherwise ignored by tool commands.\n\
- Output defaults to structuredContent JSON when present, otherwise text. -o json|text overrides rendering (not sent to the tool).\n\
- tools [quoted-glob] lists synopses; help <tool> [tool ...] or <tool> --help shows full schema-derived help without dispatch.\n\
- Tool exits: 0 success, 1 tool error, 2 usage, 3 needs approval (call directly instead), 4 blocked, 5 rejected, 127 unknown command. Run timeout: 124; cancellation: 130.\n\n";

const QUOTING: &str = "Quoting idioms:\n\
- Literal multiline argument, with expansion inside the heredoc disabled:\n\
```bash\n\
text=\"$(cat <<'EOF'\n\
literal $variables and \"quotes\"\n\
EOF\n\
)\"\n\
```\n\
- Read text with \"$(< /tmp/result.txt)\" (command substitution strips trailing newlines).\n\
- Batch arguments with xargs -n1; print shell-escaped values with printf '%q\\n' \"$text\".\n\
- Pass binary as base64 text: \"$(base64 -w0 /tmp/x.png)\".\n";

/// Describe the resolved inner catalog. Admission limits belong to the runtime,
/// so the caller supplies its configured concurrency and per-script call caps.
/// Shell/VFS/Python defaults come from the backend's own limit types.
pub fn bash_tool_description(
    tools: &[ToolSpec],
    max_concurrent_inner_calls: usize,
    max_inner_calls_per_script: usize,
) -> String {
    let mut tools: Vec<_> = tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut text = format!(
        "{PURPOSE}{DIALECT}Available builtins (tool commands below override same-named builtins):\n{}\n\n{PYTHON}Tool commands:\n",
        builtin_names()
    );
    for tool in &tools {
        text.push_str("- ");
        text.push_str(&synopsis(&tool.name, &tool.schema, &tool.description));
        text.push('\n');
    }
    text.push('\n');
    text.push_str(ARGUMENTS);
    text.push_str(QUOTING);
    if let Some(idiom) = examples::attachment_idiom(&tools) {
        text.push_str(&format!(
            "- Create a PNG attachment from VFS bytes: `{idiom}`.\n"
        ));
    }
    text.push_str(&examples::pipeline(&tools));
    text.push_str(&limits(
        max_concurrent_inner_calls,
        max_inner_calls_per_script,
    ));
    text
}

fn builtin_names() -> &'static str {
    static NAMES: OnceLock<String> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = BashKitBackend.builtin_names();
        names.sort();
        names.dedup();
        names.join(", ")
    })
}

fn limits(concurrent: usize, calls: usize) -> String {
    let script = ScriptLimits::default();
    let fs = FsLimits::default();
    format!(
        "\nDefaults/limits: timeout {}s (timeout_secs: 0 disables it in practice); {} shell commands; stdout {} and stderr {} captured, with truncation flags/limit_hit. head_lines, tail_lines and max_output_bytes only shape the returned report. VFS: {} per file, {} total. Attachment mount: {} per file, {} cached per run; listings are empty, so use known full CIDs. Inner tools: {concurrent} concurrent, {calls} admitted calls per script. Python: {} heap and print-buffer ceilings per invocation, bounded by remaining run time; MemoryError is stderr/exit 1, not a typed limit_hit. VM limits aren't process/RSS isolation.\n",
        script.timeout_secs,
        script.max_commands,
        bytes(script.max_stdout_bytes as u64),
        bytes(script.max_stderr_bytes as u64),
        bytes(fs.max_file_size),
        bytes(fs.max_total_bytes),
        bytes(fs.max_file_size),
        bytes(fs.max_total_bytes),
        bytes(script.max_python_memory_bytes as u64),
    )
}

fn bytes(bytes: u64) -> String {
    if bytes > 0 && bytes.is_multiple_of(1_000_000) {
        format!("{} MB", bytes / 1_000_000)
    } else if bytes > 0 && bytes.is_multiple_of(1_048_576) {
        format!("{} MiB", bytes / 1_048_576)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests;
