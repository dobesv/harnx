//! Automatic session title generation.
//!
//! Mirrors the compaction pipeline (`session_ops_compaction.rs`): a cheap
//! guard (`need_generate_title`) decides when to (re)generate, a title agent
//! is resolved from config, and the actual LLM call runs on a spawned task so
//! the agent loop is never blocked. The generated title is appended to the
//! session log as `SessionLogEntry::Title`, stored on the in-memory session,
//! and surfaced via a `TitleUpdated` event. There is no fallback: if
//! generation fails or no title agent is configured, the title is left unset.

use super::*;
use crate::config::session_lock::SessionLock;

use harnx_core::message::MessageRole;

/// System prompt used when no explicit title agent is configured. Produces a
/// short natural-language title (not a slug) suitable for session listings.
pub const DEFAULT_TITLE_SYSTEM_PROMPT: &str = r#"Generate a concise session title (10 words or fewer).

Rules:
- Plain text only — no quotes, no punctuation at the end, no markdown
- Natural language (not slug/kebab-case)
- Capture the main topic or goal
- RESPOND WITH THE TITLE ONLY

Examples:
Debugging Rust async lifetime errors
Setting up PostgreSQL connection pooling
Planning a hiking trip to Patagonia"#;

/// Maximum characters kept from the transcript sent to the title model. Keeps
/// the prompt cheap (~2000 tokens) regardless of how long the exchange grew.
const MAX_TRANSCRIPT_CHARS: usize = 8000;
/// Maximum characters kept from a single message when assembling the transcript.
const MAX_SECTION_CHARS: usize = 4000;
/// Hard cap on the stored title length.
const MAX_TITLE_CHARS: usize = 200;

fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(max).collect();
    format!("{truncated}…")
}

/// Format a `label:\n<text>` section, truncated, or `None` when `text` is blank.
fn labeled_section(label: &str, text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| format!("{label}:\n{}", truncate_chars(text, MAX_SECTION_CHARS)))
}

/// The first user message text, and (only if different) the last user message.
fn first_and_last_user_text(session: &Session) -> (String, Option<String>) {
    let messages = &session.messages;
    let first = messages.iter().position(|m| m.role == MessageRole::User);
    let last = messages.iter().rposition(|m| m.role == MessageRole::User);
    let first_text = first
        .map(|i| messages[i].content.to_text())
        .unwrap_or_default();
    let last_text = match (first, last) {
        (Some(f), Some(l)) if l != f => Some(messages[l].content.to_text()),
        _ => None,
    };
    (first_text, last_text)
}

/// The concatenated assistant replies that follow the last user message.
fn assistant_after_last_user(session: &Session) -> String {
    let messages = &session.messages;
    let Some(last) = messages.iter().rposition(|m| m.role == MessageRole::User) else {
        return String::new();
    };
    messages
        .iter()
        .skip(last + 1)
        .filter(|m| m.role == MessageRole::Assistant)
        .map(|m| m.content.to_text())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Build a compact transcript for title generation. Mirrors the TUI exit
/// summary heuristic (`select_breakdown_sections` in `crates/harnx/src/main.rs`):
/// first user message + last user message (if different) + assistant replies
/// after the last user message. Deterministic and cheap — no extra LLM call.
pub fn build_title_transcript(session: &Session) -> String {
    let (first_user, last_user) = first_and_last_user_text(session);
    let assistant = assistant_after_last_user(session);

    let sections: Vec<String> = [
        labeled_section("First user message", &first_user),
        labeled_section("Latest user message", last_user.as_deref().unwrap_or("")),
        labeled_section("Latest assistant response", &assistant),
    ]
    .into_iter()
    .flatten()
    .collect();

    truncate_chars(&sections.join("\n\n"), MAX_TRANSCRIPT_CHARS)
}

/// First non-empty line, trimmed — reasoning models sometimes emit extra
/// commentary on later lines.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// Drop a leading `Title:` label (any case).
fn strip_title_label(text: &str) -> &str {
    for prefix in ["Title:", "title:", "TITLE:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return rest.trim();
        }
    }
    text
}

