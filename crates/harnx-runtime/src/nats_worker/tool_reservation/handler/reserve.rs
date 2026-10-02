//! Bounded reservation setup, claim acquisition, and startup publication.
use super::*;

impl Handler {
    pub(super) async fn reserve(&self, message: async_nats::Message) {
        let request = match serde_json::from_slice::<Reserve>(&message.payload) {
            Ok(request) => request,
            Err(cause) => {
                self.reply(
                    &message,
                    &ReserveReply::Error(error("InvalidRequest", cause)),
                )
                .await;
                return;
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        // Include both bounded setup and startup waits. A lost reply still
        // expires, but startup doesn't consume the client's renewal window.
        self.reservations.lock().await.insert(
            id.clone(),
            Instant::now() + SESSION_TOOL_SERVER_START_TIMEOUT * 2 + self.ttl,
        );
        let Some(claimed) = self.claim_reservation(&id, &request, &message).await else {
            return;
        };
        if let Some(reconciler) = &self.reconciler {
            WorkerRuntime::wait_for_tool_server_start(
                Arc::clone(reconciler),
                claimed,
                &user_token(&id),
            )
            .await;
        }
        let mut reservations = self.reservations.lock().await;
        let Some(expires) = reservations.get_mut(&id) else {
            drop(reservations);
            self.free(&id).await;
            self.reply(
                &message,
                &ReserveReply::Error(ToolReservationError {
                    code: ToolReservationErrorCode::UnknownOrExpired,
                    message: "tool reservation ended during setup".to_owned(),
                }),
            )
            .await;
            return;
        };
        *expires = Instant::now() + self.ttl;
        drop(reservations);
        self.reply(
            &message,
            &ReserveReply::Reserved(Reserved {
                protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
                attempt_id: request.attempt_id,
                reservation_id: id.clone(),
                worker_id: self.worker_id.clone(),
                server_scope: self.server_scope.clone(),
                control_subject: format!("{}.{id}", self.control_prefix),
                ttl_ms: self.ttl.as_millis() as u64,
                renew_after_ms: (self.ttl / 3).as_millis() as u64,
            }),
        )
        .await;
    }

    async fn validate_and_claim(
        &self,
        id: &str,
        request: &Reserve,
    ) -> Result<Vec<crate::config::ToolServerConfig>> {
        ensure!(
            request.protocol_version == TOOL_RESERVATION_PROTOCOL_VERSION,
            "unsupported tool reservation protocol version"
        );
        uuid::Uuid::parse_str(&request.attempt_id).context("invalid attempt_id")?;
        ensure!(
            request.session_storage_key.len() == 64
                && request
                    .session_storage_key
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "invalid session storage key"
        );
        let record = self
            .metadata
            .get(&request.session_storage_key)
            .await?
            .context("session metadata not found")?;
        record
            .metadata
            .validate_storage_key(&request.session_storage_key)?;
        let servers = tool_servers_for_view(
            &self.config.read(),
            request.view.package.as_deref(),
            &request.view.use_tools,
        );
        Ok(match &self.reconciler {
            Some(reconciler) => reconciler.claim_users(&user_token(id), servers).await,
            // Unmanaged workers return their configured scope without launching.
            None => Vec::new(),
        })
    }

    async fn claim_reservation(
        &self,
        id: &str,
        request: &Reserve,
        message: &async_nats::Message,
    ) -> Option<Vec<crate::config::ToolServerConfig>> {
        let result = tokio::time::timeout(
            SESSION_TOOL_SERVER_START_TIMEOUT,
            self.validate_and_claim(id, request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tool reservation setup timed out"))
        .and_then(|result| result);
        match result {
            Ok(claimed) => Some(claimed),
            Err(cause) => {
                self.reservations.lock().await.remove(id);
                self.free(id).await;
                self.reply(
                    message,
                    &ReserveReply::Error(error("InvalidRequest", cause)),
                )
                .await;
                None
            }
        }
    }
}
