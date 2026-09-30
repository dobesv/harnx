//! Content-addressed URL model for attachments and plans.
//!
//! URLs carry agent name + local session id, while storage keys remain
//! `session_key(agent, sid)` (64-char hex). This module provides pure
//! parsing and formatting with no I/O.
//!
//! ## URL Grammar
//!
//! ```text
//! cid:<type>:<agent>/<session-id>/<rest>
//! ```
//!
//! - `<agent>` is percent-encoded (`pantheon/atlas` → `pantheon%2Fatlas`),
//!   same as the Web UI's `/v1/agents/{agent}` path segment.
//! - `_temp` (`TEMP_AGENT_NAME`) represents sessions without an agent.
//! - `<session-id>` is the 6-char local id `[A-Za-z0-9_-]+` (may start with `-`).
//! - Media: `cid:media:<agent>/<sid>/<sha256>` — immutable, cache forever.
//! - Plans: `cid:plan:<agent>/<sid>/<slug>` — mutable, validate caches with revision ETags.
//!
//! ## Storage Key
//!
//! The `owner()` method returns `session_key(agent, sid)` (64-char hex),
//! used for NATS keys and deletion. Storage keys never appear in URLs.

use std::fmt;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub use crate::agent_config::TEMP_AGENT_NAME;

/// Marker for temp/inline sessions in URL encoding.
const TEMP_AGENT_URL_SEGMENT: &str = "_temp";

/// Prefix for all cid URLs.
pub const CID_PREFIX: &str = "cid:";

/// Reference to a session's identity (agent + local session id).
///
/// This is the caller identity that tool servers receive to understand
/// who is invoking them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    /// Agent name, or None for temp/inline sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 6-char local session id (base64url, may start with `-`).
    pub session_id: String,
}

impl SessionRef {
    /// Create a new SessionRef.
    pub fn new(agent: Option<String>, session_id: String) -> Result<Self> {
        validate_session_id(session_id.as_bytes())?;
        if let Some(ref a) = agent {
            validate_agent_name(a.as_bytes())?;
        }
        Ok(Self { agent, session_id })
    }

    /// Get the storage key (64-char hex) for this session.
    pub fn owner(&self) -> String {
        crate::session_identity::session_key(self.agent.as_deref(), &self.session_id)
    }
}

/// Item within a plan URL path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanItem {
    /// The plan's index/root.
    Index,
    /// A specific task.
    Task(String),
    /// A specific note.
    Note(String),
}

impl PlanItem {
    /// Check if this item can be parsed from the given segments.
    fn from_segments(segments: &[&str]) -> Result<Self> {
        match segments {
            [] => Ok(PlanItem::Index),
            ["tasks", id] => {
                validate_slug(id.as_bytes())?;
                Ok(PlanItem::Task(id.to_string()))
            }
            ["notes", id] => {
                validate_slug(id.as_bytes())?;
                Ok(PlanItem::Note(id.to_string()))
            }
            _ => bail!("invalid plan item path: {}", segments.join("/")),
        }
    }
}

/// Parsed cid: URL with type-specific payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidUrl {
    /// Immutable media attachment.
    Media {
        session: SessionRef,
        /// SHA-256 hash (64 hex chars).
        hash: String,
    },
    /// Mutable plan document.
    Plan {
        session: SessionRef,
        /// URL-safe slug `[a-z0-9-]+`.
        slug: String,
        /// Item within the plan (index, task, or note).
        item: PlanItem,
    },
}

impl CidUrl {
    /// Parse a cid: URL string.
    ///
    /// # Errors
    ///
    /// Returns an error for:
    /// - Invalid URL format
    /// - Bad characters in agent/session/hash/slug
    /// - Old bare `cid:<sha256>` format
    pub fn parse(s: &str) -> Result<Self> {
        let Some(rest) = s.strip_prefix(CID_PREFIX) else {
            bail!("cid URL must start with 'cid:'");
        };

        // Split type from the rest: "media:..." or "plan:..."
        let (type_str, rest) = rest
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("cid URL missing type segment"))?;

