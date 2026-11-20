//! The caller principals a session was created for.
//!
//! Trusted server-side code (harnx-serve and harnx-a2a-server) captures this
//! snapshot once, from the resolved request identity, when a session's
//! canonical metadata is first created. It is stored read-only in session
//! properties, inherited verbatim by sub-agent and handoff sessions, and never
//! recomputed later: it tailors prompts and tools, and is not an
//! authorization boundary — access checks keep using live request
//! memberships.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most entries one principals list keeps; later entries are dropped.
pub const USER_PRINCIPALS_MAX_LIST: usize = 100;
/// Most bytes one principal string keeps; longer strings are dropped.
pub const USER_PRINCIPALS_MAX_TEXT: usize = 512;
/// Encoded snapshot budget, leaving room in the session-properties namespace
/// for its property wrapper, the owner ID, and other session properties.
const USER_PRINCIPALS_MAX_ENCODED_BYTES: usize = 48 * 1024;
const JSON_NULL_ENCODED_BYTES: usize = 4;
const JSON_CONTROL_ESCAPE_ENCODED_BYTES: usize = 6;

/// Caller principals as resolved at session creation.
///
/// Serialized as exactly four keys: `user_id`, `users`, `groups`, `roles`.
/// Lists keep the resolver's order. Constructors and the stored-value parser
/// are module functions so the type itself stays one cohesive cluster of
/// field-reading methods.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPrincipals {
    /// The raw resolved user id (the session owner id); null when anonymous,
    /// so every record carries all four keys.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Expanded identities from users.yaml aliases; includes `user_id`.
    #[serde(default)]
    pub users: Vec<String>,
    /// Header and users.yaml groups as resolved at creation.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Header and users.yaml roles as resolved at creation.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl UserPrincipals {
    /// True when there is nothing to record: no user id and empty lists.
    pub fn is_anonymous(&self) -> bool {
        self.user_id.is_none()
            && self.users.is_empty()
            && self.groups.is_empty()
            && self.roles.is_empty()
    }

    /// The stored property value: the four keys as a JSON object.
    pub fn to_property_value(&self) -> Value {
        serde_json::to_value(self).expect("user principals are JSON-serializable")
    }
}

/// No identity at all: CLI/TUI sessions without a configured user, and
/// sessions created before this snapshot existed.
pub fn anonymous() -> UserPrincipals {
    UserPrincipals::default()
}

/// The fallback for sessions created before principals were recorded: the
/// stored owner id, and nothing else. A legacy session never gets more
/// than an anonymous caller with the same user id would.
pub fn legacy_fallback(user_id: Option<&str>) -> UserPrincipals {
    match user_id {
        None => anonymous(),
        Some(user_id) => from_resolved(Some(user_id), None, &[], &[]),
    }
}

/// Build a snapshot from a resolved request identity. `expanded_users` is
/// the users.yaml alias expansion when one ran; without one the caller's
/// own id is the whole identity. Per-list limits and a combined encoded-size
/// budget drop excess values rather than failing session creation.
pub fn from_resolved(
    user_id: Option<&str>,
    expanded_users: Option<&[String]>,
    groups: &[String],
    roles: &[String],
) -> UserPrincipals {
    let user_id = user_id.and_then(|user| acceptable(user).then(|| user.to_string()));
    let users = expanded_users
        .map(|users| users.to_vec())
        .or_else(|| user_id.clone().map(|user_id| vec![user_id]))
        .unwrap_or_default();
    let mut principals = UserPrincipals {
        users: clamp_users(users, user_id.as_deref()),
        groups: clamp(groups.to_vec()),
        roles: clamp(roles.to_vec()),
        user_id,
    };
    clamp_encoded_size(&mut principals);
    principals
}

