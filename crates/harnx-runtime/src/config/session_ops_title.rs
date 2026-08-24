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

use harnx_core::message::MessageRole;

/// Result of attempting to resolve a title agent from configuration.
///
/// Distinguishes three error cases that were previously collapsed into one
/// generic "no title agent configured" message:
///
/// - `NotConfigured`: Neither `AgentConfig.title_agent` nor global `ConfigData.title_agent`
///   is set. The error message MUST be exactly "no title agent configured".
/// - `NotFound`: A title-agent name is configured but no such agent file exists.
///   The error includes the configured name.
/// - `LoadError`: Agent file exists but fails to load/parse. The underlying error
///   is surfaced with the agent name in context.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum TitleAgentResolution {
    /// Title agent is not configured (neither agent frontmatter nor global config).
    NotConfigured,
    /// Title agent name is configured but the agent file was not found.
    NotFound {
        /// The configured title agent name.
        name: String,
    },
    /// Title agent file exists but failed to load or parse.
    LoadError {
        /// The configured title agent name.
        name: String,
        /// The underlying error from `retrieve_agent` or `resolve_variables`.
        source: anyhow::Error,
    },
    /// Title agent resolved successfully.
    Ok(crate::config::agent::Agent),
}

impl TitleAgentResolution {
    /// Convert to `Result<Option<Agent>>` for callers that need Result-based
    /// error handling.
    ///
    /// - `Ok(Some(agent))` = successfully resolved
    /// - `Ok(None)` = not configured
    /// - `Err(..)` = configured but broken (precise error with agent name)
    ///
    /// The `NotConfigured` → `Ok(None)` mapping lets callers attach their own
    /// context message (e.g. `generate_title` turns it into a hard error, while
    /// background generation silently skips).
    pub(crate) fn into_result(self) -> Result<Option<crate::config::agent::Agent>> {
        match self {
            Self::NotConfigured => Ok(None),
            Self::NotFound { name } => Err(anyhow::anyhow!("title agent '{}' not found", name)),
            Self::LoadError { name, source } => {
                Err(source.context(format!("title agent '{}' failed to load", name)))
            }
            Self::Ok(agent) => Ok(Some(agent)),
        }
    }
}

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
/// new title, emit `TitleGenerationFailed` and warn on error, and ignore the
/// "nothing to title" case.
pub(crate) fn handle_title_result(result: anyhow::Result<Option<String>>) {
    match result {
        Ok(Some(title)) => {
            info!("session title generated: {title:?}");
            harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                harnx_core::event::SessionEvent::TitleUpdated(title),
            ));
        }
        Ok(None) => debug!("title generation produced nothing"),
        Err(err) => {
            warn!("Failed to generate session title: {err:#}");
            harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                harnx_core::event::SessionEvent::TitleGenerationFailed(format!("{err:#}")),
            ));
        }
    }
}

/// Decide which title-agent name to look up and how to qualify it, given the
/// active agent's own `title_agent` frontmatter, the global
/// `ConfigData.title_agent`, and the active agent's package.
///
/// The name can come from two sources, which resolve differently:
///   1. The active agent's own `title_agent` frontmatter — a bare peer name
///      means a peer *within the active agent's package*, so it resolves
///      package-relative.
///   2. The global `ConfigData.title_agent` — this refers to a *top-level*
///      agent, so a bare name must resolve at the top level (NOT prefixed with
///      whatever package agent happens to be active). Resolving it
///      package-relative was the bug behind #103: running a package agent (e.g.
///      `pantheon/sisyphus`) rewrote `title-agent` into `pantheon/title-agent`,
///      which does not exist.
///
/// Returns `(name, resolved_name)` where `name` is the bare configured name and
/// `resolved_name` is the (possibly package-qualified) name to look up first.
/// Returns `None` when no title agent is configured from either source.
pub(crate) fn resolve_title_agent_name(
    agent_title_agent: Option<&str>,
    global_title_agent: Option<&str>,
    active_pkg: Option<&str>,
) -> Option<(String, String)> {
    if let Some(name) = agent_title_agent {
        // Source 1: active agent frontmatter → package-relative.
        let resolved =
            harnx_core::package_namespace::resolve_package_relative_name(name, active_pkg);
        Some((name.to_string(), resolved))
    } else {
        // Source 2: global config → resolve at top level. Passing `None` still
        // honors an explicitly qualified `pkg/foo` or `/foo`, but keeps a bare
        // name top-level.
        let name = global_title_agent?;
        let resolved = harnx_core::package_namespace::resolve_package_relative_name(name, None);
        Some((name.to_string(), resolved))
    }
}

