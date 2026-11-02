use super::super::store::map_internal;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rmcp::model::ErrorData;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    scope: String,
    after: String,
}

pub(super) struct KeyPage {
    pub(super) keys: Vec<String>,
    pub(super) next_cursor: Option<String>,
}

impl KeyPage {
    /// Select from sorted keys before fetching bodies. Cursors use keys rather
    /// than offsets so deletion of a previous page doesn't skip the next item.
    pub(super) fn select(
        keys: impl IntoIterator<Item = String>,
        scope: &str,
        limit: Option<u64>,
        cursor: Option<&str>,
    ) -> Result<Self, ErrorData> {
        let after = cursor.map(|raw| decode_cursor(raw, scope)).transpose()?;
        let limit = limit.unwrap_or(20).clamp(1, 100) as usize;
        let mut keys: Vec<_> = keys
            .into_iter()
            .filter(|key| after.as_ref().is_none_or(|after| key > after))
            .take(limit + 1)
            .collect();
        let next_cursor =
            if keys.len() > limit {
                keys.truncate(limit);
                let cursor = Cursor {
                    scope: scope.to_string(),
                    after: keys[limit - 1].clone(),
                };
                Some(URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&cursor).map_err(|error| map_internal(error.into()))?,
                ))
            } else {
                None
            };
        Ok(Self { keys, next_cursor })
    }
}

fn decode_cursor(raw: &str, scope: &str) -> Result<String, ErrorData> {
    let cursor = URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cursor>(&bytes).ok())
        .filter(|cursor| cursor.scope == scope && cursor.after.starts_with(scope))
        .ok_or_else(|| {
            ErrorData::invalid_params(
                "invalid cursor or cursor belongs to a different list scope",
                None,
            )
        })?;
    Ok(cursor.after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> impl Iterator<Item = String> {
        (0..105).map(|index| format!("scope/{index:03}"))
    }

    #[test]
    fn page_limits_default_and_clamp() {
        for (limit, expected) in [
            (None, 20),
            (Some(0), 1),
            (Some(101), 100),
            (Some(u64::MAX), 100),
        ] {
            let page = KeyPage::select(keys(), "scope/", limit, None).unwrap();
            assert_eq!(page.keys.len(), expected);
            assert!(page.next_cursor.is_some());
        }
    }

    #[test]
    fn cursor_continues_after_deleted_key_and_checks_scope() {
        let first = KeyPage::select(keys(), "scope/", Some(2), None).unwrap();
        let cursor = first.next_cursor.as_deref();
        let next = KeyPage::select(keys().skip(2), "scope/", Some(100), cursor).unwrap();
        assert_eq!(next.keys[0], "scope/002");
        assert!(KeyPage::select(keys(), "other/", None, cursor).is_err());
        assert!(KeyPage::select(keys(), "scope/", None, Some("invalid")).is_err());
        let last = KeyPage::select(keys(), "scope/", None, next.next_cursor.as_deref()).unwrap();
        assert_eq!(last.keys, ["scope/102", "scope/103", "scope/104"]);
        assert!(last.next_cursor.is_none());
    }
}
