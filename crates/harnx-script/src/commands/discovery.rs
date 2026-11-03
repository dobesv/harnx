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
            return ScriptCommandOutput {
                stdout: TOOLS_HELP.into(),
                ..Default::default()
            };
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
        ScriptCommandOutput {
            stdout,
            ..Default::default()
        }
    }

    fn help(&self, args: &[String]) -> ScriptCommandOutput {
        if args.is_empty() || args == ["--help"] {
            return ScriptCommandOutput {
                stdout: HELP_HELP.into(),
                ..Default::default()
            };
        }
        let mut output = ScriptCommandOutput::default();
        let mut documents = Vec::new();
        for name in args {
            match self.tool_document(name) {
                Some(document) => documents.push(document),
                None => {
                    output.exit_code = 127;
                    output.stderr.push_str(&unknown_command_message(
                        name,
                        self.catalog.keys().map(String::as_str),
                    ));
                    output.stderr.push('\n');
                }
            }
        }
        output.stdout = documents.join("\n");
        output
    }

    fn tool_document(&self, name: &str) -> Option<String> {
        match name {
            "tools" => Some(TOOLS_HELP.into()),
            "help" => Some(HELP_HELP.into()),
            _ => self
                .catalog
                .get(name)
                .map(|tool| help::help(&tool.name, &tool.schema, &tool.description)),
        }
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
