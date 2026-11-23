//! `.credentials` dispatch tests: parsing, fenced background execution and
//! the operator-identity note. The connect flow's transport itself is covered
//! by the `harnx-runtime` credentials engine tests; these pin the TUI seams.

use super::*;
use harnx_runtime::operator_tools::credentials::CredentialsAction;

#[test]
fn parses_credentials_dot_commands() {
    assert_eq!(
        crate::credentials::parse_credentials_dot_command(".credentials")
            .unwrap()
            .unwrap(),
        CredentialsAction::List
    );
    assert_eq!(
        crate::credentials::parse_credentials_dot_command(".credentials list")
            .unwrap()
            .unwrap(),
        CredentialsAction::List
    );
    assert_eq!(
        crate::credentials::parse_credentials_dot_command("/credentials connect jira")
            .unwrap()
            .unwrap(),
        CredentialsAction::Connect {
            server: "jira".into()
        }
    );
    assert_eq!(
        crate::credentials::parse_credentials_dot_command("  .credentials disconnect pendo  ")
            .unwrap()
            .unwrap(),
        CredentialsAction::Disconnect {
            server: "pendo".into()
        }
    );
    // Usage errors surface; other lines are not credentials commands.
    assert!(
        crate::credentials::parse_credentials_dot_command(".credentials remove")
            .unwrap()
            .is_err()
    );
    assert!(crate::credentials::parse_credentials_dot_command(".connect jira").is_none());
    assert!(crate::credentials::parse_credentials_dot_command(".credentialsx").is_none());
}

#[tokio::test]
async fn credentials_command_runs_on_the_fenced_background_task() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let baseline = tui.app.transcript.len();
    // Without an active session the transport fails with the same clear
    // session error as other operator commands, and the busy state clears.
    tui.run_command(".credentials list").await.unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), tui.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    tui.handle_tui_event(event).await.unwrap();
    assert!(!tui.app.llm_busy);
    assert!(tui.current_prompt_abort.is_none());
    let rendered = tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .map(|entry| match entry {
            TranscriptItem::SystemText(text) | TranscriptItem::ErrorText(text) => text.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("active session"),
        "credentials without a session must report the missing session, got: {rendered}"
    );
    // The operator-identity note names whose credentials are managed even
    // though no identity is configured in tests.
    assert!(
        rendered.contains("No user_id set") || rendered.contains("Managing credentials for"),
        "expected an identity note, got: {rendered}"
    );
}

#[tokio::test]
async fn credentials_usage_error_renders_inline_without_a_task() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let baseline = tui.app.transcript.len();
    tui.run_command(".credentials bogus").await.unwrap();
    let usage = tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .filter_map(|entry| match entry {
            TranscriptItem::ErrorText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        usage.contains("unknown credentials command"),
        "expected a usage error, got: {usage}"
    );
    // No background task was started for the usage error.
    assert!(tui.current_prompt_abort.is_none());
}

#[tokio::test]
async fn credentials_update_renders_the_connect_link_as_a_markdown_row() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    // Drive the handler directly with a synthetic task so the URL rendering
    // is covered without a live connect flow.
    let abort = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(abort.clone());
    let baseline = tui.app.transcript.len();
    tui.handle_tui_event(TuiEvent::CredentialsUpdate {
        task: abort,
        text: "Connect jira\n[Open the connect page](https://tools.example.test/c/abc)\nhttps://tools.example.test/c/abc".into(),
    })
    .await
    .unwrap();
    assert!(tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .any(|item| matches!(
            item,
            TranscriptItem::MarkdownLink { text, url }
                if text == "Open the connect page"
                    && url == "https://tools.example.test/c/abc"
        )));
    assert!(tui
        .app
        .transcript
        .iter()
        .skip(baseline)
        .any(|item| matches!(
            item,
            TranscriptItem::SystemText(text) if text.contains("https://tools.example.test/c/abc")
        )));
}

#[tokio::test]
async fn stale_credentials_update_is_ignored() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let current = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(current.clone());
    let baseline = tui.app.transcript.len();
    let stale = harnx_runtime::utils::create_abort_signal();
    tui.handle_tui_event(TuiEvent::CredentialsUpdate {
        task: stale,
        text: "stale".into(),
    })
    .await
    .unwrap();
    assert_eq!(tui.app.transcript.len(), baseline);
}
