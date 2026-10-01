use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use harnx_runtime::config::SessionMeta;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ERROR_STATUS_MARKER;

pub(crate) const CURSOR_VERSION: u32 = 1;
pub(crate) const MAX_CURSOR_LEN: usize = 1024;
pub(crate) const DEFAULT_CURSOR_LIMIT: usize = 50;
pub(crate) const MAX_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorTimestamp {
    pub(crate) secs: i64,
    pub(crate) nanos: u32,
}

impl From<SystemTime> for CursorTimestamp {
    fn from(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(d) => CursorTimestamp {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(e) => {
                let d = e.duration();
                let secs = -(d.as_secs() as i64);
                let nanos = d.subsec_nanos();
                if nanos == 0 {
                    CursorTimestamp { secs, nanos: 0 }
                } else {
                    CursorTimestamp {
                        secs: secs - 1,
                        nanos: 1_000_000_000 - nanos,
                    }
                }
            }
        }
    }
}

impl TryFrom<CursorTimestamp> for SystemTime {
    type Error = &'static str;
    fn try_from(ts: CursorTimestamp) -> Result<Self, Self::Error> {
        if ts.nanos >= 1_000_000_000 {
            return Err("cursor timestamp nanoseconds out of range");
        }
        if ts.secs >= 0 {
            UNIX_EPOCH
                .checked_add(Duration::new(ts.secs as u64, ts.nanos))
                .ok_or("cursor timestamp overflow")
        } else {
            let abs_secs = ts.secs.unsigned_abs();
            let base = UNIX_EPOCH
                .checked_sub(Duration::from_secs(abs_secs))
                .ok_or("cursor timestamp underflow")?;
            base.checked_add(Duration::from_nanos(ts.nanos as u64))
                .ok_or("cursor timestamp overflow")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionCursor {
    pub(crate) v: u32,
    pub(crate) id: String,
    pub(crate) modified: Option<CursorTimestamp>,
}

pub(crate) fn encode_cursor(session: &SessionMeta) -> Result<String> {
    let cursor = SessionCursor {
        v: CURSOR_VERSION,
        id: session.id.clone(),
        modified: session.modified.map(CursorTimestamp::from),
    };
    let json_bytes = serde_json::to_vec(&cursor)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json_bytes))
}

pub(crate) fn decode_cursor(raw: &str) -> Result<(SessionCursor, Option<SystemTime>)> {
    if raw.is_empty() {
        bail!("cursor parameter must not be empty{ERROR_STATUS_MARKER}400");
    }
    if raw.len() > MAX_CURSOR_LEN {
        bail!("cursor parameter exceeds maximum allowed length{ERROR_STATUS_MARKER}400");
    }
    let trimmed = raw.trim_end_matches('=');
    let json_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .map_err(|_| anyhow!("invalid cursor encoding{ERROR_STATUS_MARKER}400"))?;

    let cursor: SessionCursor = serde_json::from_slice(&json_bytes)
        .map_err(|_| anyhow!("invalid cursor payload{ERROR_STATUS_MARKER}400"))?;

    if cursor.v != CURSOR_VERSION {
        bail!(
            "unsupported cursor version: {}{ERROR_STATUS_MARKER}400",
            cursor.v
        );
    }
    if cursor.id.is_empty() {
        bail!("invalid cursor: id must not be empty{ERROR_STATUS_MARKER}400");
    }

    let modified = match cursor.modified {
        Some(ts) => {
            Some(SystemTime::try_from(ts).map_err(|msg| anyhow!("{msg}{ERROR_STATUS_MARKER}400"))?)
        }
        None => None,
    };

    Ok((cursor, modified))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionPaginationQuery {
    pub(crate) limit: usize,
    pub(crate) cursor: Option<SessionCursor>,
    pub(crate) cursor_modified: Option<SystemTime>,
}

#[derive(Default)]
struct SessionPaginationParams {
    limit: Option<usize>,
    cursor: Option<(SessionCursor, Option<SystemTime>)>,
}

impl SessionPaginationParams {
    fn parse_parameter(&mut self, part: &str) -> Result<()> {
        let (raw_key, raw_val) = part.split_once('=').unwrap_or((part, ""));
        let key = crate::percent_decode(raw_key);
        let val = crate::percent_decode(raw_val);

        match key.as_str() {
            "limit" => parse_unique_parameter(&mut self.limit, &key, &val, parse_session_limit),
            "cursor" => parse_unique_parameter(&mut self.cursor, &key, &val, decode_cursor),
            _ => Ok(()),
        }
    }

    fn into_query(self) -> Option<SessionPaginationQuery> {
        if self.limit.is_none() && self.cursor.is_none() {
            return None;
        }
        let (cursor, cursor_modified) = match self.cursor {
            Some((cursor, modified)) => (Some(cursor), modified),
            None => (None, None),
        };
        Some(SessionPaginationQuery {
            limit: self.limit.unwrap_or(DEFAULT_CURSOR_LIMIT),
            cursor,
            cursor_modified,
        })
    }
}

fn parse_unique_parameter<T>(
    slot: &mut Option<T>,
    key: &str,
    value: &str,
    parse: impl FnOnce(&str) -> Result<T>,
) -> Result<()> {
    if slot.is_some() {
        bail!("duplicate query parameter '{key}'{ERROR_STATUS_MARKER}400");
    }
    *slot = Some(parse(value)?);
    Ok(())
}

fn parse_session_limit(value: &str) -> Result<usize> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| anyhow!("limit must be a positive integer{ERROR_STATUS_MARKER}400"))?;
    if parsed == 0 {
        bail!("limit must be a positive integer{ERROR_STATUS_MARKER}400");
    }
    Ok(parsed.min(MAX_LIMIT as u64) as usize)
}

