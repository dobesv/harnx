use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceInContent {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    #[serde(default)]
    pub(crate) replace_all: Option<bool>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListPlansParams {
    /// Optional 64-character session owner key.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) owner: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddPlanParams {
    /// Human plan name. It is slugified for the plan URL.
    pub(crate) name: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) title: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) executor: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) git_branch: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) github_owner_repo: Option<String>,
    #[serde(default, deserialize_with = "zero_as_none")]
    pub(crate) parent_issue: Option<u64>,
    /// Accepted for older callers but left out of the schema, so a model
    /// sees one body parameter rather than two it must choose between.
    #[serde(default, deserialize_with = "blank_as_none")]
    #[schemars(skip)]
    pub(crate) body: Option<String>,
    /// Markdown body of the plan. Omit to start with an empty body.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) content: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetPlanParams {
    /// Full plan URL, for example `cid:plan:pantheon%2Fatlas/armDRA/my-plan`.
    pub(crate) plan: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePlanParams {
    /// Full `cid:plan:` index URL.
    pub(crate) plan: String,
    /// Synonym of `replace_content`, accepted but left out of the schema
    /// for the same reason as `AddPlanParams::body`.
    #[serde(default, deserialize_with = "blank_as_none")]
    #[schemars(skip)]
    pub(crate) content: Option<String>,
    /// New markdown body, replacing the current one. Set at most one of
    /// `replace_content`, `append_content` and `replace_in_content`.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) replace_content: Option<String>,
    /// Markdown appended to the current body on a new line.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) append_content: Option<String>,
    /// Replace exact text within the current body.
    #[serde(default)]
    pub(crate) replace_in_content: Option<ReplaceInContent>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) title: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) executor: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) git_branch: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) github_owner_repo: Option<String>,
    #[serde(default, deserialize_with = "zero_as_none")]
    pub(crate) parent_issue: Option<u64>,
    #[serde(default)]
    pub(crate) tasks: Option<Vec<TaskSpec>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeletePlanParams {
    pub(crate) plan: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListTasksParams {
    pub(crate) plan: String,
    #[serde(default = "default_open_status")]
    pub(crate) filter: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) tag: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetTaskParams {
    pub(crate) plan: String,
    /// Full task URL belonging to `plan`.
    pub(crate) id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddTaskParams {
    pub(crate) title: String,
    pub(crate) plan: String,
    /// Optional URL-safe task ID.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) id: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) status: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) body: Option<String>,
    /// Full `cid:plan:.../tasks/...` URLs.
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateTaskParams {
    pub(crate) plan: String,
    /// Full task URL belonging to `plan`.
    pub(crate) id: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) title: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) executor: Option<String>,
    #[serde(default)]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) status: Option<String>,
    /// New markdown body, replacing the current one. Set at most one of
    /// `replace_body`, `append_body` and `replace_in_body`.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) replace_body: Option<String>,
    /// Markdown appended to the current body on a new line.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) append_body: Option<String>,
    /// Replace exact text within the current body.
    #[serde(default)]
    pub(crate) replace_in_body: Option<ReplaceInContent>,
    #[serde(default)]
    pub(crate) dependencies: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteTaskParams {
    pub(crate) plan: String,
    pub(crate) id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskSpec {
    pub(crate) title: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) status: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) body: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) assignee: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) executor: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListNotesParams {
    pub(crate) plan: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddNoteParams {
    pub(crate) plan: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) id: Option<String>,
    pub(crate) body: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetNoteParams {
    pub(crate) plan: String,
    /// Full note URL belonging to `plan`.
    pub(crate) note_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteNoteParams {
    pub(crate) plan: String,
    pub(crate) note_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateNoteParams {
    pub(crate) plan: String,
    pub(crate) note_id: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) author: Option<String>,
    /// New markdown body, replacing the current one. Set at most one of
    /// `replace_body`, `append_body` and `replace_in_body`.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) replace_body: Option<String>,
    /// Markdown appended to the current body on a new line.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) append_body: Option<String>,
    /// Replace exact text within the current body.
    #[serde(default)]
    pub(crate) replace_in_body: Option<ReplaceInContent>,
}

fn default_open_status() -> String {
    "open".to_string()
}

/// GPT models fill optional string parameters with `""` instead of leaving
/// them out. Reading a blank value as omitted keeps a placeholder from
/// colliding with a real value in a set-at-most-one group, or from
/// overwriting stored metadata during an update.
fn blank_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|text| !text.trim().is_empty()))
}

/// GitHub issue numbers start at 1, so `0` is a model's placeholder for
/// "no parent issue".
fn zero_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<u64>::deserialize(deserializer)?;
    Ok(value.filter(|issue| *issue != 0))
}
