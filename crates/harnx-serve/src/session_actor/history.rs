use super::*;
use crate::transcript_snapshot::HistorySnapshot;

impl LoadedHistorySnapshot {
    fn from_loaded(loaded: crate::LoadedNatsSession) -> Self {
        let mut warnings = loaded.session.replay_warnings.clone();
        let snapshot = HistorySnapshot::from_entries(
            &loaded.entries,
            &loaded.base_session,
            loaded.worker_active,
        )
        .unwrap_or_else(|error| {
            warnings.push(format!("Failed to build session transcript: {error:#}"));
            HistorySnapshot::default()
        });
        // A presentation failure must not discard durable control state or lease status.
        Self {
            messages: snapshot.messages,
            compaction: snapshot.compaction,
            tokens_usage: Some(crate::ag_ui::UsageContextSnapshot::from_session(
                &loaded.session,
            )),
            warnings,
            entries: Some(loaded.entries),
            session_base: Some(loaded.base_session),
            worker_active: loaded.worker_active,
        }
    }
}

impl SessionActor {
    pub(super) fn subscription(&self) -> SubscribeResult {
        SubscribeResult {
            snapshot: self.history_snapshot.clone(),
            compaction: self.compaction.clone(),
            history_warnings: self.history_warnings.clone(),
            state: self.state.clone(),
            events: self.broadcast_tx.subscribe(),
            log_entries: self.log_entries.clone(),
            tokens_usage: self.tokens_usage.clone(),
            session_base: self.session_base.clone(),
        }
    }

    pub(super) async fn refresh_history_snapshot(&mut self) {
        let loaded = self.load_history_snapshot().await;
        self.apply_history_snapshot(loaded);
    }

    fn apply_history_snapshot(&mut self, loaded: LoadedHistorySnapshot) {
        // `Interrupting` is this actor's own append. The actor answers one
        // command at a time, so today no refresh can run while one is in
        // flight; the guard is here to keep the state machine right if the
        // append ever moves off the command path.
        if self.active_run.is_none() && !matches!(self.state, SessionState::Interrupting) {
            self.state = derive_state_from_log(loaded.entries.as_deref());
        }
        self.history_snapshot = loaded.messages;
        self.compaction = loaded.compaction;
        self.history_warnings = loaded.warnings;
        self.log_entries = loaded.entries;
        self.tokens_usage = loaded.tokens_usage;
        self.session_base = loaded.session_base;
        self.worker_active = loaded.worker_active;
    }

    async fn load_history_snapshot(&self) -> LoadedHistorySnapshot {
        if self.actor_config.call_fn.is_some() {
            return self.test_history_snapshot().await;
        }
        match crate::load_nats_session_state(
            &self.actor_config.base_config,
            self.key.target(),
            &self.key.session,
        ).await {
            Ok(loaded) if loaded.session.agent_name.as_deref() == Some(self.key.agent()) => {
                LoadedHistorySnapshot::from_loaded(loaded)
            }
            Ok(loaded) => LoadedHistorySnapshot {
                warnings: vec![format!(
                    "Failed to load session history: session belongs to agent '{}' rather than '{}'",
                    loaded.session.agent_name.as_deref().unwrap_or("unknown"), self.key.agent()
                )],
                ..Default::default()
            },
            Err(error) if error.to_string().contains("Not Found") => Default::default(),
            Err(error) => LoadedHistorySnapshot {
                warnings: vec![format!("Failed to load session history: {error:#}")],
                ..Default::default()
            },
        }
    }

    async fn test_history_snapshot(&self) -> LoadedHistorySnapshot {
        let prompt_config = prompt_config_for_agent_session_from_global(
            &self.actor_config.base_config,
            &self.key,
            true,
        )
        .await;
        let messages = prompt_config
            .read()
            .session
            .as_ref()
            .map(|session| crate::ag_ui::history_messages_for_snapshot(&session.messages))
            .unwrap_or_default();
        // The test executor has no durable NATS log or metadata base.
        LoadedHistorySnapshot {
            messages,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
