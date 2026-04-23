//! `Macro` + `MacroVariable` — the user-definable command macros loaded
//! from the `macros/` config directory. Pure data + pure methods.

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Macro {
    #[serde(default)]
    pub variables: Vec<MacroVariable>,
    pub steps: Vec<String>,
}

impl Macro {
    pub fn resolve_variables(&self, args: &[String]) -> Result<IndexMap<String, String>> {
        let mut output = IndexMap::new();
        for (i, variable) in self.variables.iter().enumerate() {
            let value = if variable.rest && i == self.variables.len() - 1 {
                if args.len() > i {
                    Some(args[i..].join(" "))
                } else {
                    variable.default.clone()
                }
            } else {
                args.get(i)
                    .map(|v| v.to_string())
                    .or_else(|| variable.default.clone())
            };
            let value =
                value.ok_or_else(|| anyhow!("Missing value for variable '{}'", variable.name))?;
            output.insert(variable.name.clone(), value);
        }
        Ok(output)
    }

    pub fn usage(&self, name: &str) -> String {
        let mut parts = vec![name.to_string()];
        for (i, variable) in self.variables.iter().enumerate() {
            let part = match (
                variable.rest && i == self.variables.len() - 1,
                variable.default.is_some(),
            ) {
                (true, true) => format!("[{}]...", variable.name),
                (true, false) => format!("<{}>...", variable.name),
                (false, true) => format!("[{}]", variable.name),
                (false, false) => format!("<{}>", variable.name),
            };
            parts.push(part);
        }
        parts.join(" ")
    }

    pub fn interpolate_command(command: &str, variables: &IndexMap<String, String>) -> String {
        let mut output = command.to_string();
        for (key, value) in variables {
            output = output.replace(&format!("{{{{{key}}}}}"), value);
        }
        output
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MacroVariable {
    pub name: String,
    #[serde(default)]
    pub rest: bool,
    pub default: Option<String>,
}
