use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub(crate) owner: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddPlanParams {
    /// Human plan name. It is slugified for the plan URL.
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
    pub(crate) parent_issue: Option<u64>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
    pub(crate) content: Option<String>,
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
    #[serde(default)]
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListNotesParams {
    pub(crate) plan: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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

fn default_open_status() -> String {
    "open".to_string()
}
