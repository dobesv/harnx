use super::*;
use serde_json::Value;
use std::fs;
use std::time::Duration;

fn temp_test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harnx-mcp-plans-{}-{}", label, gen_id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn extract_text(result: CallToolResult) -> String {
    result.content[0]
        .raw
        .as_text()
        .map(|text| text.text.clone())
        .unwrap_or_else(|| panic!("unexpected content: {:?}", result.content[0]))
}

fn extract_id(summary: &str) -> String {
    summary.split_whitespace().nth(2).unwrap().to_string()
}

#[test]
fn plan_last_activity_uses_latest_file_mtime_not_dir_mtime() {
    let dir = temp_test_dir("plan-last-activity-latest-file");
    let plan_dir = dir.join("plan-a");
    fs::create_dir_all(&plan_dir).unwrap();

    let dir_mtime = plan_dir.metadata().unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(50));

    fs::write(plan_dir.join("plan.md"), "plan").unwrap();
    let plan_mtime = plan_dir
        .join("plan.md")
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));

    fs::create_dir_all(plan_dir.join("tasks")).unwrap();
    fs::write(plan_dir.join("tasks/task-1.md"), "task").unwrap();
    let task_mtime = plan_dir
        .join("tasks/task-1.md")
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));

    fs::create_dir_all(plan_dir.join("notes")).unwrap();
    fs::write(plan_dir.join("notes/note-1.md"), "note").unwrap();
    let note_mtime = plan_dir
        .join("notes/note-1.md")
        .metadata()
        .unwrap()
        .modified()
        .unwrap();

    let actual = plan_last_activity(&plan_dir).unwrap();
    let expected = plan_mtime.max(task_mtime).max(note_mtime);

    assert_eq!(actual, expected);
    assert!(actual > dir_mtime);
}

#[test]
fn plan_last_activity_falls_back_to_dir_mtime_for_empty_plan() {
    let dir = temp_test_dir("plan-last-activity-empty-plan");
    let plan_dir = dir.join("plan-a");
    fs::create_dir_all(&plan_dir).unwrap();

    let expected = plan_dir.metadata().unwrap().modified().unwrap();
    let actual = plan_last_activity(&plan_dir).unwrap();

    assert_eq!(actual, expected);
}

#[tokio::test]
async fn add_and_get_task() {
    let dir = temp_test_dir("add-and-get-task");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Task 1".to_string(),
            plan: "plan-a".to_string(),
            summary: Some("sum".to_string()),
            author: Some("author".to_string()),
            assignee: None,
            executor: None,
            tags: vec!["rust".to_string()],
            status: None,
            body: Some("body".to_string()),
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    let got = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "Task 1");
    assert_eq!(value["summary"], "sum");
    assert_eq!(value["body"], "body");
}

