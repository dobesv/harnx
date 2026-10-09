//! Session-scoped A2A task store.
//!
//! Persists A2A state in the harnx session's KV namespace so the existing GC removes it.
//! Keys use the storage key (SHA-256 of agent/sid), not the local session id shown to clients.
//!
//! Key layout:
//! - `sessions/{storage_key}/meta` - session metadata (includes `dev.harnx.a2a` binding)
//! - `sessions/{storage_key}/a2a/tasks/{uuid}` - task records
//! - `sessions/{storage_key}/a2a/index` - per-session task index for efficient listing
//! - `sessions/{storage_key}/a2a/messages/{sha256}` - message dedupe records
//!
//! Task ID format: `{local_id}.{uuid}` - local_id has no `.` per base64url alphabet.

use crate::{exports::Export, identity::RequestIdentity};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session_metadata::SessionMetadataStore;
use std::sync::Arc;

mod binding;
mod dedupe;
mod ids;
mod index;
mod records;

pub use binding::{validate_binding, A2aBinding, A2A_BINDING_NAMESPACE, A2A_BINDING_VERSION};
pub use dedupe::{
    create_dedupe_lru, message_fingerprint, message_id_hash, DedupeEntry, DedupeKey, DedupeLru,
};
pub use ids::{assert_local_id_no_dot, format_task_id, new_task_id, parse_task_id};
pub use index::{to_index_state, IndexState, DANGLING_TASK_GRACE_PERIOD};
pub use records::{TaskChanges, TaskRecord, TaskSeed, TaskVersion};

/// Errors callers can map without matching broker error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    FingerprintMismatch,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "task not found",
            Self::FingerprintMismatch => "messageId reused with different parts",
        })
    }
}
impl std::error::Error for StoreError {}

/// Authorization scope for a context, derived from the resolved export.
#[derive(Clone, Copy)]
pub struct ContextAccess<'a> {
    pub export: &'a Export,
    pub owner: &'a RequestIdentity,
    pub local_id: &'a str,
}

#[derive(Clone, Copy)]
pub struct TaskAccess<'a> {
    pub export: &'a Export,
    pub owner: &'a RequestIdentity,
    pub task_id: &'a str,
}

#[derive(Clone, Copy)]
pub struct MessageIdentity<'a> {
    pub message_id: &'a str,
    pub fingerprint: &'a str,
}

/// A2A task store operations.
pub struct A2aStore {
    store: SessionMetadataStore,
    dedupe_lru: DedupeLru,
    access_rules: Option<Arc<AccessRules>>,
}

impl A2aStore {
    /// Create a new A2A store wrapper.
    pub fn new(store: SessionMetadataStore) -> Self {
        Self::new_with_access_rules(store, None)
    }

    /// Use the same rules for every context lookup, including runner operations.
    pub fn new_with_access_rules(
        store: SessionMetadataStore,
        access_rules: Option<Arc<AccessRules>>,
    ) -> Self {
        Self {
            store,
            dedupe_lru: create_dedupe_lru(),
            access_rules,
        }
    }

    pub fn access_rules(&self) -> Option<&AccessRules> {
        self.access_rules.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exports::Export,
        identity::{Principal, RequestIdentity},
    };
    use chrono::Utc;

    #[test]
    fn task_id_format_and_parse() {
        harnx_core::require_nextest();
        let local_id = "abc123"; // base64url, no '.'
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let task_id = format_task_id(local_id, uuid);
        assert_eq!(task_id, "abc123.01234567-89ab-cdef-0123-456789abcdef");

        let (parsed_local, parsed_uuid) = parse_task_id(&task_id).unwrap();
        assert_eq!(parsed_local, local_id);
        assert_eq!(parsed_uuid, uuid);
    }

    enum NoncanonicalUuid {
        Uppercase,
        Simple,
        Braced,
        Urn,
    }