        match type_str {
            "media" => Self::parse_media(rest),
            "plan" => Self::parse_plan(rest),
            other => bail!("unknown cid URL type: '{}", other),
        }
    }

    fn parse_media(rest: &str) -> Result<Self> {
        // Format: <agent>/<session-id>/<hash>
        let (agent_encoded, session_and_hash) = rest
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("media URL missing agent segment"))?;
        let (session_id, hash) = session_and_hash
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("media URL missing session-id segment"))?;

        validate_session_id(session_id.as_bytes())?;
        validate_hash(hash.as_bytes())?;

        let agent = decode_agent_segment(agent_encoded)?;
        let session = SessionRef {
            agent,
            session_id: session_id.to_string(),
        };

        Ok(CidUrl::Media {
            session,
            hash: hash.to_string(),
        })
    }

    fn parse_plan(rest: &str) -> Result<Self> {
        // Format: <agent>/<session-id>/<slug>[/<item-path>]
        let (agent_encoded, session_and_slug) = rest
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("plan URL missing agent segment"))?;
        let (session_id, slug_and_item) = session_and_slug
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("plan URL missing session-id segment"))?;

        validate_session_id(session_id.as_bytes())?;

        // Split slug from optional item path
        let (slug, item) = if let Some((slug_str, item_path)) = slug_and_item.split_once('/') {
            let item_segments: Vec<&str> = item_path.split('/').collect();
            (slug_str, PlanItem::from_segments(&item_segments)?)
        } else {
            (slug_and_item, PlanItem::Index)
        };

        validate_slug(slug.as_bytes())?;

        let agent = decode_agent_segment(agent_encoded)?;
        let session = SessionRef {
            agent,
            session_id: session_id.to_string(),
        };

        Ok(CidUrl::Plan {
            session,
            slug: slug.to_string(),
            item,
        })
    }

    /// Get the session reference for this URL.
    pub fn session(&self) -> &SessionRef {
        match self {
            CidUrl::Media { session, .. } => session,
            CidUrl::Plan { session, .. } => session,
        }
    }

    /// Get the storage owner key (64-char hex).
    pub fn owner(&self) -> String {
        self.session().owner()
    }

    /// Get the KV/object-store key for this URL.
    ///
    /// Media: `media/<owner>/<hash>`
    /// Plan: `plan/<owner>/<slug>/plan`, `plan/<owner>/<slug>/tasks/<id>`, `plan/<owner>/<slug>/notes/<id>`
    pub fn kv_key(&self) -> String {
        match self {
            CidUrl::Media { session, hash } => {
                format!("media/{}/{}", session.owner(), hash)
            }
            CidUrl::Plan {
                session,
                slug,
                item,
            } => {
                let base = format!("plan/{}/{}", session.owner(), slug);
                match item {
                    PlanItem::Index => format!("{}/plan", base),
                    PlanItem::Task(id) => format!("{}/tasks/{}", base, id),
                    PlanItem::Note(id) => format!("{}/notes/{}", base, id),
                }
            }
        }
    }

    /// Is this URL pointing to immutable content?
    ///
    /// Media is immutable; plans are mutable.
    pub fn is_immutable(&self) -> bool {
        matches!(self, CidUrl::Media { .. })
    }
}

impl fmt::Display for CidUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CidUrl::Media { session, hash } => {
                write!(
                    f,
                    "cid:media:{}/{}",
                    encode_agent_segment(&session.agent),
                    session.session_id
                )?;
                write!(f, "/{}", hash)
            }
            CidUrl::Plan {
                session,
                slug,
                item,
            } => {
                write!(
                    f,
                    "cid:plan:{}/{}",
                    encode_agent_segment(&session.agent),
                    session.session_id
                )?;
                write!(f, "/{}", slug)?;
                match item {
                    PlanItem::Index => {}
                    PlanItem::Task(id) => write!(f, "/tasks/{}", id)?,
                    PlanItem::Note(id) => write!(f, "/notes/{}", id)?,
                }
                Ok(())
            }
        }
    }
}

/// Encode the agent name for use in a URL path segment.
fn encode_agent_segment(agent: &Option<String>) -> String {
    match agent {
        None => TEMP_AGENT_URL_SEGMENT.to_string(),
        Some(name) => urlencoding::encode(name).to_string(),
    }
}

