//! Tab-completion behaviour for the `.`-prefixed commands.

use super::*;

#[tokio::test]
async fn command_completions_separate_names_from_usage_and_offer_subcommands() {
    let config = test_config();
    let tui = Tui::init(&config).await.unwrap();

    for command in [".rewind", ".edit", ".delete", ".info"] {
        let completions = tui.compute_completions(command, command.len()).await;
        assert!(
            completions
                .iter()
                .all(|(value, _)| !["<n>", "[server]", "[name]", "<n>-<m>"]
                    .iter()
                    .any(|usage| value.contains(usage))),
            "{command} command completion contains a usage hint: {completions:?}"
        );
    }

    let edit_names = tui.compute_completions(".edit", ".edit".len()).await;
    assert_eq!(
        edit_names
            .iter()
            .filter(|(value, _)| value == ".edit message ")
            .count(),
        1,
        "duplicate .edit message completion: {edit_names:?}"
    );
    let delete_names = tui.compute_completions(".delete", ".delete".len()).await;
    assert_eq!(
        delete_names
            .iter()
            .filter(|(value, _)| value == ".delete message ")
            .count(),
        1,
        "duplicate .delete message completion: {delete_names:?}"
    );

    let mut edit_subcommands: Vec<String> = tui
        .compute_completions(".edit ", ".edit ".len())
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    edit_subcommands.sort();
    assert_eq!(
        edit_subcommands,
        ["agent", "config", "message", "rag-docs", "session"]
    );

    let mut delete_subcommands: Vec<String> = tui
        .compute_completions(".delete ", ".delete ".len())
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    delete_subcommands.sort();
    assert_eq!(
        delete_subcommands,
        ["agent", "agent-data", "macro", "message", "rag", "session"]
    );
}
