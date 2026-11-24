use std::sync::Arc;

use crate::{ScriptCommand, ScriptLimits, ScriptPythonFunctions};

use super::{resolver, BashKitBackend, CommandAdapter, RunControl};

pub(super) fn build_shell(
    limits: &ScriptLimits,
    commands: Vec<Arc<dyn ScriptCommand>>,
    python_functions: Option<ScriptPythonFunctions>,
    control: &Arc<RunControl>,
) -> bashkit::Bash {
    let mut builder = BashKitBackend::builder(limits);
    if let Some(functions) = python_functions {
        builder = builder.python_with_external_handler(
            super::python_limits(limits),
            functions.names.clone(),
            super::python::handler(functions, control.clone()),
        );
    }
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
