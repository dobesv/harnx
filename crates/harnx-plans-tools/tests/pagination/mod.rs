use super::*;
use serde_json::{json, Value};

// Write fixtures without touching session activity, so list-only effects are observable.
async fn store_plan(
    context: &TestContext,
    caller: &SessionRef,
    slug: &str,
) -> harnx_core::cid_url::CidUrl {
    use harnx_blob_store::plans::{
        create_document, ensure_plans_bucket, serialize_plan, PlanDocument, PlanFrontMatter,
    };
    use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef as CidSession};
    let url = CidUrl::Plan {
        session: CidSession::new(caller.agent.clone(), caller.session_id.clone()).unwrap(),
        slug: slug.to_string(),
        item: PlanItem::Index,
    };
    let document = PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            title: Some(slug.to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        },
        body: String::new(),
    };
    let store = ensure_plans_bucket(&context.jetstream, 1).await.unwrap();
    create_document(&store, &url, &serialize_plan(&document).unwrap())
        .await
        .unwrap();
    url
}

fn listed_ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|item| item["id"].as_str().expect("item ID").to_string())
        .collect()
}

fn assert_page(page: &Value, expected_ids: &[String], has_more: bool) {
    assert_eq!(listed_ids(page), expected_ids);
    if has_more {
        assert!(
            page["next_cursor"].is_string(),
            "expected another page: {page}"
        );
    } else {
        assert_eq!(page.get("next_cursor"), Some(&Value::Null));
    }
}

async fn list_page(context: &TestContext, tool: &str, args: Value) -> Value {
    json_result(&context.invoke(tool, args, None).await)
}

async fn collect_plan_pages(context: &TestContext, first: &Value) -> Vec<String> {
    let mut ids = listed_ids(first);
    let mut cursor = first["next_cursor"].clone();
    for _ in 0..10 {
        if cursor.is_null() {
            break;
        }
        let page = list_page(context, "list_plans", json!({"cursor": cursor})).await;
        ids.extend(listed_ids(&page));
        cursor = page["next_cursor"].clone();
    }
    assert!(cursor.is_null(), "pagination must terminate");
    ids
}

#[tokio::test]
async fn list_plans_defaults_to_caller_and_all_sessions_is_explicit() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let own = store_plan(&context, &context.caller, "own").await;
    let other_caller = SessionRef {
        session_id: "other-session".to_string(),
        ..context.caller.clone()
    };
    let other = store_plan(&context, &other_caller, "other").await;
    // An explicit caller exercises the same scoped default as tool invocations with identity.
    let own_page = json_result(
        &context
            .invoke("list_plans", json!({}), Some(context.caller.clone()))
            .await,
    );
    assert_page(&own_page, &[own.to_string()], false);

    for (args, caller) in [
        (json!({"all_sessions": true}), Some(context.caller.clone())),
        (json!({}), None),
    ] {
        let page = json_result(&context.invoke("list_plans", args, caller).await);
        let mut expected = [own.clone(), other.clone()];
        expected.sort_by_key(|url| url.kv_key());
        assert_page(&page, &expected.map(|url| url.to_string()), false);
    }
    let other_page = json_result(
        &context
            .invoke(
                "list_plans",
                json!({"owner": other.owner()}),
                Some(context.caller.clone()),
            )
            .await,
    );
    assert_page(&other_page, &[other.to_string()], false);
    let conflict = context
        .invoke(
            "list_plans",
            json!({"owner": own.owner(), "all_sessions": true}),
            None,
        )
        .await;
    assert_error(
        &conflict,
        "owner and all_sessions: true cannot be used together",
    );
    let invalid = context
        .invoke("list_plans", json!({"owner": "not-an-owner"}), None)
        .await;
    assert_error(
        &invalid,
        "owner must be a 64-character lowercase hexadecimal session key",
    );
    let empty_caller = SessionRef {
        session_id: "empty-session".to_string(),
        ..context.caller.clone()
    };
    let empty = json_result(
        &context
            .invoke("list_plans", json!({}), Some(empty_caller))
            .await,
    );
    assert_eq!(empty, json!({"items": [], "next_cursor": null}));
}

