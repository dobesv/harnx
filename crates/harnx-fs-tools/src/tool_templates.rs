//! Call templates the client renders for each filesystem tool.
//!
//! `FsToolset::tools` must attach these, because `ToolSpec.meta` is the only
//! place any client reads a template from. Even `--mcp-stdio` goes through
//! `harnx_toolset_server`'s adapter, which rebuilds `list_tools` from
//! `ToolSpec`; nothing serves `FsServer`'s own `ServerHandler` outside tests.
//! The templates used to live only in that handler, so every fs tool call
//! rendered as a raw YAML dump of its arguments. Both sides read these consts
//! now so a parity test can catch either one drifting.

pub(crate) const READ_CALL: &str = "📖 {{ args.path }}{% if args.offset %} +{{ args.offset }}{% endif %}{% if args.limit is not none %} [:{{ args.limit }}]{% endif %}{% if args.tail is not none %} [tail:{{ args.tail }}]{% endif %}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.head_lines is not none %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines is not none %} [tail_lines:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes is not none %} [:{{ args.max_output_bytes }}b]{% endif %}";

pub(crate) const WRITE_CALL: &str = "✏️ {{ args.path }} ({{ args.content | length }}ch)";

pub(crate) const EDIT_CALL: &str = "🔧 {{ args.path }}{% if args.replace_all %} [all]{% endif %}\n▸ {{ args.old_text | truncate(60) }}\n↳ {{ args.new_text | truncate(60) }}";

pub(crate) const INSERT_CALL: &str = "➕ {{ args.path }}:{{ args.insert_line | default(value=\"end\") }}{% if args.column %}:{{ args.column }}{% endif %}\n↳ {{ args.insert_text | truncate(60) }}";

pub(crate) const RE_REPLACE_CALL: &str = "🔁 {{ args.path }}{% if args.replace_all %} [all]{% endif %}\n▸ /{{ args.pattern }}/\n↳ {{ args.replacement | truncate(60) }}";

pub(crate) const LS_CALL: &str = "📂 {{ args.path }}{% if args.recursive %} -r{% endif %}";

pub(crate) const GREP_CALL: &str = "🔍 /{{ args.pattern }}/{% if args.ignore_case %}i{% endif %}{% if args.path %} {{ args.path }}{% endif %}{% if args.include %} [{{ args.include }}]{% endif %}{% if args.context_lines %} ±{{ args.context_lines }}{% endif %}{% if args.max_results %} [max:{{ args.max_results }}]{% endif %}";

pub(crate) const FIND_CALL: &str = "🔎 {{ args.pattern }}{% if args.path %} {{ args.path }}{% endif %}{% if args.max_results %} [max:{{ args.max_results }}]{% endif %}";

pub(crate) const ROLLBACK_FILE_CALL: &str = "⏪ rollback {{ args.commit_id | truncate(8, end='') }}{% if args.repo_path %} @ {{ args.repo_path }}{% endif %}";
