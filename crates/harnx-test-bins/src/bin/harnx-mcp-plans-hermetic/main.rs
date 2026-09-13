//! Hermetic MCP server that directly serves `PlansServer<InMemoryStore>` over stdio.
//!
//! PURPOSE: Integration testing the direct `ServerHandler` implementation of
//! `PlansServer` without any `McpToolsetAdapter` layer.
//!
//! This binary proves the wire-level behavior of the `domain_result` wrapper
//! in `harnx-mcp-plans-core/src/server/handler.rs` — that domain errors return
//! `Ok(CallToolResult { is_error: Some(true) })` rather than JSON-RPC error frames.
//!
//! DO NOT use `harnx-plans-tools --mcp-stdio` for this purpose: that path routes
//! through `harnx_toolset_server::run_toolset_main(PlansToolset)` -> `McpToolsetAdapter`,
//! which converts handler `Err` to `isError` results at the adapter layer.
//! Tests against that path would pass even WITHOUT the ServerHandler error-mapping fix.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use harnx_mcp_plans_core::model::{
    NewNote, NewPlan, NewTask, Note, NoteId, NoteMetaUpdate, Page, PageToken, Plan, PlanId,
    PlanMetaUpdate, Target, Task, TaskFilter, TaskId, TaskMetaUpdate,
};
use harnx_mcp_plans_core::server::{PlansServer, ServerMeta};
use harnx_mcp_plans_core::store::{PlanStore, StoreError};
use rmcp::ServiceExt;

/// In-memory store for hermetic testing.
///
/// Supports:
/// - Plans with bodies
/// - Notes with bodies (under plans)
/// - All standard `PlanStore` operations
struct InMemoryStore {
    plans: Mutex<BTreeMap<String, (Plan, String)>>,
    notes: Mutex<BTreeMap<(String, String), (Note, String)>>,
}

impl InMemoryStore {
    fn new() -> Self {
        Self {
            plans: Mutex::new(BTreeMap::new()),
            notes: Mutex::new(BTreeMap::new()),
        }
    }
}

#[async_trait]
impl PlanStore for InMemoryStore {
    async fn list_plans(
        &self,
        _target: &Target,
        _page: Option<PageToken>,
    ) -> Result<Page<Plan>, StoreError> {
        let plans = self.plans.lock().unwrap();
        Ok(Page {
            items: plans.values().map(|(p, _)| p.clone()).collect(),
            next: None,
        })
    }