#[tokio::test]
async fn list_plans_paginates_sorted_keys_clamps_limits_and_continues_after_deletion() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let mut plans = Vec::new();
    for index in (0..105).rev() {
        plans.push(store_plan(&context, &context.caller, &format!("page-{index:03}")).await);
    }
    plans.sort_by_key(|url| url.kv_key());
    let expected: Vec<_> = plans.iter().map(ToString::to_string).collect();

    let first = list_page(&context, "list_plans", json!({})).await;
    assert_page(&first, &expected[..20], true);
    let clamped = list_page(&context, "list_plans", json!({"limit": 1000})).await;
    assert_page(&clamped, &expected[..100], true);
    let last = list_page(
        &context,
        "list_plans",
        json!({"limit": 1000, "cursor": clamped["next_cursor"]}),
    )
    .await;
    assert_page(&last, &expected[100..], false);
    let minimum = list_page(&context, "list_plans", json!({"limit": 0})).await;
    assert_page(&minimum, &expected[..1], true);

    let store = context
        .jetstream
        .get_key_value(harnx_blob_store::plans::PLAN_BUCKET)
        .await
        .unwrap();
    store.delete(plans[19].kv_key()).await.unwrap();
    assert_eq!(collect_plan_pages(&context, &first).await, expected);

    let invalid = context
        .invoke("list_plans", json!({"cursor": "invalid"}), None)
        .await;
    assert_error(&invalid, "invalid cursor");
    let wrong_scope = context
        .invoke(
            "list_plans",
            json!({"owner": plans[0].owner(), "cursor": first["next_cursor"]}),
            None,
        )
        .await;
    assert_error(&wrong_scope, "different list scope");
}

#[tokio::test]
async fn list_plans_counts_live_keys_without_fetching_children_or_other_pages() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = store_plan(&context, &context.caller, "a").await;
    let next = store_plan(&context, &context.caller, "aa").await;
    let store = context
        .jetstream
        .get_key_value(harnx_blob_store::plans::PLAN_BUCKET)
        .await
        .unwrap();
    // Invalid UTF-8 makes any accidental body fetch fail, even without parsing.
    store.put(next.kv_key(), vec![0xff].into()).await.unwrap();
    let prefix = harnx_blob_store::plans::plan_prefix(&plan).unwrap();
    for key in [
        "tasks/plan",
        "notes/one",
        "notes/two",
        "tasks/deleted",
        "notes/purged",
    ] {
        store
            .put(format!("{prefix}{key}"), vec![0xff].into())
            .await
            .unwrap();
    }
    store
        .delete(format!("{prefix}tasks/deleted"))
        .await
        .unwrap();
    store.purge(format!("{prefix}notes/purged")).await.unwrap();
    store
        .put(
            format!(
                "{}tasks/unrelated",
                harnx_blob_store::plans::plan_prefix(&next).unwrap()
            ),
            vec![0xff].into(),
        )
        .await
        .unwrap();
    let deleted = store_plan(&context, &context.caller, "0-deleted").await;
    store.delete(deleted.kv_key()).await.unwrap();
    let purged = store_plan(&context, &context.caller, "0-purged").await;
    store.purge(purged.kv_key()).await.unwrap();

    let page = list_page(&context, "list_plans", json!({"limit": 1})).await;
    assert_page(&page, &[plan.to_string()], true);
    assert_eq!(page["items"][0]["task_count"], 1);
    assert_eq!(page["items"][0]["note_count"], 2);
}

#[tokio::test]
async fn list_plans_touches_only_returned_page_sessions() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let mut plans = Vec::new();
    for session_id in ["page-owner-a", "page-owner-b", "page-owner-c"] {
        let caller = SessionRef {
            session_id: session_id.to_string(),
            ..context.caller.clone()
        };
        plans.push(store_plan(&context, &caller, "touch").await);
    }
    plans.sort_by_key(|url| url.kv_key());
    let first = json_result(
        &context
            .invoke(
                "list_plans",
                json!({"all_sessions": true, "limit": 1}),
                Some(context.caller.clone()),
            )
            .await,
    );
    assert_page(&first, &[plans[0].to_string()], true);
    let activity = context
        .jetstream
        .get_key_value("harnx_sessions")
        .await
        .expect("activity bucket");
    assert_session_activity(&activity, &plans, 1).await;

    let second = json_result(
        &context
            .invoke(
                "list_plans",
                json!({"all_sessions": true, "limit": 1, "cursor": first["next_cursor"]}),
                None,
            )
            .await,
    );
    assert_page(&second, &[plans[1].to_string()], true);
    assert_session_activity(&activity, &plans, 2).await;
}

async fn assert_session_activity(
    activity: &async_nats::jetstream::kv::Store,
    plans: &[harnx_core::cid_url::CidUrl],
    touched_count: usize,
) {
    for (index, plan) in plans.iter().enumerate() {
        let entry = activity
            .get(format!("sessions/{}/activity", plan.owner()))
            .await
            .unwrap();
        assert_eq!(
            entry.is_some(),
            index < touched_count,
            "only returned page sessions are touched"
        );
    }
}

async fn create_child_items(context: &TestContext, plan: &str) {
    for id in ["e", "d", "c", "b", "a"] {
        create_task(context, plan, id, Vec::new()).await;
        let result = context
            .invoke(
                "add_note",
                json!({"plan": plan, "id": id, "body": id}),
                None,
            )
            .await;
        assert_ne!(result["isError"], true);
    }
}