/// Strip one layer of matching surrounding single or double quotes.
fn strip_surrounding_quotes(text: &str) -> &str {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return text;
    };
    let is_quote = first == '"' || first == '\'';
    if is_quote && chars.next_back() == Some(first) {
        return text[first.len_utf8()..text.len() - first.len_utf8()].trim();
    }
    text
}

/// Clean the raw model output into a stored title: collapse to one line, drop a
/// leading `Title:` label, strip surrounding quotes/markdown emphasis and any
/// trailing sentence punctuation, then cap length.
fn post_process_title(raw: &str) -> String {
    let title = first_line(raw.trim());
    let title = strip_title_label(title);
    let title = strip_surrounding_quotes(title);
    let title = title
        .trim_matches(|c| c == '*' || c == '_' || c == '`')
        .trim();
    let title = title
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .trim_end();
    truncate_chars(title, MAX_TITLE_CHARS)
}

/// Handle the outcome of a spawned `generate_title`: emit `TitleUpdated` on a
/// new title, warn on error, ignore the "nothing to title" case.
fn handle_title_result(result: anyhow::Result<Option<String>>) {
    match result {
        Ok(Some(title)) => {
            harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                harnx_core::event::SessionEvent::TitleUpdated(title),
            ));
        }
        Ok(None) => {}
        Err(err) => warn!("Failed to generate session title: {err}"),
    }
}

impl Config {
    /// Run the periodic, best-effort session maintenance that fires after each
    /// completed turn: threshold-triggered compaction, then title (re)generation.
    /// Both are internally guarded and spawn their own work, so this returns
    /// immediately.
    pub fn run_post_turn_maintenance(config: GlobalConfig) {
        Self::maybe_compact_session(config.clone());
        Self::maybe_generate_title(config);
    }

    /// Fire-and-forget title generation. Checks the threshold guard, resolves a
    /// title agent (returning early if none is configured), marks the session as
    /// `titling`, and spawns the generation on a background task. Mirrors
    /// `maybe_compact_session`.
    pub fn maybe_generate_title(config: GlobalConfig) {
        if !Self::claim_titling(&config) {
            return;
        }

        // Only proceed if a title agent is actually configured; otherwise clear
        // the flag we just set and bail (no fallback title generation).
        if Self::resolve_title_agent(&config).is_none() {
            Self::clear_titling(&config, None);
            return;
        }

        let titling_session_id = config
            .read()
            .session
            .as_ref()
            .map(|session| session.id.clone());

        tokio::spawn(async move {
            let result = Self::generate_title(&config).await;
            Self::clear_titling(&config, titling_session_id.as_deref());
            handle_title_result(result);
        });
    }

    /// Under the write lock: if the active session needs a title, set its
    /// `titling` flag and return `true`. Returns `false` (no-op) otherwise.
    fn claim_titling(config: &GlobalConfig) -> bool {
        let mut guard = config.write();
        let threshold = guard.title_update_threshold;
        let Some(session) = guard.session.as_mut() else {
            return false;
        };
        if !session.need_generate_title(threshold) {
            return false;
        }
        session.set_titling(true);
        true
    }

    /// Clear the `titling` flag on the active session. When `session_id` is
    /// `Some`, only clear if the active session still matches (guards against
    /// session swaps during the spawned task).
    fn clear_titling(config: &GlobalConfig, session_id: Option<&str>) {
        if let Some(session) = config.write().session.as_mut() {
            if session_id.is_none_or(|id| session.id == id) {
                session.set_titling(false);
            }
        }
    }

