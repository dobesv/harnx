use harnx_plans_tools::PlansToolset;
use harnx_test_bins::{spawn_nats_server, NatsServerHandle};
use harnx_toolset::{SessionRef, ToolInvocation, ToolInvocationContext, Toolset};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct TestContext {
    _nats: NatsServerHandle,
    toolset: PlansToolset,
    jetstream: async_nats::jetstream::Context,
    caller: SessionRef,
}

#[derive(Clone, Copy)]
struct PlanItems<'a> {
    plan: &'a str,
    task: &'a str,
    dependency: &'a str,
    note: &'a str,
}

impl TestContext {
    async fn start() -> Option<Self> {
        let nats = spawn_nats_server().await.expect("start nats-server")?;
        let client = async_nats::connect(nats.url())
            .await
            .expect("connect test NATS");
        let jetstream = async_nats::jetstream::new(client);
        static NEXT_SESSION: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(1);
        let number = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Self {
            toolset: PlansToolset::with_nats_url(nats.url().to_string()),
            _nats: nats,
            jetstream,
            caller: SessionRef {
                agent: Some("pantheon/atlas".to_string()),
                session_id: format!("t{number:05}"),
            },
        })
    }

    async fn invoke(&self, tool: &str, args: Value, session: Option<SessionRef>) -> Value {
        self.toolset
            .invoke_with_context(ToolInvocation {
                tool: tool.to_string(),
                args,
                context: ToolInvocationContext {
                    invoking_session: session,
                    ..ToolInvocationContext::default()
                },
                cancel: CancellationToken::new(),
            })
            .await
            .expect("plans invocation returns a result")
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().expect("text result")
}

fn cid_from(result: &Value) -> String {
    text(result)
        .split_whitespace()
        .find(|word| word.starts_with("cid:plan:"))
        .expect("result contains cid:plan URL")
        .trim_end_matches([',', '.'])
        .to_string()
}

fn json_result(result: &Value) -> Value {
    serde_json::from_str(text(result)).expect("JSON tool result")
}

fn assert_error(result: &Value, expected: &str) {
    assert_eq!(result["isError"], true, "expected tool error: {result}");
    assert!(
        text(result).contains(expected),
        "unexpected error: {}",
        text(result)
    );
}

