//! CLI selection and rendering for default and archived transcript views.

use crate::cli::DumpSessionArgs;
use anyhow::Result;
use harnx_core::event::AgentEventSink;
use harnx_core::session::SessionLogEntry;
use harnx_core::tool::ToolDeclaration;
use harnx_core::transcript_view::{default_view, TranscriptView};
use harnx_runtime::config::SessionFormat;
use std::collections::HashMap;
use std::sync::Arc;

pub struct DumpRenderer<'a> {
    args: &'a DumpSessionArgs,
    declarations: HashMap<String, ToolDeclaration>,
    sink: Arc<dyn AgentEventSink>,
}

impl<'a> DumpRenderer<'a> {
    pub fn new(
        args: &'a DumpSessionArgs,
        declarations: HashMap<String, ToolDeclaration>,
        sink: Arc<dyn AgentEventSink>,
    ) -> Self {
        Self {
            args,
            declarations,
            sink,
        }
    }

    /// Text needs full raw history for ancestry and mutation resolution; exports
    /// receive the caller's effective snapshot (or newly appended follow rows).
    pub fn replay(&self, entries: &[(u64, SessionLogEntry)], after_seq: u64) -> Result<()> {
        match self.args.format {
            SessionFormat::Text => self.replay_text(entries, after_seq)?,
            SessionFormat::Yaml => {
                for (_, entry) in entries {
                    print!("{}", harnx_runtime::config::yaml_doc(entry)?);
                }
            }
            SessionFormat::Json => {
                for (_, entry) in entries {
                    print!("{}", harnx_runtime::config::jsonl_line(entry)?);
                }
            }
        }
        Ok(())
    }

    fn replay_text(&self, entries: &[(u64, SessionLogEntry)], after_seq: u64) -> Result<()> {
        let mut view = self.view(entries)?;
        view.compaction = view
            .compaction
            .filter(|marker| marker.compaction_seq > after_seq);
        view.entries.retain(|(seq, _)| *seq > after_seq);
        let hint = view
            .compaction
            .as_ref()
            .map(|marker| self.history_command(marker.compaction_seq));
        harnx_runtime::replay_transcript_view_to_sink(
            &view,
            hint.as_deref(),
            &self.declarations,
            self.sink.clone(),
        );
        Ok(())
    }

    fn view(&self, entries: &[(u64, SessionLogEntry)]) -> Result<TranscriptView> {
        match self.args.before_compaction {
            Some(selector) => selector.view(entries),
            None => default_view(entries),
        }
    }

    fn history_command(&self, seq: u64) -> String {
        let agent = self.args.agent_name.as_str();
        let session = self.args.session_id.as_str();
        let seq = seq.to_string();
        let mut args = vec!["harnx", "dump", "session"];
        // A shell quote alone doesn't stop clap interpreting a leading '-'.
        if agent.starts_with('-') || session.starts_with('-') {
            args.extend(["--before-compaction", &seq, "--", agent, session]);
        } else {
            args.extend([agent, session, "--before-compaction", &seq]);
        }
        shell_words::join(args)
    }
}

#[cfg(test)]
pub(crate) mod tests;
