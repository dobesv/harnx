// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

#[derive(Debug, Deserialize)]
pub struct ReadFileParams {
    pub path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub grep: Option<String>,
    #[serde(default)]
    pub head_lines: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct WriteFileParams {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct EditFileParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct InsertParams {
    pub path: String,
    #[serde(default)]
    pub insert_line: Option<usize>,
    pub insert_text: String,
    #[serde(default)]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ReReplaceParams {
    pub path: String,
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListDirectoryParams {
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SearchFilesParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include: Option<String>,
    #[serde(default)]
    pub context_lines: Option<usize>,
    #[serde(default)]
    pub ignore_case: Option<bool>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FindFilesParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct RollbackParams {
    pub commit_id: String,
    pub repo_path: String,
}

impl JsonSchema for ReadFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let offset = generator.subschema_for::<Option<usize>>();
        let limit = generator.subschema_for::<Option<usize>>();
        let tail = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the file to read. Prefer this tool over shell commands like sed, cat, head, tail for reading files. If the path is an image file (PNG/JPEG/GIF/WebP), it is returned as viewable image content.", path),
                ("offset", "Start reading at this line number (1-indexed). Use to read a specific line range instead of `sed -n 'N,Mp'`.", offset),
                ("limit", "Maximum number of lines to return from offset. Combine with offset to read a range.", limit),
                ("tail", "Return only the last N lines of the file.", tail),
                ("grep", "Filter lines by regex pattern before returning.", grep),
                ("head_lines", "Return only the first N lines. Prefer this over piping through head.", head_lines),
                ("tail_lines", "Return only the last N lines. Prefer this over piping through tail.", tail_lines),
                ("max_output_bytes", "Truncate output to at most this many bytes.", max_output_bytes),
            ],
            &["path"],
        )
    }
}

impl JsonSchema for WriteFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WriteFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let content = generator.subschema_for::<String>();
        object_schema_with_desc(
            vec![
                (
                    "path",
                    "Absolute path to the file to write or create.",
                    path,
                ),
                (
                    "content",
                    "Full file content to write (replaces existing content).",
                    content,
                ),
            ],
            &["path", "content"],
        )
    }
}

impl JsonSchema for EditFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("EditFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let old_text = generator.subschema_for::<String>();
        let new_text = generator.subschema_for::<String>();
        let replace_all = generator.subschema_for::<Option<bool>>();
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the file to edit.", path),
                (
                    "old_text",
                    "Exact text to find and replace. Must match exactly including whitespace.",
                    old_text,
                ),
                ("new_text", "Replacement text.", new_text),
                (
                    "replace_all",
                    "If true, replace all occurrences. Default: replace only the first.",
                    replace_all,
                ),
            ],
            &["path", "old_text", "new_text"],
        )
    }
}

impl JsonSchema for InsertParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("InsertParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let insert_line = generator.subschema_for::<Option<usize>>();
        let insert_text = generator.subschema_for::<String>();
        let column = generator.subschema_for::<Option<usize>>();
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the file to insert into.", path),
                ("insert_line", "Insert after this line number. 0 = prepend before line 1; N = insert after line N; omit (or use N = total lines) to append to the end of the file.", insert_line),
                ("insert_text", "Text to insert.", insert_text),
                ("column", "1-indexed byte offset within the line for mid-line insertion. Default: 1 (start of line).", column),
            ],
            &["path", "insert_text"],
        )
    }
}

impl JsonSchema for ReReplaceParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReReplaceParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let pattern = generator.subschema_for::<String>();
        let replacement = generator.subschema_for::<String>();
        let replace_all = generator.subschema_for::<Option<bool>>();
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the file.", path),
                ("pattern", "fancy_regex pattern (supports lookahead/lookbehind). Use $0 for full match, $1/$2 for groups.", pattern),
                ("replacement", "Replacement string. Use $0/$1/$2 for capture groups.", replacement),
                ("replace_all", "If true, replace all matches. Default: replace only the first (errors if more than one match).", replace_all),
            ],
            &["path", "pattern", "replacement"],
        )
    }
}

impl JsonSchema for ListDirectoryParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ListDirectoryParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let recursive = generator.subschema_for::<Option<bool>>();
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the directory to list. Prefer this tool over running bash ls.", path),
                ("recursive", "If true, list recursively. Default: false.", recursive),
            ],
            &["path"],
        )
    }
}

impl JsonSchema for SearchFilesParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SearchFilesParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let pattern = generator.subschema_for::<String>();
        let path = generator.subschema_for::<Option<String>>();
        let include = generator.subschema_for::<Option<String>>();
        let context_lines = generator.subschema_for::<Option<usize>>();
        let ignore_case = generator.subschema_for::<Option<bool>>();
        let max_results = generator.subschema_for::<Option<usize>>();
        object_schema_with_desc(
            vec![
                (
                    "pattern",
                    "Regex pattern to search for. Prefer this tool over running bash grep.",
                    pattern,
                ),
                (
                    "path",
                    "Directory to search in. Defaults to project root.",
                    path,
                ),
                (
                    "include",
                    "Glob pattern to filter files (e.g. '*.rs').",
                    include,
                ),
                (
                    "context_lines",
                    "Number of lines of context around each match.",
                    context_lines,
                ),
                ("ignore_case", "Case-insensitive search.", ignore_case),
                (
                    "max_results",
                    "Maximum number of matching lines to return.",
                    max_results,
                ),
            ],
            &["pattern"],
        )
    }
}

impl JsonSchema for FindFilesParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FindFilesParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let pattern = generator.subschema_for::<String>();
        let path = generator.subschema_for::<Option<String>>();
        let max_results = generator.subschema_for::<Option<usize>>();
        object_schema_with_desc(
            vec![
                ("pattern", "Glob pattern to match file paths (e.g. '**/*.rs'). Prefer this tool over running bash find.", pattern),
                ("path", "Directory to search in. Defaults to project root.", path),
                ("max_results", "Maximum number of results to return.", max_results),
            ],
            &["pattern"],
        )
    }
}

impl JsonSchema for RollbackParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RollbackParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let commit_id = generator.subschema_for::<String>();
        let repo_path = generator.subschema_for::<String>();
        object_schema_with_desc(
            vec![
                (
                    "commit_id",
                    "The harnx snapshot commit SHA shown in a prior tool response diff header.",
                    commit_id,
                ),
                (
                    "repo_path",
                    "Absolute path to the git repository root to roll back.",
                    repo_path,
                ),
            ],
            &["commit_id", "repo_path"],
        )
    }
}
