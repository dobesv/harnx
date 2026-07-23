use super::*;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ListTasksParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    #[serde(default = "default_open_status")]
    pub filter: String,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct GetTaskParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AddTaskParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub title: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct UpdateTaskParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub replace_body: Option<String>,
    #[serde(default)]
    pub append_body: Option<String>,
    #[serde(default)]
    pub replace_in_body: Option<ReplaceInContent>,
    #[serde(default)]
    pub dependencies: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ReplaceInContent {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DeleteTaskParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ListPlansParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AddPlanParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub github_owner_repo: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub tasks: Option<Vec<TaskSpec>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct GetPlanParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct UpdatePlanParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub name: String,
    #[serde(default)]
    pub replace_content: Option<String>,
    #[serde(default)]
    pub append_content: Option<String>,
    #[serde(default)]
    pub replace_in_content: Option<ReplaceInContent>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub github_owner_repo: Option<String>,
    #[serde(default)]
    pub tasks: Option<Vec<TaskSpec>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DeletePlanParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ListNotesParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AddNoteParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    #[serde(default)]
    pub id: Option<String>,
    pub body: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct GetNoteParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DeleteNoteParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct UpdateNoteParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub plan: String,
    pub note_id: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub replace_body: Option<String>,
    #[serde(default)]
    pub append_body: Option<String>,
    #[serde(default)]
    pub replace_in_body: Option<ReplaceInContent>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub executor: Option<String>,
}

impl_json_schema!(
    ReplaceInContent,
    "ReplaceInContent",
    |generator: &mut SchemaGenerator| vec![
        (
            "old_text",
            "Text to find and replace",
            generator.subschema_for::<String>()
        ),
        (
            "new_text",
            "Replacement text",
            generator.subschema_for::<String>()
        ),
        (
            "replace_all",
            "If true, replace all occurrences; default replaces first only",
            generator.subschema_for::<Option<bool>>()
        ),
    ],
    &["old_text", "new_text"]
);

impl_json_schema!(
    ListTasksParams,
    "ListTasksParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        (
            "filter",
            "Filter by status: 'open' (default), 'closed', or 'all'",
            generator.subschema_for::<String>()
        ),
        (
            "tag",
            "Optional tag filter",
            generator.subschema_for::<Option<String>>()
        ),
    ],
    &["plan"]
);

impl_json_schema!(
    GetTaskParams,
    "GetTaskParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("id", "Task ID", generator.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    AddTaskParams,
    "AddTaskParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("title", "Task title", generator.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional task summary",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional task author",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional task assignee",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional task executor",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "Task tags",
            generator.subschema_for::<Vec<String>>()
        ),
        (
            "status",
            "Task status; defaults to 'open'",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Task body markdown",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "Task dependency IDs",
            generator.subschema_for::<Vec<String>>()
        ),
    ],
    &["plan", "title"]
);

impl_json_schema!(
    UpdateTaskParams,
    "UpdateTaskParams",
    |generator: &mut SchemaGenerator| {
        vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("id", "Task ID", generator.subschema_for::<String>()),
        (
            "title",
            "Optional task title",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional task summary",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional task author",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional task assignee",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional task executor",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "Replace tags with provided list",
            generator.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "status",
            "Optional new status",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "replace_body",
            "Replace entire task body with this content. Keep under 1000 words.",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "append_body",
            "Append text to task body. Auto-inserts newline separator if needed. Keep under 1000 words.",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "replace_in_body",
            "Surgically replace text in task body.",
            generator.subschema_for::<Option<ReplaceInContent>>()
        ),
        (
            "dependencies",
            "Replace dependencies with provided list",
            generator.subschema_for::<Option<Vec<String>>>()
        ),
    ]
    },
    &["plan", "id"]
);

impl_json_schema!(
    DeleteTaskParams,
    "DeleteTaskParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("id", "Task ID", generator.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    ListPlansParams,
    "ListPlansParams",
    |_generator: &mut SchemaGenerator| vec![],
    &[]
);

