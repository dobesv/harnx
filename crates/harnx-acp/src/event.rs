use harnx_core::event::{AgentEvent, AgentSource};

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum NestedAcpEvent {
    Text(String),
    Agent(AgentEvent, Option<AgentSource>),
}