/// Decode and validate an agent name from a URL path segment.
fn decode_agent_segment(segment: &str) -> Result<Option<String>> {
    if segment == TEMP_AGENT_URL_SEGMENT {
        return Ok(None);
    }

    let decoded = urlencoding::decode(segment)
        .map_err(|e| anyhow::anyhow!("invalid percent-encoding in agent segment: {}", e))?;

    let bytes = decoded.as_bytes();
    validate_agent_name(bytes)?;
    Ok(Some(decoded.into_owned()))
}

fn is_session_id_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_')
}

fn is_slug_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-')
}

fn is_agent_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'/')
}

/// Validate a session id: `[A-Za-z0-9_-]+`, 6+ chars, may start with `-`.
fn validate_session_id(id: &[u8]) -> Result<()> {
    if id.len() < 6 {
        bail!(
            "session id too short (min 6 chars): '{}'",
            String::from_utf8_lossy(id)
        );
    }
    if !id.iter().copied().all(is_session_id_byte) {
        bail!(
            "session id contains invalid characters (allowed: A-Za-z0-9_-): '{}'",
            String::from_utf8_lossy(id)
        );
    }
    Ok(())
}

/// Validate a SHA-256 hash: 64 lowercase hex chars.
fn validate_hash(hash: &[u8]) -> Result<()> {
    if hash.len() != 64 {
        bail!("hash must be 64 hex characters, got {} chars", hash.len());
    }
    if !hash
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        bail!("hash contains invalid characters (allowed: 0-9, a-f)");
    }
    Ok(())
}

/// Validate a slug: `[a-z0-9-]+`.
fn validate_slug(slug: &[u8]) -> Result<()> {
    if slug.is_empty() {
        bail!("slug cannot be empty");
    }
    if !slug.iter().copied().all(is_slug_byte) {
        bail!(
            "slug contains invalid characters (allowed: a-z, 0-9, -): '{}'",
            String::from_utf8_lossy(slug)
        );
    }
    Ok(())
}