    async fn get_plan(&self, _target: &Target, plan: &PlanId) -> Result<Plan, StoreError> {
        self.plans
            .lock()
            .unwrap()
            .get(plan.as_str())
            .map(|(p, _)| p.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn add_plan(&self, _target: &Target, new_plan: NewPlan) -> Result<Plan, StoreError> {
        let mut plans = self.plans.lock().unwrap();
        if plans.contains_key(&new_plan.id) {
            return Err(StoreError::AlreadyExists);
        }
        let plan = Plan {
            id: new_plan.id.clone(),
            title: new_plan.title,
            summary: new_plan.summary,
            author: new_plan.author,
            assignee: new_plan.assignee,
            executor: new_plan.executor,
            git_branch: new_plan.git_branch,
            github_owner_repo: new_plan.github_owner_repo,
            created_at: jiff::Timestamp::now(),
            updated_at: None,
        };
        plans.insert(new_plan.id, (plan.clone(), String::new()));
        Ok(plan)
    }

    async fn update_plan_meta(
        &self,
        _target: &Target,
        plan: &PlanId,
        update: PlanMetaUpdate,
    ) -> Result<Plan, StoreError> {
        let mut plans = self.plans.lock().unwrap();
        let (existing, _body) = plans.get_mut(plan.as_str()).ok_or(StoreError::NotFound)?;
        if let Some(title) = update.title {
            existing.title = Some(title);
        }
        if let Some(summary) = update.summary {
            existing.summary = Some(summary);
        }
        if let Some(author) = update.author {
            existing.author = Some(author);
        }
        if let Some(assignee) = update.assignee {
            existing.assignee = Some(assignee);
        }
        if let Some(executor) = update.executor {
            existing.executor = Some(executor);
        }
        existing.updated_at = Some(jiff::Timestamp::now());
        Ok(existing.clone())
    }

    async fn delete_plan(&self, _target: &Target, plan: &PlanId) -> Result<(), StoreError> {
        self.plans
            .lock()
            .unwrap()
            .remove(plan.as_str())
            .ok_or(StoreError::NotFound)?;
        Ok(())
    }

    async fn read_plan_body(&self, _target: &Target, plan: &PlanId) -> Result<String, StoreError> {
        self.plans
            .lock()
            .unwrap()
            .get(plan.as_str())
            .map(|(_, body)| body.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn write_plan_body(
        &self,
        _target: &Target,
        plan: &PlanId,
        body: &str,
    ) -> Result<(), StoreError> {
        let mut plans = self.plans.lock().unwrap();
        if let Some((existing, stored_body)) = plans.get_mut(plan.as_str()) {
            existing.updated_at = Some(jiff::Timestamp::now());
            stored_body.clear();
            stored_body.push_str(body);
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    async fn list_tasks(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _filter: TaskFilter,
        _page: Option<PageToken>,
    ) -> Result<Page<Task>, StoreError> {
        Ok(Page {
            items: vec![],
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
        Err(StoreError::NotFound)
    }

    async fn update_task_meta(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
        _update: TaskMetaUpdate,
    ) -> Result<Task, StoreError> {
        Err(StoreError::NotFound)
    }

    async fn delete_task(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
    ) -> Result<(), StoreError> {
        Err(StoreError::NotFound)
    }

    async fn read_task_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
    ) -> Result<String, StoreError> {
        Err(StoreError::NotFound)
    }

    async fn write_task_body(
        &self,
        _target: &Target,
        _plan: &PlanId,
        _task: &TaskId,
        _body: &str,
    ) -> Result<(), StoreError> {
        Err(StoreError::NotFound)
    }

    async fn list_notes(
        &self,
        _target: &Target,
        plan: &PlanId,
        _page: Option<PageToken>,
    ) -> Result<Page<Note>, StoreError> {
        // Check plan exists first
        if !self.plans.lock().unwrap().contains_key(plan.as_str()) {
            return Err(StoreError::NotFound);
        }
        let notes = self.notes.lock().unwrap();
        let plan_notes: Vec<Note> = notes
            .iter()
            .filter(|((p, _), _)| p == plan.as_str())
            .map(|(_, (n, _))| n.clone())
            .collect();
        Ok(Page {
            items: plan_notes,
            next: None,
        })
    }

    async fn get_note(
        &self,
        _target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<Note, StoreError> {
        let notes = self.notes.lock().unwrap();
        notes
            .get(&(plan.as_str().to_string(), note.clone()))
            .map(|(n, _)| n.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn add_note(
        &self,
        _target: &Target,
        plan: &PlanId,
        new_note: NewNote,
    ) -> Result<Note, StoreError> {
        // Check plan exists first
        if !self.plans.lock().unwrap().contains_key(plan.as_str()) {
            return Err(StoreError::NotFound);
        }
        let note = Note {
            id: new_note.id,
            summary: new_note.summary,
            author: new_note.author,
            created_at: jiff::Timestamp::now(),
            updated_at: None,
        };
        let note_id = note.id.clone();
        self.notes.lock().unwrap().insert(
            (plan.as_str().to_string(), note_id),
            (note.clone(), String::new()),
        );
        Ok(note)
    }

    async fn update_note_meta(
        &self,
        _target: &Target,
        plan: &PlanId,
        note: &NoteId,
        update: NoteMetaUpdate,
    ) -> Result<Note, StoreError> {
        let mut notes = self.notes.lock().unwrap();
        let (existing, _) = notes
            .get_mut(&(plan.as_str().to_string(), note.clone()))
            .ok_or(StoreError::NotFound)?;
        if let Some(summary) = update.summary {
            existing.summary = Some(summary);
        }
        if let Some(author) = update.author {
            existing.author = Some(author);
        }
        Ok(existing.clone())
    }

    async fn delete_note(
        &self,
        _target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<(), StoreError> {
        self.notes
            .lock()
            .unwrap()
            .remove(&(plan.as_str().to_string(), note.clone()))
            .ok_or(StoreError::NotFound)?;
        Ok(())
    }

    async fn read_note_body(
        &self,
        _target: &Target,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<String, StoreError> {
        let notes = self.notes.lock().unwrap();
        notes
            .get(&(plan.as_str().to_string(), note.clone()))
            .map(|(_, body)| body.clone())
            .ok_or(StoreError::NotFound)
    }

    async fn write_note_body(
        &self,
        _target: &Target,
        plan: &PlanId,
        note: &NoteId,
        body: &str,
    ) -> Result<(), StoreError> {
        let mut notes = self.notes.lock().unwrap();
        if let Some((existing, stored_body)) =
            notes.get_mut(&(plan.as_str().to_string(), note.clone()))
        {
            existing.updated_at = Some(jiff::Timestamp::now());
            stored_body.clear();
            stored_body.push_str(body);
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store = Arc::new(InMemoryStore::new());
    let server = PlansServer::with_meta(
        store,
        ServerMeta::new(
            "harnx-mcp-plans-hermetic",
            "PlansServer stdio hermetic test",
        ),
    );
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
