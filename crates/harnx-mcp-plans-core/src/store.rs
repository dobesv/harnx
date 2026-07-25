use anyhow::Error as AnyhowError;
use async_trait::async_trait;
use thiserror::Error;

use crate::model::{
    NewNote, NewPlan, NewTask, Note, NoteId, NoteMetaUpdate, Page, PageToken, Plan, PlanId,
    PlanMetaUpdate, Target, Task, TaskFilter, TaskId, TaskMetaUpdate,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error(transparent)]
    Backend(#[from] AnyhowError),
}

/// Storage backend for plans, tasks, and notes.
///
/// ## Plan ID contract
///
/// `add_plan` returns the plan's canonical ID, which a backend may derive itself
/// (the GitHub backend uses the issue number) instead of keeping `NewPlan.id`.
/// Every method taking a `PlanId` accepts either that canonical ID or the
/// client-provided `NewPlan.id`, since MCP callers address plans by name; callers
/// that already hold a canonical ID should pass it, as resolving a name may cost
/// the backend an extra lookup.
#[async_trait]
pub trait PlanStore: Send + Sync {
    async fn list_plans(
        &self,
        target: &Target,
        page: Option<PageToken>,
    ) -> Result<Page<Plan>, StoreError>;
    async fn get_plan(&self, target: &Target, plan: &PlanId) -> Result<Plan, StoreError>;
    async fn add_plan(&self, target: &Target, new_plan: NewPlan) -> Result<Plan, StoreError>;
    async fn update_plan_meta(
        &self,
        target: &Target,
        plan: &PlanId,
        update: PlanMetaUpdate,
    ) -> Result<Plan, StoreError>;
    async fn delete_plan(&self, target: &Target, plan: &PlanId) -> Result<(), StoreError>;

    async fn read_plan_body(&self, target: &Target, plan: &PlanId) -> Result<String, StoreError>;
    async fn write_plan_body(
        &self,
        target: &Target,
        plan: &PlanId,
        body: &str,
    ) -> Result<(), StoreError>;

    async fn list_tasks(
        &self,
        target: &Target,
        plan: &PlanId,
        filter: TaskFilter,
        page: Option<PageToken>,
    ) -> Result<Page<Task>, StoreError>;
    async fn get_task(
        &self,
        target: &Target,
        plan: &PlanId,
        task: &TaskId,
    ) -> Result<Task, StoreError>;
    async fn add_task(
        &self,
        target: &Target,
        plan: &PlanId,
        new_task: NewTask,
    ) -> Result<Task, StoreError>;
    async fn update_task_meta(
        &self,
        target: &Target,
        plan: &PlanId,
        task: &TaskId,
        update: TaskMetaUpdate,
    ) -> Result<Task, StoreError>;
    async fn delete_task(
        &self,
        target: &Target,
        plan: &PlanId,
        task: &TaskId,
    ) -> Result<(), StoreError>;

    async fn read_task_body(
        &self,
        target: &Target,
        plan: &PlanId,
        task: &TaskId,
    ) -> Result<String, StoreError>;
    async fn write_task_body(
        &self,
        target: &Target,
        plan: &PlanId,
        task: &TaskId,
        body: &str,
    ) -> Result<(), StoreError>;

    async fn list_notes(
        &self,
        target: &Target,
        plan: &PlanId,
        page: Option<PageToken>,
    ) -> Result<Page<Note>, StoreError>;
    async fn get_note(
        &self,
        target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<Note, StoreError>;
    async fn add_note(
        &self,
        target: &Target,
        plan: &PlanId,
        new_note: NewNote,
    ) -> Result<Note, StoreError>;
    async fn update_note_meta(
        &self,
        target: &Target,
        plan: &PlanId,
        note: &NoteId,
        update: NoteMetaUpdate,
    ) -> Result<Note, StoreError>;
    async fn delete_note(
        &self,
        target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<(), StoreError>;

    async fn read_note_body(
        &self,
        target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<String, StoreError>;
    async fn write_note_body(
        &self,
        target: &Target,
        plan: &PlanId,
        note: &NoteId,
        body: &str,
    ) -> Result<(), StoreError>;
}

pub fn _assert_obj_safe(_: &dyn PlanStore) {}
