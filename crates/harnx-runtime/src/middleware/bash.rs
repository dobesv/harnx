use super::frame::Frame;
use anyhow::{bail, ensure, Result};
use harnx_script::{ScriptCommand, ScriptCommandContext, ScriptCommandOutput, ScriptHostContext};
use serde_json::Value;
use std::sync::Arc;

pub(super) const OPERATIONS: &[&str] = &[
    "tool_input",
    "fetch",
    "next",
    "respond",
    "deny",
    "confirm",
    "answer",
    "save",
    "saved",
    "interact",
];

pub(super) fn commands(frame: &Arc<Frame>) -> Vec<Arc<dyn ScriptCommand>> {
    OPERATIONS
        .iter()
        .map(|name| {
            Arc::new(Command {
                name,
                frame: frame.clone(),
            }) as Arc<dyn ScriptCommand>
        })
        .collect()
}

struct Command {
    name: &'static str,
    frame: Arc<Frame>,
}

#[async_trait::async_trait]
impl ScriptCommand for Command {
    fn name(&self) -> &str {
        self.name
    }

    async fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        let host = ScriptHostContext {
            cancel: ctx.cancel,
            control: ctx.control,
        };
        match self.execute(args, stdin, &host).await {
            Ok(output) => Ok(output),
            Err(error) => Err(self.frame.fail(format!("{error:#}"), &host)),
        }
    }
}

impl Command {
    async fn execute(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        host: &ScriptHostContext,
    ) -> Result<ScriptCommandOutput> {
        match self.name {
            "confirm" => {
                let (args, id) = keyed_arguments(args)?;
                ensure!(!args.is_empty(), "confirm requires a message");
                let approved = self.frame.confirm(args.join(" "), id, host)?;
                Ok(ScriptCommandOutput {
                    exit_code: i32::from(approved != true),
                    ..Default::default()
                })
            }
            "answer" | "saved" => {
                ensure!(args.len() == 1, "{} requires one key", self.name);
                let value = if self.name == "answer" {
                    self.frame.answer(&args[0])?
                } else {
                    self.frame.saved(&args[0])?
                };
                Ok(optional_output(value))
            }
            "interact" => {
                let (args, id) = keyed_arguments(args)?;
                let text = json_text(args, stdin)?
                    .ok_or_else(|| anyhow::anyhow!("interact requires a surface JSON"))?;
                let message: Value = serde_json::from_str(text)?;
                let response = self.frame.interact(message, id, host)?;
                Ok(optional_output(Some(response)))
            }
            "save" => {
                ensure!(!args.is_empty(), "save requires a key and JSON value");
                let value = required_json(&args[1..], stdin)?;
                self.frame.save(args[0].clone(), value)?;
                Ok(ScriptCommandOutput::default())
            }
            _ => self
                .execute_value(args, stdin, host)
                .await
                .map(|value| optional_output(Some(value))),
        }
    }

    async fn execute_value(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        host: &ScriptHostContext,
    ) -> Result<Value> {
        match self.name {
            "fetch" => {
                ensure!(args.len() == 1, "fetch requires one HTTP(S) URL");
                self.frame.fetch(&args[0], host).await
            }
            "tool_input" => {
                ensure!(args.is_empty(), "tool_input takes no arguments");
                Ok(self.frame.input.clone())
            }
            "next" => {
                let (arguments, id) = next_arguments(args, stdin)?;
                self.frame.call_next(arguments, id, host).await
            }
            "respond" => self.frame.respond(required_json(args, stdin)?),
            "deny" => {
                ensure!(!args.is_empty(), "deny requires a reason");
                self.frame.deny(args.join(" "))
            }
            _ => bail!("unknown middleware operation"),
        }
    }
}

fn next_arguments(
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<(Option<Value>, Option<String>)> {
    let (args, id) = keyed_arguments(args)?;
    let json = json_text(args, stdin)?;
    Ok((json.map(serde_json::from_str).transpose()?, id))
}

fn keyed_arguments(mut args: &[String]) -> Result<(&[String], Option<String>)> {
    let id = match args.first().map(String::as_str) {
        Some("--id") => {
            ensure!(args.len() >= 2, "--id requires a key");
            let id = args[1].clone();
            args = &args[2..];
            Some(id)
        }
        Some(flag) if flag.starts_with("--id=") => {
            let id = flag[5..].to_string();
            args = &args[1..];
            Some(id)
        }
        _ => None,
    };
    Ok((args, id))
}

fn required_json(args: &[String], stdin: Option<&[u8]>) -> Result<Value> {
    let text = json_text(args, stdin)?
        .ok_or_else(|| anyhow::anyhow!("respond requires a result envelope"))?;
    Ok(serde_json::from_str(text)?)
}

fn json_text<'a>(args: &'a [String], stdin: Option<&'a [u8]>) -> Result<Option<&'a str>> {
    ensure!(
        args.len() <= 1,
        "expected a single JSON argument or JSON on stdin"
    );
    if let Some(argument) = args.first() {
        return Ok(Some(argument));
    }
    Ok(stdin
        .map(std::str::from_utf8)
        .transpose()?
        .map(str::trim)
        .filter(|value| !value.is_empty()))
}

fn optional_output(value: Option<Value>) -> ScriptCommandOutput {
    match value {
        Some(value) => ScriptCommandOutput {
            stdout: format!("{value}\n"),
            ..Default::default()
        },
        None => ScriptCommandOutput {
            exit_code: 1,
            ..Default::default()
        },
    }
}
