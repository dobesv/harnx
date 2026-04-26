//! System-level variable interpolation (`{{__os__}}`, `{{__shell__}}`, …)
//! and the host-shell detection used by prompt templates.
//!
//! Pure utilities that read process environment / OS info; no harnx
//! runtime state. Used by `AgentConfig::from_markdown` /
//! `AgentConfig::interpolated_instructions` and by harnx-side shell
//! command dispatch.

use crate::agent_config::AgentConfig;
use anyhow::Result;
use minijinja::{Environment, UndefinedBehavior, Value};
use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::sync::LazyLock;

/// The host shell detected from the environment at startup.
pub static SHELL: LazyLock<Shell> = LazyLock::new(detect_shell);

/// Describes a host shell: its canonical name plus the `cmd arg "<script>"`
/// invocation shape used to run inline commands.
pub struct Shell {
    pub name: String,
    pub cmd: String,
    pub arg: String,
}

impl Shell {
    fn new(name: &str, cmd: &str, arg: &str) -> Self {
        Self {
            name: name.into(),
            cmd: cmd.into(),
            arg: arg.into(),
        }
    }
}

#[derive(serde::Serialize)]
struct AgentContext<'a> {
    name: &'a str,
    model: Option<&'a str>,
    model_fallbacks: &'a [String],
    temperature: Option<f64>,
    top_p: Option<f64>,
    use_tools: Option<Vec<String>>,
    documents: &'a [String],
    agent_default_session: Option<&'a str>,
    compaction_agent: Option<&'a str>,
    conversation_starters: &'a [String],
}

pub fn detect_shell() -> Shell {
    let cmd = LazyLock::force(&SHELL_CMD).clone();
    let name = cmd
        .as_ref()
        .and_then(|v| Path::new(v).file_stem().and_then(|v| v.to_str()))
        .map(|v| {
            if v == "nu" {
                "nushell".into()
            } else {
                v.to_lowercase()
            }
        });
    let (cmd, name) = match (cmd.as_deref(), name.as_deref()) {
        (Some(cmd), Some(name)) => (cmd, name),
        _ => {
            if cfg!(windows) {
                ("cmd.exe", "cmd")
            } else {
                ("/bin/sh", "sh")
            }
        }
    };
    let shell_arg = match name {
        "powershell" => "-Command",
        "cmd" => "/C",
        _ => "-c",
    };
    Shell::new(name, cmd, shell_arg)
}

static SHELL_CMD: LazyLock<Option<String>> = LazyLock::new(|| {
    if cfg!(windows) {
        // Check for PowerShell before falling back to COMSPEC (which usually
        // points to cmd.exe). Prefer pwsh (PowerShell 7+) over powershell.exe
        // (Windows PowerShell 5.x), and only use COMSPEC if neither is found.
        if let Ok(program_files) = env::var("ProgramFiles") {
            if Path::new(&format!(r"{}\PowerShell\7\pwsh.exe", program_files)).exists() {
                return Some("pwsh.exe".to_string());
            }
        }
        if Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe").exists() {
            return Some("powershell.exe".to_string());
        }
        env::var("COMSPEC").ok()
    } else {
        env::var("SHELL").ok()
    }
});

/// Current local time, formatted RFC-3339 with second precision.
pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub fn render_template(template: &str, agent: &AgentConfig) -> Result<String> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let agent_ctx = AgentContext {
        name: agent.name(),
        model: agent.model_id(),
        model_fallbacks: agent.model_fallbacks(),
        temperature: agent.temperature(),
        top_p: agent.top_p(),
        use_tools: agent.use_tools(),
        documents: agent.documents(),
        agent_default_session: agent.agent_default_session(),
        compaction_agent: agent.compaction_agent(),
        conversation_starters: agent.conversation_staters(),
    };

    let locale = sys_locale::get_locale().unwrap_or_default();
    let current_time = now();
    let current_dir = env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let os_distro = {
        let info = os_info::get();
        if env::consts::OS == "linux" {
            format!("{info} (linux)")
        } else {
            info.to_string()
        }
    };

    let mut ctx: BTreeMap<String, Value> = BTreeMap::new();
    ctx.insert("__os__".to_string(), Value::from(env::consts::OS));
    ctx.insert(
        "__os_family__".to_string(),
        Value::from(env::consts::FAMILY),
    );
    ctx.insert("__arch__".to_string(), Value::from(env::consts::ARCH));
    ctx.insert("__shell__".to_string(), Value::from(SHELL.name.clone()));
    ctx.insert("__locale__".to_string(), Value::from(locale));
    ctx.insert("__now__".to_string(), Value::from(current_time));
    ctx.insert("__cwd__".to_string(), Value::from(current_dir));
    ctx.insert("__os_distro__".to_string(), Value::from(os_distro));
    ctx.insert("agent".to_string(), Value::from_serialize(&agent_ctx));

    for (k, v) in agent.variables() {
        ctx.insert(k.clone(), Value::from(v.clone()));
    }

    env.render_str(template, ctx)
        .map_err(|e| anyhow::anyhow!("Template error in agent '{}': {}", agent.name(), e))
}
