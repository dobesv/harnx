//! Ordered SendMessage checks under the dedupe/admission gate.
use super::{
    errors::{map_error, not_found, permission_denied},
    task_view::validate_history,
    Admission, HarnxHandler,
};
use crate::{
    identity::Principal,
    input_map::message_to_input,
    runner::{RunnerError, TurnRequest},
    store::{
        message_fingerprint, parse_task_id, ContextAccess, DedupeEntry, DedupeKey, MessageIdentity,
        TaskRecord,
    },
};
use a2a_lf::{A2AError, Message, Role, SendMessageRequest};

struct MessageAdmission {
    context: Option<String>,
    message: Message,
    fingerprint: String,
    lru_key: DedupeKey,
}

impl MessageAdmission {
    fn new(handler: &HarnxHandler, owner: &Principal, message: Message) -> Result<Self, A2AError> {
        let context = message_context(&message)?;
        let fingerprint = String::new();
        let lru_key = DedupeKey {
            cluster: handler
                .export
                .cluster
                .as_deref()
                .unwrap_or("__local__")
                .to_owned(),
            export: handler.export.public_name.clone(),
            owner: owner.user_id().map(str::to_owned),
            message_id: message.message_id.clone(),
        };
        Ok(Self {
            context,
            message,
            fingerprint,
            lru_key,
        })
    }

    fn gate_key(&self, handler: &HarnxHandler, owner: &Principal) -> String {
        let first_message = self.context.is_none().then_some(&self.message.message_id);
        serde_json::to_string(&(
            &handler.export.public_name,
            owner.user_id(),
            self.context.as_deref(),
            first_message,
        ))
        .expect("gate key serializes")
    }
}

impl HarnxHandler {
    pub(super) async fn admit(
        &self,
        owner: Principal,
        message: Message,
    ) -> Result<Admission, A2AError> {
        let mut request = MessageAdmission::new(self, &owner, message)?;
        // Serialize dedupe and admission, not the whole turn. New-context retries
        // share this gate until their server-allocated context reaches the LRU.
        let gate = self.backend.gate(request.gate_key(self, &owner));
        let _guard = gate.lock().await;
        let target = self.authorize_target(&owner, &request).await?;
        request.fingerprint = message_fingerprint(&request.message.parts);
        if let Some(record) = self.dedupe(&owner, &request).await? {
            return Ok(Admission::retry(record));
        }
        self.check_running()?;
        let busy = self
            .reconcile_context(&owner, request.context.as_deref())
            .await?;
        self.reject_terminal_target(&owner, target).await?;
        self.check_running()?;
        if busy {
            return Err(map_error(RunnerError::Busy.into()));
        }
        self.admit_new_message(&owner, request).await
    }

    async fn authorize_target(
        &self,
        owner: &Principal,
        request: &MessageAdmission,
    ) -> Result<Option<TaskRecord>, A2AError> {
        if let Some(context) = &request.context {
            self.backend
                .store
                .resolve_context(&self.export, owner, context)
                .await
                .map_err(map_error)?
                .ok_or_else(not_found)?;
        } else if self.backend.store.access_rules().is_some_and(|rules| {
            !rules.can_create_session(
                &self.export.agent_ref(),
                &owner.user_id().into_iter().collect::<Vec<_>>(),
            )
        }) {
            return Err(permission_denied());
        }
        match &request.message.task_id {
            Some(id) => self.task(owner, id).await.map(Some),
            None => Ok(None),
        }
    }

    async fn dedupe(
        &self,
        owner: &Principal,
        request: &MessageAdmission,
    ) -> Result<Option<TaskRecord>, A2AError> {
        if let Some(local_id) = &request.context {
            return self
                .backend
                .store
                .dedupe_task(
                    ContextAccess {
                        export: &self.export,
                        owner,
                        local_id,
                    },
                    MessageIdentity {
                        message_id: &request.message.message_id,
                        fingerprint: &request.fingerprint,
                    },
                )
                .await
                .map_err(map_error);
        }
        let Some(id) = self
            .backend
            .store
            .check_dedupe_lru(&request.lru_key, &request.fingerprint)
            .map_err(map_error)?
        else {
            return Ok(None);
        };
        self.task(owner, &id).await.map(Some)
    }

    fn check_running(&self) -> Result<(), A2AError> {
        if self.backend.abort.aborted() {
            return Err(A2AError::internal("server is shutting down"));
        }
        Ok(())
    }

