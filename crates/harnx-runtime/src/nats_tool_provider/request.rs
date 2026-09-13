//! Encoding, journaling and decoding one NATS tool invocation.
use super::*;

pub(super) struct ToolCallInput<'a> {
    pub name: &'a str,
    pub arguments: Value,
    pub id: Option<&'a str>,
}

impl NatsToolProvider {
    pub(super) fn prepare_request(
        &self,
        arguments: Value,
        route: &RegisteredTool,
        tool_call_id: Option<&str>,
    ) -> Result<PendingToolRequest, ToolError> {
        let call_id = Uuid::new_v4().to_string();
        let request = ToolRequest {
            replay: None,
            operation_id: call_id.clone(),
            call_id: call_id.clone(),
            tool: route.raw_name.clone(),
            args: arguments,
            parent_session_id: self.parent_session_id.clone(),
            tool_call_id: tool_call_id.map(str::to_string),
            capabilities: BTreeSet::from([EXECUTION_CONTEXT_NAMESPACE.to_string()]),
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
            registration_key: registration_key(&self.instance_id, &route.server),
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
    ) -> Result<ToolProviderOutput, ToolError> {
        let Some(route) = self.resolve_route(call.name) else {
            return Err(ToolError::Recoverable(anyhow!(
                "NATS tool is not registered: {}",
                call.name
            )));
        };
        let pending = self.prepare_request(call.arguments, &route, call.id)?;
        let call_id = pending.call_id.clone();
        self.record_invocation(&pending.durable, call.name, &route.server)
            .await
            .map_err(ToolError::Fatal)?;
        self.register_operation(&call_id)
            .await
            .map_err(ToolError::Fatal)?;
        let message = self.await_response(pending, abort).await?;
        self.decode_reply(message, call_id, route)
    }

    pub(super) fn decode_reply(
        &self,
        message: async_nats::Message,
        call_id: String,
        route: RegisteredTool,
    ) -> Result<ToolProviderOutput, ToolError> {
        let reply: ToolReply = serde_json::from_slice(&message.payload).map_err(|error| {
            ToolError::Recoverable(anyhow!("invalid reply from tool server: {error}"))
        })?;
        if reply.call_id != call_id {
            return Err(ToolError::Recoverable(anyhow!(
                "tool server returned a mismatched call ID"
            )));
        }
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

    pub(super) fn decode_recorded_reply(
        reply: ToolReply,
        provenance: ToolObservationProvenance,
    ) -> Result<ToolProviderOutput, ToolError> {
        match reply.result {
            Ok(mut value) => {
                let execution_context = extract_execution_context(&mut value, provenance);
                Ok(ToolProviderOutput {
                    value,
                    execution_context,
                })
            }
            Err(ToolErrorPayload::Recoverable(message)) => {
                Err(ToolError::Recoverable(anyhow!(message)))
            }
            Err(ToolErrorPayload::Fatal(message)) => Err(ToolError::Fatal(anyhow!(message))),
        }
    }
}
