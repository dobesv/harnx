//! Encoding, journaling and decoding one NATS tool invocation.
use super::*;

/// Payload slack reserved on top of each serialized `ToolRequest` for the
/// NATS message envelope (headers counted toward `max_payload` on 2.11+,
/// telemetry propagation entries, protocol framing). Script-mode pre-flight
/// subtracts this so an admitted inner call can't exceed the broker limit.
const JOURNAL_ENVELOPE_MARGIN_BYTES: usize = 1024;

pub(super) struct ToolCallInput<'a> {
    pub name: &'a str,
    pub arguments: Value,
    pub origin: ToolCallOrigin<'a>,
    pub run_context: Option<&'a crate::nats_session_metadata::RunLimitsRecord>,
}

/// Broker `max_payload` minus the envelope margin, saturating at zero.
/// Kept as a function so the margin arithmetic itself is unit-testable.
fn script_effective_max_bytes(server_max_payload: usize) -> usize {
    server_max_payload.saturating_sub(JOURNAL_ENVELOPE_MARGIN_BYTES)
}

/// The request fields the pre-flight probe copies from the provider
/// snapshot rather than from the inner call: parent identity, run context,
/// and the trusted session owner. Bundled so the probe signature stays
/// stable as fields are added to `ToolRequest`.
pub(super) struct ProbeContext<'a> {
    pub parent_session_id: &'a Option<String>,
    pub parent_agent: &'a Option<String>,
    pub parent_local_session_id: &'a Option<String>,
    pub run_context: &'a Option<crate::nats_session_metadata::RunLimitsRecord>,
    pub user_id: &'a Option<String>,
}

impl Default for ProbeContext<'_> {
    fn default() -> Self {
        Self {
            parent_session_id: &None,
            parent_agent: &None,
            parent_local_session_id: &None,
            run_context: &None,
            user_id: &None,
        }
    }
}

/// The `ToolRequest` the pre-flight probe serializes. A free function so
/// tests can assert it matches the shape `prepare_request_with_progress`
/// builds for a real inner call.
fn probe_request(tool: &str, args: &Value, context: &ProbeContext<'_>) -> ToolRequest {
    let call_id = Uuid::new_v4().to_string();
    ToolRequest {
        user_id: context.user_id.clone(),
        input_responses: None,
        request_state: None,
        run_context: context.run_context.as_ref().map(|record| {
            harnx_toolset::AutonomousRunContext {
                snapshot: serde_json::to_value(record).expect("run limits serialize"),
                started_at_ms: chrono::Utc::now()
                    .timestamp_millis()
                    .try_into()
                    .expect("current UTC time is positive"),
            }
        }),
        replay: None,
        operation_id: call_id.clone(),
        tool: tool.to_string(),
        args: args.clone(),
        parent_session_id: context.parent_session_id.clone(),
        parent_agent: context.parent_agent.clone(),
        parent_local_session_id: context.parent_local_session_id.clone(),
        // Real inner calls pass their call id through the fork, so the probe
        // must count it too; `tool_round` only affects size by its digit
        // count.
        call_id: call_id.clone(),
        tool_call_id: Some(call_id),
        tool_round: Some(1),
        capabilities: BTreeSet::from([
            EXECUTION_CONTEXT_NAMESPACE.to_string(),
            CAPABILITY_TOOL_PROGRESS.to_string(),
            CAPABILITY_NEW_MESSAGE_PENDING.to_string(),
        ]),
    }
}

impl NatsToolProvider {
    /// Attachment reads use this round's broker and credentials, not a new
    /// connection or a process-global store. Constructing the fetcher does no I/O.
    pub(crate) fn script_attachment_fetcher(
        &self,
    ) -> std::sync::Arc<dyn harnx_script::fs_attachments::AttachmentFetcher> {
        crate::bash_provider::attachments::fetcher(async_nats::jetstream::new(self.client.clone()))
    }

