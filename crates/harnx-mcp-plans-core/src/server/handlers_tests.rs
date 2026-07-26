//! Handler tests against a backend that assigns its own canonical plan IDs.
//!
//! The GitHub backend numbers plans by issue number while MCP callers address
//! plans by name, so these tests pin the handler side of that contract: creates
//! must carry a usable title, and follow-up writes must land on the plan that was
//! just created rather than on a second one.

use std::sync::Arc;
use std::sync::Mutex;

use super::*;
use crate::model::{NoteId, Page, PageToken, TaskId};

#[derive(Debug, Clone)]
struct StoredPlan {
    /// Canonical (backend-assigned) ID, e.g. a GitHub issue number.
    id: PlanId,
    /// Client-provided name recorded at creation, e.g. GitHub `client_id` front-matter.
    name: String,
    plan: Plan,
    body: String,
}

#[derive(Debug, Default)]
struct StoreState {
    next_number: u64,
    plans: Vec<StoredPlan>,
    added: Vec<NewPlan>,
    body_writes: Vec<(PlanId, String)>,
    meta_updates: Vec<(PlanId, PlanMetaUpdate)>,
}

/// Store double that mirrors the GitHub backend's ID contract: `add_plan` returns
/// a canonical ID distinct from the client-provided name, and both the canonical
/// ID and the name resolve to the same plan.
#[derive(Debug)]
struct CanonicalIdStore {
    state: Mutex<StoreState>,
}

