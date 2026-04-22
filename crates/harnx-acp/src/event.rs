use harnx_core::event::{AgentEvent, AgentSource};

#[derive(Clone, Debug)]
pub enum NestedAcpEvent {
    Text(String),
    Agent(AgentEvent, Option<AgentSource>),
}