    /// Capture the same parent identity and replica policy used for tool requests.
    pub(crate) fn script_media_store(
        &self,
    ) -> std::sync::Arc<dyn crate::bash_inner::media::MediaStore> {
        let owner = self.parent_local_session_id.as_ref().and_then(|session| {
            harnx_core::cid_url::SessionRef::new(self.parent_agent.clone(), session.clone()).ok()
        });
        crate::bash_inner::media::nats_store(
            async_nats::jetstream::new(self.client.clone()),
            owner,
            self.journal_replicas,
        )
    }

    /// Script-mode `bash` pre-flight (T3.4): real broker `max_payload` plus a
    /// probe that serializes the exact `ToolRequest` shape this module
    /// publishes, so inner calls fail with a clear recoverable error before
    /// dispatch instead of an opaque NATS publish failure. The probe mirrors
    /// `prepare_request_with_progress` field for field: inner calls carry
    /// their call id as `tool_call_id` and run in a parent round, and every
    /// optional capability is included so the estimate never undercounts a
    /// progress-enabled call or one dispatched with a message pending.
    pub(crate) fn script_payload_limit(&self) -> crate::bash_inner::PayloadLimit {
        use std::sync::Arc;
        // Broker payload limit minus slack for the message envelope: NATS
        // 2.11+ counts headers toward `max_payload`, and dispatch attaches
        // idempotency/instance/call-id/content-type headers plus telemetry
        // propagation entries and protocol framing. The probe also serializes
        // the script-facing tool name, while real dispatch resolves the
        // route's raw_name — the 1 KiB margin covers that delta and the full
        // header set with generous slack.
        let max_bytes = script_effective_max_bytes(self.client.server_info().max_payload);
        let parent_session_id = self.parent_session_id.clone();
        let parent_agent = self.parent_agent.clone();
        let parent_local_session_id = self.parent_local_session_id.clone();
        let run_context = self.run_context.clone();
        let user_id = self.session_owner.clone();
        let request_size: crate::bash_inner::RequestSizeProbe = Arc::new(move |tool, args| {
            let context = ProbeContext {
                parent_session_id: &parent_session_id,
                parent_agent: &parent_agent,
                parent_local_session_id: &parent_local_session_id,
                run_context: &run_context,
                user_id: &user_id,
            };
            let request = probe_request(tool, args, &context);
            serde_json::to_vec(&request)
                .expect("tool request serializes")
                .len()
        });
        crate::bash_inner::PayloadLimit {
            max_bytes,
            request_size,
        }
    }

    #[cfg(test)]
    pub(super) fn prepare_request(
        &self,
        arguments: Value,
        route: &RegisteredTool,
        tool_call_id: Option<&str>,
    ) -> Result<PendingToolRequest, ToolError> {
        self.prepare_request_with_progress(
            arguments,
            route,
            ToolCallOrigin {
                tool_call_id,
                tool_round: None,
            },
            false,
            self.run_context.as_ref(),
        )
    }

    fn prepare_request_with_progress(
        &self,
        arguments: Value,
        route: &RegisteredTool,
        origin: ToolCallOrigin<'_>,
        enable_progress: bool,
        run_context: Option<&crate::nats_session_metadata::RunLimitsRecord>,
    ) -> Result<PendingToolRequest, ToolError> {
        let call_id = origin
            .tool_call_id
            .and_then(current_physical_invocation_for)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut capabilities = BTreeSet::from([EXECUTION_CONTEXT_NAMESPACE.to_string()]);
        if enable_progress {
            capabilities.insert(CAPABILITY_TOOL_PROGRESS.to_string());
        }
        if self.dispatch_with_message_pending(route) {
            capabilities.insert(CAPABILITY_NEW_MESSAGE_PENDING.to_string());
        }
        // T4.3: a rerun after a tool challenge carries the latest challenge's
        // accepted responses and opaque state beside `run_context`, never
        // inside the arguments. Absent on first calls and on every dispatch
        // outside the challenge owner's leaf (allowlisted `call_tool`).
        let tool_input = crate::middleware::current_tool_input();
        let request = ToolRequest {
            // Trusted caller identity: the session owner (or the operator
            // identity for operator calls), set by the worker — never from
            // model arguments.
            user_id: self.session_owner.clone(),
            input_responses: tool_input.as_ref().map(|input| input.responses.clone()),
            request_state: tool_input
                .as_ref()
                .map(|input| input.request_state.clone())
                .filter(|state| !state.is_null()),
            run_context: run_context.map(|record| harnx_toolset::AutonomousRunContext {
                snapshot: serde_json::to_value(record).expect("run limits serialize"),
                started_at_ms: chrono::Utc::now()
                    .timestamp_millis()
                    .try_into()
                    .expect("current UTC time is positive"),
            }),
            replay: None,
            operation_id: call_id.clone(),
            call_id: call_id.clone(),
            tool: route.raw_name.clone(),
            args: arguments,
            parent_session_id: self.parent_session_id.clone(),
            parent_agent: self.parent_agent.clone(),
            parent_local_session_id: self.parent_local_session_id.clone(),
            tool_call_id: origin.tool_call_id.map(str::to_string),
            tool_round: origin.tool_round,
            capabilities,
        };
        self.prepare_recorded_request(request, route)
    }