fn assert_linked_resource(result: &Value, url: &str, name: &str) {
    let content = result["content"].as_array().expect("content array");
    assert!(
        content.iter().any(|block| {
            block["type"] == "resource_link"
                && block["uri"] == url
                && block["name"] == name
                && block["mimeType"] == "text/markdown; charset=utf-8"
        }),
        "missing {name} resource link for {url}: {result}"
    );
    let markdown_target = format!("]({url})");
    assert!(
        content.iter().any(|block| {
            block["type"] == "text"
                && block["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&markdown_target))
        }),
        "missing markdown link for {url}: {result}"
    );
}

fn assert_item_resources(result: &Value, plan: &str, item: &str, name: &str) {
    assert_linked_resource(result, plan, "plan");
    assert_linked_resource(result, item, name);
}

fn assert_resource_label(result: &Value, url: &str) {
    let id = url.rsplit('/').next().expect("item URL has an ID");
    let label = format!("[{id}]({url})");
    assert!(
        result["content"]
            .as_array()
            .expect("content array")
            .iter()
            .any(|block| {
                block["type"] == "text" && block["text"].as_str().unwrap_or("").contains(&label)
            }),
        "missing fallback label for {url}: {result}"
    );
}

async fn create_plan(context: &TestContext, name: &str) -> String {
    let result = context
        .invoke(
            "add_plan",
            json!({"name": name, "title": "Test plan", "content": "initial body"}),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(result["isError"], true);
    let url = cid_from(&result);
    assert_linked_resource(&result, &url, "plan");
    url
}

#[tokio::test]
async fn invoke_with_context_requires_identity_before_connecting() {
    let toolset = PlansToolset::with_nats_url("nats://127.0.0.1:1");
    let result = toolset
        .invoke_with_context(ToolInvocation {
            tool: "add_plan".to_string(),
            args: json!({"name": "offline"}),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        })
        .await
        .expect("missing identity is a tool result");
    assert_error(&result, "caller session identity required to create plan");
}

#[tokio::test]
async fn add_plan_requires_context_slugifies_and_deduplicates() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let missing = context
        .invoke("add_plan", json!({"name": "NATS Attachments"}), None)
        .await;
    assert_error(&missing, "caller session identity required to create plan");

    let first = create_plan(&context, "NATS Attachments !!!").await;
    let second = create_plan(&context, "NATS Attachments !!!").await;
    assert!(
        first.ends_with("/nats-attachments"),
        "unexpected URL: {first}"
    );
    assert!(
        second.ends_with("/nats-attachments-2"),
        "unexpected URL: {second}"
    );

    let bare = context
        .invoke("get_plan", json!({"plan": "nats-attachments"}), None)
        .await;
    let bare_task = context
        .invoke("get_task", json!({"plan": first, "id": "task-one"}), None)
        .await;
    assert_error(
        &bare_task,
        "cid:plan:<agent>/<session-id>/<slug>/tasks/<task-id>",
    );
    assert_error(&bare, "expected cid:plan:<agent>/<session-id>/<slug>");
}

#[tokio::test]
async fn blank_task_title_uses_task_url_as_resource_label() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Blank Task Title").await;
    let added = context
        .invoke(
            "add_task",
            json!({
                "plan": plan,
                "id": "blank-title",
                "title": "  \n ",
                "body": "task body",
            }),
            None,
        )
        .await;
    let task = cid_from(&added);
    assert_item_resources(&added, &plan, &task, "task");
    assert_resource_label(&added, &task);

    let fetched = context
        .invoke("get_task", json!({"plan": plan, "id": task}), None)
        .await;
    assert_item_resources(&fetched, &plan, &task, "task");
    assert_resource_label(&fetched, &task);

    let updated = context
        .invoke(
            "update_task",
            json!({"plan": plan, "id": task, "title": "  \n "}),
            None,
        )
        .await;
    assert_item_resources(&updated, &plan, &task, "task");
    assert_resource_label(&updated, &task);
}

#[tokio::test]
async fn plan_task_and_note_crud_uses_full_urls() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "CRUD Plan").await;
    assert_plan_update(&context, &plan).await;
    let task = create_task(&context, &plan, "first-task", Vec::new()).await;
    let dependent = create_task(&context, &plan, "dependent", vec![task.clone()]).await;
    let note = create_note(&context, &plan).await;
    let items = PlanItems {
        plan: &plan,
        task: &dependent,
        dependency: &task,
        note: &note,
    };
    assert_task_update(&context, items).await;
    assert_note_update(&context, &plan, &note).await;
    assert_plan_lists_items(&context, items).await;
    delete_items_and_plan(&context, items).await;
}

#[tokio::test]
async fn get_plan_segregates_task_and_note_ids() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Segregated Children").await;
    let task = create_task(&context, &plan, "implementation", Vec::new()).await;
    let note = create_note(&context, &plan).await;

    let fetched = context
        .invoke("get_plan", json!({"plan": plan}), None)
        .await;
    let value = json_result(&fetched);
    assert_eq!(value["task_ids"], json!([task]));
    assert_eq!(value["note_ids"], json!([note]));
}

#[tokio::test]
async fn list_plans_ignores_task_named_plan() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Task Named Plan").await;
    create_task(&context, &plan, "plan", Vec::new()).await;
    let owner = harnx_core::cid_url::CidUrl::parse(&plan).unwrap().owner();

    let listed = context
        .invoke("list_plans", json!({"owner": owner}), None)
        .await;
    let plans = json_result(&listed);
    assert_eq!(plans.as_array().unwrap().len(), 1);
    assert_eq!(plans[0]["id"], plan);
    assert_eq!(plans[0]["task_count"], 1);
}

#[tokio::test]
async fn update_plan_requires_caller_when_creating() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Deleted Before Upsert").await;
    context
        .invoke("delete_plan", json!({"plan": plan}), None)
        .await;

    let result = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "summary": "recreated"}),
            None,
        )
        .await;
    assert_error(&result, "caller session identity required to create a plan");
}

