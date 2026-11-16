use serde::Deserialize;

/// Environment variable a protocol template's script reads to learn which
/// structured stdout/stderr contract the runtime expects of it.
pub const TOOL_PROTOCOL_ENV: &str = "HARNX_TOOL_PROTOCOL";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolProtocol {
    Jsonl,
}

impl ToolProtocol {
    pub(crate) fn env_value(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
        }
    }
}
