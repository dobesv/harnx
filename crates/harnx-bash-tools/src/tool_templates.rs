//! Call templates the client renders for each bash tool.
//!
//! `BashToolset::tools` must attach these, because `ToolSpec.meta` is the only
//! place any client reads a template from. Even `--mcp-stdio` goes through
//! `harnx_toolset_server`'s adapter, which rebuilds `list_tools` from
//! `ToolSpec`; nothing serves `BashServer`'s own `ServerHandler` outside tests.
//! The templates used to live only in that handler, so every bash tool call
//! rendered as a raw YAML dump of its arguments. Both sides read these consts
//! now so a parity test can catch either one drifting.

pub(crate) const EXEC_CALL: &str = "```{{ args.command | shebang_lang }}\n$ {{ args.command | strip_shebang }}\n```{% if args.working_dir or args.timeout_secs or args.head_lines or args.tail_lines or args.max_output_bytes %}\n{% if args.working_dir %}({{ args.working_dir }}) {% endif %}{% if args.timeout_secs %}[{{ args.timeout_secs }}s] {% endif %}{% if args.head_lines is not none %}[head:{{ args.head_lines }}] {% endif %}{% if args.tail_lines is not none %}[tail_lines:{{ args.tail_lines }}] {% endif %}{% if args.max_output_bytes is not none %}[:{{ args.max_output_bytes }}b] {% endif %}{% endif %}";

pub(crate) const READ_EXEC_LOG_CALL: &str = "📋 log {{ args.execution_id }}/{{ args.stream }}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.offset %} +{{ args.offset }}{% endif %}{% if args.limit %} [:{{ args.limit }}]{% endif %}{% if args.tail %} [tail:{{ args.tail }}]{% endif %}{% if args.head_lines is not none %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines is not none %} [tail_lines:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes is not none %} [:{{ args.max_output_bytes }}b]{% endif %}";

pub(crate) const SPAWN_CALL: &str = "```{{ args.command | shebang_lang }}\n$ {{ args.command | strip_shebang }} &\n```{% if args.working_dir %}\n({{ args.working_dir }}) {% endif %}";

pub(crate) const WAIT_CALL: &str = "⏳ wait {{ args.execution_id }}{% if args.timeout_secs %} [{{ args.timeout_secs }}s]{% endif %}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.head_lines %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines %} [tail:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes %} [:{{ args.max_output_bytes }}b]{% endif %}";

pub(crate) const TERMINATE_CALL: &str =
    "🛑 kill {{ args.execution_id }}{% if args.signal %} ({{ args.signal }}){% endif %}";

pub(crate) const ROLLBACK_FILE_CALL: &str = "⏪ rollback {{ args.commit_id | truncate(8, end='') }}{% if args.repo_path %} @ {{ args.repo_path }}{% endif %}";
