//! The arguments of the write tools. Their JSON schemas are in `params.rs`.

use harnx_git_history::DiffLevel;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct WriteFileParams {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub diff: Option<DiffLevel>,
}

#[derive(Debug, Deserialize)]
pub struct EditFileParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
    #[serde(default)]
    pub diff: Option<DiffLevel>,
}

#[derive(Debug, Deserialize)]
pub struct InsertParams {
    pub path: String,
    #[serde(default)]
    pub insert_line: Option<usize>,
    pub insert_text: String,
    #[serde(default)]
    pub column: Option<usize>,
    #[serde(default)]
    pub diff: Option<DiffLevel>,
}

#[derive(Debug, Deserialize)]
pub struct ReReplaceParams {
    pub path: String,
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
    #[serde(default)]
    pub diff: Option<DiffLevel>,
}
