//! CLI dump coverage for the shared default-view replay adapter.

use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::replay_default_view_to_sink as replay_text_dump;
use std::collections::HashMap;
use std::sync::Arc;

mod tests;
