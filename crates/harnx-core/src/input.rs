//! `Input` — the composed user prompt for a turn: raw text, parsed
//! arguments, medias, tool-call context, session/agent bindings. Pure
//! data + pure accessors. Config-aware operations (from_str, stream,
//! create_client, build_messages, etc.) live in `harnx::config::input`
//! as free functions.

use crate::agent_config::AgentConfig;
use crate::crypto::sha256;
use crate::message::{ImageUrl, MessageContent, MessageContentPart, MessageContentToolCalls};
use crate::tool::ToolResult;

use std::collections::HashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SUMMARY_MAX_WIDTH: usize = 80;

#[derive(Debug, Clone)]
pub struct Input {
    pub text: String,
    pub raw: (String, Vec<String>),
    pub patched_text: Option<String>,
    pub last_reply: Option<String>,
    pub continue_output: Option<String>,
    pub regenerate: bool,
    pub medias: Vec<String>,
    pub data_urls: HashMap<String, String>,
    pub tool_calls: Option<MessageContentToolCalls>,
    pub agent: AgentConfig,
    pub rag_name: Option<String>,
    pub with_session: bool,
    pub with_agent: bool,
    /// When true, `Session::build_messages` will prepend the agent's system
    /// prompt even though the session already has messages.  Set by
    /// `set_agent()` — the only path that swaps agents after construction
    /// (e.g. compaction).
    pub inject_system_prompt: bool,
    /// User text injected after tool-call results (pending message consumed
    /// mid-tool-loop).  Appended as a trailing User message in
    /// `build_messages()`.
    pub injected_user_text: Option<String>,
}

impl Input {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.medias.is_empty()
    }

    pub fn data_urls(&self) -> HashMap<String, String> {
        self.data_urls.clone()
    }

    pub fn tool_calls(&self) -> &Option<MessageContentToolCalls> {
        &self.tool_calls
    }

    pub fn text(&self) -> String {
        match self.patched_text.clone() {
            Some(text) => text,
            None => self.text.clone(),
        }
    }

    pub fn clear_patch(&mut self) {
        self.patched_text = None;
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    pub fn continue_output(&self) -> Option<&str> {
        self.continue_output.as_deref()
    }

    pub fn set_continue_output(&mut self, output: &str) {
        let output = match &self.continue_output {
            Some(v) => format!("{v}{output}"),
            None => output.to_string(),
        };
        self.continue_output = Some(output);
    }

    pub fn regenerate(&self) -> bool {
        self.regenerate
    }

    pub fn rag_name(&self) -> Option<&str> {
        self.rag_name.as_deref()
    }

    pub fn merge_tool_results(
        mut self,
        output: String,
        thought: Option<String>,
        tool_results: Vec<ToolResult>,
    ) -> Self {
        match self.tool_calls.as_mut() {
            Some(exist_tool_results) => {
                exist_tool_results.merge(tool_results, output, thought);
            }
            None => {
                self.tool_calls = Some(MessageContentToolCalls::new(tool_results, output, thought))
            }
        }
        self
    }

    pub fn set_injected_user_text(&mut self, text: String) {
        self.injected_user_text = Some(text);
    }

    pub fn injected_user_text(&self) -> Option<&str> {
        self.injected_user_text.as_deref()
    }

    pub fn agent(&self) -> &AgentConfig {
        &self.agent
    }

    pub fn agent_mut(&mut self) -> &mut AgentConfig {
        &mut self.agent
    }

    pub fn with_session(&self) -> bool {
        self.with_session
    }

    pub fn with_agent(&self) -> bool {
        self.with_agent
    }

    pub fn inject_system_prompt(&self) -> bool {
        self.inject_system_prompt
    }

    pub fn summary(&self) -> String {
        let text: String = self
            .text
            .trim()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if text.width_cjk() > SUMMARY_MAX_WIDTH {
            let mut sum_width = 0;
            let mut chars = vec![];
            for c in text.chars() {
                sum_width += c.width_cjk().unwrap_or(1);
                if sum_width > SUMMARY_MAX_WIDTH - 3 {
                    chars.extend(['.', '.', '.']);
                    break;
                }
                chars.push(c);
            }
            chars.into_iter().collect()
        } else {
            text
        }
    }

    pub fn raw(&self) -> String {
        let (text, files) = &self.raw;
        let mut segments = files.to_vec();
        if !segments.is_empty() {
            segments.insert(0, ".file".into());
        }
        if !text.is_empty() {
            if !segments.is_empty() {
                segments.push("--".into());
            }
            segments.push(text.clone());
        }
        segments.join(" ")
    }

    pub fn render(&self) -> String {
        let text = self.text();
        if self.medias.is_empty() {
            return text;
        }
        let tail_text = if text.is_empty() {
            String::new()
        } else {
            format!(" -- {text}")
        };
        let files: Vec<String> = self
            .medias
            .iter()
            .cloned()
            .map(|url| resolve_data_url(&self.data_urls, url))
            .collect();
        format!(".file {}{}", files.join(" "), tail_text)
    }

    pub fn message_content(&self) -> MessageContent {
        if self.medias.is_empty() {
            MessageContent::Text(self.text())
        } else {
            let mut list: Vec<MessageContentPart> = self
                .medias
                .iter()
                .cloned()
                .map(|url| MessageContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                })
                .collect();
            if !self.text.is_empty() {
                list.insert(0, MessageContentPart::Text { text: self.text() });
            }
            MessageContent::Array(list)
        }
    }
}

pub fn resolve_data_url(data_urls: &HashMap<String, String>, data_url: String) -> String {
    if data_url.starts_with("data:") {
        let hash = sha256(&data_url);
        if let Some(path) = data_urls.get(&hash) {
            return path.to_string();
        }
        data_url
    } else {
        data_url
    }
}