#[tokio::test]
async fn add_task_with_agent_id() {
    let dir = temp_test_dir("add-task-agent-id");
    let server = PlansServer::new(dir.clone());

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Agent ID Task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("my-task-id".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap();
    let returned_id = extract_id(&extract_text(add));
    assert_eq!(returned_id, "my-task-id");

    let path = dir.join("plan-a").join("tasks").join("my-task-id.md");
    assert!(path.exists(), "task file should exist at my-task-id.md");
}

#[tokio::test]
async fn add_task_duplicate_id_error() {
    let dir = temp_test_dir("add-task-dup-id");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "First".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("dup-id".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap();

    let err = server
        .handle_add_task(AddTaskParams {
            title: "Second".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("dup-id".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("already exists"),
        "expected 'already exists' in: {}",
        err.message
    );
}

#[tokio::test]
async fn add_task_invalid_id_rejected() {
    let dir = temp_test_dir("add-task-invalid-id");
    let server = PlansServer::new(dir);

    // Slash in ID
    let err = server
        .handle_add_task(AddTaskParams {
            title: "Bad ID".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("bad/id".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("alphanumeric") || err.message.contains("1-64"),
        "expected validation error, got: {}",
        err.message
    );

    // Empty ID
    let err2 = server
        .handle_add_task(AddTaskParams {
            title: "Empty ID".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap_err();
    assert!(
        err2.message.contains("alphanumeric") || err2.message.contains("1-64"),
        "expected validation error, got: {}",
        err2.message
    );
}

#[tokio::test]
async fn add_task_auto_id_fallback() {
    let dir = temp_test_dir("add-task-auto-id");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Auto ID".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));
    assert!(!id.is_empty(), "auto-generated ID should not be empty");
}

#[tokio::test]
async fn add_note_with_agent_id() {
    let dir = temp_test_dir("add-note-agent-id");
    let server = PlansServer::new(dir.clone());

    let add = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("my-note-id".to_string()),
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();
    let text = extract_text(add);
    assert!(
        text.contains("my-note-id"),
        "result should mention my-note-id, got: {}",
        text
    );

    let path = dir.join("plan-a").join("notes").join("my-note-id.md");
    assert!(path.exists(), "note file should exist at my-note-id.md");
}

#[tokio::test]
async fn add_note_invalid_id_rejected() {
    let dir = temp_test_dir("add-note-invalid-id");
    let server = PlansServer::new(dir);

    let err = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("bad/id".to_string()),
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("alphanumeric") || err.message.contains("1-64"),
        "expected validation error, got: {}",
        err.message
    );
}

#[test]
fn validate_id_rejects_invalid() {
    assert!(validate_id("").is_err(), "empty string should be rejected");
    assert!(validate_id("bad/id").is_err(), "slash should be rejected");
    assert!(validate_id("bad id").is_err(), "space should be rejected");
    assert!(
        validate_id(&"a".repeat(65)).is_err(),
        "65-char id should be rejected"
    );
    assert!(validate_id("good-id").is_ok(), "good-id should be accepted");
    assert!(validate_id("ABC_123").is_ok(), "ABC_123 should be accepted");
    assert!(validate_id("a").is_ok(), "single char should be accepted");
    assert!(
        validate_id(&"a".repeat(64)).is_ok(),
        "64-char id should be accepted"
    );
}

#[tokio::test]
async fn update_task_fields() {
    let dir = temp_test_dir("update-task-fields");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Before".to_string(),
            plan: "plan-a".to_string(),
            summary: Some("old summary".to_string()),
            author: None,
            assignee: Some("alice".to_string()),
            executor: None,
            tags: vec![],
            status: Some("open".to_string()),
            body: Some("body".to_string()),
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    server
        .handle_update_task(UpdateTaskParams {
            plan: "plan-a".to_string(),
            id: id.clone(),
            title: Some("After".to_string()),
            summary: Some("new summary".to_string()),
            author: None,
            assignee: Some("bob".to_string()),
            executor: None,
            tags: None,
            status: Some("in_progress".to_string()),
            replace_body: None,
            append_body: None,
            replace_in_body: None,
            dependencies: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "After");
    assert_eq!(value["status"], "in_progress");
    assert_eq!(value["summary"], "new summary");
    assert_eq!(value["assignee"], "bob");
}

#[tokio::test]
async fn append_body_via_update_task() {
    let dir = temp_test_dir("append-task-body");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Append".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: Some("line1".to_string()),
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    server
        .handle_update_task(UpdateTaskParams {
            plan: "plan-a".to_string(),
            id: id.clone(),
            append_body: Some("line2".to_string()),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: None,
            status: None,
            replace_body: None,
            replace_in_body: None,
            dependencies: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["body"], "line1\nline2");
}

#[tokio::test]
async fn delete_task() {
    let dir = temp_test_dir("delete-task");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Delete me".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    server
        .handle_delete_task(DeleteTaskParams {
            plan: "plan-a".to_string(),
            id: id.clone(),
        })
        .await
        .unwrap();

    let err = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id,
        })
        .await
        .unwrap_err();
    assert!(err.message.contains("not found"));
}

#[tokio::test]
async fn list_tasks_scoped_to_plan() {
    // Tasks are plan-scoped; each plan's list shows only that plan's tasks
    let dir = temp_test_dir("list-tasks-scoped");
    let server = PlansServer::new(dir);

    for plan in ["plan-a", "plan-b"] {
        server
            .handle_add_task(AddTaskParams {
                title: format!("task for {plan}"),
                plan: plan.to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
    }

    // Listing plan-a returns only plan-a's task
    let result_a = server
        .handle_list_tasks(ListTasksParams {
            plan: "plan-a".to_string(),
            filter: "all".to_string(),
            tag: None,
        })
        .await
        .unwrap();
    let items_a: Value = serde_json::from_str(&extract_text(result_a)).unwrap();
    assert_eq!(items_a.as_array().unwrap().len(), 1);
    assert_eq!(items_a[0]["plan"], "plan-a");

    // Listing plan-b returns only plan-b's task
    let result_b = server
        .handle_list_tasks(ListTasksParams {
            plan: "plan-b".to_string(),
            filter: "all".to_string(),
            tag: None,
        })
        .await
        .unwrap();
    let items_b: Value = serde_json::from_str(&extract_text(result_b)).unwrap();
    assert_eq!(items_b.as_array().unwrap().len(), 1);
    assert_eq!(items_b[0]["plan"], "plan-b");
}

#[tokio::test]
async fn list_tasks_by_tag() {
    let dir = temp_test_dir("list-tasks-by-tag");
    let server = PlansServer::new(dir);

    // Create task with "urgent" tag
    server
        .handle_add_task(AddTaskParams {
            title: "tagged task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec!["urgent".to_string()],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    // Create task without the tag
    server
        .handle_add_task(AddTaskParams {
            title: "untagged task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec!["normal".to_string()],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    let result = server
        .handle_list_tasks(ListTasksParams {
            filter: "all".to_string(),
            tag: Some("urgent".to_string()),
            plan: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "tagged task");
}

#[tokio::test]
async fn update_task_cross_plan_move() {
    // Tasks are scoped to a plan — update stays within the plan
    let dir = temp_test_dir("update-task-cross-plan");
    let server = PlansServer::new(dir.clone());

    let add = server
        .handle_add_task(AddTaskParams {
            title: "task to update".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    // Update the task status (stays in plan-a)
    server
        .handle_update_task(UpdateTaskParams {
            plan: "plan-a".to_string(),
            id: id.clone(),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: None,
            status: Some("closed".to_string()),
            replace_body: None,
            append_body: None,
            replace_in_body: None,
            dependencies: None,
        })
        .await
        .unwrap();

    // File should still be in plan-a/tasks/
    assert!(
        dir.join("plan-a/tasks")
            .join(format!("{}.md", normalize_id(&id)))
            .exists(),
        "task file should remain in plan-a"
    );

    // get_task should return the updated status
    let got = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["plan"], "plan-a");
    assert_eq!(value["status"], "closed");
}

#[tokio::test]
async fn update_plan_append_content() {
    let dir = temp_test_dir("update-plan-append-content");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("line1".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: None,
            append_content: Some("line2".to_string()),
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(
        value["body"],
        "line1
line2"
    );
}

#[tokio::test]
async fn update_plan_replace_in_content() {
    let dir = temp_test_dir("update-plan-replace-in-content");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello world".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: None,
            append_content: None,
            replace_in_content: Some(ReplaceInContent {
                old_text: "world".to_string(),
                new_text: "there".to_string(),
                replace_all: None,
            }),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["body"], "hello there");
}

#[tokio::test]
async fn update_plan_replace_in_content_not_found() {
    let dir = temp_test_dir("update-plan-replace-in-content-not-found");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello world".to_string()),
        })
        .await
        .unwrap();

    let err = server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: None,
            append_content: None,
            replace_in_content: Some(ReplaceInContent {
                old_text: "missing".to_string(),
                new_text: "there".to_string(),
                replace_all: None,
            }),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected not found error: {}",
        err.message
    );
}

#[tokio::test]
async fn update_plan_replace_in_content_empty_old_text_error() {
    let dir = temp_test_dir("update-plan-replace-in-empty-old-text");
    let server = PlansServer::new(dir);

    let err = server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: None,
            append_content: None,
            replace_in_content: Some(ReplaceInContent {
                old_text: "".to_string(),
                new_text: "something".to_string(),
                replace_all: None,
            }),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("empty"),
        "expected empty old_text error: {}",
        err.message
    );
}

#[tokio::test]
async fn update_plan_two_content_fields_error() {
    let dir = temp_test_dir("update-plan-two-content-fields-error");
    let server = PlansServer::new(dir);

    let err = server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: Some("one".to_string()),
            append_content: Some("two".to_string()),
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("at most one"),
        "expected exclusivity error: {}",
        err.message
    );
}

#[tokio::test]
async fn update_plan_no_content_preserves_body() {
    let dir = temp_test_dir("update-plan-no-content-preserves-body");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("keep me".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: None,
            append_content: None,
            replace_in_content: None,
            title: Some("Renamed".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "Renamed");
    assert_eq!(value["body"], "keep me");
}

#[tokio::test]
async fn update_note_fields() {
    let dir = temp_test_dir("update-note-fields");
    let server = PlansServer::new(dir);

    server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("my-note".to_string()),
            body: "hello world".to_string(),
            summary: Some("before".to_string()),
            author: Some("alice".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_update_note(UpdateNoteParams {
            plan: "plan-a".to_string(),
            note_id: "my-note".to_string(),
            summary: Some("after".to_string()),
            author: Some("bob".to_string()),
            replace_body: None,
            append_body: None,
            replace_in_body: Some(ReplaceInContent {
                old_text: "world".to_string(),
                new_text: "there".to_string(),
                replace_all: None,
            }),
        })
        .await
        .unwrap();

    let got = server
        .handle_get_note(GetNoteParams {
            plan: "plan-a".to_string(),
            note_id: "my-note".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["summary"], "after");
    assert_eq!(value["author"], "bob");
    assert_eq!(value["body"], "hello there");
}

#[tokio::test]
async fn update_note_not_found() {
    let dir = temp_test_dir("update-note-not-found");
    let server = PlansServer::new(dir);

    let err = server
        .handle_update_note(UpdateNoteParams {
            plan: "plan-a".to_string(),
            note_id: "missing".to_string(),
            summary: None,
            author: None,
            replace_body: Some("body".to_string()),
            append_body: None,
            replace_in_body: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected not found error: {}",
        err.message
    );
}

#[tokio::test]
async fn update_plan_batch_creates_tasks() {
    let dir = temp_test_dir("update-plan-batch");
    let server = PlansServer::new(dir.clone());

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: Some("plan body".to_string()),
            append_content: None,
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: Some(vec![
                TaskSpec {
                    title: "batch task 1".to_string(),
                    id: None,
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
                TaskSpec {
                    title: "batch task 2".to_string(),
                    id: None,
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
            ]),
        })
        .await
        .unwrap();

    // Both tasks should exist in tasks/ dir
    let tasks_dir = dir.join("plan-a/tasks");
    let task_files: Vec<_> = std::fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert_eq!(task_files.len(), 2);
}

#[tokio::test]
async fn update_plan_batch_rejects_duplicate_id() {
    let dir = temp_test_dir("update-plan-batch-dup-id");
    let server = PlansServer::new(dir);

    // Pre-create a task with id "existing-id"
    server
        .handle_add_task(AddTaskParams {
            title: "existing".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: Some("existing-id".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap();

    // Try to batch-create a task with the same id — should fail
    let result = server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: Some("".to_string()),
            append_content: None,
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: Some(vec![TaskSpec {
                title: "duplicate id task".to_string(),
                id: Some("existing-id".to_string()),
                tags: vec![],
                status: None,
                body: None,
                dependencies: vec![],
                summary: None,
                author: None,
                assignee: None,
                executor: None,
            }]),
        })
        .await;
    assert!(result.is_err(), "batch with pre-existing id should fail");
}

#[tokio::test]
async fn update_plan_batch_rejects_intra_batch_duplicate_id() {
    let dir = temp_test_dir("update-plan-batch-intra-dup-id");
    let server = PlansServer::new(dir);

    // Two TaskSpecs with the same id in the same batch — should fail
    let result = server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: Some("".to_string()),
            append_content: None,
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: Some(vec![
                TaskSpec {
                    title: "task one".to_string(),
                    id: Some("shared-id".to_string()),
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
                TaskSpec {
                    title: "task two".to_string(),
                    id: Some("shared-id".to_string()),
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
            ]),
        })
        .await;
    assert!(
        result.is_err(),
        "intra-batch duplicate IDs should be rejected"
    );
}

#[tokio::test]
async fn update_plan_batch_creates_tasks_with_ids() {
    let dir = temp_test_dir("update-plan-batch-with-ids");
    let server = PlansServer::new(dir.clone());

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            replace_content: Some("plan body".to_string()),
            append_content: None,
            replace_in_content: None,
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            tasks: Some(vec![
                TaskSpec {
                    title: "first task".to_string(),
                    id: Some("alpha-task".to_string()),
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
                TaskSpec {
                    title: "second task".to_string(),
                    id: Some("beta-task".to_string()),
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                },
            ]),
        })
        .await
        .unwrap();

    let alpha_path = dir.join("plan-a").join("tasks").join("alpha-task.md");
    let beta_path = dir.join("plan-a").join("tasks").join("beta-task.md");
    assert!(alpha_path.exists(), "alpha-task.md should exist");
    assert!(beta_path.exists(), "beta-task.md should exist");
}

#[tokio::test]
async fn add_note_duplicate_id_error() {
    let dir = temp_test_dir("add-note-dup-id");
    let server = PlansServer::new(dir);

    server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("dup-note-id".to_string()),
            body: "first note".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();

    let err = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("dup-note-id".to_string()),
            body: "second note".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("already exists"),
        "expected 'already exists' in: {}",
        err.message
    );
}

#[tokio::test]
async fn list_tasks_by_plan() {
    let dir = temp_test_dir("list-tasks-by-plan");
    let server = PlansServer::new(dir);

    for plan in ["plan-a", "plan-b"] {
        server
            .handle_add_task(AddTaskParams {
                title: format!("task for {plan}"),
                plan: plan.to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
    }

    let result = server
        .handle_list_tasks(ListTasksParams {
            filter: "all".to_string(),
            tag: None,
            plan: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["plan"], "plan-a");
    assert_eq!(items[0]["title"], "task for plan-a");
}

#[tokio::test]
async fn add_task_creates_tasks_subdir() {
    let dir = temp_test_dir("add-task-creates-tasks-subdir");
    let server = PlansServer::new(dir.clone());

    server
        .handle_add_task(AddTaskParams {
            title: "Task path".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    assert!(dir.join("plan-a").join("tasks").exists());
}

#[tokio::test]
async fn get_task_missing_id() {
    let dir = temp_test_dir("get-task-missing-id");
    let server = PlansServer::new(dir);

    let err = server
        .handle_get_task(GetTaskParams {
            plan: "plan-a".to_string(),
            id: "task-deadbeef".to_string(),
        })
        .await
        .unwrap_err();
    assert!(err.message.contains("not found"));
}

#[tokio::test]
async fn add_and_get_plan() {
    let dir = temp_test_dir("add-and-get-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello".to_string()),
        })
        .await
        .unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "Test Plan");
    assert_eq!(value["body"], "hello");
}

#[tokio::test]
async fn add_plan_duplicate_error() {
    let dir = temp_test_dir("add-plan-duplicate-error");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello".to_string()),
        })
        .await
        .unwrap();

    let err = server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello again".to_string()),
        })
        .await
        .unwrap_err();
    assert!(err.message.contains("already exists"));
}

#[tokio::test]
async fn update_plan_creates_if_missing() {
    let dir = temp_test_dir("update-plan-creates-if-missing");
    let server = PlansServer::new(dir.clone());

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            title: Some("Created".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            replace_content: Some("new body".to_string()),
            append_content: None,
            replace_in_content: None,
            tasks: None,
        })
        .await
        .unwrap();

    assert!(dir.join("plan-a").join("plan.md").exists());
    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "Created");
    assert_eq!(value["body"], "new body");
}

#[tokio::test]
async fn update_plan_preserves_metadata() {
    let dir = temp_test_dir("update-plan-preserves-metadata");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("before".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_update_plan(UpdatePlanParams {
            name: "plan-a".to_string(),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            replace_content: Some("after".to_string()),
            append_content: None,
            replace_in_content: None,
            tasks: None,
        })
        .await
        .unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["title"], "Test Plan");
    assert_eq!(value["body"], "after");
}

#[tokio::test]
async fn delete_plan() {
    let dir = temp_test_dir("delete-plan");
    let server = PlansServer::new(dir.clone());

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello".to_string()),
        })
        .await
        .unwrap();

    server
        .handle_delete_plan(DeletePlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();

    assert!(!dir.join("plan-a").exists());
}

#[tokio::test]
async fn list_plans_returns_counts() {
    let dir = temp_test_dir("list-plans-returns-counts");
    let server = PlansServer::new(dir);

    server
        .handle_add_plan(AddPlanParams {
            name: "plan-a".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            body: Some("hello".to_string()),
        })
        .await
        .unwrap();
    for idx in 0..2 {
        server
            .handle_add_task(AddTaskParams {
                title: format!("Task {idx}"),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
    }
    server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();

    let result = server.handle_list_plans().await.unwrap();
    let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["task_count"], 2);
    assert_eq!(items[0]["note_count"], 1);
}

#[tokio::test]
async fn add_and_get_note() {
    let dir = temp_test_dir("add-and-get-note");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: Some("sum".to_string()),
            author: Some("author".to_string()),
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    let got = server
        .handle_get_note(GetNoteParams {
            plan: "plan-a".to_string(),
            note_id: id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["summary"], "sum");
    assert_eq!(value["body"], "note body");
}

#[tokio::test]
async fn delete_note() {
    let dir = temp_test_dir("delete-note");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    server
        .handle_delete_note(DeleteNoteParams {
            plan: "plan-a".to_string(),
            note_id: id.clone(),
        })
        .await
        .unwrap();

    let err = server
        .handle_get_note(GetNoteParams {
            plan: "plan-a".to_string(),
            note_id: id,
        })
        .await
        .unwrap_err();
    assert!(err.message.contains("not found"));
}

#[tokio::test]
async fn list_notes() {
    let dir = temp_test_dir("list-notes");
    let server = PlansServer::new(dir);

    for idx in 0..2 {
        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: format!("note {idx}"),
                summary: Some(format!("summary {idx}")),
                author: None,
            })
            .await
            .unwrap();
    }

    let result = server
        .handle_list_notes(ListNotesParams {
            plan: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let summaries = items
        .iter()
        .map(|item| item["summary"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(summaries.contains(&"summary 0"));
    assert!(summaries.contains(&"summary 1"));
}

#[tokio::test]
async fn add_note_creates_notes_subdir() {
    let dir = temp_test_dir("add-note-creates-notes-subdir");
    let server = PlansServer::new(dir.clone());

    server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();

    assert!(dir.join("plan-a").join("notes").exists());
}

#[tokio::test]
async fn get_note_returns_frontmatter() {
    let dir = temp_test_dir("get-note-returns-frontmatter");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: Some("test summary".to_string()),
            author: Some("author".to_string()),
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));

    let got = server
        .handle_get_note(GetNoteParams {
            plan: "plan-a".to_string(),
            note_id: id,
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["summary"], "test summary");
}

#[tokio::test]
async fn get_plan_legacy_raw_markdown() {
    let dir = temp_test_dir("get-plan-legacy-raw-markdown");
    let server = PlansServer::new(dir.clone());

    let plan_dir = dir.join("plan-a");
    fs::create_dir_all(&plan_dir).unwrap();
    fs::write(plan_dir.join("plan.md"), "# Legacy Plan\n\nbody text").unwrap();

    let got = server
        .handle_get_plan(GetPlanParams {
            name: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["id"], "plan-a");
    assert_eq!(value["body"], "# Legacy Plan\n\nbody text");
}

#[tokio::test]
async fn normalize_note_id_prefix() {
    let dir = temp_test_dir("normalize-note-id-prefix");
    let server = PlansServer::new(dir);

    let add = server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: None,
            body: "note body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();
    let id = normalize_id(&extract_id(&extract_text(add)));

    let got = server
        .handle_get_note(GetNoteParams {
            plan: "plan-a".to_string(),
            note_id: format!("note-{id}"),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
    assert_eq!(value["id"], id);
}

#[tokio::test]
async fn list_tasks_filter() {
    let dir = temp_test_dir("list-tasks-filter");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "Open".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: Some("open".to_string()),
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    server
        .handle_add_task(AddTaskParams {
            title: "Closed".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: Some("closed".to_string()),
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    let result = server
        .handle_list_tasks(ListTasksParams {
            filter: "open".to_string(),
            tag: None,
            plan: "plan-a".to_string(),
        })
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 1);
    assert_eq!(value[0]["title"], "Open");
}

#[tokio::test]
async fn task_file_in_tasks_subdir() {
    let dir = temp_test_dir("task-file-in-subdir");
    let server = PlansServer::new(dir.clone());

    let add = server
        .handle_add_task(AddTaskParams {
            title: "Task path".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: None,
            id: None,
            dependencies: vec![],
        })
        .await
        .unwrap();
    let id = extract_id(&extract_text(add));
    let path = dir
        .join("plan-a")
        .join("tasks")
        .join(format!("{}.md", normalize_id(&id)));
    assert!(path.exists());
}

#[tokio::test]
async fn get_task_wrong_plan_fails() {
    let dir = temp_test_dir("get-task-wrong-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            id: Some("my-task".to_string()),
            body: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    let err = server
        .handle_get_task(GetTaskParams {
            plan: "plan-b".to_string(),
            id: "my-task".to_string(),
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected 'not found' in: {}",
        err.message
    );
}

#[tokio::test]
async fn update_task_wrong_plan_fails() {
    let dir = temp_test_dir("update-task-wrong-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            id: Some("my-task".to_string()),
            body: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    let err = server
        .handle_update_task(UpdateTaskParams {
            plan: "plan-b".to_string(),
            id: "my-task".to_string(),
            title: Some("new title".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: None,
            status: None,
            replace_body: None,
            append_body: None,
            replace_in_body: None,
            dependencies: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected 'not found' in: {}",
        err.message
    );
}

#[tokio::test]
async fn append_body_wrong_plan_fails() {
    let dir = temp_test_dir("append-task-wrong-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            body: Some("body".to_string()),
            id: Some("my-task".to_string()),
            dependencies: vec![],
        })
        .await
        .unwrap();

    let err = server
        .handle_update_task(UpdateTaskParams {
            plan: "plan-b".to_string(),
            id: "my-task".to_string(),
            title: None,
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: None,
            status: None,
            replace_body: None,
            append_body: Some("appended".to_string()),
            replace_in_body: None,
            dependencies: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected 'not found' in: {}",
        err.message
    );
}

#[tokio::test]
async fn delete_task_wrong_plan_fails() {
    let dir = temp_test_dir("delete-task-wrong-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_task(AddTaskParams {
            title: "task".to_string(),
            plan: "plan-a".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            status: None,
            id: Some("my-task".to_string()),
            body: None,
            dependencies: vec![],
        })
        .await
        .unwrap();

    let err = server
        .handle_delete_task(DeleteTaskParams {
            plan: "plan-b".to_string(),
            id: "my-task".to_string(),
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected 'not found' in: {}",
        err.message
    );
}

#[tokio::test]
async fn delete_note_wrong_plan_fails() {
    let dir = temp_test_dir("delete-note-wrong-plan");
    let server = PlansServer::new(dir);

    server
        .handle_add_note(AddNoteParams {
            plan: "plan-a".to_string(),
            id: Some("my-note".to_string()),
            body: "body".to_string(),
            summary: None,
            author: None,
        })
        .await
        .unwrap();

    let err = server
        .handle_delete_note(DeleteNoteParams {
            plan: "plan-b".to_string(),
            note_id: "my-note".to_string(),
        })
        .await
        .unwrap_err();
    assert!(
        err.message.contains("not found"),
        "expected 'not found' in: {}",
        err.message
    );
}

#[tokio::test]
async fn cleanup_deletes_stale_plan_but_keeps_fresh_plan() {
    let dir = temp_test_dir("cleanup-stale-plan");
    let stale_plan = dir.join("stale-plan");
    let fresh_plan = dir.join("fresh-plan");

    fs::create_dir_all(&stale_plan).unwrap();
    fs::write(stale_plan.join("plan.md"), "stale").unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    fs::create_dir_all(&fresh_plan).unwrap();
    fs::write(fresh_plan.join("plan.md"), "fresh").unwrap();

    run_cleanup_pass(&dir, Duration::from_millis(10)).await;

    assert!(!stale_plan.exists(), "stale plan should be deleted");
    assert!(fresh_plan.exists(), "fresh plan should be kept");
}

#[test]
fn validate_plan_name_rejects_invalid() {
    assert!(validate_plan_name("").is_err(), "empty string rejected");
    assert!(validate_plan_name("   ").is_err(), "whitespace rejected");
    assert!(validate_plan_name("a/b").is_err(), "slash rejected");
    assert!(validate_plan_name("../etc").is_err(), "traversal rejected");
    assert!(
        validate_plan_name("my-plan").is_ok(),
        "normal name accepted"
    );
    assert!(
        validate_plan_name("plan a").is_ok(),
        "space normalized to hyphen"
    );
}