    pub(super) fn prepare_recorded_request(
        &self,
        request: ToolRequest,
        route: &RegisteredTool,
    ) -> Result<PendingToolRequest, ToolError> {
        let call_id = request.call_id.clone();
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(HDR_IDEMPOTENCY_KEY, call_id.as_str());
        headers.insert(HDR_INSTANCE_ID, self.instance_id.as_str());
        headers.insert(HDR_CALL_ID, call_id.as_str());
        headers.insert(HDR_CONTENT_TYPE, JSON_CONTENT_TYPE);
        harnx_telemetry::propagate::inject_current_into_nats(&mut headers);
        let payload = serde_json::to_vec(&request).map_err(|error| {
            ToolError::Fatal(anyhow!("failed to encode NATS tool request: {error}"))
        })?;
        Ok(PendingToolRequest {
            durable: request,
            call_id,
            server: route.server.clone(),
            stop_on_new_message: route.stop_on_new_message,
            // Liveness watches the registration dispatch actually resolved:
            // a user-scoped tool's registration lives under
            // `{scope}.{identity}.u.{U}`, not the server's global key.
            registration_key: route.registration_key.clone(),
            subject: self
                .instance_id
                .tool_subject(&route.server, &route.raw_name),
            request: async_nats::Request::new()
                .headers(headers)
                .payload(payload.into())
                .timeout(route.request_timeout),
        })
    }

    pub(super) async fn call_registered_tool(
        &self,
        call: ToolCallInput<'_>,
        abort: &AbortSignal,
        progress: Option<std::sync::Arc<dyn ToolProgress>>,
    ) -> Result<ToolProviderOutput, ToolError> {
        let Some(route) = self.resolve_route(call.name) else {
            return Err(ToolError::Recoverable(anyhow!(
                "NATS tool is not registered: {}",
                call.name
            )));
        };
        // Invariant (b): discovery already filtered by session owner, but the
        // guard also covers a stale route reached through a raw alias.
        if !route.visible_to(self.session_owner.as_deref()) {
            return Err(ToolError::Recoverable(anyhow!(
                "NATS tool '{}' is not available to this session's identity",
                call.name
            )));
        }
        let run_context = call.run_context.or(self.run_context.as_ref());
        let pending = self.prepare_request_with_progress(
            call.arguments,
            &route,
            call.origin,
            progress.is_some(),
            run_context,
        )?;
        let call_id = pending.call_id.clone();
        let progress_route =
            progress.map(|progress| self.progress_dispatcher.register(call_id.clone(), progress));
        let request = pending.durable;
        if let Some(record) = run_context.filter(|record| record.is_expired_at(chrono::Utc::now()))
        {
            return Err(ToolError::Fatal(
                crate::nats_session_metadata::run_limits::DeadlineExpired::before_dispatch(record),
            ));
        }
        Box::pin(self.record_invocation(&request, (call.name, &route.server)))
            .await
            .map_err(ToolError::Fatal)?;
        let pending = self.prepare_recorded_request(request.clone(), &route)?;
        let result = async {
            let message = self.await_response(pending, abort).await?;
            let mut reply = Self::parse_reply(message, &call_id)?;
            if let Some(progress_route) = progress_route {
                progress_route.finish(reply.final_progress.take());
            }
            self.decode_reply_value(reply, call_id, route)
        }
        .await;
        self.with_recorded_partial_result(&request, result).await
    }