async fn assert_issue_metadata(
    context: &TestContext,
    plan: &str,
    expected: Option<u64>,
    external_url: Option<&str>,
) -> Value {
    let fetched = context
        .invoke("get_plan", json!({"plan": plan}), None)
        .await;
    let value = json_result(&fetched);
    assert_eq!(value.get("github_issue"), Some(&json!(expected)));
    assert_eq!(value.get("external_task_url"), Some(&json!(external_url)));
    assert!(value.get("parent_issue").is_none());

    let listed = json_result(&context.invoke("list_plans", json!({}), None).await);
    let listed_plan = listed
        .as_array()
        .expect("plans array")
        .iter()
        .find(|value| value["id"] == plan)
        .expect("plan is listed");
    assert_eq!(listed_plan.get("github_issue"), Some(&json!(expected)));
    assert_eq!(
        listed_plan.get("external_task_url"),
        Some(&json!(external_url))
    );
    assert!(listed_plan.get("parent_issue").is_none());

    let url = harnx_core::cid_url::CidUrl::parse(plan).expect("plan URL");
    let store = context
        .jetstream
        .get_key_value(harnx_blob_store::plans::PLAN_BUCKET)
        .await
        .expect("plans bucket");
    let stored = harnx_blob_store::plans::get_document(&store, &url)
        .await
        .expect("read stored plan")
        .expect("plan exists in NATS");
    let document = harnx_blob_store::plans::parse_plan(&stored.content).expect("plan front matter");
    assert_eq!(document.front.github_issue, expected);
    assert_eq!(document.front.external_task_url.as_deref(), external_url);
    assert_eq!(stored.content.contains("github_issue:"), expected.is_some());
    assert_eq!(
        stored.content.contains("external_task_url:"),
        external_url.is_some()
    );
    assert!(!stored.content.contains("parent_issue:"));
    value
}

#[tokio::test]
async fn github_issue_is_persisted_on_create_and_can_be_changed() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let added = context
        .invoke(
            "add_plan",
            json!({
                "name": "Parent Issue",
                "github_owner_repo": "dobesv/harnx",
                "github_issue": 2266,
                "content": "issue investigation",
            }),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(added["isError"], true, "add_plan failed: {added}");
    let plan = cid_from(&added);
    assert_issue_metadata(&context, &plan, Some(2266), None).await;

    let updated = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "github_issue": 2175}),
            None,
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
    let fetched = assert_issue_metadata(&context, &plan, Some(2175), None).await;
    assert_eq!(fetched["github_owner_repo"], "dobesv/harnx");
    assert_eq!(fetched["body"], "issue investigation");
}

#[tokio::test]
async fn existing_plan_can_set_github_issue_and_placeholder_updates_preserve_it() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Set Parent Issue").await;
    assert_issue_metadata(&context, &plan, None, None).await;
    let task_url = "https://tracker.invalid/browse/HARNX-2266";
    for args in [
        json!({"plan": plan, "github_issue": 2266, "external_task_url": task_url}),
        json!({"plan": plan, "summary": "omitted issue"}),
        json!({"plan": plan, "github_issue": null, "external_task_url": null}),
        json!({"plan": plan, "github_issue": 0, "external_task_url": ""}),
        json!({"plan": plan, "parent_issue": 0, "external_task_url": " \t\n "}),
        json!({"plan": plan, "parent_issue": null}),
        json!({"plan": plan, "github_owner_repo": "legacy repo value"}),
    ] {
        let updated = context.invoke("update_plan", args, None).await;
        assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
        assert_issue_metadata(&context, &plan, Some(2266), Some(task_url)).await;
    }
    let fetched = json_result(
        &context
            .invoke("get_plan", json!({"plan": plan}), None)
            .await,
    );
    assert_eq!(fetched["title"], "Test plan");
    assert_eq!(fetched["body"], "initial body");
    assert_eq!(fetched["summary"], "omitted issue");
}