    /// Resolve the configured title agent: `AgentConfig.title_agent` first, then
    /// the global `ConfigData.title_agent`. Returns `None` when neither is set
    /// (no title generation). Mirrors `resolve_compaction_agent`.
    fn resolve_title_agent(config: &GlobalConfig) -> Option<crate::config::agent::Agent> {
        let active_agent_name = config.read().extract_agent().name().to_string();
        let active_pkg = harnx_core::package_namespace::pkg_from_qualified(&active_agent_name);
        let name = {
            let guard = config.read();
            let agent = guard.extract_agent();
            agent
                .title_agent()
                .map(str::to_owned)
                .or_else(|| guard.title_agent.clone())?
        };

        let resolved_name =
            harnx_core::package_namespace::resolve_package_relative_name(&name, active_pkg);
        match config.read().retrieve_agent(&resolved_name) {
            Ok(mut title_agent) => {
                if let Err(e) = self::agent::resolve_variables(&mut title_agent) {
                    warn!("Failed to resolve variables for title_agent '{name}': {e}");
                }
                Some(title_agent)
            }
            Err(e) => {
                warn!("Failed to load title_agent '{name}': {e}; skipping title generation");
                None
            }
        }
    }

    /// Perform the actual title generation. Returns `Ok(Some(title))` on success,
    /// `Ok(None)` when there is nothing to title (empty transcript / model
    /// returned an empty string), and `Err` on LLM/model failure.
    async fn generate_title(config: &GlobalConfig) -> Result<Option<String>> {
        let title_agent = Self::resolve_title_agent(config)
            .context("no title agent configured")?
            .into_config();

        let (transcript, session_id, tokens) = {
            let guard = config.read();
            let session = guard.session.as_ref().context("No session")?;
            (
                build_title_transcript(session),
                session.id.clone(),
                session.tokens,
            )
        };
        if transcript.trim().is_empty() {
            return Ok(None);
        }

        let mut input =
            harnx_core::input::Input::new(transcript.clone(), (transcript, vec![]), title_agent);
        input.with_session = false;
        input.with_agent = true;

        let raw = crate::config::input::fetch_chat_text(&mut input, config).await?;
        let title = post_process_title(&raw);
        if title.is_empty() {
            return Ok(None);
        }

        // Persist to the session log + in-memory session, but only if the active
        // session is still the one we titled (it may have been swapped).
        //
        // CRITICAL: This runs on a background task that may start BEFORE the
        // agent loop's SessionLock is released. We cannot use a blocking acquire
        // (would deadlock). Use bounded try_acquire with fallback.
        {
            // Derive session path WITHOUT holding the write guard.
            let session_path = {
                let guard = config.read();
                let Some(session) = guard.session.as_ref() else {
                    return Ok(None);
                };
                if session.id != session_id {
                    return Ok(None);
                }
                // Skip ephemeral / unsaved sessions or sessions without a path.
                if session.save_session() == Some(false) {
                    // Just set in-memory title; no log persistence needed.
                    let mut guard = config.write();
                    if let Some(session) = guard.session.as_mut() {
                        if session.id == session_id {
                            session.set_title(title.clone());
                            session.set_title_last_updated_tokens(tokens);
                        }
                    }
                    return Ok(Some(title));
                }
                match (&session.path, &session.sessions_dir) {
                    (Some(path), _) => std::path::PathBuf::from(path),
                    (None, Some(dir)) => dir.join(format!("{}.yaml", session.id)),
                    (None, None) => {
                        // No persistence path - skip log append, set in-memory only.
                        let mut guard = config.write();
                        if let Some(session) = guard.session.as_mut() {
                            if session.id == session_id {
                                session.set_title(title.clone());
                                session.set_title_last_updated_tokens(tokens);
                            }
                        }
                        return Ok(Some(title));
                    }
                }
            };

            // Try to acquire the session lock with bounded retries.
            // This avoids deadlocking against the same-process runner lock
            // (which is released shortly after the turn ends).
            let lock = {
                let session_path = session_path.clone();
                tokio::task::spawn_blocking(move || {
                    let mut attempts = 0;
                    let max_attempts = 20; // ~2s total with 100ms sleeps
                    let sleep_ms = 100;
                    loop {
                        match SessionLock::try_acquire(&session_path) {
                            Ok(Some(lock)) => return Ok(lock),
                            Ok(None) => {
                                // Lock held by another process; retry
                                attempts += 1;
                                if attempts >= max_attempts {
                                    return Err(anyhow::anyhow!(
                                        "title persist: could not acquire session lock after {} attempts",
                                        attempts
                                    ));
                                }
                                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                            }
                            Err(e) => return Err(e),
                        }
                    }
                })
                .await
                .context("title persist lock task join")?
            };

            match lock {
                Ok(_session_lock) => {
                    // Lock acquired - now take the write guard and persist.
                    let mut guard = config.write();
                    let Some(session) = guard.session.as_mut() else {
                        return Ok(None);
                    };
                    if session.id != session_id {
                        return Ok(None);
                    }
                    crate::config::session::record_title(session, title.clone(), false, tokens);
                    // Lock dropped at end of scope along with guard.
                }
                Err(e) => {
                    // Failed to acquire lock - fall back to in-memory only.
                    // The title will be persisted on next locked save (dirty flag not set
                    // since we skip the log append; title will re-derive from log on reload).
                    log::warn!(
                        "title persist: could not acquire session lock for {}: {}; setting in-memory only",
                        session_id,
                        e
                    );
                    let mut guard = config.write();
                    if let Some(session) = guard.session.as_mut() {
                        if session.id == session_id {
                            session.set_title(title.clone());
                            session.set_title_last_updated_tokens(tokens);
                        }
                    }
                }
            }
        }

        Ok(Some(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{Message, MessageContent};
    use harnx_core::session::Session;

    fn user(text: &str) -> Message {
        Message::new(MessageRole::User, MessageContent::Text(text.to_string()))
    }
    fn assistant(text: &str) -> Message {
        Message::new(
            MessageRole::Assistant,
            MessageContent::Text(text.to_string()),
        )
    }
    fn session_with(messages: Vec<Message>) -> Session {
        Session {
            messages,
            ..Session::default()
        }
    }

    #[test]
    fn transcript_single_exchange_has_first_user_and_assistant() {
        let session = session_with(vec![
            user("How do I fix async lifetimes?"),
            assistant("Use owned data."),
        ]);
        let t = build_title_transcript(&session);
        assert!(t.contains("How do I fix async lifetimes?"));
        assert!(t.contains("Use owned data."));
        // Only one user message: no separate "Latest user message" section.
        assert!(!t.contains("Latest user message:"));
    }

    #[test]
    fn transcript_multi_exchange_includes_first_and_last_user() {
        let session = session_with(vec![
            user("first question about postgres"),
            assistant("answer one"),
            user("later question about pooling"),
            assistant("answer two"),
        ]);
        let t = build_title_transcript(&session);
        assert!(t.contains("first question about postgres"));
        assert!(t.contains("later question about pooling"));
        assert!(t.contains("answer two"));
        // Assistant reply before the last user message is not the "latest response".
        assert!(!t.contains("answer one"));
    }

    #[test]
    fn transcript_empty_session_is_empty() {
        let session = Session::default();
        assert!(build_title_transcript(&session).trim().is_empty());
    }

    #[test]
    fn post_process_strips_quotes_prefix_and_extra_lines() {
        assert_eq!(
            post_process_title("  Debugging async errors  "),
            "Debugging async errors"
        );
        assert_eq!(
            post_process_title("\"Setting up Postgres\""),
            "Setting up Postgres"
        );
        assert_eq!(post_process_title("'Planning a trip'"), "Planning a trip");
        assert_eq!(
            post_process_title("Title: Fix the parser"),
            "Fix the parser"
        );
        assert_eq!(
            post_process_title("Rust ownership refactor\n\nHere is why..."),
            "Rust ownership refactor"
        );
    }

    #[test]
    fn post_process_strips_trailing_punctuation_and_markdown() {
        assert_eq!(
            post_process_title("Fixing the login bug."),
            "Fixing the login bug"
        );
        assert_eq!(post_process_title("Configuring CI!"), "Configuring CI");
        assert_eq!(post_process_title("**Bold title**"), "Bold title");
        assert_eq!(post_process_title("`code title`"), "code title");
    }

    #[test]
    fn post_process_caps_length() {
        let long = "word ".repeat(200);
        let out = post_process_title(&long);
        assert!(out.chars().count() <= MAX_TITLE_CHARS + 1); // +1 for the ellipsis
    }
}