/// Validate an agent name.
///
/// Agent names allow `[A-Za-z0-9_/-]` and must not be empty.
/// Forbidden: `:`, `@`, `.`, spaces.
///
/// Note: `/` is allowed for package-qualified agents like `pantheon/atlas`.
fn validate_agent_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        bail!("agent name cannot be empty");
    }
    if name == TEMP_AGENT_NAME.as_bytes() {
        bail!("agent name '{}' is reserved", TEMP_AGENT_NAME);
    }
    if name.iter().copied().any(|byte| !is_agent_byte(byte)) {
        let name = String::from_utf8_lossy(name);
        let ch = name
            .chars()
            .find(|&ch| !ch.is_ascii() || !is_agent_byte(ch as u8))
            .expect("invalid agent name byte has a corresponding character");
        bail!(
            "agent name '{}' contains forbidden character '{}' (allowed: A-Za-z0-9, -, _, /)",
            name,
            ch
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_media_url_with_agent() {
        let url = CidUrl::parse("cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
        match url {
            CidUrl::Media { session, hash } => {
                assert_eq!(session.agent, Some("pantheon/atlas".to_string()));
                assert_eq!(session.session_id, "armDRA");
                assert_eq!(
                    hash,
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                );
            }
            _ => panic!("expected media URL"),
        }
    }

    const TEST_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct MediaParseCase {
        agent_segment: &'static str,
        expected_agent: Option<&'static str>,
    }

    fn assert_media_session(case: &MediaParseCase) {
        let input = format!("cid:media:{}/armDRA/{TEST_HASH}", case.agent_segment);
        let CidUrl::Media { session, hash } = CidUrl::parse(&input).unwrap() else {
            panic!("expected media URL");
        };
        assert_eq!(session.agent.as_deref(), case.expected_agent);
        assert_eq!(session.session_id, "armDRA");
        assert_eq!(hash, TEST_HASH);
    }

    #[test]
    fn parse_media_agent_segments() {
        let cases = [
            MediaParseCase {
                agent_segment: "pantheon%2Fatlas",
                expected_agent: Some("pantheon/atlas"),
            },
            MediaParseCase {
                agent_segment: "atlas",
                expected_agent: Some("atlas"),
            },
            MediaParseCase {
                agent_segment: "_temp",
                expected_agent: None,
            },
        ];
        for case in &cases {
            assert_media_session(case);
        }
    }

    #[test]
    fn parse_plan_url_index() {
        let url = CidUrl::parse("cid:plan:pantheon%2Fatlas/armDRA/my-plan").unwrap();
        match &url {
            CidUrl::Plan {
                session,
                slug,
                item,
            } => {
                assert_eq!(session.agent, Some("pantheon/atlas".to_string()));
                assert_eq!(session.session_id, "armDRA");
                assert_eq!(slug, "my-plan");
                assert_eq!(item, &PlanItem::Index);
                assert_eq!(
                    url.kv_key(),
                    format!("plan/{}/my-plan/plan", session.owner())
                );
            }
            _ => panic!("expected plan URL"),
        }
    }

    #[test]
    fn parse_plan_url_task() {
        let url = CidUrl::parse("cid:plan:pantheon%2Fatlas/armDRA/my-plan/tasks/t01").unwrap();
        match &url {
            CidUrl::Plan {
                session,
                slug: _,
                item,
            } => {
                assert_eq!(item, &PlanItem::Task("t01".to_string()));
                assert_eq!(
                    url.kv_key(),
                    format!("plan/{}/my-plan/tasks/t01", session.owner())
                );
            }
            _ => panic!("expected plan URL"),
        }
    }

    #[test]
    fn parse_plan_url_note() {
        let url = CidUrl::parse("cid:plan:pantheon%2Fatlas/armDRA/my-plan/notes/note-1").unwrap();
        match url {
            CidUrl::Plan { item, .. } => {
                assert_eq!(item, PlanItem::Note("note-1".to_string()));
            }
            _ => panic!("expected plan URL"),
        }
    }

    #[test]
    fn round_trip_media() {
        let original = "cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = CidUrl::parse(original).unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn round_trip_plan() {
        let original = "cid:plan:pantheon%2Fatlas/armDRA/my-plan/tasks/t01";
        let url = CidUrl::parse(original).unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn round_trip_session_with_dash_prefix() {
        let original = "cid:media:_temp/-dash-XYZ/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = CidUrl::parse(original).unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn reject_old_bare_format() {
        let result =
            CidUrl::parse("cid:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(result.is_err(), "should reject bare hash format");
    }

    #[test]
    fn reject_bad_agent_charset() {
        let result = CidUrl::parse("cid:media:bad:agent/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        // This fails at decoding - percent-decoding "bad:agent" gives the literal string "bad:agent"
        // which contains a forbidden ':'
        assert!(result.is_err(), "should reject colon in agent name");
    }

    #[test]
    fn reject_short_session_id() {
        let result = CidUrl::parse("cid:media:_temp/short/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(result.is_err(), "should reject short session id");
    }

    #[test]
    fn reject_bad_hash_chars() {
        let result =
            CidUrl::parse("cid:media:_temp/armDRA/invalid-hash-not-hex-64-chars-total-00000");
        assert!(result.is_err(), "should reject non-hex hash");
    }

    #[test]
    fn reject_uppercase_hash() {
        // Hash must be lowercase
        let result = CidUrl::parse("cid:media:_temp/armDRA/0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF");
        assert!(result.is_err(), "should reject uppercase hash");
    }

    #[test]
    fn owner_key() {
        let url = CidUrl::parse("cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
        let owner = url.owner();
        assert_eq!(owner.len(), 64);
        assert!(owner
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn kv_key_media() {
        let url = CidUrl::parse("cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
        let key = url.kv_key();
        assert!(key.starts_with("media/"));
        assert!(key.contains("/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn kv_key_plan() {
        let url = CidUrl::parse("cid:plan:pantheon%2Fatlas/armDRA/my-plan/tasks/t01").unwrap();
        let key = url.kv_key();
        assert!(key.starts_with("plan/"));
        assert!(key.ends_with("/my-plan/tasks/t01"));
    }

    #[test]
    fn is_immutable() {
        let media_url = CidUrl::parse("cid:media:_temp/abcDEF/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
        assert!(media_url.is_immutable());

        let plan_url = CidUrl::parse("cid:plan:_temp/abcDEF/my-plan").unwrap();
        assert!(!plan_url.is_immutable());
    }

    #[test]
    fn reject_empty_agent() {
        let result = CidUrl::parse(
            "cid:media:/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        assert!(result.is_err());
    }
}
