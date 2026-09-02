//! Call templates the client renders for each bash tool.
//!
//! `BashToolset::tools` must attach these, because `ToolSpec.meta` is the only
//! place any client reads a template from. Even `--mcp-stdio` goes through
//! `harnx_toolset_server`'s adapter, which rebuilds `list_tools` from
//! `ToolSpec`; nothing serves `BashServer`'s own `ServerHandler` outside tests.
//! The templates used to live only in that handler, so every bash tool call
//! rendered as a raw YAML dump of its arguments. Both sides read these consts
//! now so a parity test can catch either one drifting.
//!
//! ## Template rules
//!
//! - **Include the tool name** (`render_tool_call_template` only exposes
//!   `{ "args": args }` in the context; the name must be baked into the
//!   template as literal text).
//! - **Both TUI and CLI suppress the fallback header** when a rendered
//!   call_template body exists (TUI: `render.rs:111-116`; CLI:
//!   `cli_event_sink.rs:292-295`). A name-only template makes the tool name
//!   invisible.
//! - **Use the shared generator** for command templates: both the NATS
//!   registration site (`toolset.rs`) and MCP handler (`server/handler.rs`)
//!   call `command_template_call(name)`. The parity test
//!   `bash_toolset_call_templates_match_mcp_handler` enforces this; any
//!   future registration path must reuse the same helper.

pub(crate) const EXEC_CALL: &str = "```{{ args.command | shebang_lang }}\n$ {{ args.command | strip_shebang }}\n```{% if args.working_dir or args.timeout_secs or args.head_lines or args.tail_lines or args.max_output_bytes %}\n{% if args.working_dir %}({{ args.working_dir }}) {% endif %}{% if args.timeout_secs %}[{{ args.timeout_secs }}s] {% endif %}{% if args.head_lines is not none %}[head:{{ args.head_lines }}] {% endif %}{% if args.tail_lines is not none %}[tail_lines:{{ args.tail_lines }}] {% endif %}{% if args.max_output_bytes is not none %}[:{{ args.max_output_bytes }}b] {% endif %}{% endif %}";

pub(crate) const READ_EXEC_LOG_CALL: &str = "📋 log {{ args.execution_id }}/{{ args.stream }}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.offset %} +{{ args.offset }}{% endif %}{% if args.limit %} [:{{ args.limit }}]{% endif %}{% if args.tail %} [tail:{{ args.tail }}]{% endif %}{% if args.head_lines is not none %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines is not none %} [tail_lines:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes is not none %} [:{{ args.max_output_bytes }}b]{% endif %}";

pub(crate) const SPAWN_CALL: &str = "```{{ args.command | shebang_lang }}\n$ {{ args.command | strip_shebang }} &\n```{% if args.working_dir %}\n({{ args.working_dir }}) {% endif %}";

pub(crate) const WAIT_CALL: &str = "⏳ wait {{ args.execution_id }}{% if args.timeout_secs %} [{{ args.timeout_secs }}s]{% endif %}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.head_lines %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines %} [tail:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes %} [:{{ args.max_output_bytes }}b]{% endif %}";

pub(crate) const TERMINATE_CALL: &str =
    "🛑 kill {{ args.execution_id }}{% if args.signal %} ({{ args.signal }}){% endif %}";

pub(crate) const ROLLBACK_FILE_CALL: &str = "⏪ rollback {{ args.commit_id | truncate(8, end='') }}{% if args.repo_path %} @ {{ args.repo_path }}{% endif %}";

pub(crate) const COMMAND_TEMPLATE_CALL_BODY: &str = "{% if args is mapping %}{% for key, value in args | dictsort %} {{ key }}={{ value | string | replace(\"\\r\", \" \") | replace(\"\\n\", \" \") | truncate(80) }}{% endfor %}{% endif %}";

/// Build a call template for a YAML-defined command tool.
///
/// Tool names match `^[a-zA-Z][a-zA-Z0-9_]*$`, so embedding them is safe. The
/// client renderer exposes only `args`, which requires baking the name into the template.
pub(crate) fn command_template_call(name: &str) -> String {
    let mut template = String::with_capacity(name.len() + COMMAND_TEMPLATE_CALL_BODY.len());
    template.push_str(name);
    template.push_str(COMMAND_TEMPLATE_CALL_BODY);
    template
}

#[cfg(test)]
mod tests {
    use super::command_template_call;
    use harnx_core::tool::render_tool_call_template;
    use serde_json::{json, Value};

    fn render(args: &Value) -> String {
        render_tool_call_template(&command_template_call("gh_issue_view"), args, "")
            .expect("command call template should render")
    }

    #[test]
    fn command_template_call_renders_compact_arguments() {
        let rendered = render(&json!({
            "number": 1630,
            "repo": "dobesv/harnx"
        }));
        assert_eq!(rendered, "gh_issue_view number=1630 repo=dobesv/harnx");

        assert_eq!(render(&json!({})), "gh_issue_view");
        assert_eq!(render(&Value::Null), "gh_issue_view");

        // Non-string scalars coerce through `| string`; keys stay dictsorted.
        // MiniJinja renders booleans Jinja-style (`True`/`False`, capitalized).
        assert_eq!(
            render(&json!({"draft": true, "ratio": 1.5})),
            "gh_issue_view draft=True ratio=1.5"
        );

        let rendered = render(&json!({"body": "first\r\nsecond"}));
        assert_eq!(rendered, "gh_issue_view body=first  second");
        assert!(!rendered.contains(['\r', '\n']));

        let rendered = render(&json!({"body": "x".repeat(100)}));
        assert!(rendered.ends_with("..."), "missing ellipsis: {rendered:?}");
        assert_eq!(rendered.chars().count(), "gh_issue_view body=".len() + 80);
    }
}