async fn assert_child_pages(context: &TestContext, tool: &str, plan: &str, other: &str) {
    let child = if tool == "list_tasks" {
        "tasks"
    } else {
        "notes"
    };
    let store = context
        .jetstream
        .get_key_value(harnx_blob_store::plans::PLAN_BUCKET)
        .await
        .unwrap();
    let unrelated = harnx_core::cid_url::CidUrl::parse(&format!("{other}/{child}/broken")).unwrap();
    store
        .put(unrelated.kv_key(), vec![0xff].into())
        .await
        .unwrap();
    for (id, purge) in [("00-deleted", false), ("00-purged", true)] {
        let url = harnx_core::cid_url::CidUrl::parse(&format!("{plan}/{child}/{id}")).unwrap();
        store.put(url.kv_key(), vec![0xff].into()).await.unwrap();
        if purge {
            store.purge(url.kv_key()).await.unwrap();
        } else {
            store.delete(url.kv_key()).await.unwrap();
        }
    }

    let last_url = harnx_core::cid_url::CidUrl::parse(&format!("{plan}/{child}/e")).unwrap();
    let original = store.get(last_url.kv_key()).await.unwrap().unwrap();
    store
        .put(last_url.kv_key(), vec![0xff].into())
        .await
        .unwrap();
    let first = list_page(context, tool, json!({"plan": plan, "limit": 2})).await;
    assert_page(
        &first,
        &[format!("{plan}/{child}/a"), format!("{plan}/{child}/b")],
        true,
    );
    store.put(last_url.kv_key(), original).await.unwrap();

    let second = list_page(
        context,
        tool,
        json!({"plan": plan, "limit": 2, "cursor": first["next_cursor"]}),
    )
    .await;
    assert_page(
        &second,
        &[format!("{plan}/{child}/c"), format!("{plan}/{child}/d")],
        true,
    );
    let last = list_page(
        context,
        tool,
        json!({"plan": plan, "limit": 2, "cursor": second["next_cursor"]}),
    )
    .await;
    assert_page(&last, &[last_url.to_string()], false);

    let wrong_plan = context
        .invoke(
            tool,
            json!({"plan": other, "cursor": first["next_cursor"]}),
            None,
        )
        .await;
    assert_error(&wrong_plan, "different list scope");
}

#[tokio::test]
async fn list_tasks_paginates_and_fetches_only_page_documents() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Task Pages").await;
    let other = create_plan(&context, "Unrelated Tasks").await;
    create_child_items(&context, &plan).await;
    assert_child_pages(&context, "list_tasks", &plan, &other).await;
}

#[tokio::test]
async fn list_notes_paginates_and_fetches_only_page_documents() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Note Pages").await;
    let other = create_plan(&context, "Unrelated Notes").await;
    create_child_items(&context, &plan).await;
    assert_child_pages(&context, "list_notes", &plan, &other).await;
}

#[tokio::test]
async fn list_tasks_filters_within_each_page() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let plan = create_plan(&context, "Filtered Task Pages").await;
    for id in ["e", "d", "c", "b", "a"] {
        create_task(&context, &plan, id, Vec::new()).await;
    }

    let closed = context
        .invoke(
            "update_task",
            json!({"plan": plan, "id": format!("{plan}/tasks/a"), "status": "closed"}),
            None,
        )
        .await;
    assert_ne!(closed["isError"], true);
    let open_page = list_page(&context, "list_tasks", json!({"plan": plan, "limit": 1})).await;
    assert!(listed_ids(&open_page).is_empty());
    assert!(open_page["next_cursor"].is_string());
    for filter in ["closed", "all"] {
        let page = list_page(
            &context,
            "list_tasks",
            json!({"plan": plan, "limit": 1, "filter": filter}),
        )
        .await;
        assert_eq!(listed_ids(&page), [format!("{plan}/tasks/a")]);
    }

    let tagged = context
        .invoke(
            "update_task",
            json!({"plan": plan, "id": format!("{plan}/tasks/b"), "tags": ["selected"]}),
            None,
        )
        .await;
    assert_ne!(tagged["isError"], true);
    let empty = list_page(
        &context,
        "list_tasks",
        json!({"plan": plan, "limit": 1, "tag": "selected"}),
    )
    .await;
    assert!(listed_ids(&empty).is_empty());
    assert!(empty["next_cursor"].is_string());
    let matching = list_page(
        &context,
        "list_tasks",
        json!({"plan": plan, "limit": 1, "tag": "selected", "cursor": empty["next_cursor"]}),
    )
    .await;
    assert_eq!(listed_ids(&matching), [format!("{plan}/tasks/b")]);
}