    pub(super) fn decode_reply(
        &self,
        message: async_nats::Message,
        call_id: String,
        route: RegisteredTool,
    ) -> Result<ToolProviderOutput, ToolError> {
        let reply = Self::parse_reply(message, &call_id)?;
        self.decode_reply_value(reply, call_id, route)
    }

    fn parse_reply(
        message: async_nats::Message,
        expected_call_id: &str,
    ) -> Result<ToolReply, ToolError> {
        let reply: ToolReply = serde_json::from_slice(&message.payload).map_err(|error| {
            ToolError::Recoverable(anyhow!("invalid reply from tool server: {error}"))
        })?;
        if reply.call_id != expected_call_id {
            return Err(ToolError::Recoverable(anyhow!(
                "tool server returned a mismatched call ID"
            )));
        }
        Ok(reply)
    }

    fn decode_reply_value(
        &self,
        reply: ToolReply,
        call_id: String,
        route: RegisteredTool,
    ) -> Result<ToolProviderOutput, ToolError> {
        Self::decode_recorded_reply(
            reply,
            ToolObservationProvenance::new(
                self.instance_id.to_string(),
                route.server,
                route.raw_name,
                call_id,
            ),
        )
    }

    // qual:allow(iosp) — reply decoding routes one wire shape per arm: challenge, result, recoverable error; the input_required fast path must run before result normalization so a challenge never becomes an error result.
    pub(super) fn decode_recorded_reply(
        reply: ToolReply,
        provenance: ToolObservationProvenance,
    ) -> Result<ToolProviderOutput, ToolError> {
        // A challenge rides next to the compatibility error in `result`;
        // newer callers suspend on it and ignore `result` entirely.
        if let Some(input) = reply.input_required {
            return Self::decode_input_required(input, &provenance);
        }
        match reply.result {
            Ok(mut value) => {
                let execution_context = extract_execution_context(&mut value, provenance);
                Ok(ToolProviderOutput {
                    input_required: None,
                    value,
                    execution_context,
                })
            }
            Err(ToolErrorPayload::Recoverable(message)) => {
                Err(ToolError::Recoverable(anyhow!(message)))
            }
            Err(ToolErrorPayload::Fatal(message)) => Err(ToolError::Fatal(anyhow!(message))),
            Err(ToolErrorPayload::Interrupted(interrupted)) => {
                Err(ToolError::Fatal(anyhow!(interrupted)))
            }
        }
    }