impl_json_schema!(
    AddPlanParams,
    "AddPlanParams",
    |generator: &mut SchemaGenerator| vec![
        ("name", "Plan name", generator.subschema_for::<String>()),
        (
            "title",
            "Optional plan title",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional plan summary",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional plan author",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional plan assignee",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional plan executor",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "git_branch",
            "Optional git branch name",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "github_owner_repo",
            "Optional GitHub owner/repo string",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Plan body markdown",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "tasks",
            "Optional list of tasks to create with plan",
            generator.subschema_for::<Option<Vec<TaskSpec>>>()
        ),
    ],
    &["name"]
);

impl_json_schema!(
    GetPlanParams,
    "GetPlanParams",
    |generator: &mut SchemaGenerator| {
        vec![("name", "Plan name", generator.subschema_for::<String>())]
    },
    &["name"]
);

impl_json_schema!(
    UpdatePlanParams,
    "UpdatePlanParams",
    |generator: &mut SchemaGenerator| {
        vec![
            ("name", "Plan name", generator.subschema_for::<String>()),
            (
                "replace_content",
                "Replace entire plan body with this content. Keep under 1000 words.",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "append_content",
                "Append text to plan body. Auto-inserts newline separator if needed. Keep under 1000 words.",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "replace_in_content",
                "Surgically replace text in plan body.",
                generator.subschema_for::<Option<ReplaceInContent>>()
            ),
            (
                "title",
                "Optional plan title",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "summary",
                "Optional plan summary",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "author",
                "Optional plan author",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "assignee",
                "Optional plan assignee",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "executor",
                "Optional plan executor",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "git_branch",
                "Optional git branch name",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "github_owner_repo",
                "Optional GitHub owner/repo string",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "tasks",
                "Optional list of tasks to create during update; partial failures do not roll back successes",
                generator.subschema_for::<Option<Vec<TaskSpec>>>()
            ),
        ]
    },
    &["name"]
);

impl_json_schema!(
    DeletePlanParams,
    "DeletePlanParams",
    |generator: &mut SchemaGenerator| {
        vec![("name", "Plan name", generator.subschema_for::<String>())]
    },
    &["name"]
);

impl_json_schema!(
    ListNotesParams,
    "ListNotesParams",
    |generator: &mut SchemaGenerator| {
        vec![("plan", "Plan name", generator.subschema_for::<String>())]
    },
    &["plan"]
);

impl_json_schema!(
    AddNoteParams,
    "AddNoteParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        (
            "id",
            "Optional note ID",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Note body markdown",
            generator.subschema_for::<String>()
        ),
        (
            "summary",
            "Optional note summary",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional note author",
            generator.subschema_for::<Option<String>>()
        ),
    ],
    &["plan", "body"]
);

impl_json_schema!(
    GetNoteParams,
    "GetNoteParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("note_id", "Note ID", generator.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    DeleteNoteParams,
    "DeleteNoteParams",
    |generator: &mut SchemaGenerator| vec![
        ("plan", "Plan name", generator.subschema_for::<String>()),
        ("note_id", "Note ID", generator.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    UpdateNoteParams,
    "UpdateNoteParams",
    |generator: &mut SchemaGenerator| {
        vec![
            ("plan", "Plan name", generator.subschema_for::<String>()),
            ("note_id", "Note ID", generator.subschema_for::<String>()),
            (
                "summary",
                "Optional note summary",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "author",
                "Optional note author",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "replace_body",
                "Replace entire note body with this content. Keep under 1000 words.",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "append_body",
                "Append text to note body. Auto-inserts newline separator if needed. Keep under 1000 words.",
                generator.subschema_for::<Option<String>>()
            ),
            (
                "replace_in_body",
                "Surgically replace text in note body.",
                generator.subschema_for::<Option<ReplaceInContent>>()
            ),
        ]
    },
    &["plan", "note_id"]
);

impl_json_schema!(
    TaskSpec,
    "TaskSpec",
    |generator: &mut SchemaGenerator| vec![
        ("title", "Task title", generator.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "Task tags",
            generator.subschema_for::<Vec<String>>()
        ),
        (
            "dependencies",
            "Task dependencies",
            generator.subschema_for::<Vec<String>>()
        ),
        (
            "status",
            "Task status",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Task body",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Task summary",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Task author",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Task assignee",
            generator.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Task executor",
            generator.subschema_for::<Option<String>>()
        ),
    ],
    &["title"]
);
