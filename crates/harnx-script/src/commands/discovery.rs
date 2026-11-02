use std::sync::Arc;

use async_trait::async_trait;
use globset::Glob;

use crate::{
    cli_map::edit_distance, help, ScriptCommand, ScriptCommandContext, ScriptCommandOutput,
};

use super::{failure, Catalog};

pub(super) const TOOLS_HELP: &str = "tools [pattern]\n    List tool synopses, optionally filtered by a quoted glob (for example tools 'fs_*').\n";
pub(super) const HELP_HELP: &str = "help <tool> [tool ...]\n    Show schema-derived help. Use tools to list available tool commands.\n";

pub(super) fn commands(catalog: Arc<Catalog>) -> Vec<Arc<dyn ScriptCommand>> {
    vec![
        Arc::new(Discovery {
            catalog: catalog.clone(),
            name: "tools",
        }),
        Arc::new(Discovery {
            catalog,
            name: "help",
        }),
    ]
}

struct Discovery {
    catalog: Arc<Catalog>,
    name: &'static str,
}

#[async_trait]
impl ScriptCommand for Discovery {
    fn name(&self) -> &str {
        self.name
    }

    async fn run(
        &self,
        args: &[String],
        _stdin: Option<&[u8]>,
        _ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        Ok(if self.name == "tools" {
            self.tools(args)
        } else {
            self.help(args)
        })
    }
}

impl Discovery {
    fn tools(&self, args: &[String]) -> ScriptCommandOutput {
        if args == ["--help"] {
            return text(TOOLS_HELP.into());
        }
        if args.len() > 1 {
            return failure(2, TOOLS_HELP);
        }
        let pattern = match Glob::new(args.first().map_or("*", String::as_str)) {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => return failure(2, format!("invalid tools pattern: {error}")),
        };
        let mut stdout = String::new();
        for tool in self
            .catalog
            .values()
            .filter(|tool| pattern.is_match(&tool.name))
        {
            stdout.push_str(&help::synopsis(&tool.name, &tool.schema, &tool.description));
            stdout.push('\n');
        }
        text(stdout)
    }

    fn help(&self, args: &[String]) -> ScriptCommandOutput {
        if args.is_empty() || args == ["--help"] {
            return text(HELP_HELP.into());
        }
        let mut output = ScriptCommandOutput::default();
        for name in args {
            let document = match name.as_str() {
                "tools" => Some(TOOLS_HELP.into()),
                "help" => Some(HELP_HELP.into()),
                _ => self
                    .catalog
                    .get(name)
                    .map(|tool| help::help(&tool.name, &tool.schema, &tool.description)),
            };
            if let Some(document) = document {
                if !output.stdout.is_empty() {
                    output.stdout.push('\n');
                }
                output.stdout.push_str(&document);
            } else {
                output.exit_code = 127;
                output.stderr.push_str(&unknown_command_message(
                    name,
                    self.catalog.keys().map(String::as_str),
                ));
                output.stderr.push('\n');
            }
        }
        output
    }
}

fn text(stdout: String) -> ScriptCommandOutput {
    ScriptCommandOutput {
        stdout,
        ..Default::default()
    }
}

pub(crate) fn unknown_command_message<'a>(
    name: &str,
    names: impl Iterator<Item = &'a str>,
) -> String {
    let suggestion = names
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min()
        .map(|(_, candidate)| format!("did you mean '{candidate}'? "))
        .unwrap_or_default();
    format!("no such command '{name}'; {suggestion}run 'tools' to list")
}
