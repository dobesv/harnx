//! What happens to a claimed sandbox when its delayed session bind cannot
//! apply: conditional bind, destroy-if-provably-untracked, TTL otherwise.

use super::lifecycle::{connect_failing_activation, connect_invocation, scripted_api_error};
use super::*;

/// Script a claim-creation-time side effect that stores `value` under the
/// session's sandbox binding, standing in for a concurrent writer that won
/// the binding while this connect was still resolving its own sandbox.
fn script_concurrent_binding_value(
    fixture: &ToolsetFixture,
    session_id: &str,
    value: serde_json::Value,
) {
    let store = fixture.metadata.clone();
    let key = harnx_core::session_identity::session_key(Some("coder"), session_id);
    fixture
        .api
        .create_hooks
        .lock()
        .push_back(Box::pin(async move {
            store
                .replace_tool_context_value(
                    ToolContextEntry {
                        session_id: &key,
                        key: SANDBOX_CONTEXT_KEY,
                    },
                    value,
                )
                .await
                .expect("concurrent bind write");
        }));
}

/// A connect replacing a binding written by a concurrent connect.
fn script_concurrent_bind(fixture: &ToolsetFixture, session_id: &str, sandbox_id: &str) {
    script_concurrent_binding_value(
        fixture,
        session_id,
        json!({"version": 1, "sandbox_id": sandbox_id}),
    );
}

/// A delete failure whose message is distinct from the activation error's
/// ("scripted Kubernetes error"), so the test can tell them apart.
fn scripted_delete_error() -> anyhow::Error {
    let mut status = kube::error::Status::failure("scripted delete rejection", "Forbidden")
        .with_code(403)
        .boxed();
    status.details = Some(kube::core::response::StatusDetails {
        name: String::new(),
        group: String::new(),
        kind: String::new(),
        uid: String::new(),
        causes: Vec::new(),
        retry_after_seconds: 0,
    });
    crate::kubernetes::scripted_kubernetes_error(kube::Error::Api(status))
}

/// Run an ID-less connect whose activation fails with a scripted 403 after
/// the claim was created, so the unready-bind path decides the claim's fate.
async fn connect_failing_after_concurrent_write(
    fixture: &ToolsetFixture,
    session_id: &str,
    call_id: &str,
) -> ToolInvokeError {
    fixture
        .api
        .get_errors
        .lock()
        .push_back(scripted_api_error(403));
    fixture
        .sandbox
        .invoke_with_context(connect_invocation(call_id, session_id, json!({})))
        .await
        .unwrap_err()
}

/// When the session record is missing, the unready bind fails with unknown
/// commit status: the activation error must still reach the caller, and the
/// claim must be left to its TTL rather than destroyed — the write may have
/// landed, and deleting could take the recorded sandbox with it.
#[tokio::test(flavor = "multi_thread")]
async fn connect_reports_the_activation_error_when_the_unready_bind_fails() -> Result<()> {
    // The fixture registers the session, but the connect runs as one with no
    // metadata record, so the binding cannot be written.
    let Some((fixture, error)) =
        connect_failing_activation("session-bind-fail", "call-bind-fail", "session-unrecorded")
            .await?
    else {
        return Ok(());
    };

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    // The reported error is the 403 activation failure, not the bind error.
    assert!(error
        .to_string()
        .contains("ensure_active ended after 1 attempt"));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    // The claim was created before the activation failure.
    assert_eq!(fixture.api.creates.lock().len(), 1);
    // Unknown commit status: the claim is not destroyed.
    assert!(fixture.api.deletes.lock().is_empty());
    Ok(())
}

