//! Live display authority. Stop caches only reject; they never authorize durable output.
use super::AdvisoryEnvelope;
use anyhow::Result;
use harnx_core::event::{AgentEvent, AgentEventSink};
use harnx_execution_control::{CancelReceipt, ExecutionStore};
use parking_lot::RwLock;
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Default, Debug)]
pub struct LiveEventState {
    active: Arc<RwLock<Attachment>>,
    stopped: Arc<RwLock<HashSet<String>>>,
}

#[derive(Default, Debug)]
struct Attachment {
    execution_id: Option<String>,
    attached_generation: Option<String>,
    retired: bool,
}

impl LiveEventState {
    /// A new attachment cannot be changed by the previous attachment's reader.
    /// Retain accepted stops across prompt changes and reconnects.
    pub fn fork(&self) -> Self {
        Self {
            active: Default::default(),
            stopped: self.stopped.clone(),
        }
    }

    /// A detached reader must never re-arm its queue after the UI replaces it.
    pub fn retire(&self) {
        let mut attachment = self.active.write();
        attachment.retired = true;
        attachment.execution_id = None;
        attachment.attached_generation = None;
    }

    pub fn replacement(&self) -> Self {
        self.retire();
        self.fork()
    }

    pub fn same_attachment(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.active, &other.active)
    }

    pub fn active(&self) -> Option<String> {
        let attachment = self.active.read();
        attachment
            .attached_generation
            .clone()
            .or_else(|| attachment.execution_id.clone())
    }

    /// Dedicated followers keep their admitted UI generation. Gate refreshes
    /// can revoke it, but cannot turn that follower into the next prompt's UI.
    pub fn bind(&self, execution_id: String) {
        self.active.write().attached_generation = Some(execution_id);
    }

    pub fn select(&self, execution_id: Option<String>) {
        let mut attachment = self.active.write();
        if !attachment.retired {
            attachment.execution_id = execution_id;
        }
    }

    pub fn stop(&self, execution_id: &str) {
        self.stopped.write().insert(execution_id.into());
    }

    pub fn is_stopped(&self, execution_id: &str) -> bool {
        self.stopped.read().contains(execution_id)
    }

    pub fn accept_stop(&self, receipt: &CancelReceipt) {
        if receipt.cancelled {
            if let Some(id) = &receipt.execution_id {
                self.stop(id);
            }
        }
    }

    pub fn matches(&self, execution_id: Option<&str>) -> bool {
        let attachment = self.active.read();
        !attachment.retired
            && attachment.execution_id.as_deref() == execution_id
            && attachment
                .attached_generation
                .as_deref()
                .is_none_or(|attached| Some(attached) == execution_id)
    }

    pub fn allows(&self, execution_id: Option<&str>) -> bool {
        execution_id.is_some_and(|id| self.matches(Some(id)) && !self.stopped.read().contains(id))
    }

    pub fn should_render(&self, envelope: &AdvisoryEnvelope, last_durable_seq: u64) -> bool {
        self.allows(envelope.execution_id.as_deref()) && envelope.after_seq >= last_durable_seq
    }

    /// Read authority before draining a subscription, including after reconnect.
    /// Unknown authority fails closed; a missing ID is never inferred from an event.
    pub async fn refresh(&self, store: &ExecutionStore, session: &str) -> Result<()> {
        let result = self.load(store, session).await;
        if result.is_err() {
            self.select(None);
        }
        result
    }

    async fn load(&self, store: &ExecutionStore, session: &str) -> Result<()> {
        let (generation, stopped) = match store.gate_root(session).await? {
            Some(root) => {
                let Some(generation) = store.gate_generation_if_registered(&root, session).await?
                else {
                    self.select(None);
                    return Ok(());
                };
                let stopped = store.gate_stop(&root, &generation).await?.is_some();
                (generation, stopped)
            }
            None => {
                let Some(operation) = store.current(session).await? else {
                    self.select(None);
                    return Ok(());
                };
                let stopped = store.is_stop_fenced(&operation.reference).await?;
                (operation.reference, stopped)
            }
        };
        if stopped {
            self.stop(&generation.execution_id);
        }
        self.select(Some(generation.execution_id));
        Ok(())
    }
}

/// Creation-bound follower output, including live projections of durable status.
/// Explicit historical replay does not use this sink.
pub(crate) struct LiveGenerationSink {
    pub sink: Arc<dyn AgentEventSink>,
    pub state: LiveEventState,
    pub execution_id: String,
}

impl AgentEventSink for LiveGenerationSink {
    fn emit(&self, event: AgentEvent) {
        self.emit_live(event, &self.execution_id);
    }

    fn emit_live(&self, event: AgentEvent, execution_id: &str) {
        if execution_id == self.execution_id && self.state.allows(Some(execution_id)) {
            self.sink.emit_live(event, execution_id);
        }
    }
}

#[cfg(test)]
#[path = "registration_tests.rs"]
mod registration_tests;
