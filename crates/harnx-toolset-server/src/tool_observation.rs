//! Attest requested working-directory observations on either tool transport.
use super::*;

pub(super) struct RequestAttestation {
    pub(super) call_id: String,
    pub(super) tool: String,
    pub(super) capabilities: std::collections::BTreeSet<String>,
}

pub(super) fn finalize_execution_context(
    context: &ToolRequestContext,
    request: &ToolRequest,
    reply: &mut ToolReply,
) {
    finalize_execution_context_for_attestation(
        context,
        &RequestAttestation {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            capabilities: request.capabilities.clone(),
        },
        reply,
    );
}

pub(super) fn finalize_execution_context_for_attestation(
    context: &ToolRequestContext,
    request: &RequestAttestation,
    reply: &mut ToolReply,
) {
    let Ok(result) = &mut reply.result else {
        return;
    };
    finalize_execution_context_value(
        context.server_scope.as_str(),
        &context.server_identity,
        request,
        result,
    );
}

pub(super) fn finalize_execution_context_value(
    server_scope: &str,
    server_identity: &str,
    request: &RequestAttestation,
    result: &mut Value,
) {
    let raw_context = take_result_execution_context(result);
    if !request.capabilities.contains(EXECUTION_CONTEXT_NAMESPACE) {
        return;
    }
    let Some(raw_context) = raw_context else {
        return;
    };
    let mut observation = match serde_json::from_value::<ExecutionContextObservation>(raw_context) {
        Ok(observation) => observation,
        Err(error) => {
            log::warn!(
                "stripping malformed execution context from tool result: server={} tool={} error={error}",
                server_identity,
                request.tool
            );
            return;
        }
    };
    observation.provenance = Some(ToolObservationProvenance::new(
        server_scope,
        server_identity,
        request.tool.clone(),
        request.call_id.clone(),
    ));
    if let Err(error) = observation.validate() {
        log::warn!(
            "stripping invalid execution context from tool result: server={} tool={} error={error:#}",
            server_identity,
            request.tool
        );
        return;
    }
    if let Ok(value) = serde_json::to_value(observation) {
        put_result_execution_context(result, value);
    }
}