#[tokio::test]
async fn update_plan_upsert_persists_github_issue() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = format!(
        "cid:plan:pantheon%2Fatlas/{}/parent-issue-upsert",
        context.caller.session_id
    );
    let updated = context
        .invoke(
            "update_plan",
            json!({
                "plan": plan,
                "github_issue": 2266,
                "github_owner_repo": "dobesv/harnx",
                "external_task_url": "https://github.com/dobesv/harnx/issues/2266",
                "replace_content": "created by update",
            }),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
    let fetched = assert_issue_metadata(
        &context,
        &plan,
        Some(2266),
        Some("https://github.com/dobesv/harnx/issues/2266"),
    )
    .await;
    assert_eq!(fetched["github_owner_repo"], "dobesv/harnx");
    assert_eq!(fetched["body"], "created by update");
}

#[tokio::test]
async fn blank_optional_arguments_are_treated_as_omitted() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let added = context
        .invoke(
            "add_plan",
            json!({
                "name": "Placeholder Args",
                "title": "Kept title",
                "body": "",
                "content": "real body",
                "summary": "",
                "github_issue": 0,
            }),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(added["isError"], true, "add_plan failed: {added}");
    let plan = cid_from(&added);

    let updated = context
        .invoke(
            "update_plan",
            json!({
                "plan": plan,
                "content": "",
                "replace_content": " ",
                "append_content": "more",
                "title": "",
                "github_issue": 0,
            }),
            None,
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");

    let fetched = context
        .invoke("get_plan", json!({"plan": plan}), None)
        .await;
    let value = json_result(&fetched);
    assert_eq!(value["body"], "real body\nmore");
    assert_eq!(value["title"], "Kept title");
    assert_eq!(value["summary"], Value::Null);
}

async fn assert_plan_update(context: &TestContext, plan: &str) {
    let updated = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "append_content": "second line", "summary": "updated"}),
            None,
        )
        .await;
    assert!(text(&updated).contains(plan));
    assert_linked_resource(&updated, plan, "plan");
    let fetched = context
        .invoke("get_plan", json!({"plan": plan}), None)
        .await;
    assert_linked_resource(&fetched, plan, "plan");
    let value = json_result(&fetched);
    assert_eq!(value["body"], "initial body\nsecond line");
    assert_eq!(value["summary"], "updated");
}

async fn create_task(
    context: &TestContext,
    plan: &str,
    id: &str,
    dependencies: Vec<String>,
) -> String {
    let result = context
        .invoke(
            "add_task",
            json!({
                "plan": plan,
                "id": id,
                "title": id,
                "body": "task body",
                "dependencies": dependencies,
            }),
            None,
        )
        .await;
    let url = cid_from(&result);
    assert!(url.contains("/tasks/"));
    assert_item_resources(&result, plan, &url, "task");
    url
}

async fn assert_task_update(context: &TestContext, items: PlanItems<'_>) {
    let updated = context
        .invoke(
            "update_task",
            json!({
                "plan": items.plan,
                "id": items.task,
                "status": "closed",
                "append_body": "done",
            }),
            None,
        )
        .await;
    assert!(text(&updated).contains(items.task));
    assert_item_resources(&updated, items.plan, items.task, "task");
    let fetched = context
        .invoke(
            "get_task",
            json!({"plan": items.plan, "id": items.task}),
            None,
        )
        .await;
    assert_item_resources(&fetched, items.plan, items.task, "task");
    let value = json_result(&fetched);
    assert_eq!(value["status"], "closed");
    assert_eq!(value["dependencies"][0], items.dependency);
    assert_eq!(value["body"], "task body\ndone");
}

async fn create_note(context: &TestContext, plan: &str) -> String {
    let result = context
        .invoke(
            "add_note",
            json!({"plan": plan, "id": "design", "body": "note body"}),
            None,
        )
        .await;
    let url = cid_from(&result);
    assert!(url.contains("/notes/"));
    assert_item_resources(&result, plan, &url, "note");
    url
}

