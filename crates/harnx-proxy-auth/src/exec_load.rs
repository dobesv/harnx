use std::process::Command;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    Success { stdout: String },
    ExitFailure,
    SpawnFailed,
}

pub trait ExecRunner {
    fn run(&self, command: &str) -> ExecOutcome;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ShellRunner;

impl ExecRunner for ShellRunner {
    fn run(&self, command: &str) -> ExecOutcome {
        match Command::new("sh").arg("-c").arg(command).output() {
            Ok(output) if output.status.success() => match String::from_utf8(output.stdout) {
                Ok(stdout) => ExecOutcome::Success { stdout },
                Err(_) => ExecOutcome::ExitFailure,
            },
            Ok(_) => ExecOutcome::ExitFailure,
            Err(_) => ExecOutcome::SpawnFailed,
        }
    }
}

pub fn parse_exec_specs(
    entries: &[String],
    existing_names: &[String],
) -> anyhow::Result<Vec<ExecSpec>> {
    let mut specs = Vec::new();
    let mut all_names = existing_names.to_vec();

    for entry in entries {
        let (name, command) = entry.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--load-exec must be in <name>=<command> form: {entry}")
        })?;
        crate::load::ensure_valid_loaded_var_name(name)?;
        all_names.push(name.to_owned());
        specs.push(ExecSpec {
            name: name.to_owned(),
            command: command.to_owned(),
        });
    }

    crate::load::ensure_unique_loaded_var_names(all_names.iter().map(String::as_str))?;
    Ok(specs)
}

pub fn map_exec_outcome(spec: &ExecSpec, outcome: ExecOutcome) -> Value {
    match outcome {
        ExecOutcome::Success { stdout } => {
            let value = trim_single_trailing_newline(stdout);
            if value.is_empty() {
                tracing::debug!(name = %spec.name, "--load-exec produced empty output; using null");
                Value::Null
            } else {
                Value::String(value)
            }
        }
        ExecOutcome::ExitFailure => {
            tracing::debug!(name = %spec.name, "--load-exec command failed; using null");
            Value::Null
        }
        ExecOutcome::SpawnFailed => {
            tracing::debug!(name = %spec.name, "--load-exec command could not be spawned; using null");
            Value::Null
        }
    }
}

pub fn run_exec_value<R: ExecRunner>(spec: &ExecSpec, runner: &R) -> Value {
    map_exec_outcome(spec, runner.run(&spec.command))
}

fn trim_single_trailing_newline(mut stdout: String) -> String {
    if stdout.ends_with("\r\n") {
        stdout.truncate(stdout.len() - 2);
    } else if stdout.ends_with('\n') {
        stdout.pop();
    }
    stdout
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{filter::JaqVars, sentinel::Sentinels};
    use serde_json::json;

    /// Test double for the [`ExecRunner`] seam: returns a canned outcome
    /// without spawning a real shell, so `run_exec_value` can be exercised
    /// deterministically.
    struct FakeRunner(ExecOutcome);

    impl ExecRunner for FakeRunner {
        fn run(&self, _command: &str) -> ExecOutcome {
            self.0.clone()
        }
    }

    fn sentinels() -> Sentinels {
        Sentinels {
            uuid_key: "11111111-2222-3333-4444-555555555555".to_owned(),
            base64_key: "QUJDREVGR0hJSktMTU5PUA==".to_owned(),
            url_base64_key: "QUJDREVGR0hJSktMTU5PUA".to_owned(),
            hex_key: "00112233445566778899aabbccddeeff".to_owned(),
            email: "fake@example.com".to_owned(),
        }
    }

    #[test]
    fn parse_exec_specs_parses_and_validates() {
        let specs = parse_exec_specs(&["token=printf foo=bar".into()], &[]).unwrap();
        assert_eq!(specs[0].name, "token");
        assert_eq!(specs[0].command, "printf foo=bar");
        assert!(parse_exec_specs(&["1bad=echo hi".into()], &[]).is_err());
        assert!(parse_exec_specs(&["fake_uuid_key=echo hi".into()], &[]).is_err());
        assert!(parse_exec_specs(&["cfg=echo hi".into()], &["cfg".into()]).is_err());
    }

    #[test]
    fn map_exec_outcome_trims_newline_and_preserves_internal_content() {
        let spec = ExecSpec {
            name: "token".into(),
            command: "printf hi".into(),
        };
        assert_eq!(
            map_exec_outcome(
                &spec,
                ExecOutcome::Success {
                    stdout: "hello\nworld\n".into()
                }
            ),
            Value::String("hello\nworld".into())
        );
        assert_eq!(
            map_exec_outcome(
                &spec,
                ExecOutcome::Success {
                    stdout: "hello\r\n".into()
                }
            ),
            Value::String("hello".into())
        );
    }

    #[test]
    fn map_exec_outcome_returns_null_on_failures_or_empty_output() {
        let spec = ExecSpec {
            name: "token".into(),
            command: "printf hi".into(),
        };
        assert_eq!(
            map_exec_outcome(&spec, ExecOutcome::ExitFailure),
            Value::Null
        );
        assert_eq!(
            map_exec_outcome(&spec, ExecOutcome::SpawnFailed),
            Value::Null
        );
        assert_eq!(
            map_exec_outcome(
                &spec,
                ExecOutcome::Success {
                    stdout: String::new()
                }
            ),
            Value::Null
        );
        assert_eq!(
            map_exec_outcome(
                &spec,
                ExecOutcome::Success {
                    stdout: "\n".into()
                }
            ),
            Value::Null
        );
    }

    #[test]
    fn real_shell_runner_happy_path_and_failure() {
        let spec = ExecSpec {
            name: "token".into(),
            command: "printf 'hi there'".into(),
        };
        assert_eq!(
            run_exec_value(&spec, &ShellRunner),
            Value::String("hi there".into())
        );

        let spec = ExecSpec {
            name: "token".into(),
            command: "exit 3".into(),
        };
        assert_eq!(run_exec_value(&spec, &ShellRunner), Value::Null);
    }

    #[test]
    fn run_exec_value_uses_injected_runner() {
        let spec = ExecSpec {
            name: "token".into(),
            command: "ignored".into(),
        };
        let ok = FakeRunner(ExecOutcome::Success {
            stdout: "secret\n".into(),
        });
        assert_eq!(run_exec_value(&spec, &ok), Value::String("secret".into()));

        let fail = FakeRunner(ExecOutcome::SpawnFailed);
        assert_eq!(run_exec_value(&spec, &fail), Value::Null);
    }

    #[test]
    fn exec_var_reachable_as_jaq_var() {
        let vars = JaqVars::new_from_values(
            &sentinels(),
            String::new(),
            vec![("myexec".to_owned(), json!("hello from exec"))],
        )
        .unwrap();
        let filter = crate::filter::compile_with_vars("$myexec", &vars).unwrap();
        let output = crate::filter::apply_filter_with_vars(&filter, Value::Null, &vars).unwrap();
        assert_eq!(output, json!("hello from exec"));
    }
}