/// A fresh-claim connect whose activation fails must not overwrite a
/// binding written by a concurrent connect: the delayed unready bind
/// applies only while the binding is what this connect observed at start
/// (absent here), and the provably untracked claim is destroyed, not leaked.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_destroys_a_claim_a_concurrent_create_displaced() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-raced-create", None).await? else {
        return Ok(());
    };
    script_concurrent_bind(&fixture, "session-raced-create", "claim-concurrent");

    let error = connect_failing_after_concurrent_write(
        &fixture,
        "session-raced-create",
        "call-raced-create",
    )
    .await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    // The binding the concurrent connect wrote is untouched.
    let binding = bound_sandbox(&fixture.metadata, "session-raced-create").await?;
    assert_eq!(binding.sandbox_id, "claim-concurrent");
    // This connect's claim was the only one created and was never tracked,
    // so it was destroyed: the length guard keeps `[] == []` from passing.
    assert_eq!(fixture.api.creates.lock().len(), 1);
    assert_eq!(
        fixture.api.deletes.lock().as_slice(),
        fixture.api.creates.lock().as_slice()
    );
    Ok(())
}

/// Same race on the replacement path: a connect replacing a vanished
/// ambient sandbox must not overwrite a newer binding, and its provably
/// untracked replacement claim is destroyed instead of leaked.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_destroys_a_claim_a_concurrent_replacement_displaced() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-raced-replace", Some("claim-gone")).await?
    else {
        return Ok(());
    };
    let replacement = crate::lifecycle::claim_name("call-raced-replace");
    script_concurrent_bind(&fixture, "session-raced-replace", "claim-concurrent");
    // First get: the ambient sandbox is gone. Second get: the replacement's
    // activation fails with a non-not-found error.
    fixture.api.scripted_gets.lock().push_back(None);

    let error = connect_failing_after_concurrent_write(
        &fixture,
        "session-raced-replace",
        "call-raced-replace",
    )
    .await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    assert_eq!(
        fixture.api.creates.lock().as_slice(),
        [replacement.as_str()]
    );
    // The binding moved on to the concurrent connect's sandbox; the stale
    // ambient ID this connect observed was not overwritten.
    let binding = bound_sandbox(&fixture.metadata, "session-raced-replace").await?;
    assert_eq!(binding.sandbox_id, "claim-concurrent");
    // The replacement claim was the only one created and was never
    // tracked, so it was destroyed: the length guard keeps `[] == []`
    // from passing.
    assert_eq!(fixture.api.creates.lock().len(), 1);
    assert_eq!(
        fixture.api.deletes.lock().as_slice(),
        fixture.api.creates.lock().as_slice()
    );
    Ok(())
}

/// A duplicate of this same call may have adopted the claim (create treats
/// 409 as success) and bound it: the post-skip re-read must then spare the
/// claim, even though the conditional bind itself was skipped.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_spared_a_claim_the_session_binding_records() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-adopted", None).await? else {
        return Ok(());
    };
    let claim = crate::lifecycle::claim_name("call-adopted");
    // The duplicate binds the very claim this connect created.
    script_concurrent_bind(&fixture, "session-adopted", &claim);

    let error =
        connect_failing_after_concurrent_write(&fixture, "session-adopted", "call-adopted").await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    // The delayed bind was skipped (the binding is not absent any more),
    // but the re-read finds the claim recorded, so it is not destroyed.
    assert_eq!(fixture.api.creates.lock().as_slice(), [claim.as_str()]);
    assert!(fixture.api.deletes.lock().is_empty());
    let binding = bound_sandbox(&fixture.metadata, "session-adopted").await?;
    assert_eq!(binding.sandbox_id, claim);
    Ok(())
}

/// The post-skip re-read checks the raw stored value, not the usable
/// binding: a present-but-unusable binding that still names this claim
/// proves the session tracks it, so the claim is spared from destruction.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_spared_a_claim_an_unusable_binding_records() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-adopted-unusable", None).await? else {
        return Ok(());
    };
    let claim = crate::lifecycle::claim_name("call-adopted-unusable");
    // A duplicate of this call adopted the claim but wrote an unusable
    // binding (unknown version) naming it.
    script_concurrent_binding_value(
        &fixture,
        "session-adopted-unusable",
        json!({"version": 2, "sandbox_id": claim}),
    );

    let error = connect_failing_after_concurrent_write(
        &fixture,
        "session-adopted-unusable",
        "call-adopted-unusable",
    )
    .await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    assert_eq!(fixture.api.creates.lock().as_slice(), [claim.as_str()]);
    // The raw re-read finds the unusable binding naming this claim.
    assert!(fixture.api.deletes.lock().is_empty());
    Ok(())
}

