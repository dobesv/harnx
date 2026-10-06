use crate::{
    support::DEADLINE,
    unary::{message, Http},
};
use anyhow::Result;
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compat_legacy_send_versions_blocking_and_canonical_completion() -> Result<()> {
    let http = Http::start().await?;
    for (index, version, blocking) in [
        (0, None, true),
        (1, Some("1.0"), false),
        (2, Some("0.3"), true),
        (3, Some("1.0.12"), false),
    ] {
        let mut msg = message(&format!("compat-{index}"), None);
        msg["role"] = json!("user");
        let mut request = http.client.post(&http.url).header("X-User-ID", "alice");
        if let Some(version) = version {
            request = request.header("a2a-version", version);
        }
        let send = request.json(&json!({"jsonrpc":"2.0","id":"legacy-send","method":"message/send","params":{"message":msg,"configuration":{"blocking":blocking}}})).send();
        tokio::pin!(send);
        let response = if blocking {
            tokio::select! {
                result = &mut send => anyhow::bail!("blocking send returned before model release: {result:?}"),
                result = tokio::time::timeout(DEADLINE, http.h.llm.requested.notified()) => { result?; },
            }
            http.h.llm.release.notify_one();
            send.await?
        } else {
            send.await?
        };
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await?;
        assert_eq!(response["id"], "legacy-send");
        let task = &response["result"]["task"];
        assert_eq!(
            task["status"]["state"],
            if blocking {
                "TASK_STATE_COMPLETED"
            } else {
                "TASK_STATE_WORKING"
            },
            "{response}"
        );
        let task = if blocking {
            task.clone()
        } else {
            tokio::time::timeout(DEADLINE, http.h.llm.requested.notified()).await?;
            // Legacy state filter must pass typed decode and find the admitted task.
            for state in ["working", "TASK_STATE_WORKING"] {
                let list = http
                    .rpc(
                        "ListTasks",
                        json!({"contextId":task["contextId"],"status":state}),
                    )
                    .await?;
                assert_eq!(list["result"]["tasks"][0]["id"], task["id"], "{list}");
            }
            http.h.llm.release.notify_one();
            http.completed(&task["id"]).await?["result"].clone()
        };
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(task["history"][0]["role"], "ROLE_USER");
        assert_eq!(task["status"]["message"]["role"], "ROLE_AGENT");
        assert_eq!(task["artifacts"][0]["parts"][0]["text"], "Hello world");
        let get = http.rpc("tasks/get", json!({"id":task["id"]})).await?;
        assert_eq!(get["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compat_conflicts_versions_identity_and_public_cards() -> Result<()> {
    let http = Http::start().await?;
    for (version, config, expected) in [
        (
            "1.0",
            json!({"blocking":true,"returnImmediately":true}),
            -32602,
        ),
        (
            "1.0",
            json!({"blocking":false,"returnImmediately":false}),
            -32602,
        ),
        ("2.0", json!({}), -32009),
        ("0.2", json!({}), -32009),
    ] {
        let response: Value = http.client.post(&http.url).header("a2a-version", version).header("X-User-ID", "alice")
            .json(&json!({"jsonrpc":"2.0","id":123,"method":"message/send","params":{"message":message("rejected",None),"configuration":config}}))
            .send().await?.json().await?;
        assert_eq!(response["error"]["code"], expected, "{response}");
        assert_eq!(response["id"], 123);
    }
    let response = http
        .client
        .post(&http.url)
        .header("a2a-version", "2.0")
        .json(&json!({"jsonrpc":"2.0","id":123,"method":"message/send","params":{}}))
        .send()
        .await?;
    assert_eq!(response.status(), 401);
    let response: Value = response.json().await?;
    assert_eq!(response["error"]["code"], -32000);
    for path in ["agent-card.json", "agent.json"] {
        assert_eq!(
            http.client
                .get(format!("{}/.well-known/{path}", http.url))
                .send()
                .await?
                .status(),
            200
        );
    }
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}