pub(crate) fn parse_session_list_query(
    query: Option<&str>,
) -> Result<Option<SessionPaginationQuery>> {
    let mut params = SessionPaginationParams::default();
    for part in query
        .unwrap_or_default()
        .split('&')
        .filter(|part| !part.is_empty())
    {
        params.parse_parameter(part)?;
    }
    Ok(params.into_query())
}

pub(crate) fn session_meta_comes_after(
    cursor_id: &str,
    cursor_modified: Option<SystemTime>,
    session: &SessionMeta,
) -> bool {
    session
        .modified
        .cmp(&cursor_modified)
        .then_with(|| session.id.as_str().cmp(cursor_id))
        == std::cmp::Ordering::Less
}

pub(crate) fn paginate_sessions(
    sessions: &[SessionMeta],
    query: &SessionPaginationQuery,
) -> Result<serde_json::Value> {
    let filtered_iter = sessions.iter().filter(|s| {
        if let Some(cursor) = &query.cursor {
            session_meta_comes_after(&cursor.id, query.cursor_modified, s)
        } else {
            true
        }
    });

    let mut page: Vec<&SessionMeta> = filtered_iter.take(query.limit + 1).collect();

    let next_cursor = if page.len() > query.limit {
        page.truncate(query.limit);
        let last = page.last().expect("page has elements");
        Some(encode_cursor(last)?)
    } else {
        None
    };

    let session_values: Vec<serde_json::Value> = page
        .into_iter()
        .map(crate::format_session_summary)
        .collect();

    Ok(json!({
        "sessions": session_values,
        "next_cursor": next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(id: &str, modified: Option<SystemTime>) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            session_id: Some(id.into()),
            agent_name: Some("test-agent".into()),
            title: Some(format!("Title {id}")),
            modified,
            contexts: vec![],
            unread: false,
        }
    }

    #[test]
    fn cursor_timestamp_exact_nanosecond_preservation() {
        // Windows SystemTime uses 100ns ticks; this sub-millisecond value is exact there too.
        let base = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        let ts = CursorTimestamp::from(base);
        assert_eq!(ts.secs, 1_700_000_000);
        assert_eq!(ts.nanos, 123_456_700);

        let restored = SystemTime::try_from(ts).expect("restore SystemTime");
        assert_eq!(restored, base);

        // Before epoch
        let before_epoch = UNIX_EPOCH - Duration::new(10, 500_000_000);
        let ts_before = CursorTimestamp::from(before_epoch);
        let restored_before = SystemTime::try_from(ts_before).expect("restore before epoch");
        assert_eq!(restored_before, before_epoch);
    }

    #[test]
    fn cursor_encode_decode_round_trip() {
        let session = test_session(
            "sess-123",
            Some(UNIX_EPOCH + Duration::new(1_720_000_000, 987_654_321)),
        );
        let encoded = encode_cursor(&session).expect("encode cursor");
        let (cursor, modified) = decode_cursor(&encoded).expect("decode cursor");

        assert_eq!(cursor.v, CURSOR_VERSION);
        assert_eq!(cursor.id, "sess-123");
        let actual = session
            .modified
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        // Check every input nanosecond, including platforms finer than Windows' 100ns ticks.
        assert_eq!(
            cursor.modified,
            Some(CursorTimestamp {
                secs: actual.as_secs() as i64,
                nanos: actual.subsec_nanos(),
            })
        );
        assert_eq!(modified, session.modified);

        // Session with modified: None
        let session_none = test_session("sess-none", None);
        let encoded_none = encode_cursor(&session_none).expect("encode cursor none");
        let (cursor_none, modified_none) =
            decode_cursor(&encoded_none).expect("decode cursor none");
        assert_eq!(cursor_none.v, CURSOR_VERSION);
        assert_eq!(cursor_none.id, "sess-none");
        assert_eq!(modified_none, None);
    }

    #[test]
    fn cursor_strict_validation_and_bounds() {
        // Empty string
        assert!(decode_cursor("").is_err());

        // Exceeds max length
        let huge = "a".repeat(MAX_CURSOR_LEN + 1);
        assert!(decode_cursor(&huge).is_err());

        // Invalid base64
        assert!(decode_cursor("not-valid-base64!@#$").is_err());

        // Invalid JSON
        let invalid_json = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not json");
        assert!(decode_cursor(&invalid_json).is_err());

        // Unsupported version
        let v2_json = serde_json::to_vec(&json!({
            "v": 2,
            "id": "sess-1",
            "modified": null
        }))
        .unwrap();
        let v2_encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v2_json);
        let err = decode_cursor(&v2_encoded).unwrap_err();
        assert!(err.to_string().contains("unsupported cursor version: 2"));

        // Empty session ID
        let empty_id = serde_json::to_vec(&json!({
            "v": 1,
            "id": "",
            "modified": null
        }))
        .unwrap();
        let empty_id_encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(empty_id);
        assert!(decode_cursor(&empty_id_encoded).is_err());

        // Out of range nanoseconds
        let bad_nanos = serde_json::to_vec(&json!({
            "v": 1,
            "id": "sess-1",
            "modified": { "secs": 100, "nanos": 1_000_000_000 }
        }))
        .unwrap();
        let bad_nanos_encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bad_nanos);
        assert!(decode_cursor(&bad_nanos_encoded).is_err());
    }

    #[test]
    fn parse_session_list_query_semantics() {
        // No query / empty query
        assert_eq!(parse_session_list_query(None).unwrap(), None);
        assert_eq!(parse_session_list_query(Some("")).unwrap(), None);

        // limit only (positive integer clamped to 200)
        let q1 = parse_session_list_query(Some("limit=25")).unwrap().unwrap();
        assert_eq!(q1.limit, 25);
        assert_eq!(q1.cursor, None);

        let q_clamp = parse_session_list_query(Some("limit=500"))
            .unwrap()
            .unwrap();
        assert_eq!(q_clamp.limit, 200);

        // cursor only defaults limit to 50
        let s = test_session("sess-1", Some(UNIX_EPOCH + Duration::from_secs(100)));
        let cursor_str = encode_cursor(&s).unwrap();
        let q_cursor = parse_session_list_query(Some(&format!("cursor={cursor_str}")))
            .unwrap()
            .unwrap();
        assert_eq!(q_cursor.limit, DEFAULT_CURSOR_LIMIT);
        assert_eq!(q_cursor.cursor.unwrap().id, "sess-1");

        // limit and cursor combined
        let q_both = parse_session_list_query(Some(&format!("limit=15&cursor={cursor_str}")))
            .unwrap()
            .unwrap();
        assert_eq!(q_both.limit, 15);
        assert_eq!(q_both.cursor.unwrap().id, "sess-1");

        // Invalid limit values: 0, negative, non-integer
        assert!(parse_session_list_query(Some("limit=0")).is_err());
        assert!(parse_session_list_query(Some("limit=-5")).is_err());
        assert!(parse_session_list_query(Some("limit=abc")).is_err());
        assert!(parse_session_list_query(Some("limit=")).is_err());

        // Duplicate parameters
        assert!(parse_session_list_query(Some("limit=10&limit=20")).is_err());
        assert!(parse_session_list_query(Some(&format!(
            "cursor={cursor_str}&cursor={cursor_str}"
        )))
        .is_err());

        // Unrelated parameters retain the legacy full-array behavior.
        assert_eq!(parse_session_list_query(Some("offset=10")).unwrap(), None);
        assert_eq!(parse_session_list_query(Some("foo=bar")).unwrap(), None);

        // Malformed parameter missing '='
        assert!(parse_session_list_query(Some("limit")).is_err());
    }

    #[test]
    fn parse_query_decodes_parameters_and_ignores_empty_or_unrelated_parts() {
        assert_eq!(
            parse_session_list_query(Some("&&foo&foo=bar&&")).unwrap(),
            None
        );
        let parsed = parse_session_list_query(Some("&foo=bar&%6cimit=%32%35&&"))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.limit, 25);
        assert_eq!(parsed.cursor, None);
        assert_eq!(parsed.cursor_modified, None);

        let clamped = parse_session_list_query(Some("limit=18446744073709551615"))
            .unwrap()
            .unwrap();
        assert_eq!(clamped.limit, MAX_LIMIT);
    }

    #[test]
    fn parse_query_preserves_validation_errors_and_parameter_order() {
        let positive_integer = "limit must be a positive integer";
        let duplicate_limit = "duplicate query parameter 'limit'";
        let invalid_cursor = "invalid cursor encoding";
        for (query, message) in [
            ("limit=18446744073709551616", positive_integer),
            ("limit=1=2", positive_integer),
            ("limit=10&%6cimit=0", duplicate_limit),
            ("limit=0&limit=10", positive_integer),
            ("limit=10&limit=", duplicate_limit),
            ("cursor", "cursor parameter must not be empty"),
            ("cursor=!&limit=0", invalid_cursor),
            ("limit=0&cursor=!", positive_integer),
        ] {
            let err = parse_session_list_query(Some(query)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("{message}{ERROR_STATUS_MARKER}400"),
                "{query}"
            );
        }

        let cursor = encode_cursor(&test_session("sess-1", None)).unwrap();
        let err =
            parse_session_list_query(Some(&format!("cursor={cursor}&%63ursor=!"))).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("duplicate query parameter 'cursor'{ERROR_STATUS_MARKER}400")
        );
    }

    #[test]
    fn paginate_sessions_first_follow_end_flow() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s1 = test_session("s1", Some(base + Duration::from_secs(50)));
        let s2 = test_session("s2", Some(base + Duration::from_secs(40)));
        let s3 = test_session("s3", Some(base + Duration::from_secs(30)));
        let s4 = test_session("s4", Some(base + Duration::from_secs(20)));
        let s5 = test_session("s5", Some(base + Duration::from_secs(10)));

        let mut sessions = vec![s4.clone(), s1.clone(), s5.clone(), s2.clone(), s3.clone()];
        sessions.sort_by(crate::session_recency_ordering);

        // Page 1: limit = 2
        let q1 = parse_session_list_query(Some("limit=2")).unwrap().unwrap();
        let page1 = paginate_sessions(&sessions, &q1).unwrap();
        let items1 = page1["sessions"].as_array().unwrap();
        assert_eq!(items1.len(), 2);
        assert_eq!(items1[0]["session_id"], "s1");
        assert_eq!(items1[1]["session_id"], "s2");
        let cursor1 = page1["next_cursor"].as_str().expect("cursor on page 1");

        // Page 2: cursor = cursor1, limit = 2
        let q2 = parse_session_list_query(Some(&format!("limit=2&cursor={cursor1}")))
            .unwrap()
            .unwrap();
        let page2 = paginate_sessions(&sessions, &q2).unwrap();
        let items2 = page2["sessions"].as_array().unwrap();
        assert_eq!(items2.len(), 2);
        assert_eq!(items2[0]["session_id"], "s3");
        assert_eq!(items2[1]["session_id"], "s4");
        let cursor2 = page2["next_cursor"].as_str().expect("cursor on page 2");

        // Page 3: cursor = cursor2, limit = 2
        let q3 = parse_session_list_query(Some(&format!("limit=2&cursor={cursor2}")))
            .unwrap()
            .unwrap();
        let page3 = paginate_sessions(&sessions, &q3).unwrap();
        let items3 = page3["sessions"].as_array().unwrap();
        assert_eq!(items3.len(), 1);
        assert_eq!(items3[0]["session_id"], "s5");
        assert!(
            page3["next_cursor"].is_null(),
            "end of list must have null next_cursor"
        );

        // Page 4: cursor from s5
        let cursor3 = encode_cursor(&s5).unwrap();
        let q4 = parse_session_list_query(Some(&format!("limit=2&cursor={cursor3}")))
            .unwrap()
            .unwrap();
        let page4 = paginate_sessions(&sessions, &q4).unwrap();
        let items4 = page4["sessions"].as_array().unwrap();
        assert_eq!(items4.len(), 0);
        assert!(page4["next_cursor"].is_null());
    }

    #[test]
    fn paginate_sessions_ties_and_missing_modified_sorting() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s_time_b = test_session("sess-b", Some(base));
        let s_time_a = test_session("sess-a", Some(base));
        let s_none_b = test_session("none-b", None);
        let s_none_a = test_session("none-a", None);

        let mut sessions = vec![
            s_none_a.clone(),
            s_time_a.clone(),
            s_none_b.clone(),
            s_time_b.clone(),
        ];
        sessions.sort_by(crate::session_recency_ordering);

        // Expected sorted order: sess-b, sess-a, none-b, none-a
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["sess-b", "sess-a", "none-b", "none-a"]);

        // Page 1: limit 2
        let q1 = parse_session_list_query(Some("limit=2")).unwrap().unwrap();
        let page1 = paginate_sessions(&sessions, &q1).unwrap();
        let items1 = page1["sessions"].as_array().unwrap();
        assert_eq!(items1[0]["session_id"], "sess-b");
        assert_eq!(items1[1]["session_id"], "sess-a");
        let cursor1 = page1["next_cursor"].as_str().unwrap();

        // Page 2: follow cursor across timestamp-to-none boundary
        let q2 = parse_session_list_query(Some(&format!("limit=2&cursor={cursor1}")))
            .unwrap()
            .unwrap();
        let page2 = paginate_sessions(&sessions, &q2).unwrap();
        let items2 = page2["sessions"].as_array().unwrap();
        assert_eq!(items2[0]["session_id"], "none-b");
        assert_eq!(items2[1]["session_id"], "none-a");
        assert!(page2["next_cursor"].is_null());
    }

    #[test]
    fn paginate_sessions_robust_insert_delete_stability() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s1 = test_session("s1", Some(base + Duration::from_secs(30)));
        let s2 = test_session("s2", Some(base + Duration::from_secs(20)));
        let s3 = test_session("s3", Some(base + Duration::from_secs(10)));

        let mut sessions = vec![s1.clone(), s2.clone(), s3.clone()];
        sessions.sort_by(crate::session_recency_ordering);

        // Fetch page 1 (gets s1, cursor pointing to s1)
        let q1 = parse_session_list_query(Some("limit=1")).unwrap().unwrap();
        let page1 = paginate_sessions(&sessions, &q1).unwrap();
        let cursor1 = page1["next_cursor"].as_str().unwrap();

        // Simulate deleting s1 before fetching page 2
        let sessions_after_delete = vec![s2.clone(), s3.clone()];
        let q2 = parse_session_list_query(Some(&format!("limit=2&cursor={cursor1}")))
            .unwrap()
            .unwrap();
        let page2 = paginate_sessions(&sessions_after_delete, &q2).unwrap();
        let items2 = page2["sessions"].as_array().unwrap();
        // Page 2 correctly starts at s2 without error or skipping
        assert_eq!(items2.len(), 2);
        assert_eq!(items2[0]["session_id"], "s2");
        assert_eq!(items2[1]["session_id"], "s3");

        // Simulate inserting a newer item (s0 at t=40s) after page 1
        let s0 = test_session("s0", Some(base + Duration::from_secs(40)));
        let mut sessions_after_insert = vec![s0, s2.clone(), s3.clone()];
        sessions_after_insert.sort_by(crate::session_recency_ordering);
        let page2_insert = paginate_sessions(&sessions_after_insert, &q2).unwrap();
        let items2_insert = page2_insert["sessions"].as_array().unwrap();
        // Page 2 still starts at s2, s0 does not cause drift
        assert_eq!(items2_insert.len(), 2);
        assert_eq!(items2_insert[0]["session_id"], "s2");
        assert_eq!(items2_insert[1]["session_id"], "s3");
    }
}