impl CanonicalIdStore {
    fn new() -> Self {
        Self {
            state: Mutex::new(StoreState {
                next_number: 1000,
                ..StoreState::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state.lock().expect("store state poisoned")
    }
}

impl StoreState {
    fn find(&self, plan: &PlanId) -> Option<&StoredPlan> {
        self.plans
            .iter()
            .find(|stored| &stored.id == plan || &stored.name == plan)
    }

    fn find_mut(&mut self, plan: &PlanId) -> Option<&mut StoredPlan> {
        self.plans
            .iter_mut()
            .find(|stored| &stored.id == plan || &stored.name == plan)
    }
}

#[async_trait::async_trait]
impl PlanStore for CanonicalIdStore {
    async fn list_plans(
        &self,
        _target: &Target,
        _page: Option<PageToken>,
    ) -> Result<Page<Plan>, StoreError> {
        Ok(Page {
            items: self
                .lock()
                .plans
                .iter()
                .map(|stored| stored.plan.clone())
                .collect(),
            next: None,
        })
    }

    async fn get_plan(&self, _target: &Target, plan: &PlanId) -> Result<Plan, StoreError> {
        self.lock()
            .find(plan)
            .map(|stored| stored.plan.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn add_plan(&self, _target: &Target, new_plan: NewPlan) -> Result<Plan, StoreError> {
        let mut state = self.lock();
        state.added.push(new_plan.clone());
        let id = state.next_number.to_string();
        state.next_number += 1;
        let plan = Plan {
            id: id.clone(),
            title: new_plan.title.clone(),
            summary: new_plan.summary.clone(),
            author: new_plan.author.clone(),
            assignee: new_plan.assignee.clone(),
            executor: new_plan.executor.clone(),
            git_branch: new_plan.git_branch.clone(),
            github_owner_repo: new_plan.github_owner_repo.clone(),
            created_at: Timestamp::now(),
            updated_at: None,
        };
        state.plans.push(StoredPlan {
            id,
            name: new_plan.id,
            plan: plan.clone(),
            body: String::new(),
        });
        Ok(plan)
    }

    async fn update_plan_meta(
        &self,
        _target: &Target,
        plan: &PlanId,
        update: PlanMetaUpdate,
    ) -> Result<Plan, StoreError> {
        let mut state = self.lock();
        state.meta_updates.push((plan.clone(), update.clone()));
        let stored = state.find_mut(plan).ok_or(StoreError::NotFound)?;
        stored.plan.title = update.title.or(stored.plan.title.clone());
        stored.plan.summary = update.summary.or(stored.plan.summary.clone());
        Ok(stored.plan.clone())
    }

    async fn delete_plan(&self, _target: &Target, plan: &PlanId) -> Result<(), StoreError> {
        let mut state = self.lock();
        let before = state.plans.len();
        state
            .plans
            .retain(|stored| &stored.id != plan && &stored.name != plan);
        if state.plans.len() == before {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn read_plan_body(&self, _target: &Target, plan: &PlanId) -> Result<String, StoreError> {
        self.lock()
            .find(plan)
            .map(|stored| stored.body.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn write_plan_body(
        &self,
        _target: &Target,
        plan: &PlanId,
        body: &str,
    ) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.body_writes.push((plan.clone(), body.to_string()));
        let stored = state.find_mut(plan).ok_or(StoreError::NotFound)?;
        stored.body = body.to_string();
        Ok(())
    }

    async fn list_tasks(
        &self,
        _target: &Target,
        plan: &PlanId,
        _filter: TaskFilter,
        _page: Option<PageToken>,
    ) -> Result<Page<Task>, StoreError> {
        self.lock().find(plan).ok_or(StoreError::NotFound)?;
        Ok(Page {
            items: Vec::new(),
            next: None,
        })
    }

    async fn get_task(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
    ) -> Result<Task, StoreError> {
        Err(StoreError::NotFound)
    }

    async fn add_task(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _new_task: NewTask,
    ) -> Result<Task, StoreError> {
        unreachable!("tasks are not exercised by these tests")
    }

    async fn update_task_meta(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
        _update: TaskMetaUpdate,
    ) -> Result<Task, StoreError> {
        unreachable!("tasks are not exercised by these tests")
    }

    async fn delete_task(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
    ) -> Result<(), StoreError> {
        unreachable!("tasks are not exercised by these tests")
    }

    async fn read_task_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
    ) -> Result<String, StoreError> {
        unreachable!("tasks are not exercised by these tests")
    }

    async fn write_task_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
        _body: &str,
    ) -> Result<(), StoreError> {
        unreachable!("tasks are not exercised by these tests")
    }

    async fn list_notes(
        &self,
        _target: &Target,
        plan: &PlanId,
        _page: Option<PageToken>,
    ) -> Result<Page<Note>, StoreError> {
        self.lock().find(plan).ok_or(StoreError::NotFound)?;
        Ok(Page {
            items: Vec::new(),
            next: None,
        })
    }

    async fn get_note(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _note: &NoteId,
    ) -> Result<Note, StoreError> {
        Err(StoreError::NotFound)
    }

    async fn add_note(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _new_note: NewNote,
    ) -> Result<Note, StoreError> {
        unreachable!("notes are not exercised by these tests")
    }

    async fn update_note_meta(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _note: &NoteId,
        _update: NoteMetaUpdate,
    ) -> Result<Note, StoreError> {
        unreachable!("notes are not exercised by these tests")
    }

    async fn delete_note(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _note: &NoteId,
    ) -> Result<(), StoreError> {
        unreachable!("notes are not exercised by these tests")
    }

    async fn read_note_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _note: &NoteId,
    ) -> Result<String, StoreError> {
        unreachable!("notes are not exercised by these tests")
    }

    async fn write_note_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _note: &NoteId,
        _body: &str,
    ) -> Result<(), StoreError> {
        unreachable!("notes are not exercised by these tests")
    }
}

fn server() -> (PlansServer<CanonicalIdStore>, Arc<CanonicalIdStore>) {
    let store = Arc::new(CanonicalIdStore::new());
    (PlansServer::new(store.clone()), store)
}

#[tokio::test]
async fn update_plan_creates_with_the_requested_title() {
    let (server, store) = server();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "decouple-command-usage".to_string(),
            title: Some("Decouple command usage".to_string()),
            append_content: Some("first line".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan");

    let state = store.lock();
    assert_eq!(state.added.len(), 1, "expected exactly one create");
    assert_eq!(
        state.added[0].title,
        Some("Decouple command usage".to_string()),
        "create must carry the requested title, not a blank one"
    );
}

#[tokio::test]
async fn update_plan_creates_untitled_plan_with_name_as_title() {
    let (server, store) = server();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "no-title-given".to_string(),
            append_content: Some("body".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan");

    let state = store.lock();
    assert_eq!(
        state.added[0].title,
        Some("no-title-given".to_string()),
        "an untitled create must still get a non-blank title"
    );
}

#[tokio::test]
async fn update_plan_create_carries_requested_metadata() {
    let (server, store) = server();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "with-meta".to_string(),
            title: Some("With meta".to_string()),
            summary: Some("A summary".to_string()),
            author: Some("hestia".to_string()),
            git_branch: Some("feature/x".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan");

    let state = store.lock();
    assert_eq!(state.added[0].summary, Some("A summary".to_string()));
    assert_eq!(state.added[0].author, Some("hestia".to_string()));
    assert_eq!(state.added[0].git_branch, Some("feature/x".to_string()));
}

#[tokio::test]
async fn update_plan_writes_use_canonical_id_after_create() {
    let (server, store) = server();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "canonical".to_string(),
            title: Some("Canonical".to_string()),
            replace_content: Some("body".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan");

    let state = store.lock();
    assert_eq!(
        state.body_writes,
        vec![("1000".to_string(), "body".to_string())],
        "body write must address the ID returned by add_plan"
    );
    assert_eq!(state.meta_updates[0].0, "1000");
}

#[tokio::test]
async fn add_plan_writes_body_using_canonical_id() {
    let (server, store) = server();

    server
        .handle_add_plan(AddPlanParams {
            name: "created".to_string(),
            title: Some("Created".to_string()),
            body: Some("initial body".to_string()),
            ..Default::default()
        })
        .await
        .expect("add_plan should succeed");

    let state = store.lock();
    assert_eq!(
        state.body_writes,
        vec![("1000".to_string(), "initial body".to_string())],
        "body write must address the ID returned by add_plan"
    );
}

#[tokio::test]
async fn update_plan_updates_existing_plan_instead_of_creating_a_second_one() {
    let (server, store) = server();

    server
        .handle_add_plan(AddPlanParams {
            name: "existing".to_string(),
            title: Some("Existing".to_string()),
            body: Some("one\n".to_string()),
            ..Default::default()
        })
        .await
        .expect("add_plan should succeed");

    server
        .handle_update_plan(UpdatePlanParams {
            name: "existing".to_string(),
            append_content: Some("two".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should update the existing plan");

    let state = store.lock();
    assert_eq!(state.added.len(), 1, "no second plan should be created");
    assert_eq!(state.plans.len(), 1);
    assert_eq!(state.plans[0].body, "one\ntwo");
    assert_eq!(
        state.plans[0].plan.title,
        Some("Existing".to_string()),
        "an update without a title must not clear the existing title"
    );
}
