//! Live e2e tests for GitHubPlanStore against real GitHub API.
//!
//! **GATED**: These tests only run when `HARNX_GH_LIVE_TEST=1` is set.
//! They create real GitHub issues and require:
//! - `GITHUB_OWNER_REPO` - test-only harness repository (e.g., "owner/repo")
//! - `GITHUB_TOKEN` - a GitHub PAT with repo scope
//!
//! Tests are skipped by default in CI. To run locally:
//! ```bash
//! export HARNX_GH_LIVE_TEST=1
//! export GITHUB_OWNER_REPO=my-org/my-test-repo   # test harness only
//! export GITHUB_TOKEN=ghp_xxxx
//! cargo nextest run -p harnx-mcp-plans-github live_e2e --run-ignored ignored-only
//! ```
//!
//! **WARNING**: These tests create real issues and leave them behind for manual cleanup.
//! Use a dedicated test repository.

use std::env;
use std::sync::Arc;

use harnx_mcp_plans_core::{NewNote, NewPlan, NewTask, PlanStore, TaskFilter};
use harnx_mcp_plans_github::auth::{AuthConfig, AuthSource, GitHubAuth, RepoConfig};
use harnx_mcp_plans_github::client::GitHubClient;
use harnx_mcp_plans_github::store_github::GitHubPlanStore;

/// Check if live tests are enabled.
fn live_tests_enabled() -> bool {
    env::var("HARNX_GH_LIVE_TEST").ok().as_deref() == Some("1")
}

