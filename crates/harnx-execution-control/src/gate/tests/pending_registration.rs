use super::*;

struct PendingChild {
    tool: Operation,
    child: Operation,
    registration: GateRegistration,
}

impl PendingChild {
    async fn new(store: &ExecutionStore, root: &ExecutionContext) -> Result<Self> {
        let tool = store
            .child(
                OperationRef::new("bridge-session", "tool"),
                root.operation().clone(),
            )
            .await?;
        store.claim(&tool.reference, owner(1)).await?;
        let original = store.activate_gate(&tool.reference).await?;
        let child = store
            .session(
                "pending-child",
                Some(tool.reference.clone()),
                Some("child-g1"),
            )
            .await?;
        store.claim(&child.reference, owner(1)).await?;
        let registration = GateRegistration {
            context: WorkRegistration {
                operation: child.reference.clone(),
                kind: OperationKind::Session,
                owner: owner(1),
            }
            .context(&original),
            parent: Some(original),
            kind: OperationKind::Session,
            previous_generation: None,
        };
        // Crash/pause after writing the immutable marker, before its StartWork.
        store
            .mutate(&child.reference, |operation| {
                operation.gate_registration = Some(Box::new(registration.clone()));
                Ok(())
            })
            .await?;
        Ok(Self {
            tool,
            child,
            registration,
        })
    }

    async fn handover_parent(&self, store: &ExecutionStore, root: &ExecutionContext) -> Result<()> {
        store.claim(root.operation(), owner(2)).await?;
        let parent = store
            .gate_context(root.gate_root(), &self.tool.reference)
            .await?;
        store
            .commit_if_admissible(
                &parent,
                action(
                    "tool-handover",
                    GateAction::ReplaceOwner { owner: owner(2) },
                ),
            )
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn pending_child_registration_survives_parent_handover_but_not_stop() -> Result<()> {
    for interrupted in [false, true] {
        let (_server, store, _) = stores().await?;
        let operation = physical_root(&store).await?;
        let root = store.activate_gate(&operation.reference).await?;
        let pending = PendingChild::new(&store, &root).await?;
        let child = &pending.child;
        assert!(store.accepted_stop(&child.reference).await?.is_none());
        pending.handover_parent(&store, &root).await?;
        if interrupted {
            store.interrupt(&scope(&root), "stop").await?;
        }
        let recovered = store.activate_gate(&child.reference).await?;
        let admission = store
            .commit_if_admissible(
                &recovered,
                action("run", GateAction::AdmitWork { input: json!({}) }),
            )
            .await;
        if interrupted {
            assert!(admission.unwrap_err().is::<Interrupted>());
        } else {
            admission?;
        }
        // A later reconciliation must find the same member even though the
        // registration's original parent owner no longer owns the tool.
        assert_eq!(store.activate_gate(&child.reference).await?, recovered);
        assert_eq!(
            store
                .get(&child.reference)
                .await?
                .unwrap()
                .gate_registration
                .unwrap()
                .context,
            pending.registration.context
        );
    }
    Ok(())
}
