//! Conformance test suite for `PlanStore` implementations.
//!
//! This module provides backend-neutral test harness that exercises `PlanStore`
//! trait contract. Any implementation (`FilePlanStore`, `GitHubPlanStore`, etc.) can
//! be validated by wiring it to `run_conformance` with only constructor swap.
//!
//! ## Trait Contract Expectations
//!
//! ### Canonical IDs Returned by Create APIs
//!
//! `add_plan`, `add_task`, and `add_note` return canonical backend IDs. Client-provided
//! IDs in `NewPlan.id`, `NewTask.id`, and `NewNote.id` are advisory: backends may preserve
//! them verbatim or replace them with server-assigned canonical IDs. Follow-up CRUD
//! operations should prefer the returned canonical ID.
//!
//! ### Plans Are Also Addressable by Client-Provided ID
//!
//! Plan operations MUST accept the client-provided `NewPlan.id` in place of the
//! canonical ID, because MCP callers name plans rather than numbering them. Backends
//! that canonicalize IDs are responsible for resolving the client ID back to the plan
//! (the GitHub backend scans plan issues for matching `client_id` front-matter).
//! Duplicates resolve the same way reads do: most recently updated wins.
//!
//! ### Duplicate ID Resolution (Read-Side Deduplication)
//!
//! When multiple resources share same ID (e.g., two plans with ID "plan-a"),
//! `list_plans` / `list_tasks` / `list_notes` methods MUST return at most one entry
//! per normalized ID. Tie-breaker is `updated_at`: most recently updated resource wins.
//! If `updated_at` is `None`, treat it as older than any timestamp.
//!
//! ### Delete Semantics
//!
//! Backends may either delete permanently or close/soft-delete resources.
//! `BackendCapabilities::deletes_permanently` selects expected contract in this suite:
//!
//! - `true`: `delete_*` removes resource; subsequent `get_*` returns `StoreError::NotFound`.
//! - `false`: delete closes resource. Closed resources disappear from default
//!   `list_plans` / `list_tasks`. Direct readability of closed resources is
//!   backend-defined and not asserted here. Note deletion still returns
//!   `StoreError::NotFound` because GitHub comments are deleted permanently.
//!
//! ### Error Mapping
//!
//! - `NotFound`: requested resource does not exist.
//! - `AlreadyExists`: attempted to create resource with ID that already exists
//!   (if backend enforces uniqueness at write time; GitHub may not).
//! - `InvalidId`: provided ID contains invalid characters or is empty after
//!   normalization.
//!
//! ## Usage
//!
//! ```ignore
//! use harnx_mcp_plans_core::conformance::{run_conformance, BackendCapabilities};
//! use harnx_mcp_plans::FilePlanStore;
//!
//! #[tokio::test]
//! async fn file_store_conformance() {
//!     let temp_dir = tempfile::tempdir().unwrap();
//!     let store = FilePlanStore::new(temp_dir.path().to_path_buf());
//!     run_conformance(
//!         store,
//!         BackendCapabilities {
//!             preserves_client_id: true,
//!             deletes_permanently: true,
//!         },
//!     )
//!     .await;
//! }
//! ```

use std::sync::Arc;

use crate::{
    NewNote, NewPlan, NewTask, NoteMetaUpdate, PageToken, Plan, PlanMetaUpdate, PlanStore,
    StoreError, Task, TaskFilter, TaskMetaUpdate,
};

#[derive(Debug, Clone, Copy)]
pub struct BackendCapabilities {
    pub preserves_client_id: bool,
    pub deletes_permanently: bool,
    pub rejects_invalid_create_ids: bool,
}

/// Run full conformance test suite against `PlanStore` implementation.
///
/// This function asserts trait contract for all CRUD operations, pagination,
/// body edits, and error handling. It is backend-neutral and does not make
/// filesystem-path assumptions.
///
/// # Panics
///
/// Panics on any contract violation with descriptive message.
pub async fn run_conformance<S: PlanStore>(store: Arc<S>, caps: BackendCapabilities) {
    test_plan_crud(&store, caps).await;
    test_plan_list_pagination(&store).await;
    test_plan_duplicate_id(&store).await;

    test_task_crud(&store, caps).await;
    test_task_filtering(&store).await;
    test_task_list_pagination(&store).await;
    test_task_dependencies(&store).await;
    test_cross_plan_isolation(&store).await;

    test_note_crud(&store, caps).await;
    test_note_list_pagination(&store).await;

    test_body_edits(&store).await;

    test_not_found_errors(&store).await;
    test_already_exists_errors(&store).await;
    test_invalid_id_errors(&store, caps).await;
}