/// Get the test repository from environment.
fn get_test_repo() -> Option<(String, String)> {
    env::var("GITHUB_OWNER_REPO").ok().and_then(|s| {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() == 2 {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    })
}

/// Get the GitHub token from environment.
fn get_test_token() -> Option<String> {
    env::var("GITHUB_TOKEN").ok()
}

/// Check if live tests are enabled. Used for conditional skipping.
#[allow(dead_code)]
fn check_live_tests() -> bool {
    live_tests_enabled() && get_test_repo().is_some() && get_test_token().is_some()
}

async fn create_live_store() -> Option<GitHubPlanStore> {
    if !live_tests_enabled() {
        return None;
    }

    let (owner, repo) = get_test_repo()?;
    let token = get_test_token()?;

    let config = AuthConfig {
        base_url: "https://api.github.com".to_string(),
        repo: RepoConfig {
            owner: owner.clone(),
            repo: repo.clone(),
        },
        source: AuthSource::PersonalAccessToken(token),
    };

    let auth = GitHubAuth::new(config).ok()?;
    let client = GitHubClient::new(auth, &owner, &repo).await.ok()?;

    Some(GitHubPlanStore::new(client))
}

/// Generate a unique test plan ID.
fn unique_plan_id() -> String {
    format!("test-plan-{}", uuid::Uuid::new_v4())
}

/// Generate a unique test task ID.
fn unique_task_id() -> String {
    format!("test-task-{}", uuid::Uuid::new_v4())
}

/// Generate a unique test note ID.
fn unique_note_id() -> String {
    format!("test-note-{}", uuid::Uuid::new_v4())
}

// =============================================================================
// Live e2e Tests
// =============================================================================

#[tokio::test]
#[ignore]
async fn live_e2e_create_plan() {
    let Some(store) = create_live_store().await else {
        eprintln!("SKIPPED: Live tests not enabled (set HARNX_GH_LIVE_TEST=1, GITHUB_OWNER_REPO [test-only harness], GITHUB_TOKEN)");
        return;
    };

    let plan_id = unique_plan_id();
    let store = Arc::new(store);

    // Create a plan
    let plan = store
        .add_plan(NewPlan {
            id: plan_id.clone(),
            title: Some(format!("[TEST] {}", plan_id)),
            summary: Some("Created by live e2e test".to_string()),
            ..Default::default()
        })
        .await;

    match plan {
        Ok(plan) => {
            println!(
                "Created plan: {} (ID: {})",
                plan.title.unwrap_or_default(),
                plan.id
            );

            // Read it back
            let fetched = store.get_plan(&plan.id).await;
            assert!(fetched.is_ok(), "should be able to fetch created plan");
            let fetched = fetched.unwrap();
            assert_eq!(fetched.id, plan.id);

            // Clean up: close the issue
            let _ = store.delete_plan(&plan.id).await;
        }
        Err(e) => {
            eprintln!("Failed to create plan: {:?}", e);
            panic!("Live test failed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_e2e_create_task() {
    let Some(store) = create_live_store().await else {
        eprintln!("SKIPPED: Live tests not enabled");
        return;
    };

    let plan_id = unique_plan_id();
    let task_id = unique_task_id();
    let store = Arc::new(store);

    // Create a plan first
    let plan = store
        .add_plan(NewPlan {
            id: plan_id.clone(),
            title: Some(format!("[TEST] Task container {}", plan_id)),
            ..Default::default()
        })
        .await
        .expect("create plan");

    // Create a task
    let task = store
        .add_task(
            &plan.id,
            NewTask {
                id: task_id.clone(),
                title: format!("[TEST] Task {} in {}", task_id, plan.id),
                summary: Some("Created by live e2e test".to_string()),
                ..Default::default()
            },
        )
        .await;

    match task {
        Ok(task) => {
            println!("Created task: {} (ID: {})", task.title, task.id);

            // Read it back
            let fetched = store.get_task(&plan.id, &task.id).await;
            assert!(fetched.is_ok(), "should be able to fetch created task");

            // Clean up
            let _ = store.delete_task(&plan.id, &task.id).await;
            let _ = store.delete_plan(&plan.id).await;
        }
        Err(e) => {
            eprintln!("Failed to create task: {:?}", e);
            let _ = store.delete_plan(&plan.id).await;
            panic!("Live test failed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_e2e_create_note() {
    let Some(store) = create_live_store().await else {
        eprintln!("SKIPPED: Live tests not enabled");
        return;
    };

    let plan_id = unique_plan_id();
    let note_id = unique_note_id();
    let store = Arc::new(store);

    // Create a plan first
    let plan = store
        .add_plan(NewPlan {
            id: plan_id.clone(),
            title: Some(format!("[TEST] Note container {}", plan_id)),
            ..Default::default()
        })
        .await
        .expect("create plan");

    // Create a note
    let note = store
        .add_note(
            &plan.id,
            NewNote {
                id: note_id.clone(),
                summary: Some(format!("[TEST] Note {}", note_id)),
                author: Some("live_e2e_test".to_string()),
            },
        )
        .await;

    match note {
        Ok(note) => {
            println!("Created note: ID {})", note.id);

            // Read it back
            let fetched = store.get_note(&plan.id, &note.id).await;
            assert!(fetched.is_ok(), "should be able to fetch created note");

            // Clean up
            let _ = store.delete_note(&plan.id, &note.id).await;
            let _ = store.delete_plan(&plan.id).await;
        }
        Err(e) => {
            eprintln!("Failed to create note: {:?}", e);
            let _ = store.delete_plan(&plan.id).await;
            panic!("Live test failed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_e2e_pagination() {
    let Some(store) = create_live_store().await else {
        eprintln!("SKIPPED: Live tests not enabled");
        return;
    };

    let store = Arc::new(store);

    // List plans (should return at least an empty page)
    let page = store
        .list_plans(None)
        .await
        .expect("list plans should succeed");

    println!("Found {} plans", page.items.len());

    // If there's a next page, fetch it
    if let Some(token) = page.next {
        let next_page = store
            .list_plans(Some(token))
            .await
            .expect("next page should succeed");
        println!("Next page has {} plans", next_page.items.len());
    }
}

#[tokio::test]
#[ignore]
async fn live_e2e_full_crud_cycle() {
    let Some(store) = create_live_store().await else {
        eprintln!("SKIPPED: Live tests not enabled");
        return;
    };

    let store = Arc::new(store);
    let plan_id = unique_plan_id();
    let task_id = unique_task_id();
    let note_id = unique_note_id();

    // 1. Create plan
    let plan = store
        .add_plan(NewPlan {
            id: plan_id.clone(),
            title: Some(format!("[TEST] Full CRUD {}", plan_id)),
            summary: Some("Full CRUD test".to_string()),
            author: Some("live_e2e_test".to_string()),
            ..Default::default()
        })
        .await
        .expect("create plan");

    println!("Created plan: {}", plan.id);

    // 2. Write body
    store
        .write_plan_body(&plan.id, "# Test Plan\n\nThis is a test plan body.")
        .await
        .expect("write body");

    // 3. Read body
    let body = store.read_plan_body(&plan.id).await.expect("read body");
    assert!(body.contains("Test Plan"), "body should contain title");

    // 4. Create task
    let task = store
        .add_task(
            &plan.id,
            NewTask {
                id: task_id.clone(),
                title: format!("[TEST] Task for {}", plan_id),
                status: Some("in_progress".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("create task");

    println!("Created task: {}", task.id);

    // 5. Create note
    let note = store
        .add_note(
            &plan.id,
            NewNote {
                id: note_id.clone(),
                summary: Some("Test note".to_string()),
                author: Some("live_e2e_test".to_string()),
            },
        )
        .await
        .expect("create note");

    println!("Created note: {}", note.id);

    // 6. List tasks
    let tasks = store
        .list_tasks(&plan.id, TaskFilter::default(), None)
        .await
        .expect("list tasks");
    assert!(
        tasks.items.iter().any(|t| t.id == task.id),
        "should find created task"
    );

    // 7. List notes
    let notes = store.list_notes(&plan.id, None).await.expect("list notes");
    assert!(
        notes.items.iter().any(|n| n.id == note.id),
        "should find created note"
    );

    // 8. Cleanup
    store
        .delete_note(&plan.id, &note.id)
        .await
        .expect("delete note");
    store
        .delete_task(&plan.id, &task.id)
        .await
        .expect("delete task");
    store.delete_plan(&plan.id).await.expect("delete plan");

    println!("Cleaned up plan: {}", plan.id);
}