/// Parse a stored property value. Unknown keys are ignored so a newer
/// Harnx can add fields without breaking this reader. A record without a
/// `users` list reads like a legacy one: the owner id alone is the whole
/// identity.
pub fn from_property_value(value: &Value) -> Option<UserPrincipals> {
    let object = value.as_object()?;
    let user_id = match object.get("user_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(user)) => (!user.is_empty()).then(|| user.to_string()),
        _ => return None,
    };
    let users = match object.get("users") {
        None => user_id
            .clone()
            .map(|user_id| vec![user_id])
            .unwrap_or_default(),
        Some(value) => string_list(Some(value))?,
    };
    Some(UserPrincipals {
        user_id,
        users,
        groups: string_list(object.get("groups"))?,
        roles: string_list(object.get("roles"))?,
    })
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

fn acceptable(text: &str) -> bool {
    !text.is_empty() && text.len() <= USER_PRINCIPALS_MAX_TEXT
}

fn clamp_users(mut values: Vec<String>, user_id: Option<&str>) -> Vec<String> {
    values.retain(|value| acceptable(value));
    if values.len() > USER_PRINCIPALS_MAX_LIST {
        let user_index =
            user_id.and_then(|user_id| values.iter().position(|value| value == user_id));
        if let (Some(user_id), Some(index)) = (user_id, user_index) {
            if index >= USER_PRINCIPALS_MAX_LIST {
                values.truncate(USER_PRINCIPALS_MAX_LIST - 1);
                values.push(user_id.to_string());
            } else {
                values.truncate(USER_PRINCIPALS_MAX_LIST);
            }
        } else {
            values.truncate(USER_PRINCIPALS_MAX_LIST);
        }
    }
    values
}

fn clamp(mut values: Vec<String>) -> Vec<String> {
    values.retain(|value| acceptable(value));
    values.truncate(USER_PRINCIPALS_MAX_LIST);
    values
}

fn clamp_encoded_size(principals: &mut UserPrincipals) {
    let mut encoded_size = encoded_size(principals);
    while encoded_size > USER_PRINCIPALS_MAX_ENCODED_BYTES {
        if remove_last_encoded_value(&mut principals.roles, &mut encoded_size)
            || remove_last_encoded_value(&mut principals.groups, &mut encoded_size)
        {
            continue;
        }
        if let Some(index) = removable_user_index(principals) {
            remove_encoded_value(&mut principals.users, index, &mut encoded_size);
        } else {
            break;
        }
    }
}

fn encoded_size(principals: &UserPrincipals) -> usize {
    // JSON strings quote `"` and `\\`, escape control characters as `\\uXXXX`,
    // and encode other characters as UTF-8 bytes.
    b"{\"user_id\":".len()
        + principals
            .user_id
            .as_deref()
            .map_or(JSON_NULL_ENCODED_BYTES, encoded_json_string_len)
        + b",\"users\":".len()
        + encoded_string_list_len(&principals.users)
        + b",\"groups\":".len()
        + encoded_string_list_len(&principals.groups)
        + b",\"roles\":".len()
        + encoded_string_list_len(&principals.roles)
        + 1
}

fn encoded_string_list_len(values: &[String]) -> usize {
    values
        .iter()
        .map(|value| encoded_json_string_len(value))
        .sum::<usize>()
        + values.len().saturating_sub(1)
        + 2
}

fn encoded_json_string_len(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => JSON_CONTROL_ESCAPE_ENCODED_BYTES,
            _ => character.len_utf8(),
        })
        .sum::<usize>()
        + 2
}

fn remove_last_encoded_value(values: &mut Vec<String>, encoded_size: &mut usize) -> bool {
    if let Some(value) = values.pop() {
        *encoded_size -= encoded_json_string_len(&value) + usize::from(!values.is_empty());
        true
    } else {
        false
    }
}

fn remove_encoded_value(values: &mut Vec<String>, index: usize, encoded_size: &mut usize) {
    let value = values.remove(index);
    *encoded_size -= encoded_json_string_len(&value) + usize::from(!values.is_empty());
}

