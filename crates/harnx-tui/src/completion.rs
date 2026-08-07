//! Completion candidates for the `.`-prefixed commands.

use std::collections::HashSet;

/// Completions for a partial first word such as `.inf`.
///
/// A dispatch name can appear more than once in `COMMANDS`, since commands
/// sharing a name differ only in the arguments they document. Offering it twice
/// would put a duplicate in the picker, so only the first is kept.
pub(crate) fn command_name_completions(filter: &str) -> Vec<(String, Option<String>)> {
    let mut seen = HashSet::new();
    harnx_runtime::commands::COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(filter) && seen.insert(command.name))
        .map(|command| {
            (
                format!("{} ", command.name),
                Some(command.description.to_string()),
            )
        })
        .collect()
}
