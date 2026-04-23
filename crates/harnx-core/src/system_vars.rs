//! System-level variable interpolation (`{{__os__}}`, `{{__shell__}}`, …)
//! and the host-shell detection used by prompt templates.
//!
//! Pure utilities that read process environment / OS info; no harnx
//! runtime state. Used by `AgentConfig::from_markdown` /
//! `AgentConfig::interpolated_instructions` and by harnx-side shell
//! command dispatch.

use fancy_regex::{Captures, Regex};
use std::env;
use std::path::Path;
use std::sync::LazyLock;

/// Regex matching `{{variable_name}}` placeholders with word-character keys.
pub static RE_VARIABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{(\w+)\}\}").unwrap());

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
    pub fn new(name: &str, cmd: &str, arg: &str) -> Self {
        Self {
            name: name.to_string(),
            cmd: cmd.to_string(),
            arg: arg.to_string(),
        }
    }
}

/// Detect the host shell from `HARNX_SHELL` / `PSModulePath` / `SHELL`.
pub fn detect_shell() -> Shell {
    let cmd = env::var("HARNX_SHELL").ok().or_else(|| {
        if cfg!(windows) {
            if let Ok(ps_module_path) = env::var("PSModulePath") {
                let ps_module_path = ps_module_path.to_lowercase();
                if ps_module_path.starts_with(r"c:\users") {
                    if ps_module_path.contains(r"\powershell\7\") {
                        return Some("pwsh.exe".to_string());
                    } else {
                        return Some("powershell.exe".to_string());
                    }
                }
            }
            None
        } else {
            env::var("SHELL").ok()
        }
    });
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
        "powershel" => "-Command",
        "cmd" => "/C",
        _ => "-c",
    };
    Shell::new(name, cmd, shell_arg)
}

/// Current local time, formatted RFC-3339 with second precision.
pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Replace `{{__os__}}`, `{{__os_distro__}}`, `{{__os_family__}}`,
/// `{{__arch__}}`, `{{__shell__}}`, `{{__locale__}}`, `{{__now__}}`, and
/// `{{__cwd__}}` placeholders in `text` with their runtime-resolved values.
/// Unknown placeholders (e.g. user variables) are left untouched.
pub fn interpolate_variables(text: &mut String) {
    *text = RE_VARIABLE
        .replace_all(text, |caps: &Captures<'_>| {
            let key = &caps[1];
            match key {
                "__os__" => env::consts::OS.to_string(),
                "__os_distro__" => {
                    let info = os_info::get();
                    if env::consts::OS == "linux" {
                        format!("{info} (linux)")
                    } else {
                        info.to_string()
                    }
                }
                "__os_family__" => env::consts::FAMILY.to_string(),
                "__arch__" => env::consts::ARCH.to_string(),
                "__shell__" => SHELL.name.clone(),
                "__locale__" => sys_locale::get_locale().unwrap_or_default(),
                "__now__" => now(),
                "__cwd__" => env::current_dir()
                    .map(|v| v.display().to_string())
                    .unwrap_or_default(),
                _ => format!("{{{{{key}}}}}"),
            }
        })
        .to_string();
}