fn assert_preserved_id(caps: BackendCapabilities, returned_id: &str, client_id: &str, kind: &str) {
    if caps.preserves_client_id {
        assert_eq!(
            returned_id, client_id,
            "{kind} should preserve client-provided ID"
        );
    } else {
        assert_ne!(returned_id, "", "{kind} returned ID should be non-empty");
    }
}

async fn assert_deleted_plan_contract<S: PlanStore>(
    store: &Arc<S>,
    id: &str,
    caps: BackendCapabilities,
) {
    if caps.deletes_permanently {
        let err = store
            .get_plan(&crate::model::Target::Local, &id.to_string())
            .await
            .expect_err("get_plan on deleted plan should fail");
        assert!(
            matches!(err, StoreError::NotFound),
            "expected NotFound after delete"
        );
    } else {
        let listed = collect_all_plans(store).await;
        assert!(
            listed.iter().all(|plan| plan.id != id),
            "closed plan should not appear in default list_plans results"
        );
    }
}

async fn assert_deleted_task_contract<S: PlanStore>(
    store: &Arc<S>,
    plan_id: &str,
    task_id: &str,
    caps: BackendCapabilities,
) {
    if caps.deletes_permanently {
        let err = store
            .get_task(
                &crate::model::Target::Local,
                &plan_id.to_string(),
                &task_id.to_string(),
            )
            .await
            .expect_err("get_task on deleted task should fail");
        assert!(
            matches!(err, StoreError::NotFound),
            "expected NotFound after delete"
        );
    } else {
        let listed = collect_all_tasks(store, plan_id).await;
        assert!(
            listed.iter().all(|task| task.id != task_id),
            "closed task should not appear in default list_tasks results"
        );
    }
}

async fn collect_all_plans<S: PlanStore>(store: &Arc<S>) -> Vec<Plan> {
    let mut items = Vec::new();
    let mut next: Option<PageToken> = None;
    loop {
        let page = store
            .list_plans(&crate::model::Target::Local, next.clone())
            .await
            .expect("list_plans should succeed");
        items.extend(page.items);
        match page.next {
            Some(token) => next = Some(token),
            None => break,
        }
    }
    items
}

async fn collect_all_tasks<S: PlanStore>(store: &Arc<S>, plan_id: &str) -> Vec<Task> {
    let mut items = Vec::new();
    let mut next: Option<PageToken> = None;
    loop {
        let page = store
            .list_tasks(
                &crate::model::Target::Local,
                &plan_id.to_string(),
                TaskFilter::default(),
                next.clone(),
            )
            .await
            .expect("list_tasks should succeed");
        items.extend(page.items);
        match page.next {
            Some(token) => next = Some(token),
            None => break,
        }
    }
    items
}

/// Assert plan operations accept the client-provided ID, not only the canonical one.
async fn assert_client_id_addresses_plan<S: PlanStore>(
    store: &Arc<S>,
    canonical_id: &str,
    client_id: &str,
) {
    let target = crate::model::Target::Local;
    let fetched = store
        .get_plan(&target, &client_id.to_string())
        .await
        .expect("get_plan by client-provided ID should succeed");
    assert_eq!(
        fetched.id, canonical_id,
        "client-provided ID must resolve to the same plan as the canonical ID"
    );
    store
        .read_plan_body(&target, &client_id.to_string())
        .await
        .expect("read_plan_body by client-provided ID should succeed");
}

