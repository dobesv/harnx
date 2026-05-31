// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskFrontMatter {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    pub(crate) plan: String,
    #[serde(default = "default_open_status")]
    pub(crate) status: String,
    pub(crate) created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskRecord {
    #[serde(flatten)]
    pub(crate) front: TaskFrontMatter,
    #[serde(default)]
    pub(crate) body: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct TaskWithBody<'a> {
    #[serde(flatten)]
    pub(crate) front: &'a TaskFrontMatter,
    pub(crate) body: &'a str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PlanFrontMatter {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) executor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) github_owner_repo: Option<String>,
    pub(crate) created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PlanRecord {
    #[serde(flatten)]
    pub(crate) front: PlanFrontMatter,
    #[serde(default)]
    pub(crate) body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NoteFrontMatter {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<String>,
    pub(crate) created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NoteRecord {
    #[serde(flatten)]
    pub(crate) front: NoteFrontMatter,
    #[serde(default)]
    pub(crate) body: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ListTasksParams {
    pub(crate) plan: String,
    #[serde(default = "default_open_status")]
    pub(crate) filter: String,
    #[serde(default)]
    pub(crate) tag: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct GetTaskParams {
    pub(crate) plan: String,
    pub(crate) id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct AddTaskParams {
    pub(crate) title: String,
    pub(crate) plan: String,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    #[serde(default)]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UpdateTaskParams {
    pub(crate) plan: String,
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    #[serde(default)]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) replace_body: Option<String>,
    #[serde(default)]
    pub(crate) append_body: Option<String>,
    #[serde(default)]
    pub(crate) replace_in_body: Option<ReplaceInContent>,
    #[serde(default)]
    pub(crate) dependencies: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ReplaceInContent {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    #[serde(default)]
    pub(crate) replace_all: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct DeleteTaskParams {
    pub(crate) plan: String,
    pub(crate) id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ListPlansParams {}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct AddPlanParams {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    #[serde(default)]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) git_branch: Option<String>,
    #[serde(default)]
    pub(crate) github_owner_repo: Option<String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct GetPlanParams {
    pub(crate) name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UpdatePlanParams {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) replace_content: Option<String>,
    #[serde(default)]
    pub(crate) append_content: Option<String>,
    #[serde(default)]
    pub(crate) replace_in_content: Option<ReplaceInContent>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    #[serde(default)]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) git_branch: Option<String>,
    #[serde(default)]
    pub(crate) github_owner_repo: Option<String>,
    #[serde(default)]
    pub(crate) tasks: Option<Vec<TaskSpec>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct DeletePlanParams {
    pub(crate) name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ListNotesParams {
    pub(crate) plan: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct AddNoteParams {
    pub(crate) plan: String,
    #[serde(default)]
    pub(crate) id: Option<String>,
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct GetNoteParams {
    pub(crate) plan: String,
    pub(crate) note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct DeleteNoteParams {
    pub(crate) plan: String,
    pub(crate) note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UpdateNoteParams {
    pub(crate) plan: String,
    pub(crate) note_id: String,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) replace_body: Option<String>,
    #[serde(default)]
    pub(crate) append_body: Option<String>,
    #[serde(default)]
    pub(crate) replace_in_body: Option<ReplaceInContent>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct TaskSpec {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    #[serde(default)]
    pub(crate) executor: Option<String>,
}

fn object_schema_with_desc(properties: Vec<(&str, &str, Schema)>, required: &[&str]) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, desc, property_schema) in properties {
        let mut prop = property_schema.as_value().clone();
        if let Some(obj) = prop.as_object_mut() {
            obj.insert("description".to_string(), Value::String(desc.to_string()));
        }
        property_map.insert(name.to_string(), prop);
    }
    schema.insert("properties".to_string(), Value::Object(property_map));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));

    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .map(|name| Value::String((*name).to_string()))
                    .collect(),
            ),
        );
    }

    schema.into()
}

macro_rules! impl_json_schema {
    ($type:ty, $title:expr, $properties_fn:expr, $required:expr) => {
        impl JsonSchema for $type {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($title)
            }

            fn schema_id() -> Cow<'static, str> {
                Cow::Borrowed(concat!(module_path!(), "::", $title))
            }

            fn json_schema(gen: &mut SchemaGenerator) -> Schema {
                object_schema_with_desc($properties_fn(gen), $required)
            }
        }
    };
}

impl_json_schema!(
    ReplaceInContent,
    "ReplaceInContent",
    |gen: &mut SchemaGenerator| vec![
        (
            "old_text",
            "Text to find and replace",
            gen.subschema_for::<String>()
        ),
        (
            "new_text",
            "Replacement text",
            gen.subschema_for::<String>()
        ),
        (
            "replace_all",
            "If true, replace all occurrences; default replaces first only",
            gen.subschema_for::<Option<bool>>()
        ),
    ],
    &["old_text", "new_text"]
);

impl_json_schema!(
    ListTasksParams,
    "ListTasksParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "filter",
            "Filter by status: 'open' (default), 'closed', or 'all'",
            gen.subschema_for::<String>()
        ),
        (
            "tag",
            "Optional tag filter",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["plan"]
);

impl_json_schema!(
    GetTaskParams,
    "GetTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    AddTaskParams,
    "AddTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Short title", gen.subschema_for::<String>()),
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        ("tags", "Optional tags", gen.subschema_for::<Vec<String>>()),
        (
            "status",
            "Initial status (default: 'open')",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Optional markdown body",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "List of dependency keys or IDs",
            gen.subschema_for::<Vec<String>>()
        ),
    ],
    &["title", "plan"]
);

impl_json_schema!(
    UpdateTaskParams,
    "UpdateTaskParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "Replace tags with provided list",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "status",
            "Optional new status",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_body",
            "Replace entire task body with this content. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "append_body",
            "Append text to task body. Auto-inserts newline separator if needed. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_in_body",
            "Surgically replace text in task body.",
            gen.subschema_for::<Option<ReplaceInContent>>()
        ),
        (
            "dependencies",
            "Replace dependencies with provided list",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
    ]
    },
    &["plan", "id"]
);

impl_json_schema!(
    DeleteTaskParams,
    "DeleteTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    ListPlansParams,
    "ListPlansParams",
    |_gen: &mut SchemaGenerator| vec![],
    &[]
);

impl_json_schema!(
    AddPlanParams,
    "AddPlanParams",
    |gen: &mut SchemaGenerator| vec![
        ("name", "Plan name", gen.subschema_for::<String>()),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "git_branch",
            "Optional git branch",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "github_owner_repo",
            "Optional owner/repo",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Optional markdown body",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["name"]
);

impl_json_schema!(
    GetPlanParams,
    "GetPlanParams",
    |gen: &mut SchemaGenerator| vec![("name", "Plan name", gen.subschema_for::<String>()),],
    &["name"]
);

impl_json_schema!(
    UpdatePlanParams,
    "UpdatePlanParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("name", "Plan name", gen.subschema_for::<String>()),
        (
            "replace_content",
            "Replace entire plan body with this content. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "append_content",
            "Append text to plan body. Auto-inserts newline separator if needed. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_in_content",
            "Surgically replace text in plan body.",
            gen.subschema_for::<Option<ReplaceInContent>>()
        ),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "git_branch",
            "Optional git branch",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "github_owner_repo",
            "Optional owner/repo",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tasks",
            "Optional tasks to create in batch",
            gen.subschema_for::<Option<Vec<TaskSpec>>>()
        ),
    ]
    },
    &["name"]
);

impl_json_schema!(
    DeletePlanParams,
    "DeletePlanParams",
    |gen: &mut SchemaGenerator| vec![("name", "Plan name", gen.subschema_for::<String>()),],
    &["name"]
);

impl_json_schema!(
    ListNotesParams,
    "ListNotesParams",
    |gen: &mut SchemaGenerator| vec![("plan", "Plan name", gen.subschema_for::<String>()),],
    &["plan"]
);

impl_json_schema!(
    AddNoteParams,
    "AddNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "id",
            "Optional note ID",
            gen.subschema_for::<Option<String>>()
        ),
        ("body", "Note markdown body", gen.subschema_for::<String>()),
        (
            "summary",
            "Optional note summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional note author",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["plan", "body"]
);

impl_json_schema!(
    GetNoteParams,
    "GetNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    DeleteNoteParams,
    "DeleteNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    UpdateNoteParams,
    "UpdateNoteParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
        ("summary", "Optional note summary", gen.subschema_for::<Option<String>>()),
        ("author", "Optional note author", gen.subschema_for::<Option<String>>()),
        ("replace_body", "Replace entire note body with this content. Keep under 1000 words.", gen.subschema_for::<Option<String>>()),
        ("append_body", "Append text to note body. Auto-inserts newline separator if needed. Keep under 1000 words.", gen.subschema_for::<Option<String>>()),
        ("replace_in_body", "Surgically replace text in note body.", gen.subschema_for::<Option<ReplaceInContent>>()),
    ]
    },
    &["plan", "note_id"]
);

impl_json_schema!(
    TaskSpec,
    "TaskSpec",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Task title", gen.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            gen.subschema_for::<Option<String>>()
        ),
        ("tags", "Task tags", gen.subschema_for::<Vec<String>>()),
        (
            "dependencies",
            "Task dependencies",
            gen.subschema_for::<Vec<String>>()
        ),
        (
            "status",
            "Task status",
            gen.subschema_for::<Option<String>>()
        ),
        ("body", "Task body", gen.subschema_for::<Option<String>>()),
        (
            "summary",
            "Task summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Task author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Task assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Task executor",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["title"]
);
