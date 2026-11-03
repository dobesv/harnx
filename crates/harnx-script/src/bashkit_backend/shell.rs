use std::sync::Arc;

use crate::{ScriptCommand, ScriptLimits};

use super::{resolver, BashKitBackend, CommandAdapter, RunControl};

pub(super) fn build_shell(
    limits: &ScriptLimits,
    commands: Vec<Arc<dyn ScriptCommand>>,
    control: &Arc<RunControl>,
) -> bashkit::Bash {
    let mut builder = BashKitBackend::builder(limits);
    // A tool catalog registers discovery commands. Keep standalone backend
    // diagnostics unchanged when no catalog is installed.
    if commands.iter().any(|command| command.name() == "tools") {
        builder = builder.command_resolver(Arc::new(resolver::MissingCommandResolver {
            names: commands
                .iter()
                .map(|command| command.name().to_owned())
                .collect(),
        }));
    }
    for command in commands {
        builder = builder.builtin(
            command.name().to_owned(),
            Box::new(CommandAdapter {
                command,
                control: control.clone(),
            }),
        );
    }
    builder.build()
}
