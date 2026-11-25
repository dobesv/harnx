use async_trait::async_trait;
use harnx_script::{ScriptCommand, ScriptCommandContext, ScriptCommandOutput};
use serde_json::Value;

/// R6 (Bash): the validated call arguments, printed as JSON on stdout.
pub(super) struct ToolInputCommand {
    pub(super) input: Value,
}

#[async_trait]
impl ScriptCommand for ToolInputCommand {
    fn name(&self) -> &str {
        "tool_input"
    }

    async fn run(
        &self,
        args: &[String],
        _stdin: Option<&[u8]>,
        _ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        Ok(self.execute(args))
    }
}

impl ToolInputCommand {
    pub(super) fn execute(&self, args: &[String]) -> ScriptCommandOutput {
        if !args.is_empty() {
            return ScriptCommandOutput {
                exit_code: 2,
                stderr: "tool_input takes no arguments\n".to_string(),
                ..Default::default()
            };
        }
        ScriptCommandOutput {
            stdout: format!("{}\n", self.input),
            ..Default::default()
        }
    }
}