    async fn reconcile_context(
        &self,
        owner: &Principal,
        context: Option<&str>,
    ) -> Result<bool, A2AError> {
        let Some(context) = context else {
            return Ok(false);
        };

        // Fast path: check index for non-terminal tasks
        let key = self
            .backend
            .store
            .resolve_context(&self.export, owner, context)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;

        let non_terminal_entries = self
            .backend
            .store
            .list_non_terminal_entries(&key)
            .await
            .map_err(map_error)?;

        if non_terminal_entries.is_empty() {
            // No active tasks
            return Ok(false);
        }

        // Load and reconcile only non-terminal tasks
        let mut busy = false;
        for entry in &non_terminal_entries {
            let record_opt = self
                .backend
                .store
                .get_task(&key, &entry.task_id)
                .await
                .map_err(map_error)?;

            let Some(record) = record_opt else {
                self.handle_missing_entry(&key, entry).await;
                continue;
            };

            if record.task.status.state.is_terminal() {
                self.backend
                    .store
                    .repair_index_best_effort(&key, &record)
                    .await;
            }
            busy |= !self
                .reconcile(owner, record)
                .await?
                .task
                .status
                .state
                .is_terminal();
        }
        Ok(busy)
    }

    async fn reject_terminal_target(
        &self,
        owner: &Principal,
        target: Option<TaskRecord>,
    ) -> Result<(), A2AError> {
        let Some(target) = target else {
            return Ok(());
        };
        if self
            .task(owner, &target.task.id)
            .await?
            .task
            .status
            .state
            .is_terminal()
        {
            return Err(A2AError::invalid_params(
                "terminal tasks cannot be continued; send a new message with contextId",
            ));
        }
        Ok(())
    }

    async fn admit_new_message(
        &self,
        owner: &Principal,
        request: MessageAdmission,
    ) -> Result<Admission, A2AError> {
        let MessageAdmission {
            context,
            message,
            fingerprint,
            lru_key,
        } = request;
        // Only new admissions validate parts. Invalid input must not allocate a session.
        let input =
            message_to_input(&message, self.limits).map_err(|error| map_error(error.into()))?;
        // Orphan fencing mutates the abort flag. Resume a fresh handle after it settles.
        let session = self.session(owner, context.as_deref()).await?;
        let message_id = message.message_id.clone();
        let started = self
            .backend
            .runner
            .start_turn_with_input(
                TurnRequest {
                    export: &self.export,
                    owner,
                    session: session.clone(),
                    message,
                },
                input,
            )
            .await
            .map_err(map_error)?;
        self.backend
            .store
            .put_message_dedupe(
                session.storage_key(),
                MessageIdentity {
                    message_id: &message_id,
                    fingerprint: &fingerprint,
                },
                &started.snapshot.task.id,
            )
            .await
            .map_err(map_error)?;
        if context.is_none() {
            self.backend.store.record_dedupe_lru(
                lru_key,
                DedupeEntry {
                    task_id: started.snapshot.task.id.clone(),
                    fingerprint,
                },
            );
        }
        Ok(Admission {
            snapshot: started.snapshot,
            events: Some(started.events),
            deduped: false,
        })
    }

    pub(super) async fn admit_request(
        &self,
        owner: Principal,
        req: SendMessageRequest,
    ) -> Result<(Admission, Option<i32>), A2AError> {
        let length = req
            .configuration
            .as_ref()
            .and_then(|config| config.history_length);
        validate_history(length)?;
        if req
            .configuration
            .as_ref()
            .is_some_and(|config| config.task_push_notification_config.is_some())
        {
            return Err(A2AError::push_notification_not_supported());
        }
        if !valid_user_message(&req.message) {
            return Err(A2AError::invalid_params(
                "message requires messageId, ROLE_USER and parts",
            ));
        }
        let handler = self.clone();
        // Admission and dedupe persistence must finish even when HTTP is dropped.
        let admission = tokio::spawn(async move { handler.admit(owner, req.message).await })
            .await
            .map_err(|_| A2AError::internal("request failed"))??;
        Ok((admission, length))
    }
}

fn valid_user_message(message: &Message) -> bool {
    !message.message_id.is_empty() && message.role == Role::User && !message.parts.is_empty()
}

fn message_context(message: &Message) -> Result<Option<String>, A2AError> {
    let Some(id) = &message.task_id else {
        return Ok(message.context_id.clone());
    };
    let (derived, _) = parse_task_id(id).map_err(|_| not_found())?;
    if message
        .context_id
        .as_ref()
        .is_some_and(|context| context != derived)
    {
        return Err(A2AError::invalid_params("contextId and taskId must match"));
    }
    Ok(Some(derived.to_owned()))
}