async fn assert_note_update(context: &TestContext, plan: &str, note: &str) {
    let updated = context
        .invoke(
            "update_note",
            json!({"plan": plan, "note_id": note, "append_body": "more"}),
            None,
        )
        .await;
    assert_item_resources(&updated, plan, note, "note");
    let fetched = context
        .invoke("get_note", json!({"plan": plan, "note_id": note}), None)
        .await;
    assert_item_resources(&fetched, plan, note, "note");
    assert_eq!(json_result(&fetched)["body"], "note body\nmore");
}

async fn assert_plan_lists_items(context: &TestContext, items: PlanItems<'_>) {
    let fetched = context
        .invoke("get_plan", json!({"plan": items.plan}), None)
        .await;
    let value = json_result(&fetched);
    assert!(value["task_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == items.task));
    assert!(value["note_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == items.note));

    let tasks = context
        .invoke(
            "list_tasks",
            json!({"plan": items.plan, "filter": "all"}),
            None,
        )
        .await;
    assert!(json_result(&tasks)
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == items.task));
    let notes = context
        .invoke("list_notes", json!({"plan": items.plan}), None)
        .await;
    assert!(json_result(&notes)
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == items.note));

    let owner = harnx_core::cid_url::CidUrl::parse(items.plan)
        .unwrap()
        .owner();
    let plans = context
        .invoke("list_plans", json!({"owner": owner}), None)
        .await;
    let listed = json_result(&plans);
    assert_eq!(listed[0]["task_count"], 2);
    assert_eq!(listed[0]["note_count"], 1);
}

async fn delete_items_and_plan(context: &TestContext, items: PlanItems<'_>) {
    for task in [items.dependency, items.task] {
        context
            .invoke("delete_task", json!({"plan": items.plan, "id": task}), None)
            .await;
    }
    context
        .invoke(
            "delete_note",
            json!({"plan": items.plan, "note_id": items.note}),
            None,
        )
        .await;
    context
        .invoke("delete_plan", json!({"plan": items.plan}), None)
        .await;
    let missing = context
        .invoke("get_plan", json!({"plan": items.plan}), None)
        .await;
    assert_error(&missing, "plan not found");
}

#[tokio::test]
async fn list_plans_filters_owner_and_touches_activity() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Owner Plan").await;
    let parsed = harnx_core::cid_url::CidUrl::parse(&plan).unwrap();
    let owner = parsed.owner();
    let listed = context
        .invoke("list_plans", json!({"owner": owner}), None)
        .await;
    assert_eq!(json_result(&listed).as_array().unwrap().len(), 1);

    let activity = context
        .jetstream
        .get_key_value("harnx_sessions")
        .await
        .expect("activity bucket");
    let key = format!("sessions/{}/activity", parsed.owner());
    assert!(activity.get(&key).await.unwrap().is_some());
}

#[tokio::test]
async fn external_task_url_supports_create_update_and_upsert_without_github() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let task_url = "https://tracker.invalid/browse/HARNX-2266?view=detail#comments";
    let added = context
        .invoke(
            "add_plan",
            json!({"name": "External Task", "external_task_url": format!("  {task_url} \n")}),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(added["isError"], true, "add_plan failed: {added}");
    let plan = cid_from(&added);
    let fetched = assert_issue_metadata(&context, &plan, None, Some(task_url)).await;
    assert_eq!(fetched["github_owner_repo"], Value::Null);

    let updated_url = "http://tracker.invalid/tasks/HARNX-2266";
    let updated = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "external_task_url": updated_url}),
            None,
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
    assert_issue_metadata(&context, &plan, None, Some(updated_url)).await;

    let upsert_plan = format!(
        "cid:plan:pantheon%2Fatlas/{}/external-upsert",
        context.caller.session_id
    );
    let upsert = context
        .invoke(
            "update_plan",
            json!({"plan": upsert_plan, "external_task_url": task_url}),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(upsert["isError"], true, "update_plan failed: {upsert}");
    assert_issue_metadata(&context, &upsert_plan, None, Some(task_url)).await;
}

