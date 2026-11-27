use super::{
    run_bash_script, start_script_server, stdout_block, time_server_config, workspace_binary,
    EnvGuard, TOKEN,
};
use crate::common;
use anyhow::Result;
use harnx_blob_store::plans::{create_document, ensure_plans_bucket};
use harnx_core::cid_url::CidUrl;
use harnx_runtime::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use std::fmt::Write;

const PLAN: &str = "cid:plan:pantheon%2Fatlas/script/roadmap";

#[tokio::test(flavor = "multi_thread")]
async fn nats_plans_read_tools_render_json_and_python_dicts() -> Result<()> {
    let Some(broker) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, broker.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let binary = workspace_binary("harnx-plans-tools")?;
    let mut server = time_server_config(&binary.to_string_lossy());
    server.name = "plans".into();
    let (scope, _supervisor, config) = start_script_server(
        broker.url(),
        server,
        "__plans__plans",
        &["plans_get_*", "plans_list_*"],
    )
    .await?;

    let task = format!("{PLAN}/tasks/review");
    let note = format!("{PLAN}/notes/design");
    seed_documents(broker.url(), &task, &note).await?;
    let plan_arg = format!("--plan {PLAN}");
    let cases = [
        ("plans_get_plan", plan_arg.clone(), ".id", PLAN),
        (
            "plans_get_task",
            format!("{plan_arg} --id {task}"),
            ".id",
            &task,
        ),
        (
            "plans_get_note",
            format!("{plan_arg} --note-id design"),
            ".id",
            &note,
        ),
        (
            "plans_list_plans",
            "--all-sessions".into(),
            ".items[0].id",
            PLAN,
        ),
        ("plans_list_tasks", plan_arg.clone(), ".items[0].id", &task),
        ("plans_list_notes", plan_arg, ".items[0].id", &note),
    ];
    let mut script = String::from("set -o pipefail\n");
    let mut expected = Vec::new();
    for (tool, args, query, id) in cases {
        for output in ["", "-o text", "-o json"] {
            writeln!(script, "{tool} {args} {output} | jq -r '{query}'")?;
            expected.push(id);
        }
    }
    write!(
        script,
        r#"python3 - <<'PY'
print(tools.plans_get_plan(plan={PLAN:?})['body'])
print(tools.plans_get_task(plan={PLAN:?}, id={task:?})['body'])
print(tools.plans_get_note(plan={PLAN:?}, note_id='design')['body'])
print(tools.plans_list_plans(all_sessions=True)['items'][0]['id'])
print(tools.plans_list_tasks(plan={PLAN:?})['items'][0]['id'])
print(tools.plans_list_notes(plan={PLAN:?})['items'][0]['id'])
PY
"#,
    )?;
    expected.extend(["Plan body", "Task body", "Note body", PLAN, &task, &note]);
    let output = run_bash_script(&config, &scope, &script).await?;
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(&output), expected.join("\n"), "{output}");

    let output = run_bash_script(
        &config,
        &scope,
        "help plans_get_plan\nhelp plans_get_task\nhelp plans_get_note\n\
         help plans_list_plans\nhelp plans_list_tasks\nhelp plans_list_notes",
    )
    .await?;
    assert_eq!(output["status"], "ok", "{output}");
    let help = stdout_block(&output);
    assert_eq!(help.matches("FIELDS (-o json)").count(), 6, "{help}");
    assert_eq!(help.matches("array of object").count(), 3, "{help}");
    assert_eq!(
        help.matches("one page of matching items as JSON").count(),
        3,
        "{help}"
    );
    assert_eq!(
        help.matches("opaque cursor for the next page").count(),
        3,
        "{help}"
    );
    for description in [
        "canonical URLs of tasks in the plan",
        "canonical URLs of prerequisite tasks",
        "short description of the note",
    ] {
        assert!(help.contains(description), "{help}");
    }
    Ok(())
}

async fn seed_documents(url: &str, task: &str, note: &str) -> Result<()> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(url)
        .await?;
    let store = ensure_plans_bucket(&async_nats::jetstream::new(client), 1).await?;
    for (id, metadata, body) in [
        (PLAN, "title: Roadmap\n".to_string(), "Plan body"),
        (task, format!("title: Review\nplan: {PLAN}\n"), "Task body"),
        (note, "summary: Design\n".to_string(), "Note body"),
    ] {
        let content =
            format!("---\nid: {id}\ncreated_at: '2026-01-01T00:00:00Z'\n{metadata}---\n{body}");
        create_document(&store, &CidUrl::parse(id)?, &content).await?;
    }
    Ok(())
}
