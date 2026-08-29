use harnx_core::execution_context::{
    take_result_execution_context, ExecutionContextObservation, ToolObservationProvenance,
};
use serde_json::Value;

pub(super) fn extract_execution_context(
    result: &mut Value,
    provenance: ToolObservationProvenance,
) -> Option<ExecutionContextObservation> {
    let raw = take_result_execution_context(result)?;
    let server_identity = provenance.server_identity.clone();
    let tool_name = provenance.tool_name.clone();
    let call_id = provenance.call_id.clone();
    let mut observation = match serde_json::from_value::<ExecutionContextObservation>(raw) {
        Ok(observation) => observation,
        Err(error) => {
            log::warn!(
                "ignoring malformed tool execution context: server={server_identity} tool={tool_name} call_id={call_id} error={error}"
            );
            return None;
        }
    };
    observation.provenance = Some(provenance);
    if let Err(error) = observation.validate() {
        log::warn!(
            "ignoring invalid tool execution context: server={server_identity} tool={tool_name} call_id={call_id} error={error:#}"
        );
        return None;
    }
    Some(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE;
    use serde_json::json;

    #[test]
    fn replaces_untrusted_provenance_and_removes_private_context() {
        let private_path = "/private/tool-server/workspace";
        let mut observation = ExecutionContextObservation::observe(
            std::path::Path::new(private_path),
            std::path::Path::new(private_path),
        );
        observation.workspace_root = private_path.to_string();
        observation.working_directory = private_path.to_string();
        let mut result = json!({
            "content": [],
            "_meta": {
                EXECUTION_CONTEXT_NAMESPACE: observation,
                "public": true
            }
        });
        let context = extract_execution_context(
            &mut result,
            ToolObservationProvenance::new("attested-scope", "attested-server", "read", "call-1"),
        )
        .expect("valid observation");

        let provenance = context.provenance.expect("worker provenance");
        assert_eq!(provenance.server_scope, "attested-scope");
        assert_eq!(provenance.server_identity, "attested-server");
        assert_eq!(provenance.tool_name, "read");
        assert_eq!(provenance.call_id, "call-1");
        assert_eq!(result["_meta"], json!({"public": true}));
        assert!(!result.to_string().contains(private_path));
    }

    #[test]
    fn strips_malformed_context_without_failing_the_result() {
        let mut result = json!({
            "content": [{"type": "text", "text": "completed"}],
            "_meta": { EXECUTION_CONTEXT_NAMESPACE: {"version": 999} }
        });
        assert!(extract_execution_context(
            &mut result,
            ToolObservationProvenance::new("scope", "server", "read", "call-1"),
        )
        .is_none());
        assert_eq!(
            result,
            json!({
                "content": [{"type": "text", "text": "completed"}]
            })
        );
    }
}