async fn test_plan_crud<S: PlanStore>(store: &Arc<S>, caps: BackendCapabilities) {
    let plan1 = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "auto-gen-test-plan".to_string(),
                title: Some("Auto-generated ID plan".to_string()),
                ..NewPlan::default()
            },
        )
        .await
        .expect("add_plan with provided ID should succeed");
    assert!(!plan1.id.is_empty(), "plan ID should be non-empty");
    assert!(plan1.title.as_deref() == Some("Auto-generated ID plan"));

    let client_id = "custom-plan-id";
    let plan2 = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: client_id.to_string(),
                title: Some("Custom ID plan".to_string()),
                summary: Some("Plan summary".to_string()),
                author: Some("hestia".to_string()),
                ..NewPlan::default()
            },
        )
        .await
        .expect("add_plan with custom ID should succeed");
    assert_preserved_id(caps, &plan2.id, client_id, "plan");
    assert_eq!(plan2.summary.as_deref(), Some("Plan summary"));
    assert_eq!(plan2.author.as_deref(), Some("hestia"));

    let fetched = store
        .get_plan(&crate::model::Target::Local, &plan2.id)
        .await
        .expect("get_plan should succeed");
    assert_eq!(fetched.id, plan2.id);
    assert_eq!(fetched.title, plan2.title);
    assert_client_id_addresses_plan(store, &plan2.id, client_id).await;

    let updated = store
        .update_plan_meta(
            &crate::model::Target::Local,
            &plan2.id,
            PlanMetaUpdate {
                title: Some("Updated title".to_string()),
                assignee: Some("atlas".to_string()),
                ..PlanMetaUpdate::default()
            },
        )
        .await
        .expect("update_plan_meta should succeed");
    assert_eq!(updated.title.as_deref(), Some("Updated title"));
    assert_eq!(updated.assignee.as_deref(), Some("atlas"));

    store
        .write_plan_body(&crate::model::Target::Local, &plan2.id, "Plan body content")
        .await
        .expect("write_plan_body should succeed");
    let body = store
        .read_plan_body(&crate::model::Target::Local, &plan2.id)
        .await
        .expect("read_plan_body should succeed");
    assert!(
        body.ends_with("Plan body content"),
        "plan body should round-trip body payload, got {body:?}"
    );

    store
        .delete_plan(&crate::model::Target::Local, &plan2.id)
        .await
        .expect("delete_plan should succeed");
    assert_deleted_plan_contract(store, &plan2.id, caps).await;

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan1.id)
        .await;
}

async fn test_plan_list_pagination<S: PlanStore>(store: &Arc<S>) {
    let ids: Vec<_> = (0..5).map(|i| format!("pagination-plan-{}", i)).collect();

    for id in &ids {
        let _ = store.delete_plan(&crate::model::Target::Local, id).await;
    }

    for id in &ids {
        store
            .add_plan(
                &crate::model::Target::Local,
                NewPlan {
                    id: id.clone(),
                    title: Some(format!("Plan {}", id)),
                    ..NewPlan::default()
                },
            )
            .await
            .expect("add_plan should succeed");
    }

    let page1 = store
        .list_plans(&crate::model::Target::Local, None)
        .await
        .expect("list_plans should succeed");
    assert!(!page1.items.is_empty(), "page 1 should have items");

    if let Some(next) = page1.next {
        let page2 = store
            .list_plans(&crate::model::Target::Local, Some(next))
            .await
            .expect("list_plans page 2 should succeed");
        let page1_ids: std::collections::HashSet<_> = page1.items.iter().map(|p| &p.id).collect();
        for plan in &page2.items {
            assert!(
                !page1_ids.contains(&plan.id),
                "pagination pages should not overlap"
            );
        }
    }

    for plan in collect_all_plans(store).await {
        if plan.id.starts_with("pagination-plan-") {
            let _ = store
                .delete_plan(&crate::model::Target::Local, &plan.id)
                .await;
        }
    }
}