fn removable_user_index(principals: &UserPrincipals) -> Option<usize> {
    let protected_index = principals
        .user_id
        .as_ref()
        .and_then(|user_id| principals.users.iter().position(|value| value == user_id));
    principals
        .users
        .iter()
        .enumerate()
        .rfind(|(index, _)| Some(*index) != protected_index)
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_exactly_the_four_keys_in_order() {
        let principals = UserPrincipals {
            user_id: Some("alice".into()),
            users: vec!["alice".into(), "bob".into()],
            groups: vec!["engineering".into()],
            roles: vec!["reviewer".into()],
        };
        assert_eq!(
            principals.to_property_value(),
            json!({
                "user_id": "alice",
                "users": ["alice", "bob"],
                "groups": ["engineering"],
                "roles": ["reviewer"],
            })
        );
        let round =
            from_property_value(&principals.to_property_value()).expect("stored value parses");
        assert_eq!(round, principals);
    }

    #[test]
    fn anonymous_serializes_null_user_id_and_empty_lists() {
        let value = anonymous().to_property_value();
        assert_eq!(
            value,
            json!({"user_id": null, "users": [], "groups": [], "roles": []})
        );
        assert_eq!(from_property_value(&value), Some(anonymous()));
        assert!(anonymous().is_anonymous());
    }

    #[test]
    fn legacy_fallback_derives_only_from_the_owner_id() {
        let fallback = legacy_fallback(Some("owner"));
        assert_eq!(
            fallback,
            UserPrincipals {
                user_id: Some("owner".into()),
                users: vec!["owner".into()],
                groups: Vec::new(),
                roles: Vec::new(),
            }
        );
        assert_eq!(legacy_fallback(None), anonymous());
    }

    #[test]
    fn from_resolved_keeps_resolver_order_and_falls_back_to_singleton() {
        let expanded = vec!["alice".to_string(), "bob".to_string()];
        let principals = from_resolved(
            Some("alice"),
            Some(&expanded),
            &["header-group".into(), "alias-group".into()],
            &["role".into()],
        );
        assert_eq!(principals.users, ["alice", "bob"]);
        assert_eq!(principals.groups, ["header-group", "alias-group"]);
        assert_eq!(principals.roles, ["role"]);

        let singleton = from_resolved(Some("carol"), None, &[], &[]);
        assert_eq!(singleton.users, ["carol"]);
        assert_eq!(from_resolved(None, None, &[], &[]), anonymous());
    }

    #[test]
    fn from_resolved_drops_oversized_entries_instead_of_failing() {
        let long = "x".repeat(USER_PRINCIPALS_MAX_TEXT + 1);
        let many: Vec<String> = (0..USER_PRINCIPALS_MAX_LIST + 5)
            .map(|index| format!("g{index}"))
            .collect();
        let principals = from_resolved(Some(&long), None, &many, &[]);
        assert_eq!(principals.user_id, None);
        assert_eq!(principals.users, Vec::<String>::new());
        assert_eq!(principals.groups.len(), USER_PRINCIPALS_MAX_LIST);
        assert_eq!(principals.groups.first().map(String::as_str), Some("g0"));
    }

    #[test]
    fn from_resolved_preserves_user_id_when_it_falls_beyond_the_list_limit() {
        let users: Vec<String> = (0..USER_PRINCIPALS_MAX_LIST)
            .map(|index| format!("alias-{index}"))
            .chain(std::iter::once("resolved-user".to_string()))
            .collect();

        let principals = from_resolved(Some("resolved-user"), Some(&users), &[], &[]);

        assert_eq!(principals.users.len(), USER_PRINCIPALS_MAX_LIST);
        assert_eq!(
            &principals.users[..USER_PRINCIPALS_MAX_LIST - 1],
            &users[..USER_PRINCIPALS_MAX_LIST - 1]
        );
        assert_eq!(
            principals.users.last().map(String::as_str),
            Some("resolved-user")
        );
    }

    #[test]
    fn encoded_size_matches_json_serialization_for_escaped_and_unicode_strings() {
        let strings: Vec<String> = (0u8..=0x1f)
            .map(char::from)
            .chain(['"', '\\', 'é', '🦀'])
            .map(|character| character.to_string())
            .collect();
        let principals = UserPrincipals {
            user_id: Some(strings[0].clone()),
            users: strings.clone(),
            groups: strings.clone(),
            roles: strings,
        };

        assert_eq!(
            encoded_size(&principals),
            serde_json::to_vec(&principals).unwrap().len()
        );
    }

    #[test]
    fn from_resolved_clamps_combined_encoded_size_in_deterministic_order() {
        let oversized: Vec<String> = (0..USER_PRINCIPALS_MAX_LIST)
            .map(|_| "\0".repeat(USER_PRINCIPALS_MAX_TEXT))
            .collect();
        let user_id = oversized[0].as_str();
        let principals = from_resolved(Some(user_id), Some(&oversized), &oversized, &oversized);
        let encoded = serde_json::to_vec(&principals).unwrap();

        assert!(encoded.len() <= USER_PRINCIPALS_MAX_ENCODED_BYTES);
        assert!(principals.roles.is_empty());
        assert!(principals.groups.is_empty());
        assert_eq!(principals.user_id.as_deref(), Some(user_id));
        assert_eq!(principals.users.first().map(String::as_str), Some(user_id));
        assert!(principals.users.len() < USER_PRINCIPALS_MAX_LIST);
        assert_eq!(
            from_resolved(Some(user_id), Some(&oversized), &oversized, &oversized),
            principals
        );
    }

    #[test]
    fn from_property_value_rejects_non_string_user_ids() {
        for user_id in [json!(42), json!(true), json!([]), json!({})] {
            assert_eq!(
                from_property_value(&json!({
                    "user_id": user_id,
                    "groups": ["engineering"],
                    "roles": ["reviewer"],
                })),
                None
            );
        }
    }

    #[test]
    fn from_property_value_accepts_absent_null_and_string_user_ids() {
        for user_id in [None, Some(Value::Null), Some(json!("alice"))] {
            let mut value = json!({"groups": ["engineering"], "roles": ["reviewer"]});
            if let Some(user_id) = user_id {
                value["user_id"] = user_id;
            }
            let principals = from_property_value(&value).expect("valid user_id shape");
            assert_eq!(principals.user_id.as_deref(), value["user_id"].as_str());
            assert_eq!(principals.groups, ["engineering"]);
            assert_eq!(principals.roles, ["reviewer"]);
        }
    }

    #[test]
    fn from_property_value_tolerates_unknown_and_rejects_wrong_shapes() {
        let value = json!({
            "user_id": "alice",
            "users": ["alice"],
            "groups": [],
            "roles": [],
            "future_field": {"anything": true},
        });
        let principals = from_property_value(&value).expect("unknown key ignored");
        assert_eq!(principals.user_id.as_deref(), Some("alice"));

        assert_eq!(from_property_value(&json!("text")), None);
        assert_eq!(from_property_value(&json!({"users": "not-a-list"})), None);
        assert_eq!(from_property_value(&json!({"groups": {"a": "b"}})), None);
        // Null lists read as empty, so a partial newer record still loads.
        assert_eq!(
            from_property_value(&json!({"user_id": "alice"})),
            Some(legacy_fallback(Some("alice")))
        );
    }
}
