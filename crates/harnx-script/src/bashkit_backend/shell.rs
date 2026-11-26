use std::sync::Arc;

use crate::{ScriptCommand, ScriptLimits, ScriptPythonFunctions};

use super::{python_lines::PreludePython, resolver, BashKitBackend, CommandAdapter, RunControl};

pub(super) fn build_shell(
    limits: &ScriptLimits,
    commands: Vec<Arc<dyn ScriptCommand>>,
    python_functions: Option<ScriptPythonFunctions>,
    control: &Arc<RunControl>,
) -> bashkit::Bash {
    let mut builder = BashKitBackend::builder(limits);
    if let Some(functions) = python_functions {
        builder = with_python(builder, limits, functions, control);
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

/// Registers `python` and `python3` with the run's host functions. A prelude
/// needs BashKit's `Python` type directly, which the builder's own method does
/// not take.
fn with_python(
    builder: bashkit::BashBuilder,
    limits: &ScriptLimits,
    functions: ScriptPythonFunctions,
    control: &Arc<RunControl>,
) -> bashkit::BashBuilder {
    let python_limits = super::python_limits(limits);
    let names = functions.names.clone();
    let prelude = functions.prelude.clone();
    let handler = super::python::handler(functions, control.clone());
    let Some(prelude) = prelude else {
        return builder.python_with_external_handler(python_limits, names, handler);
    };
    ["python", "python3"]
        .into_iter()
        .fold(builder, |builder, name| {
            let python = bashkit::Python::with_limits(python_limits.clone())
                .with_external_handler_and_prelude(names.clone(), handler.clone(), prelude.clone());
            builder.builtin(name, Box::new(PreludePython::new(python, &prelude)))
        })
}
