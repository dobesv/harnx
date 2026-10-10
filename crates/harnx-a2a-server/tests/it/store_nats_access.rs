//! Access rules must authorize before reading or migrating a session's task index.
use super::store_nats::{bind, create, export, snapshot, start_store};
use anyhow::Result;
use axum::http::HeaderMap;
use chrono::Utc;
use harnx_a2a_server::{
    exports::Export,
    identity::{Identity, Principal, RequestIdentity},
    store::{parse_task_id, A2aStore, TaskRecord},
};
use harnx_core::{
    access_rules::AccessRules, session_identity::session_key, user_aliases::UserAliases,
};
use harnx_runtime::nats_session_metadata::{
    a2a_task_key, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::sync::Arc;

async fn seed_legacy_task(metadata: &SessionMetadataStore, key: &str) -> Result<String> {
    let task = snapshot("access_index");
    let task_id = task.id.clone();
    let (_, uuid) = parse_task_id(&task_id)?;
    let record = TaskRecord {
        version: 1,
        task,
        user_msg_id: "user-msg".into(),
        user_msg_seq: 42,
        execution_id: "execution".into(),
        revision: 1,
        stream_seq: 0,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    metadata
        .put_a2a_task(
            &a2a_task_key(key, uuid),
            serde_json::to_vec(&record)?.into(),
        )
        .await?;
    Ok(task_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_index_authorizes_before_migration_and_keeps_admin_scope() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [pkg/agent@test-cluster]\n    users: [alice, bob]\n  - agents: [pkg/agent@test-cluster]\n    users: [admin]\n    scopes: [admin]\n",
    )?);
    let store = A2aStore::new_with_access_rules(metadata.clone(), Some(rules.clone()));
    assert!(std::ptr::eq(store.access_rules().unwrap(), rules.as_ref()));
    let export = export("pkg/agent");
    let local_id = "access_index";
    let key = bind(&metadata, &store, &export, local_id).await?;
    let task_id = seed_legacy_task(&metadata, &key).await?;

    // A denied lookup must not trigger the legacy full-bucket scan or create an index.
    for user in [Principal::User("bob".into()), Principal::Anonymous] {
        assert!(store
            .list_task_index(&export, &user.clone().into(), local_id)
            .await?
            .is_none());
        assert!(store
            .get_task_for_export(&export, &user.clone().into(), &task_id)
            .await?
            .is_none());
        assert!(metadata.get_a2a_task_index(&key).await?.is_none());
    }

    let admin = Principal::User("admin".into());
    let (_, index) = store
        .list_task_index(&export, &admin.clone().into(), local_id)
        .await?
        .unwrap();
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].task_id, task_id);
    assert!(store
        .get_task_for_export(&export, &admin.clone().into(), &task_id)
        .await?
        .is_some());
    assert_eq!(
        store.get_binding(&key).await?.unwrap().owner.as_deref(),
        Some("alice")
    );

    // Index presence cannot turn strict ownership off when no policy is configured.
    let strict = A2aStore::new(metadata);
    assert!(strict.access_rules().is_none());
    assert!(strict
        .list_task_index(&export, &admin.clone().into(), local_id)
        .await?
        .is_none());
    assert!(strict
        .list_task_index(&export, &Principal::User("alice".into()).into(), local_id)
        .await?
        .is_some());
    Ok(())
}

struct OwnerMatrixFixture<'a> {
    metadata: &'a SessionMetadataStore,
    identity: &'a Identity,
    store: A2aStore,
    export: Export,
}

impl OwnerMatrixFixture<'_> {
    async fn assert_case(
        &self,
        local_id: &str,
        case: &(Option<&str>, Option<&str>, bool),
        enabled: bool,
    ) -> Result<()> {
        let (caller, stored, strict_allowed) = *case;
        let expected = strict_allowed && (!enabled || caller.is_some());
        let key = session_key(Some(&self.export.agent), local_id);
        self.metadata
            .create(&SessionMetadata::new(
                local_id,
                SessionInitializer::named(&self.export.agent, Default::default()),
            ))
            .await?
            .unwrap();
        let principal = stored
            .map(|user| Principal::User(user.into()))
            .unwrap_or(Principal::Anonymous);
        self.store
            .bind_context(&key, &self.export, &principal)
            .await?;
        let task_id = create(&self.store, &key, snapshot(local_id)).await?.task.id;
        let request = match caller {
            Some(user) => {
                let mut headers = HeaderMap::new();
                headers.insert("x-user", user.parse()?);
                self.identity.resolve_request(&headers).unwrap()
            }
            None => RequestIdentity::from(Principal::Anonymous),
        };
        assert_eq!(
            self.store
                .resolve_context(&self.export, &request, local_id)
                .await?
                .is_some(),
            expected,
            "context {local_id} {caller:?} {stored:?}"
        );
        assert_eq!(
            self.store
                .get_task_for_export(&self.export, &request, &task_id)
                .await?
                .is_some(),
            expected,
            "task {local_id} {caller:?} {stored:?}"
        );
        assert_eq!(
            self.store
                .list_task_index(&self.export, &request, local_id)
                .await?
                .is_some(),
            expected,
            "list {local_id} {caller:?} {stored:?}"
        );
        assert_eq!(
            self.store
                .get_binding(&key)
                .await?
                .unwrap()
                .owner
                .as_deref(),
            stored
        );
        Ok(())
    }
}

/// Exercise caller-only alias access with and without rules, preserving raw owners.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_nats_context_and_task_owner_matrix() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let aliases = Arc::new(UserAliases::from_yaml(
        "- name: Display\n  identities: [alice, bob, bob]\n- name: Overlap\n  identities: [bob, carol]\n",
    )?);
    let identity = Identity::new(&["x-user".into()])?.with_user_aliases(Some(aliases));
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [pkg/agent@test-cluster]\n    users: [bob, unknown]\n",
    )?);
    let cases = [
        (Some("alice"), Some("bob"), true),
        (Some("bob"), Some("alice"), true),
        (Some("alice"), Some("alice"), true),
        (Some("alice"), Some("carol"), false),
        (Some("carol"), Some("alice"), false),
        (Some("carol"), Some("bob"), true),
        (Some("alice"), Some("unrelated"), false),
        (Some("unknown"), Some("unknown"), true),
        (Some("unknown"), Some("alice"), false),
        (Some("Display"), Some("alice"), false),
        (Some("alice"), None, false),
        (None, Some("alice"), false),
        (None, None, true),
    ];
    for enabled in [false, true] {
        let fixture = OwnerMatrixFixture {
            metadata: &metadata,
            identity: &identity,
            store: A2aStore::new_with_access_rules(
                metadata.clone(),
                enabled.then(|| rules.clone()),
            ),
            export: export("pkg/agent"),
        };
        for (index, case) in cases.iter().enumerate() {
            let local_id = format!("alias_{}_{}", enabled, index);
            fixture.assert_case(&local_id, case, enabled).await?;
        }
    }
    Ok(())
}