#[tokio::test]
async fn legacy_parent_issue_callers_use_canonical_metadata() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let added = context
        .invoke(
            "add_plan",
            json!({"name": "Legacy Issue", "parent_issue": 2266}),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(added["isError"], true, "add_plan failed: {added}");
    let plan = cid_from(&added);
    assert_issue_metadata(&context, &plan, Some(2266), None).await;
    let updated = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "parent_issue": 2175}),
            None,
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
    assert_issue_metadata(&context, &plan, Some(2175), None).await;

    let upsert_plan = format!(
        "cid:plan:pantheon%2Fatlas/{}/legacy-upsert",
        context.caller.session_id
    );
    let upsert = context
        .invoke(
            "update_plan",
            json!({"plan": upsert_plan, "parent_issue": 2266}),
            Some(context.caller.clone()),
        )
        .await;
    assert_ne!(upsert["isError"], true, "update_plan failed: {upsert}");
    assert_issue_metadata(&context, &upsert_plan, Some(2266), None).await;
}

#[tokio::test]
async fn legacy_stored_parent_issue_is_read_and_migrated_on_update() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Legacy Stored Issue").await;
    let url = harnx_core::cid_url::CidUrl::parse(&plan).unwrap();
    let store = context
        .jetstream
        .get_key_value(harnx_blob_store::plans::PLAN_BUCKET)
        .await
        .unwrap();
    let legacy = format!(
        "---\nid: {plan}\nparent_issue: 2266\ncreated_at: 2026-09-29T00:00:00Z\n---\nlegacy body"
    );
    store.put(url.kv_key(), legacy.into()).await.unwrap();

    let fetched = json_result(
        &context
            .invoke("get_plan", json!({"plan": plan}), None)
            .await,
    );
    assert_eq!(fetched["github_issue"], 2266);
    assert!(fetched.get("parent_issue").is_none());
    let listed = json_result(&context.invoke("list_plans", json!({}), None).await);
    assert_eq!(listed[0]["github_issue"], 2266);
    assert!(listed[0].get("parent_issue").is_none());

    let updated = context
        .invoke(
            "update_plan",
            json!({"plan": plan, "summary": "migrated"}),
            None,
        )
        .await;
    assert_ne!(updated["isError"], true, "update_plan failed: {updated}");
    let fetched = assert_issue_metadata(&context, &plan, Some(2266), None).await;
    assert_eq!(fetched["body"], "legacy body");
    assert_eq!(fetched["created_at"], "2026-09-29T00:00:00Z");
    assert_eq!(fetched["github_owner_repo"], Value::Null);
}

#[tokio::test]
async fn invalid_external_task_urls_do_not_write_plan_metadata() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Invalid Task URL").await;
    let missing_plan = format!(
        "cid:plan:pantheon%2Fatlas/{}/invalid-upsert",
        context.caller.session_id
    );
    for invalid in [
        "HARNX-2266",
        "/browse/HARNX-2266",
        "//tracker.invalid/task",
        "not a url",
        "file:///tmp/task",
        "javascript:alert(1)",
        "data:text/plain,task",
        "ftp://tracker.invalid/task",
        "https://",
        "https://?task=2266",
        "https://tracker.invalid:bad/task",
        "https:///tracker.invalid/task",
        "https:tracker.invalid/task?next=https://tracker.invalid",
        "https://tracker.invalid/task with spaces",
        "https://tracker.invalid/ta\nsk",
        "https://tracker.invalid/ta\\sk",
    ] {
        for (tool, args) in [
            (
                "add_plan",
                json!({"name": "Invalid URL Create", "external_task_url": invalid}),
            ),
            (
                "update_plan",
                json!({"plan": missing_plan, "external_task_url": invalid}),
            ),
            (
                "update_plan",
                json!({"plan": plan, "github_issue": 2266, "external_task_url": invalid}),
            ),
        ] {
            let result = context
                .invoke(tool, args, Some(context.caller.clone()))
                .await;
            assert_error(
                &result,
                "external_task_url must be a valid absolute http or https URL",
            );
        }
    }
    assert_issue_metadata(&context, &plan, None, None).await;
    let listed = json_result(&context.invoke("list_plans", json!({}), None).await);
    assert_eq!(
        listed.as_array().unwrap().len(),
        1,
        "invalid creates must not store documents"
    );
}