async fn test_plan_duplicate_id<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "duplicate-plan-id".to_string(),
                title: Some("First plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("first add_plan should succeed");

    let result = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "duplicate-plan-id".to_string(),
                title: Some("Second plan".to_string()),
                ..Default::default()
            },
        )
        .await;

    if let Err(err) = result {
        assert!(
            matches!(err, StoreError::AlreadyExists),
            "expected AlreadyExists for duplicate plan, got {:?}",
            err
        );
    }

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_task_crud<S: PlanStore>(store: &Arc<S>, caps: BackendCapabilities) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "task-crud-plan".to_string(),
                title: Some("Task CRUD plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    let task1 = store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "auto-gen-test-task".to_string(),
                title: "Auto-task".to_string(),
                ..NewTask::default()
            },
        )
        .await
        .expect("add_task with provided ID should succeed");
    assert!(!task1.id.is_empty());
    assert_eq!(task1.plan, plan.id);

    let client_id = "custom-task-id";
    let task2 = store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: client_id.to_string(),
                title: "Custom task".to_string(),
                summary: Some("Task summary".to_string()),
                author: Some("apollo".to_string()),
                assignee: None,
                executor: None,
                tags: vec!["backend".to_string()],
                status: None,
                dependencies: vec![],
            },
        )
        .await
        .expect("add_task with custom ID should succeed");
    assert_preserved_id(caps, &task2.id, client_id, "task");
    assert_eq!(task2.summary.as_deref(), Some("Task summary"));
    assert_eq!(task2.author.as_deref(), Some("apollo"));

    let fetched = store
        .get_task(&crate::model::Target::Local, &plan.id, &task2.id)
        .await
        .expect("get_task should succeed");
    assert_eq!(fetched.id, task2.id);
    if caps.preserves_client_id {
        let fetched_by_client_id = store
            .get_task(
                &crate::model::Target::Local,
                &plan.id,
                &client_id.to_string(),
            )
            .await
            .expect("get_task by client-provided ID should succeed when IDs are preserved");
        assert_eq!(fetched_by_client_id.id, client_id);
    }

    let updated = store
        .update_task_meta(
            &crate::model::Target::Local,
            &plan.id,
            &task2.id,
            TaskMetaUpdate {
                title: Some("Updated task title".to_string()),
                summary: None,
                author: None,
                assignee: Some("hephaestus".to_string()),
                executor: None,
                status: Some("in_progress".to_string()),
                dependencies: None,
                tags: None,
            },
        )
        .await
        .expect("update_task_meta should succeed");
    assert_eq!(updated.title, "Updated task title");
    assert_eq!(updated.assignee.as_deref(), Some("hephaestus"));
    assert_eq!(updated.status, "in_progress");

    store
        .write_task_body(
            &crate::model::Target::Local,
            &plan.id,
            &task2.id,
            "Task body",
        )
        .await
        .expect("write_task_body should succeed");
    let body = store
        .read_task_body(&crate::model::Target::Local, &plan.id, &task2.id)
        .await
        .expect("read_task_body should succeed");
    assert!(
        body.ends_with("Task body"),
        "task body should round-trip body payload, got {body:?}"
    );

    store
        .delete_task(&crate::model::Target::Local, &plan.id, &task2.id)
        .await
        .expect("delete_task should succeed");
    assert_deleted_task_contract(store, &plan.id, &task2.id, caps).await;

    let _ = store
        .delete_task(&crate::model::Target::Local, &plan.id, &task1.id)
        .await;
    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_task_filtering<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "task-filter-plan".to_string(),
                title: Some("Task filter plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "task-open".to_string(),
                title: "Open task".to_string(),
                tags: vec!["frontend".to_string()],
                status: Some("open".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_task should succeed");

    store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "task-done".to_string(),
                title: "Done task".to_string(),
                tags: vec!["backend".to_string()],
                status: Some("done".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_task should succeed");

    let open_tasks = store
        .list_tasks(
            &crate::model::Target::Local,
            &plan.id,
            TaskFilter {
                status: Some("open".to_string()),
                tag: None,
            },
            None,
        )
        .await
        .expect("list_tasks with status filter should succeed");
    assert!(open_tasks.items.iter().all(|t| t.status == "open"));

    let backend_tasks = store
        .list_tasks(
            &crate::model::Target::Local,
            &plan.id,
            TaskFilter {
                status: None,
                tag: Some("backend".to_string()),
            },
            None,
        )
        .await
        .expect("list_tasks with tag filter should succeed");
    assert!(backend_tasks
        .items
        .iter()
        .all(|t| t.tags.contains(&"backend".to_string())));

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_task_list_pagination<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "task-pag-plan".to_string(),
                title: Some("Task pagination plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    for i in 0..5 {
        store
            .add_task(
                &crate::model::Target::Local,
                &plan.id,
                NewTask {
                    id: format!("task-pag-{}", i),
                    title: format!("Task {}", i),
                    ..Default::default()
                },
            )
            .await
            .expect("add_task should succeed");
    }

    let page1 = store
        .list_tasks(
            &crate::model::Target::Local,
            &plan.id,
            TaskFilter::default(),
            None,
        )
        .await
        .expect("list_tasks should succeed");
    assert!(!page1.items.is_empty(), "page 1 should have items");

    if let Some(next) = page1.next {
        let page2 = store
            .list_tasks(
                &crate::model::Target::Local,
                &plan.id,
                TaskFilter::default(),
                Some(next),
            )
            .await
            .expect("list_tasks page 2 should succeed");
        let page1_ids: std::collections::HashSet<_> = page1.items.iter().map(|t| &t.id).collect();
        for task in &page2.items {
            assert!(
                !page1_ids.contains(&task.id),
                "pagination pages should not overlap"
            );
        }
    }

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_task_dependencies<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "task-deps-plan".to_string(),
                title: Some("Task deps plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    let task1 = store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "dep-task-1".to_string(),
                title: "Dependency task".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("add_task should succeed");

    let task2 = store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "dep-task-2".to_string(),
                title: "Dependent task".to_string(),
                dependencies: vec![task1.id.clone()],
                ..Default::default()
            },
        )
        .await
        .expect("add_task with dependencies should succeed");

    assert_eq!(task2.dependencies, vec![task1.id.clone()]);

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_cross_plan_isolation<S: PlanStore>(store: &Arc<S>) {
    let plan_a = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "cross-plan-a".to_string(),
                title: Some("Cross plan A".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan A should succeed");
    let plan_b = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "cross-plan-b".to_string(),
                title: Some("Cross plan B".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan B should succeed");

    let task = store
        .add_task(
            &crate::model::Target::Local,
            &plan_a.id,
            NewTask {
                id: "cross-task".to_string(),
                title: "Cross task".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("add_task should succeed");

    let err = store
        .get_task(&crate::model::Target::Local, &plan_b.id, &task.id)
        .await
        .expect_err("get_task with wrong plan should fail");
    assert!(matches!(err, StoreError::NotFound));

    let note = store
        .add_note(
            &crate::model::Target::Local,
            &plan_a.id,
            NewNote {
                id: "cross-note".to_string(),
                summary: Some("Cross note".to_string()),
                author: None,
            },
        )
        .await
        .expect("add_note should succeed");

    let err = store
        .get_note(&crate::model::Target::Local, &plan_b.id, &note.id)
        .await
        .expect_err("get_note with wrong plan should fail");
    assert!(matches!(err, StoreError::NotFound));

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan_a.id)
        .await;
    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan_b.id)
        .await;
}

async fn test_note_crud<S: PlanStore>(store: &Arc<S>, caps: BackendCapabilities) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "note-crud-plan".to_string(),
                title: Some("Note CRUD plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    let note1 = store
        .add_note(
            &crate::model::Target::Local,
            &plan.id,
            NewNote {
                id: "auto-gen-test-note".to_string(),
                summary: Some("Auto note".to_string()),
                author: None,
            },
        )
        .await
        .expect("add_note with provided ID should succeed");
    assert!(!note1.id.is_empty());

    let client_id = "custom-note-id";
    let note2 = store
        .add_note(
            &crate::model::Target::Local,
            &plan.id,
            NewNote {
                id: client_id.to_string(),
                summary: Some("Note summary".to_string()),
                author: Some("artemis".to_string()),
            },
        )
        .await
        .expect("add_note with custom ID should succeed");
    assert_preserved_id(caps, &note2.id, client_id, "note");
    assert_eq!(note2.summary.as_deref(), Some("Note summary"));

    let fetched = store
        .get_note(&crate::model::Target::Local, &plan.id, &note2.id)
        .await
        .expect("get_note should succeed");
    assert_eq!(fetched.id, note2.id);
    if caps.preserves_client_id {
        let fetched_by_client_id = store
            .get_note(
                &crate::model::Target::Local,
                &plan.id,
                &client_id.to_string(),
            )
            .await
            .expect("get_note by client-provided ID should succeed when IDs are preserved");
        assert_eq!(fetched_by_client_id.id, client_id);
    }

    let updated = store
        .update_note_meta(
            &crate::model::Target::Local,
            &plan.id,
            &note2.id,
            NoteMetaUpdate {
                summary: Some("Updated summary".to_string()),
                author: Some("atlas".to_string()),
            },
        )
        .await
        .expect("update_note_meta should succeed");
    assert_eq!(updated.summary.as_deref(), Some("Updated summary"));
    assert_eq!(updated.author.as_deref(), Some("atlas"));

    store
        .write_note_body(
            &crate::model::Target::Local,
            &plan.id,
            &note2.id,
            "Note body",
        )
        .await
        .expect("write_note_body should succeed");
    let body = store
        .read_note_body(&crate::model::Target::Local, &plan.id, &note2.id)
        .await
        .expect("read_note_body should succeed");
    assert!(
        body.ends_with("Note body"),
        "note body should round-trip body payload, got {body:?}"
    );

    store
        .delete_note(&crate::model::Target::Local, &plan.id, &note2.id)
        .await
        .expect("delete_note should succeed");
    let err = store
        .get_note(&crate::model::Target::Local, &plan.id, &note2.id)
        .await
        .expect_err("get_note on deleted note should fail");
    assert!(matches!(err, StoreError::NotFound));

    let _ = store
        .delete_note(&crate::model::Target::Local, &plan.id, &note1.id)
        .await;
    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_note_list_pagination<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "note-pag-plan".to_string(),
                title: Some("Note pagination plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    for i in 0..5 {
        store
            .add_note(
                &crate::model::Target::Local,
                &plan.id,
                NewNote {
                    id: format!("note-pag-{}", i),
                    summary: Some(format!("Note {}", i)),
                    author: None,
                },
            )
            .await
            .expect("add_note should succeed");
    }

    let page1 = store
        .list_notes(&crate::model::Target::Local, &plan.id, None)
        .await
        .expect("list_notes should succeed");
    assert!(!page1.items.is_empty(), "page 1 should have items");

    if let Some(next) = page1.next {
        let page2 = store
            .list_notes(&crate::model::Target::Local, &plan.id, Some(next))
            .await
            .expect("list_notes page 2 should succeed");
        let page1_ids: std::collections::HashSet<_> = page1.items.iter().map(|n| &n.id).collect();
        for note in &page2.items {
            assert!(
                !page1_ids.contains(&note.id),
                "pagination pages should not overlap"
            );
        }
    }

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_body_edits<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "body-edits-plan".to_string(),
                title: Some("Body edits plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    store
        .write_plan_body(
            &crate::model::Target::Local,
            &plan.id,
            "line1\nline2\nline3",
        )
        .await
        .expect("write_plan_body should succeed");
    let body = store
        .read_plan_body(&crate::model::Target::Local, &plan.id)
        .await
        .expect("read_plan_body should succeed");
    assert!(body.ends_with("line1\nline2\nline3"));

    store
        .write_plan_body(
            &crate::model::Target::Local,
            &plan.id,
            "line1\nREPLACED\nline3",
        )
        .await
        .expect("write_plan_body replace should succeed");
    let body = store
        .read_plan_body(&crate::model::Target::Local, &plan.id)
        .await
        .expect("read_plan_body should succeed");
    assert!(body.ends_with("line1\nREPLACED\nline3"));

    store
        .write_plan_body(
            &crate::model::Target::Local,
            &plan.id,
            "line1\nREPLACED\nline3\nappended",
        )
        .await
        .expect("write_plan_body append should succeed");
    let body = store
        .read_plan_body(&crate::model::Target::Local, &plan.id)
        .await
        .expect("read_plan_body should succeed");
    assert!(body.ends_with("line1\nREPLACED\nline3\nappended"));

    let task = store
        .add_task(
            &crate::model::Target::Local,
            &plan.id,
            NewTask {
                id: "body-task".to_string(),
                title: "Body task".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("add_task should succeed");
    store
        .write_task_body(
            &crate::model::Target::Local,
            &plan.id,
            &task.id,
            "Task body content",
        )
        .await
        .expect("write_task_body should succeed");
    let task_body = store
        .read_task_body(&crate::model::Target::Local, &plan.id, &task.id)
        .await
        .expect("read_task_body should succeed");
    assert!(task_body.ends_with("Task body content"));

    let note = store
        .add_note(
            &crate::model::Target::Local,
            &plan.id,
            NewNote {
                id: "body-note".to_string(),
                summary: Some("Body note".to_string()),
                author: None,
            },
        )
        .await
        .expect("add_note should succeed");
    store
        .write_note_body(
            &crate::model::Target::Local,
            &plan.id,
            &note.id,
            "Note body content",
        )
        .await
        .expect("write_note_body should succeed");
    let note_body = store
        .read_note_body(&crate::model::Target::Local, &plan.id, &note.id)
        .await
        .expect("read_note_body should succeed");
    assert!(note_body.ends_with("Note body content"));

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_not_found_errors<S: PlanStore>(store: &Arc<S>) {
    let err = store
        .get_plan(
            &crate::model::Target::Local,
            &"nonexistent-plan".to_string(),
        )
        .await
        .expect_err("get_plan should fail for nonexistent ID");
    assert!(matches!(err, StoreError::NotFound) || matches!(err, StoreError::InvalidParams(_)));

    let err = store
        .get_task(
            &crate::model::Target::Local,
            &"nonexistent-plan".to_string(),
            &"nonexistent-task".to_string(),
        )
        .await
        .expect_err("get_task should fail for nonexistent ID");
    assert!(matches!(err, StoreError::NotFound) || matches!(err, StoreError::InvalidParams(_)));

    let err = store
        .get_note(
            &crate::model::Target::Local,
            &"nonexistent-plan".to_string(),
            &"nonexistent-note".to_string(),
        )
        .await
        .expect_err("get_note should fail for nonexistent ID");
    assert!(matches!(err, StoreError::NotFound) || matches!(err, StoreError::InvalidParams(_)));
}

async fn test_already_exists_errors<S: PlanStore>(store: &Arc<S>) {
    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "already-exists-plan".to_string(),
                title: Some("Existing plan".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("first add_plan should succeed");

    let result = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "already-exists-plan".to_string(),
                title: Some("Duplicate plan".to_string()),
                ..Default::default()
            },
        )
        .await;

    if let Err(err) = result {
        assert!(
            matches!(err, StoreError::AlreadyExists),
            "expected AlreadyExists for duplicate plan, got {:?}",
            err
        );
    }

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

async fn test_invalid_id_errors<S: PlanStore>(store: &Arc<S>, caps: BackendCapabilities) {
    if caps.rejects_invalid_create_ids {
        let err = store
            .add_plan(
                &crate::model::Target::Local,
                NewPlan {
                    id: "invalid/plan/id".to_string(),
                    title: Some("Invalid ID".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("add_plan should fail for invalid ID");
        assert!(
            matches!(err, StoreError::InvalidId(_)) || matches!(err, StoreError::InvalidParams(_)),
            "expected InvalidId or InvalidParams for invalid plan ID, got {:?}",
            err
        );
    }

    let plan = store
        .add_plan(
            &crate::model::Target::Local,
            NewPlan {
                id: "plan-invalid-task".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("add_plan should succeed");

    if caps.rejects_invalid_create_ids {
        let err = store
            .add_task(
                &crate::model::Target::Local,
                &plan.id,
                NewTask {
                    id: "invalid/task/id".to_string(),
                    title: "Invalid task".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect_err("add_task should fail for invalid ID");
        assert!(
            matches!(err, StoreError::InvalidId(_)) || matches!(err, StoreError::InvalidParams(_)),
            "expected InvalidId or InvalidParams for invalid task ID, got {:?}",
            err
        );
    }

    let _ = store
        .delete_plan(&crate::model::Target::Local, &plan.id)
        .await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn conformance_module_exists() {}
}