    fn assert_noncanonical_uuid_rejected(variant: NoncanonicalUuid) {
        harnx_core::require_nextest();
        let uuid = match variant {
            NoncanonicalUuid::Uppercase => "01234567-89AB-CDEF-0123-456789ABCDEF",
            NoncanonicalUuid::Simple => "0123456789abcdef0123456789abcdef",
            NoncanonicalUuid::Braced => "{01234567-89ab-cdef-0123-456789abcdef}",
            NoncanonicalUuid::Urn => "urn:uuid:01234567-89ab-cdef-0123-456789abcdef",
        };
        // These are valid UUID spellings, but aren't valid storage key suffixes.
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
        let error = parse_task_id(&format_task_id("abc123", uuid)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "task UUID must be lowercase and hyphenated"
        );
    }

    #[test]
    fn task_id_rejects_uppercase_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Uppercase);
    }

    #[test]
    fn task_id_rejects_simple_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Simple);
    }

    #[test]
    fn task_id_rejects_braced_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Braced);
    }

    #[test]
    fn task_id_rejects_urn_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Urn);
    }

    #[test]
    fn task_id_rejects_dot_in_local_id() {
        harnx_core::require_nextest();
        assert!(parse_task_id("abc.123.uuid").is_err());
        let result = assert_local_id_no_dot("abc.123");
        assert!(result.is_err());
    }

    #[test]
    fn assert_local_id_no_dot_passes() {
        harnx_core::require_nextest();
        assert_local_id_no_dot("abc123").unwrap();
        assert_local_id_no_dot("azAZ09-_").unwrap();
    }

    #[test]
    fn assert_local_id_no_dot_fails() {
        harnx_core::require_nextest();
        assert!(assert_local_id_no_dot("abc.123").is_err());
    }

    #[test]
    fn new_task_id_format() {
        harnx_core::require_nextest();
        let local_id = "testSid";
        let task_id = new_task_id(local_id);
        assert!(task_id.starts_with("testSid."));
        assert!(uuid::Uuid::parse_str(&task_id[8..]).is_ok());
    }

    #[test]
    fn message_fingerprint_is_deterministic() {
        harnx_core::require_nextest();
        let parts: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("hello")];
        let fp1 = message_fingerprint(&parts);
        let fp2 = message_fingerprint(&parts);
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn message_fingerprint_differs_on_change() {
        harnx_core::require_nextest();
        let parts1: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("hello")];
        let parts2: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("world")];
        let fp1 = message_fingerprint(&parts1);
        let fp2 = message_fingerprint(&parts2);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn message_fingerprint_sorts_nested_objects_but_preserves_part_order() {
        harnx_core::require_nextest();
        let a: a2a_lf::Part =
            serde_json::from_str(r#"{"data":{"z":1,"a":{"y":2,"b":3}}}"#).unwrap();
        let b: a2a_lf::Part =
            serde_json::from_str(r#"{"data":{"a":{"b":3,"y":2},"z":1}}"#).unwrap();
        assert_eq!(
            message_fingerprint(std::slice::from_ref(&a)),
            message_fingerprint(&[b])
        );
        assert_ne!(
            message_fingerprint(&[a.clone(), a2a_lf::Part::text("text")]),
            message_fingerprint(&[a2a_lf::Part::text("text"), a])
        );
    }

    fn binding_fixture() -> (A2aBinding, Export) {
        let binding = A2aBinding {
            version: A2A_BINDING_VERSION,
            export: "public-agent".into(),
            agent: "pkg/agent".into(),
            cluster: "local".into(),
            owner: Some("alice".into()),
            created_at: Utc::now(),
        };
        let export = Export {
            public_name: binding.export.clone(),
            agent: binding.agent.clone(),
            cluster: Some(binding.cluster.clone()),
            card_meta: crate::exports::AgentCardMeta {
                name: "Test".into(),
                description: "Test agent".into(),
                version: "1.0".into(),
                conversation_starters: vec![],
            },
            lookup_keys: vec![binding.export.clone()],
        };
        (binding, export)
    }

    struct BindingCase {
        public_name: &'static str,
        owner: &'static str,
        matches: bool,
    }

    const BINDING_CASES: [BindingCase; 3] = [
        BindingCase {
            public_name: "public-agent",
            owner: "alice",
            matches: true,
        },
        BindingCase {
            public_name: "other-agent",
            owner: "alice",
            matches: false,
        },
        BindingCase {
            public_name: "public-agent",
            owner: "bob",
            matches: false,
        },
    ];

    fn assert_binding_case(case: &BindingCase) {
        harnx_core::require_nextest();
        let (binding, mut export) = binding_fixture();
        export.public_name = case.public_name.into();
        assert_eq!(
            validate_binding(
                &binding,
                &export,
                &Principal::User(case.owner.into()).into(),
                None
            ),
            case.matches
        );
    }
    #[test]
    fn access_validate_binding_scopes_preserve_export_version_agent_cluster_checks() {
        harnx_core::require_nextest();
        let rules = AccessRules::from_yaml(
            "rules:\n  - agents: [pkg/agent@local]\n    users: [alice, bob]\n  - agents: [pkg/agent@local]\n    users: [admin]\n    scopes: [admin]\n",
        ).unwrap();
        let (binding, export) = binding_fixture();
        let alice: RequestIdentity = Principal::User("alice".into()).into();
        let bob: RequestIdentity = Principal::User("bob".into()).into();
        let admin: RequestIdentity = Principal::User("admin".into()).into();
        assert!(validate_binding(&binding, &export, &alice, Some(&rules)));
        assert!(!validate_binding(&binding, &export, &bob, Some(&rules)));
        assert!(validate_binding(&binding, &export, &admin, Some(&rules)));
        assert!(!validate_binding(&binding, &export, &admin, None));
        let mut legacy = binding.clone();
        legacy.owner = None;
        assert!(!validate_binding(&legacy, &export, &alice, Some(&rules)));
        assert!(validate_binding(&legacy, &export, &admin, Some(&rules)));
        assert!(validate_binding(
            &legacy,
            &export,
            &Principal::Anonymous.into(),
            None
        ));
        assert!(!validate_binding(
            &legacy,
            &export,
            &Principal::Anonymous.into(),
            Some(&rules)
        ));
        for field in ["version", "export", "agent", "cluster"] {
            let mut wrong = binding.clone();
            match field {
                "version" => wrong.version += 1,
                "export" => wrong.export = "other-export".into(),
                "agent" => wrong.agent = "other-agent".into(),
                "cluster" => wrong.cluster = "other-cluster".into(),
                _ => unreachable!(),
            }
            assert!(
                !validate_binding(&wrong, &export, &admin, Some(&rules)),
                "{field}"
            );
        }
    }

    #[test]
    fn validate_binding_matches() {
        assert_binding_case(&BINDING_CASES[0]);
    }

    #[test]
    fn validate_binding_wrong_export() {
        assert_binding_case(&BINDING_CASES[1]);
    }

    #[test]
    fn validate_binding_wrong_owner() {
        assert_binding_case(&BINDING_CASES[2]);
    }
    #[test]
    fn memberships_binding_authorization_requires_user_and_never_uses_group_owner() {
        harnx_core::require_nextest();
        let rules = AccessRules::from_yaml(
            "rules:\n  - agents: [pkg/agent@local]\n    groups: [team]\n  - agents: [pkg/agent@local]\n    roles: [supervisor]\n    scopes: [admin]\n",
        ).unwrap();
        let (binding, export) = binding_fixture();
        let mut caller = RequestIdentity {
            principal: Principal::User("bob".into()),
            groups: vec!["alice".into(), "team".into()],
            roles: vec![],
        };
        assert!(!validate_binding(&binding, &export, &caller, Some(&rules)));
        caller.principal = Principal::User("alice".into());
        assert!(validate_binding(&binding, &export, &caller, Some(&rules)));
        caller.groups.clear();
        assert!(!validate_binding(&binding, &export, &caller, Some(&rules)));
        caller.principal = Principal::User("bob".into());
        caller.roles = vec!["supervisor".into()];
        assert!(validate_binding(&binding, &export, &caller, Some(&rules)));
        assert!(!validate_binding(&binding, &export, &caller, None));
        caller.principal = Principal::Anonymous;
        assert!(!validate_binding(&binding, &export, &caller, Some(&rules)));
    }
}