/// Choose which name to look up for the title agent, given the bare configured
/// `name`, its (possibly package-qualified) `resolved_name`, and whether an
/// agent file exists at `resolved_name`.
///
/// When a package prefix was applied (`resolved_name != name`) but no agent
/// file exists at that package path, fall back to the bare top-level name.
/// Otherwise use `resolved_name` as-is — crucially, this means a package agent
/// file that EXISTS but fails to load surfaces its own error rather than being
/// masked by a top-level agent of the same name (#103 follow-up).
///
/// The fallback strips a leading `/` from `name` (an explicit "top-level"
/// escape such as `/foo` resolves to the file `foo`), so we never fall back to
/// a literal slash-prefixed name that no agent file would match.
fn title_agent_lookup_name<'a>(
    name: &'a str,
    resolved_name: &'a str,
    resolved_file_exists: bool,
) -> &'a str {
    let package_prefix_applied = resolved_name != name;
    if package_prefix_applied && !resolved_file_exists {
        name.strip_prefix('/').unwrap_or(name)
    } else {
        resolved_name
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
            let guard = config.read();
            let session_state = guard.session.as_ref().map(|session| {
                (
                    session.tokens(),
                    session.title().map(str::to_owned),
                    session.title_last_updated_tokens(),
                )
            });
            debug!(
                "title generation: guard not met; threshold={}, session_state={session_state:?}",
                guard.title_update_threshold
            );
            return;
        }

        // Capture the id of the session whose titling flag we just claimed, so
        // every cleanup path (including the no-agent early return) targets that
        // exact session even if the active session were to change.
        let titling_session_id = config
            .read()
            .session
            .as_ref()
            .map(|session| session.id.clone());

        // Only skip when NO title agent is configured. A configured-but-broken
        // agent (NotFound / LoadError) must fall through so the spawned task's
        // `generate_title` surfaces the real failure via `TitleGenerationFailed`
        // — the whole point of the background failure-surfacing path. Skipping
        // those here would silently swallow them.
        let resolution = Self::resolve_title_agent(&config);
        if matches!(resolution, TitleAgentResolution::NotConfigured) {
            debug!("title generation: no title agent configured; skipping");
            Self::clear_titling(&config, titling_session_id.as_deref());
            return;
        }

        if let Some(id) = titling_session_id.as_deref() {
            info!("title generation: starting for session {id}");
        }

        tokio::spawn(async move {
            let result = harnx_core::sink::with_agent_event_sink(
                Arc::new(harnx_core::event::NullSink),
                Self::generate_title(&config),
            )
            .await;
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
    /// the global `ConfigData.title_agent`. Returns a `TitleAgentResolution`
    /// distinguishing between not configured, not found, load error, and success.
    pub(crate) fn resolve_title_agent(config: &GlobalConfig) -> TitleAgentResolution {
        let active_agent_name = config.read().extract_agent().name().to_string();
        let active_pkg = harnx_core::package_namespace::pkg_from_qualified(&active_agent_name);

        let (name, resolved_name) = {
            let guard = config.read();
            let agent = guard.extract_agent();
            let Some((name, resolved_name)) = resolve_title_agent_name(
                agent.title_agent(),
                guard.title_agent.as_deref(),
                active_pkg,
            ) else {
                return TitleAgentResolution::NotConfigured;
            };
            (name, resolved_name)
        };

        // Try the resolved name first; if a package prefix was applied and the
        // package-qualified agent FILE DOES NOT EXIST, fall back to the bare
        // top-level name. The decision is gated on file existence (not on
        // `retrieve_agent` erroring) so that a package agent file which *exists
        // but fails to load* (malformed frontmatter, unknown model, …) surfaces
        // its real error instead of being masked by a top-level agent of the
        // same name. See `title_agent_lookup_name`.
        let resolved_exists = Self::agent_file(&resolved_name).exists();
        let lookup_name = title_agent_lookup_name(&name, &resolved_name, resolved_exists);
        let lookup_path = Self::agent_file(lookup_name);
        let lookup_exists = lookup_path.exists();

        match config.read().retrieve_agent(lookup_name) {
            Ok(mut title_agent) => {
                if let Err(e) = self::agent::resolve_variables(&mut title_agent) {
                    warn!("Failed to resolve variables for title_agent '{name}': {e}");
                    return TitleAgentResolution::LoadError { name, source: e };
                }
                TitleAgentResolution::Ok(title_agent)
            }
            Err(e) => {
                warn!("Failed to load title_agent '{name}': {e}; skipping title generation");
                // Distinguish between "not found" and other errors based on file existence.
                // retrieve_agent checks both file existence and builtin agents, so we need
                // to check file existence ourselves to correctly categorize this.
                if lookup_exists {
                    // File exists but failed to load - this is a load/parse error
                    TitleAgentResolution::LoadError { name, source: e }
                } else {
                    // File doesn't exist - this is "not found"
                    TitleAgentResolution::NotFound { name }
                }
            }
        }
    }

    /// Perform the actual title generation. Returns `Ok(Some(title))` on success,
    /// `Ok(None)` when there is nothing to title (empty transcript / model
    /// returned an empty string), and `Err` on LLM/model failure.
    pub(crate) async fn generate_title(config: &GlobalConfig) -> Result<Option<String>> {
        // `into_result` maps NotFound/LoadError to precise errors and
        // NotConfigured to `Ok(None)`; turn the latter into the hard
        // "no title agent configured" error specific to explicit generation.
        let title_agent = Self::resolve_title_agent(config)
            .into_result()?
            .context("no title agent configured")?
            .into_config();
        info!(
            "title generation: using agent '{}' with model '{}'",
            title_agent.name(),
            title_agent.model().id()
        );

        let (transcript, session_id, tokens) = {
            let guard = config.read();
            let session = guard.session.as_ref().context("No session")?;
            (
                build_title_transcript(session),
                session.id.clone(),
                session.tokens,
            )
        };
        debug!("title generation: transcript length={}", transcript.len());
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

        {
            let mut guard = config.write();
            let Some(session) = guard.session.as_mut() else {
                return Ok(None);
            };
            if session.id != session_id {
                return Ok(None);
            }
            crate::config::session::record_title(session, title.clone(), false, tokens)?;
        }

        Ok(Some(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent, SessionEvent};
    use harnx_core::message::{Message, MessageContent};
    use harnx_core::session::Session;
    use std::sync::Mutex;

    // --- title-agent name resolution (#103) ---

    #[test]
    fn global_title_agent_resolves_top_level_even_inside_a_package_agent() {
        // Regression for #103: running a package agent (e.g. `pantheon/sisyphus`)
        // must NOT rewrite a globally-configured bare `title-agent` into
        // `pantheon/title-agent`. The global title agent is a top-level agent.
        let (name, resolved) = resolve_title_agent_name(
            None,                // active agent has no title_agent frontmatter
            Some("title-agent"), // global ConfigData.title_agent
            Some("pantheon"),    // active package
        )
        .expect("global title agent should resolve");
        assert_eq!(name, "title-agent");
        assert_eq!(resolved, "title-agent", "global name must stay top-level");
    }

    #[test]
    fn agent_frontmatter_title_agent_resolves_package_relative() {
        // A bare title_agent in an agent's own frontmatter refers to a peer in
        // the same package, so it resolves package-relative.
        let (name, resolved) = resolve_title_agent_name(
            Some("peer"), // active agent frontmatter
            None,
            Some("pkg"), // active package
        )
        .expect("frontmatter title agent should resolve");
        assert_eq!(name, "peer");
        assert_eq!(resolved, "pkg/peer");
    }

    #[test]
    fn agent_frontmatter_takes_precedence_over_global() {
        // When both are set, the active agent's own frontmatter wins.
        let (name, resolved) =
            resolve_title_agent_name(Some("peer"), Some("title-agent"), Some("pkg")).unwrap();
        assert_eq!(name, "peer");
        assert_eq!(resolved, "pkg/peer");
    }

    #[test]
    fn explicitly_qualified_global_title_agent_is_preserved() {
        // An explicitly qualified global name keeps its qualification and is not
        // re-prefixed with the active package.
        let (name, resolved) =
            resolve_title_agent_name(None, Some("otherpkg/foo"), Some("pkg")).unwrap();
        assert_eq!(name, "otherpkg/foo");
        assert_eq!(resolved, "otherpkg/foo");
    }

    #[test]
    fn leading_slash_global_title_agent_escapes_to_top_level() {
        let (name, resolved) = resolve_title_agent_name(None, Some("/foo"), Some("pkg")).unwrap();
        assert_eq!(name, "/foo");
        assert_eq!(resolved, "foo");
    }

    #[test]
    fn no_title_agent_configured_returns_none() {
        assert!(resolve_title_agent_name(None, None, Some("pkg")).is_none());
        assert!(resolve_title_agent_name(None, None, None).is_none());
    }

    #[test]
    fn top_level_active_agent_keeps_global_name_bare() {
        // No active package → bare global name stays bare.
        let (name, resolved) = resolve_title_agent_name(None, Some("title-agent"), None).unwrap();
        assert_eq!(name, "title-agent");
        assert_eq!(resolved, "title-agent");
    }

    // --- fallback lookup gating (#103 follow-up) ---

    #[test]
    fn lookup_name_falls_back_to_bare_when_package_agent_file_missing() {
        // Package prefix applied AND the package file doesn't exist → fall back
        // to the bare top-level name.
        assert_eq!(
            title_agent_lookup_name("title-agent", "pantheon/title-agent", false),
            "title-agent"
        );
    }

    #[test]
    fn lookup_name_strips_leading_slash_when_falling_back() {
        // A global `/foo` resolves to file `foo`. When that file is missing, the
        // fallback must return the stripped `foo`, never the literal `/foo`
        // (which no agent file would match).
        assert_eq!(title_agent_lookup_name("/foo", "foo", false), "foo");
        // When the file exists, use the resolved (stripped) name as-is.
        assert_eq!(title_agent_lookup_name("/foo", "foo", true), "foo");
    }

    #[test]
    fn lookup_name_keeps_package_name_when_package_agent_file_exists() {
        // Package prefix applied and the package file EXISTS → use the package
        // name so a broken package file surfaces its own error (not masked).
        assert_eq!(
            title_agent_lookup_name("title-agent", "pantheon/title-agent", true),
            "pantheon/title-agent"
        );
    }

    #[test]
    fn lookup_name_uses_resolved_when_no_package_prefix_applied() {
        // No package prefix (resolved == name) → never falls back, regardless
        // of the existence flag (short-circuits before the file check).
        assert_eq!(
            title_agent_lookup_name("title-agent", "title-agent", false),
            "title-agent"
        );
        assert_eq!(
            title_agent_lookup_name("title-agent", "title-agent", true),
            "title-agent"
        );
    }

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

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

    #[tokio::test]
    async fn title_failure_emits_full_error_after_isolated_generation_events() {
        let sink = Arc::new(CollectingSink::default());

        harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            let result = harnx_core::sink::with_agent_event_sink(
                Arc::new(harnx_core::event::NullSink),
                async {
                    harnx_core::sink::emit_agent_event(AgentEvent::Notice(NoticeEvent::Info(
                        "title-agent output".to_string(),
                    )));
                    Err(anyhow::Error::msg("Miss 'api_key'")
                        .context("Failed to call chat-completions api"))
                },
            )
            .await;

            handle_title_result(result);
        })
        .await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "title-agent events must stay isolated");
        match &events[0] {
            AgentEvent::Session(SessionEvent::TitleGenerationFailed(error)) => {
                assert_eq!(error, "Failed to call chat-completions api: Miss 'api_key'");
            }
            other => panic!("unexpected event: {other:?}"),
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

    // --- TitleAgentResolution error cases ---

    #[test]
    fn resolution_not_configured_when_no_title_agent_set() {
        // Neither agent frontmatter nor global config has title_agent
        let resolution = resolve_title_agent_name(
            None, // agent frontmatter
            None, // global config
            None, // active package
        );
        assert!(
            resolution.is_none(),
            "expected None when no title agent configured"
        );
    }

    #[test]
    fn resolution_not_found_message_includes_agent_name() {
        use super::*;
        // When a title agent name is configured but doesn't exist,
        // the error message should include the name
        let resolution = TitleAgentResolution::NotFound {
            name: "missing-title-agent".to_string(),
        };
        let result: Result<Option<_>> = resolution.into_result();
        let err = result.expect_err("should be error for NotFound");
        // The error message must contain both "not found" and the agent name
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not found"),
            "error message should contain 'not found': {msg}"
        );
        assert!(
            msg.contains("missing-title-agent"),
            "error message should contain agent name: {msg}"
        );
    }

    #[test]
    fn resolution_load_error_surfaces_underlying_error() {
        use super::*;
        // When a title agent exists but fails to load, the underlying error
        // should be surfaced with the agent name in context
        let underlying = anyhow::anyhow!("parse error: invalid frontmatter");
        let resolution = TitleAgentResolution::LoadError {
            name: "malformed-title-agent".to_string(),
            source: underlying,
        };
        let result: Result<Option<_>> = resolution.into_result();
        let err = result.expect_err("should be error for LoadError");
        let msg = format!("{err:#}");
        // The error message must include both the agent name and the underlying error
        assert!(
            msg.contains("malformed-title-agent"),
            "error should contain agent name: {msg}"
        );
        assert!(
            msg.contains("parse error"),
            "error should contain underlying error: {msg}"
        );
    }

    #[test]
    fn resolution_not_configured_is_ok_none() {
        use super::*;
        let resolution = TitleAgentResolution::NotConfigured;
        let result: Result<Option<_>> = resolution.into_result();
        assert!(result.is_ok(), "NotConfigured should resolve to Ok(None)");
        assert!(
            result.unwrap().is_none(),
            "NotConfigured should resolve to Ok(None)"
        );
    }

    #[test]
    fn background_titling_only_skips_when_not_configured() {
        use super::*;
        // The automatic/background guard in `maybe_generate_title` must skip
        // ONLY when no title agent is configured. A configured-but-broken agent
        // (NotFound / LoadError) must NOT be skipped — it has to fall through to
        // `generate_title` so `TitleGenerationFailed` is surfaced (#103 / CR).
        let skips = |r: &TitleAgentResolution| matches!(r, TitleAgentResolution::NotConfigured);

        assert!(skips(&TitleAgentResolution::NotConfigured));
        assert!(!skips(&TitleAgentResolution::NotFound {
            name: "title-agent".to_string(),
        }));
        assert!(!skips(&TitleAgentResolution::LoadError {
            name: "title-agent".to_string(),
            source: anyhow::anyhow!("bad yaml"),
        }));
    }

    #[test]
    fn post_process_caps_length() {
        let long = "word ".repeat(200);
        let out = post_process_title(&long);
        assert!(out.chars().count() <= MAX_TITLE_CHARS + 1); // +1 for the ellipsis
    }
}
