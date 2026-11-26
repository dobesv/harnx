//! The `PYTHON` line of `help <tool>`: how a script's Python calls the tool.

use serde_json::Value;

use crate::python_tools::{property_keys, required_keys};

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Whether Python can spell `name` as an identifier, so `tools.NAME` or a
/// keyword argument can carry it.
fn is_spellable(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic());
    first_ok && chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) && !KEYWORDS.contains(&name)
}

/// `tools.NAME(required, *, other=..., optional=None, **{...})`, or `None`
/// for a tool with no properties. Required properties are positional in
/// schema order, but only up to the first one Python cannot spell: the
/// binder gives every required property a positional slot, so a later one
/// cannot be shown by position. From there on required properties are shown
/// as keywords (`name=...` marks one as required), and unspellable ones fall
/// into the trailing `**{...}`. Optional properties follow, sorted by name so
/// the help does not depend on how the schema happened to be written.
pub(super) fn call_line(name: &str, schema: &Value) -> Option<String> {
    let keys = property_keys(schema);
    if keys.is_empty() {
        return None;
    }
    let required = required_keys(schema);
    let positional_len = required.iter().take_while(|k| is_spellable(k)).count();
    let (positional, required_keywords) = required.split_at(positional_len);
    let mut optional: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| !required.contains(k))
        .collect();
    optional.sort_unstable();
    let keywords: Vec<String> = required_keywords
        .iter()
        .filter(|k| is_spellable(k))
        .map(|k| format!("{k}=..."))
        .chain(
            optional
                .iter()
                .filter(|k| is_spellable(k))
                .map(|k| format!("{k}=None")),
        )
        .collect();
    let mut parts: Vec<String> = positional.iter().map(|k| k.to_string()).collect();
    if !keywords.is_empty() {
        parts.push("*".into());
        parts.extend(keywords);
    }
    if keys.iter().any(|k| !is_spellable(k)) {
        parts.push("**{...}".into());
    }
    let callee = if is_spellable(name) {
        format!("tools.{name}")
    } else {
        format!("getattr(tools, {})", Value::String(name.into()))
    };
    Some(format!("{callee}({})", parts.join(", ")))
}
