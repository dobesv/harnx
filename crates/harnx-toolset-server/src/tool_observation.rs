//! Attest requested working-directory observations on either tool transport.
use super::*;

pub(super) struct RequestAttestation {
    pub(super) call_id: String,
    pub(super) tool: String,
    pub(super) capabilities: std::collections::BTreeSet<String>,
}

pub(crate) fn finalize_journal_reply(
    record: &invocation_journal::RecordedInvocation,
    reply: &mut ToolReply,
    already_recorded: bool,
) {
    let Ok(result) = &mut reply.result else {
        return;
    };
    let request = RequestAttestation {
        call_id: record.request.call_id.clone(),
        tool: record.request.tool.clone(),
        capabilities: record.request.capabilities.clone(),
    };
    if already_recorded && has_journal_attestation(record, result) {
        return;
    }
    finalize_execution_context_value(&record.server_scope, &record.server, &request, result);
}

fn has_journal_attestation(
    record: &invocation_journal::RecordedInvocation,
    result: &Value,
) -> bool {
    let Some(raw) = result
        .get("_meta")
        .and_then(|meta| meta.get(EXECUTION_CONTEXT_NAMESPACE))
    else {
        return true;
    };
    if !record
        .request
        .capabilities
        .contains(EXECUTION_CONTEXT_NAMESPACE)
    {
        return false;
    }
    let Ok(observation) = serde_json::from_value::<ExecutionContextObservation>(raw.clone()) else {
        return false;
    };
    observation.validate().is_ok()
        && observation.provenance.is_some_and(|provenance| {
            provenance.server_scope == record.server_scope
                && provenance.server_identity == record.server
                && provenance.tool_name == record.request.tool
                && provenance.call_id == record.request.call_id
        })
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
