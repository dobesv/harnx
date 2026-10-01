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
                "parent_issue": 0,
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
                "parent_issue": 0,
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
