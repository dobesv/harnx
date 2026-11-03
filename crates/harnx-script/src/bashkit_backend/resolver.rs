use std::sync::Arc;

use async_trait::async_trait;
use bashkit::{Builtin, BuiltinContext, CommandResolver, ExecResult};

use crate::commands::unknown_command_message;

pub(super) struct MissingCommandResolver {
    pub names: Vec<String>,
}

impl CommandResolver for MissingCommandResolver {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Builtin>> {
        // Resolution is a diagnostic only. It never dispatches an unregistered
        // tool, opens a host path, or delegates to an external executable.
        Some(Arc::new(MissingCommand(unknown_command_message(
            name,
            self.names.iter().map(String::as_str),
        ))))
    }
}

struct MissingCommand(String);

#[async_trait]
impl Builtin for MissingCommand {
    async fn execute(&self, _ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        Ok(ExecResult {
            stderr: format!("{}\n", self.0).into(),
            exit_code: 127,
            ..Default::default()
        })
    }
}
