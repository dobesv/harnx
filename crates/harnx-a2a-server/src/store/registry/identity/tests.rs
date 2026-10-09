use super::*;
use crate::{exports::AgentCardMeta, store::DedupeKey};

fn fixture() -> (Export, RecoveryRegistration) {
    let export = Export {
        agent: "runner".into(),
        public_name: "public".into(),
        cluster: Some("cluster".into()),
        card_meta: AgentCardMeta {
            name: "runner".into(),
            description: String::new(),
            version: "1".into(),
            conversation_starters: vec![],
        },
        lookup_keys: vec!["public".into()],
    };
    let allocation = TaskAllocation::new(&export.agent, "context".into());
    let first = FirstMessageReservation {
        identity: DedupeKey {
            cluster: "cluster".into(), export: "public".into(), owner: Some("alice".into()), message_id: "message".into(),
        },
        fingerprint: String::new(),
        message: serde_json::from_value(serde_json::json!({"messageId":"message","role":"ROLE_USER","parts":[{"text":"Hello"}]})).unwrap(),
        allocation: allocation.clone(),
    };
    let record = RecoveryRegistration {
        agent: export.agent.clone(),
        export: export.public_name.clone(),
        owner: Some("alice".into()),
        allocation,
        first: Some(first),
    };
    (export, record)
}

#[test]
fn recovery_first_identity_rejects_wrong_owner_export_cluster_and_allocation() {
    let (export, original) = fixture();
    assert!(original.validate_export(&export).is_ok());
    let mut changed = original.clone();
    changed.owner = Some("bob".into());
    assert!(changed.validate_export(&export).is_err());
    changed = original.clone();
    changed.first.as_mut().unwrap().identity.export = "other".into();
    assert!(changed.validate_export(&export).is_err());
    changed = original.clone();
    changed.first.as_mut().unwrap().identity.cluster = "other".into();
    assert!(changed.validate_export(&export).is_err());
    changed = original.clone();
    changed.first.as_mut().unwrap().allocation.prompt_id = "other".into();
    assert!(changed.validate_export(&export).is_err());
    changed = original;
    changed.export = "other".into();
    assert!(changed.validate_export(&export).is_err());
}

#[test]
fn recovery_registry_conflict_checks_original_owner_but_allows_followup_allocation() {
    let (_, original) = fixture();
    let mut followup = original.clone();
    followup.first = None;
    followup.allocation =
        TaskAllocation::new(&original.agent, original.allocation.local_id.clone());
    assert!(original.identity_matches(&followup));
    followup.owner = Some("bob".into());
    assert!(!original.identity_matches(&followup));
    followup.owner = None;
    assert!(!original.identity_matches(&followup));
    followup = original.clone();
    followup.allocation.storage_key = "other".into();
    assert!(!original.identity_matches(&followup));
}
