use super::*;
use crate::tool_render::render_tool_call;
use harnx_core::event::{ToolLocation, ToolStatus};

fn args(body: &ToolCallBody) -> RenderToolCallArgs<'_> {
    RenderToolCallArgs {
        tool_name: "pkg__helper_session_wait",
        body: Some(body),
        title: None,
        status: Some(ToolStatus::Completed),
        kind: Some(ToolKind::Read),
        locations: &[],
        is_running: false,
        spinner_index: 0,
        timer: Some(" (240s)"),
        width: 100,
        theme: None,
    }
}

fn first_line(entry: &RenderedEntry) -> &Line<'static> {
    let MarkdownBlockData::Paragraph { lines, .. } = &entry.blocks[0] else {
        panic!("expected paragraph");
    };
    &lines[0]
}

#[test]
fn wait_call_template_shares_one_row_with_status_kind_and_timer() {
    let body = ToolCallBody::Markdown("@ pkg/helper [abcdefgh] wait prompt12".into());
    for (status, running, icon) in [
        (ToolStatus::Completed, false, "✓"),
        (ToolStatus::Failed, false, "✗"),
        (
            ToolStatus::InProgress,
            true,
            crate::types::SPINNER_FRAMES[0],
        ),
    ] {
        let mut args = args(&body);
        args.status = Some(status);
        args.is_running = running;
        let entry = render_tool_call(args);
        assert_eq!(entry.total_height, 1);
        let text = first_line(&entry).to_string();
        assert!(text.starts_with(icon), "{text}");
        assert!(text.contains('⎘'), "{text}");
        assert!(
            text.contains("@ pkg/helper [abcdefgh] wait prompt12"),
            "{text}"
        );
        assert!(text.ends_with(" (240s)"), "{text}");
        if running {
            assert!(text.contains("[in_progress]"), "{text}");
        }
    }
}

#[test]
fn compact_call_retains_markdown_styling_and_links() {
    let body = ToolCallBody::Markdown(
        "@ pkg/helper [**file**] wait [details](https://example.com)".into(),
    );
    let entry = render_tool_call(args(&body));
    assert_eq!(entry.total_height, 1);
    assert_eq!(entry.links[0].url, "https://example.com");
    assert!(first_line(&entry).spans.iter().any(|span| {
        span.content.contains("file")
            && span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
    }));
}

#[test]
fn compact_call_defers_when_header_metadata_or_body_needs_more_space() {
    for body in [
        ToolCallBody::Markdown("first\nsecond".into()),
        ToolCallBody::Markdown("very long call ".repeat(40)),
        ToolCallBody::Markdown(format!(
            "@ pkg/helper [abcdefgh] wait {}",
            "long ".repeat(40)
        )),
        ToolCallBody::Markdown("# heading".into()),
        ToolCallBody::Markdown("> quote".into()),
        ToolCallBody::Markdown("- item".into()),
        ToolCallBody::Markdown("waiting for progress".into()),
        ToolCallBody::Yaml("seconds: 240".into()),
    ] {
        assert!(render(&args(&body), Style::default()).is_none());
    }
    let body = ToolCallBody::Markdown("@ pkg/helper [abcdefgh] wait prompt12".into());
    let mut args = args(&body);
    args.title = Some("Live title");
    assert!(render(&args, Style::default()).is_none());
    args.title = None;
    let locations = [ToolLocation {
        path: "file.rs".into(),
        line: Some(1),
    }];
    args.locations = &locations;
    assert!(render(&args, Style::default()).is_none());
    args.locations = &[];
    args.tool_name = "fs_read";
    assert!(render(&args, Style::default()).is_none());
    args.tool_name = "session_wait";
    args.width = 4;
    assert!(render(&args, Style::default()).is_none());
}

#[test]
fn compact_wait_result_keeps_full_output_in_detail_view() {
    let output = serde_json::json!({"status": "completed", "output": {"text": "child output\n".repeat(100)}});
    let items =
        crate::tool_transcript::tool_completed_to_transcript_items(&output, Some("completed"));
    let [crate::types::TranscriptItem::ToolResultMarkdown {
        text, full_detail, ..
    }] = items.as_slice()
    else {
        panic!("expected one result row");
    };
    assert_eq!(text, "completed");
    assert!(full_detail.as_ref().unwrap().contains("child output"));
}
