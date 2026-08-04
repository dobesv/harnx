// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

impl ServerHandler for FsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-fs-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Filesystem MCP server with read (text and local images), write, edit, insert, re_replace, listing, grep, and glob tools.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let read_only = ToolAnnotations::new().read_only(true);
        let tools = vec![
                Tool::new("read", "Read a text file with line numbers, pagination, grep filtering, and smart truncation. Prefer this tool over shell commands like sed, cat, head, tail. Use offset+limit to read specific line ranges instead of sed -n. Also reads local image files (PNG, JPEG, GIF, WebP, up to 5MB) and returns them as viewable images for vision-capable models — use this to view/inspect an image file by its path.", Map::new())
                    .with_input_schema::<ReadFileParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("📖 {{ args.path }}{% if args.offset %} +{{ args.offset }}{% endif %}{% if args.limit is not none %} [:{{ args.limit }}]{% endif %}{% if args.tail is not none %} [tail:{{ args.tail }}]{% endif %}{% if args.grep %} /{{ args.grep }}/{% endif %}{% if args.head_lines is not none %} [head:{{ args.head_lines }}]{% endif %}{% if args.tail_lines is not none %} [tail_lines:{{ args.tail_lines }}]{% endif %}{% if args.max_output_bytes is not none %} [:{{ args.max_output_bytes }}b]{% endif %}")),
                Tool::new("write", "Write or create a file, replacing its contents.", Map::new())
                    .with_input_schema::<WriteFileParams>()
                    .with_meta(make_tool_meta("✏️ {{ args.path }} ({{ args.content | length }}ch)")),
                Tool::new("edit", "Replace exact text within an existing file.", Map::new())
                    .with_input_schema::<EditFileParams>()
                    .with_meta(make_tool_meta("🔧 {{ args.path }}{% if args.replace_all %} [all]{% endif %}\n▸ {{ args.old_text | truncate(60) }}\n↳ {{ args.new_text | truncate(60) }}")),
                Tool::new("insert",
                    "Insert text into a file at a specific line position.      insert_line: 0 prepends before line 1; insert_line: N inserts after line N; omit insert_line (or set N = total lines) to append to the end of the file. Optional column (1-indexed byte offset within      the line, default 1 = start of line) for mid-line insertion.      For exact-text replacement use edit; for regex replacement use re_replace.",
                    Map::new())
                    .with_input_schema::<InsertParams>()
                    .with_meta(make_tool_meta(
                        "➕ {{ args.path }}:{{ args.insert_line | default(value=\"end\") }}{% if args.column %}:{{ args.column }}{% endif %}\n↳ {{ args.insert_text | truncate(60) }}"
                    )),
                Tool::new("re_replace",
                    "Replace text in a file using a regular expression.      Uses fancy_regex syntax (supports lookahead/lookbehind).      Use $0 for the full match, $1/$2 etc. for capture groups in replacement.      Errors if pattern matches nothing. If pattern matches more than once,      set replace_all=true; otherwise only the first match is replaced.      For exact-text replacement use edit instead.",
                    Map::new())
                    .with_input_schema::<ReReplaceParams>()
                    .with_meta(make_tool_meta(
                        "🔁 {{ args.path }}{% if args.replace_all %} [all]{% endif %}\n▸ /{{ args.pattern }}/\n↳ {{ args.replacement | truncate(60) }}"
                    )),
                Tool::new("ls", "List directory contents, optionally recursively. Prefer this tool over running bash ls.", Map::new())
                    .with_input_schema::<ListDirectoryParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("📂 {{ args.path }}{% if args.recursive %} -r{% endif %}")),
                Tool::new("grep", "Search file contents with regex and optional context lines. Prefer this tool over running bash grep.", Map::new())
                    .with_input_schema::<SearchFilesParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("🔍 /{{ args.pattern }}/{% if args.ignore_case %}i{% endif %}{% if args.path %} {{ args.path }}{% endif %}{% if args.include %} [{{ args.include }}]{% endif %}{% if args.context_lines %} ±{{ args.context_lines }}{% endif %}{% if args.max_results %} [max:{{ args.max_results }}]{% endif %}")),
                Tool::new("find", "Find files by glob pattern. Prefer this tool over running bash find.", Map::new())
                    .with_input_schema::<FindFilesParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("🔎 {{ args.pattern }}{% if args.path %} {{ args.path }}{% endif %}{% if args.max_results %} [max:{{ args.max_results }}]{% endif %}")),
                Tool::new("rollback_file", "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.", Map::new())
                    .with_input_schema::<RollbackParams>()
                    .with_meta(make_tool_meta("⏪ rollback {{ args.commit_id | truncate(8, end='') }}{% if args.repo_path %} @ {{ args.repo_path }}{% endif %}")),
            ];

        Ok(ListToolsResult {
            meta: None,
            tools,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "read" => {
                let params = parse_arguments::<ReadFileParams>(request.arguments)?;
                self.read_file_impl(params).await
            }
            "write" => {
                let params = parse_arguments::<WriteFileParams>(request.arguments)?;
                self.write_file_impl(params).await
            }
            "edit" => {
                let params = parse_arguments::<EditFileParams>(request.arguments)?;
                self.edit_file_impl(params).await
            }
            "insert" => {
                let params = parse_arguments::<InsertParams>(request.arguments)?;
                self.insert_impl(params).await
            }
            "re_replace" => {
                let params = parse_arguments::<ReReplaceParams>(request.arguments)?;
                self.re_replace_impl(params).await
            }
            "ls" => {
                let params = parse_arguments::<ListDirectoryParams>(request.arguments)?;
                self.list_directory_impl(params).await
            }
            "grep" => {
                let params = parse_arguments::<SearchFilesParams>(request.arguments)?;
                self.search_files_impl(params).await
            }
            "find" => {
                let params = parse_arguments::<FindFilesParams>(request.arguments)?;
                self.find_files_impl(params).await
            }
            "rollback_file" => {
                let params = parse_arguments::<RollbackParams>(request.arguments)?;
                self.rollback_file_impl(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}
