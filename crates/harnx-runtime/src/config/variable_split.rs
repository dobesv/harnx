use super::{compaction::truncate_middle, session_lock::SessionLock, Config};
use crate::utils::{edit_file, temp_file};
use anyhow::{bail, Context, Result};

impl Config {
    pub fn list_variables(&self) -> Result<String> {
        let session = self.session.as_ref().context("No active session")?;
        if session.agent_variables().is_empty() {
            return Ok("No session variables".to_string());
        }

        Ok(session
            .agent_variables()
            .iter()
            .map(|(name, value)| format!("{name} = {}", truncate_middle(value, 200)))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub fn get_variable(&self, name: &str) -> Result<String> {
        let session = self.session.as_ref().context("No active session")?;
        session
            .agent_variables()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Variable '{name}' not set"))
    }

    pub fn set_variable(&mut self, name: &str, value: &str) -> Result<()> {
        let session_name = self
            .session
            .as_ref()
            .context("No active session")?
            .id()
            .to_string();
        let lock = SessionLock::acquire(&self.session_file(&session_name))?;

        let (agent, session) = match (self.agent.as_mut(), self.session.as_mut()) {
            (Some(agent), Some(session)) => (agent, session),
            (None, _) => bail!("No active agent"),
            (_, None) => bail!("No active session"),
        };
        let mut variables = session.agent_variables().clone();
        variables.insert(name.to_string(), value.to_string());
        agent.set_session_variables(variables);
        session.sync_agent(agent)?;

        // save_session acquires this same non-reentrant lock internally.
        drop(lock);
        self.save_session(None)
    }

    pub fn edit_variable(&mut self, name: &str) -> Result<()> {
        let current_value = self
            .session
            .as_ref()
            .context("No active session")?
            .agent_variables()
            .get(name)
            .cloned()
            .unwrap_or_default();
        let temp_file = if let Some(ref dir) = self.temp_dir_override {
            dir.join(format!("variable-edit-{}.txt", uuid::Uuid::new_v4()))
        } else {
            temp_file("variable-edit", ".txt")
        };

        std::fs::write(&temp_file, current_value)
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;

        let edit_result = self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &temp_file).with_context(|| {
                format!("Failed to edit '{}' with '{}'", temp_file.display(), editor)
            })
        });
        let edited_content = std::fs::read_to_string(&temp_file)
            .with_context(|| format!("Failed to read '{}'", temp_file.display()));
        let _ = std::fs::remove_file(&temp_file);
        edit_result?;
        let edited_content = edited_content?;

        self.set_variable(name, &edited_content)
    }

    pub fn load_variable(&mut self, name: &str, path: &str) -> Result<()> {
        let value = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read variable file '{path}'"))?;
        self.set_variable(name, &value)
    }
}