    /// Map a wire `input_required` challenge onto the core typed outcome.
    /// The producer identity is infrastructure-owned (`tool:<server>/<tool>`),
    /// never tool-supplied: a tool cannot forge another tool's interactions.
    fn decode_input_required(
        input: harnx_toolset::ToolInputRequired,
        provenance: &ToolObservationProvenance,
    ) -> Result<ToolProviderOutput, ToolError> {
        input
            .validate()
            .map_err(|error| ToolError::Recoverable(anyhow!("tool server: {error}")))?;
        Ok(ToolProviderOutput {
            value: Value::Null,
            execution_context: None,
            input_required: Some(harnx_core::tool::ToolInputRequired {
                producer: format!(
                    "tool:{}/{}",
                    provenance.server_identity, provenance.tool_name
                ),
                key: input.key,
                kind: input.kind,
                message: input.message,
                surface: input.surface,
                url: input.url,
                summary: input.summary,
                request_state: input.request_state,
            }),
        })
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// The pre-flight budget must leave the 1 KiB envelope margin below the
    /// broker's advertised `max_payload`, and must never go negative on
    /// tiny brokers.
    #[test]
    fn envelope_margin_leaves_slack_below_broker_max() {
        assert_eq!(JOURNAL_ENVELOPE_MARGIN_BYTES, 1024);
        assert_eq!(
            script_effective_max_bytes(8 * 1024 * 1024),
            8 * 1024 * 1024 - 1024
        );
        assert_eq!(script_effective_max_bytes(512), 0, "saturates at zero");
    }

    #[test]
    fn base64_creation_request_respects_broker_budget() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let size = |bytes: usize| {
            let content = STANDARD.encode(vec![0; bytes]);
            assert_eq!(content.len(), bytes.div_ceil(3) * 4);
            let args = serde_json::json!({
                "content": content,
                "mime_type": "application/octet-stream",
                "encoding": "base64"
            });
            let context = ProbeContext {
                parent_session_id: &Some("session-1".into()),
                parent_agent: &Some("agent-a".into()),
                parent_local_session_id: &Some("local-7".into()),
                ..ProbeContext::default()
            };
            let request = probe_request("attachment_create", &args, &context);
            serde_json::to_vec(&request).unwrap().len()
        };
        let default_budget = script_effective_max_bytes(1024 * 1024);
        assert!(size(760 * 1024) < default_budget);
        // 768 KiB fills 1 MiB with base64 alone, before the request envelope.
        assert!(size(768 * 1024) > default_budget);
        let handler_limit: usize = 5 * 1024 * 1024;
        assert_eq!(handler_limit.div_ceil(3) * 4, 6_990_508);
        let max_request_bytes = size(handler_limit);
        assert!(max_request_bytes > default_budget);
        assert!(max_request_bytes < script_effective_max_bytes(8 * 1024 * 1024));
    }

    /// The probe serializes the exact `ToolRequest` shape a real inner-call
    /// dispatch builds (`prepare_request_with_progress`)
    /// — same ids, round, parents, and the full capability set so a
    /// progress-enabled call is never undercounted.
    #[test]
    fn probe_request_matches_real_dispatch_shape() {
        let parents = (
            Some("session-1".to_string()),
            Some("agent-a".to_string()),
            Some("local-7".to_string()),
        );
        let context = ProbeContext {
            parent_session_id: &parents.0,
            parent_agent: &parents.1,
            parent_local_session_id: &parents.2,
            ..ProbeContext::default()
        };
        let request = probe_request("fs_read", &serde_json::json!({"path": "a"}), &context);

        assert_eq!(request.tool, "fs_read");
        assert_eq!(request.args, serde_json::json!({"path": "a"}));
        // Inner calls pass their call id through the fork: all three id
        // fields carry it.
        assert!(!request.call_id.is_empty());
        assert_eq!(request.operation_id, request.call_id);
        assert_eq!(
            request.tool_call_id.as_deref(),
            Some(request.call_id.as_str())
        );
        assert_eq!(request.tool_round, Some(1));
        assert!(request.replay.is_none());
        assert!(request.run_context.is_none());
        assert_eq!(request.parent_session_id.as_deref(), Some("session-1"));
        assert_eq!(request.parent_agent.as_deref(), Some("agent-a"));
        assert_eq!(request.parent_local_session_id.as_deref(), Some("local-7"));
        assert!(request.capabilities.contains(EXECUTION_CONTEXT_NAMESPACE));
        assert!(request.capabilities.contains(CAPABILITY_TOOL_PROGRESS));
        assert!(request
            .capabilities
            .contains(CAPABILITY_NEW_MESSAGE_PENDING));

        // The probe's serialized size tracks the real serialized request:
        // bigger arguments mean a strictly bigger probe.
        let size = |args: &Value| {
            let request = probe_request("fs_read", args, &context);
            serde_json::to_vec(&request).expect("serializes").len()
        };
        let small = serde_json::json!({"path": "a"});
        let large = serde_json::json!({"path": "a", "extra": "x".repeat(4096)});
        assert!(size(&large) > size(&small));
        assert!(size(&large) >= serde_json::to_vec(&large).expect("serializes").len());
    }
}