/// An inconclusive binding re-read must not destroy state: when the stored
/// binding cannot be decoded, the claim is left alone.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_spared_a_claim_the_binding_recheck_cannot_read() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-recheck-fail", None).await? else {
        return Ok(());
    };
    // A concurrent writer leaves a value the re-read cannot decode as a
    // binding, so the skip is confirmed but the re-read is inconclusive.
    script_concurrent_binding_value(&fixture, "session-recheck-fail", json!(42));

    let error = connect_failing_after_concurrent_write(
        &fixture,
        "session-recheck-fail",
        "call-recheck-fail",
    )
    .await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    assert_eq!(fixture.api.creates.lock().len(), 1);
    // The re-read failed, so the claim is not destroyed.
    assert!(fixture.api.deletes.lock().is_empty());
    Ok(())
}

/// Destroying the untracked claim is best effort: a failing release is
/// logged, and the caller still sees the original activation error.
#[tokio::test(flavor = "multi_thread")]
async fn connect_reports_the_activation_error_when_destroying_the_untracked_claim_fails(
) -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-destroy-fail", None).await? else {
        return Ok(());
    };
    script_concurrent_bind(&fixture, "session-destroy-fail", "claim-concurrent");
    fixture
        .api
        .delete_errors
        .lock()
        .push_back(scripted_delete_error());

    let error = connect_failing_after_concurrent_write(
        &fixture,
        "session-destroy-fail",
        "call-destroy-fail",
    )
    .await;

    // The reported error is the activation failure ("ensure_active ended
    // after 1 attempt"), not the release failure ("delete ended after…
    // scripted delete rejection").
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error
        .to_string()
        .contains("ensure_active ended after 1 attempt"));
    assert!(!error.to_string().contains("delete ended"));
    assert!(!error.to_string().contains("scripted delete rejection"));
    // The destroy was attempted against this connect's claim only: the
    // length guard keeps `[] == []` from passing.
    assert_eq!(fixture.api.creates.lock().len(), 1);
    assert_eq!(
        fixture.api.deletes.lock().as_slice(),
        fixture.api.creates.lock().as_slice()
    );
    // The concurrent connect's binding is untouched.
    let binding = bound_sandbox(&fixture.metadata, "session-destroy-fail").await?;
    assert_eq!(binding.sandbox_id, "claim-concurrent");
    Ok(())
}

/// A present-but-unusable stored binding is carried as observed, not
/// conflated with an absent one: the conditional bind still replaces it,
/// so a fresh claim with a failing activation is not needlessly destroyed.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_id_replaces_an_unusable_binding_it_observed() -> Result<()> {
    let Some(fixture) = ToolsetFixture::start("session-unusable", None).await? else {
        return Ok(());
    };
    fixture
        .metadata
        .replace_tool_context_value(
            ToolContextEntry {
                session_id: &harnx_core::session_identity::session_key(
                    Some("coder"),
                    "session-unusable",
                ),
                key: SANDBOX_CONTEXT_KEY,
            },
            json!({"version": 2, "sandbox_id": "claim-unusable"}),
        )
        .await?;

    // No concurrent writer this time: the bind must observe the same
    // unusable value it saw at resolve time and replace it.
    let error =
        connect_failing_after_concurrent_write(&fixture, "session-unusable", "call-unusable").await;

    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("scripted Kubernetes error"));
    // The claim is tracked by the replaced binding and must not be destroyed.
    let binding = bound_sandbox(&fixture.metadata, "session-unusable").await?;
    assert_eq!(binding.version, 1);
    let created = fixture.api.creates.lock().clone();
    assert_eq!(created.as_slice(), [binding.sandbox_id.as_str()]);
    assert!(fixture.api.deletes.lock().is_empty());
    Ok(())
}
