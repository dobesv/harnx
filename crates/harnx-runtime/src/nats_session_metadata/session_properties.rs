//! Properties describing the work a session does: the user it acts for, the
//! repository, branch, issue and pull request it concerns, where that work
//! lives, the address that opens it in the Web UI, and any custom values an
//! agent records.
//!
//! They live in a reserved extension namespace rather than in typed fields of
//! [`SessionMetadata`], which denies unknown fields: a typed field written by
//! a newer frontend would stop an older worker from reading the session at
//! all, while an extension is opaque to it. The namespace holds one object
//! keyed by property name, so a property added later needs no migration, and
//! each value carries its own `inherit` flag.

use super::{store::PatchGuard, MetadataRecord, SessionMetadata, SessionMetadataStore};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Reserved metadata namespace holding [`SessionProperties`].
pub const SESSION_PROPERTIES_NAMESPACE: &str = "dev.harnx.session_properties";
/// The property holding the address that opens a session in the Web UI.
pub const WEB_SESSION_URL_PROPERTY: &str = "web_session_url";
/// The property holding a session's labels.
pub const LABELS_PROPERTY: &str = "labels";
/// Most labels one session keeps.
pub const SESSION_LABELS_MAX: usize = 50;
/// Most custom properties one session keeps.
pub const CUSTOM_PROPERTIES_MAX: usize = 32;

const LABEL_MAX_CHARS: usize = 100;
const TEXT_MAX_BYTES: usize = 512;
const CUSTOM_NAME_MAX_BYTES: usize = 64;
const CUSTOM_VALUE_MAX_BYTES: usize = 2048;
const URL_MAX_BYTES: usize = 2048;
const PATH_MAX_BYTES: usize = 4096;
const BRANCH_MAX_BYTES: usize = 255;
const OWNER_REPO_MAX_BYTES: usize = 200;

/// The shape a well-known property's value must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropertyKind {
    Text,
    OwnerRepo,
    Branch,
    Number,
    Url,
    Path,
    Labels,
}

/// Whether sub-agent sessions copy a property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inheritance {
    /// Copied unless its writer says not to.
    ByDefault,
    /// Copied only when its writer asks.
    OnRequest,
    /// Describes one session alone, so it is never copied.
    Never,
}

/// A property whose meaning and type Harnx knows. Adding a row here is all a
/// new well-known property needs: validation, the tool description and the
/// inheritance all come from this table.
pub struct PropertyDefinition {
    pub name: &'static str,
    pub description: &'static str,
    kind: PropertyKind,
    pub inheritance: Inheritance,
    /// Set only by Harnx itself; agents can neither set nor clear it.
    pub read_only: bool,
}

const fn definition(
    name: &'static str,
    kind: PropertyKind,
    inheritance: Inheritance,
    description: &'static str,
) -> PropertyDefinition {
    PropertyDefinition {
        name,
        description,
        kind,
        inheritance,
        read_only: false,
    }
}

/// The well-known properties.
pub const PROPERTY_DEFINITIONS: &[PropertyDefinition] = &[
    PropertyDefinition {
        read_only: true,
        ..definition(
            "user_id",
            PropertyKind::Text,
            Inheritance::ByDefault,
            "the user the session acts for",
        )
    },
    definition(
        "github_owner_repo",
        PropertyKind::OwnerRepo,
        Inheritance::ByDefault,
        "GitHub repository as owner/repo",
    ),
    definition(
        "git_branch",
        PropertyKind::Branch,
        Inheritance::ByDefault,
        "git branch the session works on",
    ),
    definition(
        "github_issue",
        PropertyKind::Number,
        Inheritance::ByDefault,
        "GitHub issue number in github_owner_repo",
    ),
    definition(
        "github_pull_request",
        PropertyKind::Number,
        Inheritance::ByDefault,
        "GitHub pull request number in github_owner_repo",
    ),
    definition(
        "external_task_url",
        PropertyKind::Url,
        Inheritance::ByDefault,
        "http(s) URL of the task in another tracker, such as Jira or Linear",
    ),
    definition(
        "working_directory",
        PropertyKind::Path,
        Inheritance::ByDefault,
        "directory the session works in",
    ),
    definition(
        WEB_SESSION_URL_PROPERTY,
        PropertyKind::Url,
        Inheritance::Never,
        "http(s) URL that opens this session in the Web UI",
    ),
    definition(
        LABELS_PROPERTY,
        PropertyKind::Labels,
        Inheritance::OnRequest,
        "list of labels",
    ),
];

impl PropertyKind {
    /// What a value of this kind must look like, completing "<name> must ...".
    fn requirement(self) -> String {
        match self {
            Self::Text => format!("be text of at most {TEXT_MAX_BYTES} bytes with no control characters"),
            Self::OwnerRepo => "look like owner/repo, for example dobesv/harnx".to_string(),
            Self::Branch => format!(
                "be a branch name of at most {BRANCH_MAX_BYTES} bytes with no spaces or control characters"
            ),
            Self::Number => "be a positive integer".to_string(),
            Self::Url => "be an absolute http or https URL without credentials".to_string(),
            Self::Path => {
                format!("be a path of at most {PATH_MAX_BYTES} bytes with no control characters")
            }
            Self::Labels => format!(
                "be a list of at most {SESSION_LABELS_MAX} labels, each at most {LABEL_MAX_CHARS} characters with no control characters"
            ),
        }
    }

    fn accepts_text(self, text: &str) -> bool {
        match self {
            Self::Text => text.len() <= TEXT_MAX_BYTES && !text.chars().any(char::is_control),
            Self::OwnerRepo => is_owner_repo(text),
            Self::Branch => {
                text.len() <= BRANCH_MAX_BYTES
                    && !text
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control())
            }
            Self::Url => is_http_url(text),
            Self::Path => text.len() <= PATH_MAX_BYTES && !text.chars().any(char::is_control),
            Self::Number | Self::Labels => false,
        }
    }
}

impl PropertyDefinition {
    /// The stored form of `value`, or `None` when the value is empty, which
    /// removes the property.
    fn normalize(&self, value: &Value) -> Result<Option<Value>> {
        let normalized = match self.kind {
            PropertyKind::Number => normalize_number(value),
            PropertyKind::Labels => normalize_labels(value),
            kind => normalize_text(value).and_then(|text| match text {
                Some(text) if !kind.accepts_text(&text) => None,
                text => Some(text.map(Value::String)),
            }),
        };
        normalized.with_context(|| format!("{} must {}", self.name, self.kind.requirement()))
    }
}

/// The well-known property named `name`, if there is one.
pub fn property_definition(name: &str) -> Option<&'static PropertyDefinition> {
    PROPERTY_DEFINITIONS
        .iter()
        .find(|definition| definition.name == name)
}

/// How sub-agent sessions treat the property named `name`. Custom properties
/// are copied only on request.
fn inheritance(name: &str) -> Inheritance {
    property_definition(name).map_or(Inheritance::OnRequest, |definition| definition.inheritance)
}

/// Where a value Harnx recorded itself came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertySource {
    /// harnx-serve's configured public URL.
    Configured,
    /// The headers of the request that carried a prompt.
    Inferred,
}

/// One property: its value, and whether sub-agent sessions started from this
/// session copy it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProperty {
    pub value: Value,
    #[serde(default)]
    pub inherit: bool,
    /// Set when Harnx recorded the value itself rather than an agent or a
    /// person supplying it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "known_source"
    )]
    pub source: Option<PropertySource>,
    /// Attributes a newer Harnx records, kept when the property is rewritten.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// A session's properties by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionProperties(BTreeMap<String, SessionProperty>);

impl SessionProperties {
    pub fn get(&self, name: &str) -> Option<&SessionProperty> {
        self.0.get(name)
    }

    /// The value of `name` when it is a string.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.get(name)?.value.as_str()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &SessionProperty)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The properties a sub-agent session started from this one copies.
    pub fn inherited(&self) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(name, property)| {
                    property.inherit && inheritance(name) != Inheritance::Never
                })
                .map(|(name, property)| (name.clone(), property.clone()))
                .collect(),
        )
    }

    /// Set `name` to `value` as a writer supplied it, so it has no
    /// [`PropertySource`]. An existing property keeps its other attributes
    /// and, unless `inherit` says otherwise, its flag; a new one takes the
    /// property's default.
    fn put(&mut self, name: &str, value: Value, inherit: Option<bool>) {
        let inheritance = inheritance(name);
        let property = self
            .0
            .entry(name.to_string())
            .or_insert_with(|| SessionProperty {
                value: Value::Null,
                inherit: inheritance == Inheritance::ByDefault,
                source: None,
                other: BTreeMap::new(),
            });
        property.value = value;
        property.source = None;
        property.inherit = inheritance != Inheritance::Never && inherit.unwrap_or(property.inherit);
    }

    fn labels(&self) -> Vec<String> {
        self.get(LABELS_PROPERTY)
            .and_then(|property| property.value.as_array())
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn custom_count(&self) -> usize {
        self.0
            .keys()
            .filter(|name| property_definition(name).is_none())
            .count()
    }
}

/// Set one property. A blank value, `0` for a number, or an empty list
/// removes it instead. `inherit` overrides whether sub-agent sessions copy
/// it; without it an existing property keeps its flag and a new one takes the
/// well-known default, or stays private for a custom property.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyAssignment {
    pub name: String,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub inherit: Option<bool>,
}

impl PropertyAssignment {
    fn apply(&self, properties: &mut SessionProperties) -> Result<()> {
        let name = writable_name(&self.name)?;
        ensure!(
            self.inherit != Some(true) || inheritance(name) != Inheritance::Never,
            "{name} describes this session alone and is never inherited"
        );
        match normalize_value(name, &self.value)? {
            Some(value) => properties.put(name, value, self.inherit),
            None => {
                properties.0.remove(name);
            }
        }
        Ok(())
    }
}

/// A change an agent makes to its session's properties: `clear` runs first,
/// then `set`, then `remove_labels`, then `add_labels`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionPropertiesUpdate {
    #[serde(deserialize_with = "null_as_empty")]
    pub set: Vec<PropertyAssignment>,
    #[serde(deserialize_with = "null_as_empty")]
    pub clear: Vec<String>,
    #[serde(deserialize_with = "trimmed_labels")]
    pub add_labels: Vec<String>,
    #[serde(deserialize_with = "trimmed_labels")]
    pub remove_labels: Vec<String>,
}

impl SessionPropertiesUpdate {
    /// Check every name and value before anything is written.
    pub fn validate(&self) -> Result<()> {
        self.apply(&mut SessionProperties::default()).map(drop)
    }

    /// Apply the change to `properties`, returning whether anything changed.
    pub fn apply(&self, properties: &mut SessionProperties) -> Result<bool> {
        let before = properties.clone();
        for name in &self.clear {
            properties.0.remove(writable_name(name)?);
        }
        for assignment in &self.set {
            assignment.apply(properties)?;
        }
        self.edit_labels(properties)?;
        ensure!(
            properties.custom_count() <= CUSTOM_PROPERTIES_MAX,
            "a session keeps at most {CUSTOM_PROPERTIES_MAX} custom properties"
        );
        Ok(*properties != before)
    }

    fn edit_labels(&self, properties: &mut SessionProperties) -> Result<()> {
        if self.add_labels.is_empty() && self.remove_labels.is_empty() {
            return Ok(());
        }
        let mut labels = properties.labels();
        labels.retain(|label| !self.remove_labels.contains(label));
        labels.extend(self.add_labels.iter().cloned());
        let assignment = PropertyAssignment {
            name: LABELS_PROPERTY.to_string(),
            value: Value::from(labels),
            inherit: None,
        };
        assignment.apply(properties)
    }
}

/// `name` when an agent may set or clear it.
fn writable_name(name: &str) -> Result<&str> {
    match property_definition(name) {
        Some(definition) if definition.read_only => {
            bail!("{name} is set by Harnx and can't be changed")
        }
        Some(_) => Ok(name),
        None if is_custom_name(name) => Ok(name),
        None => bail!(
            "'{name}' is not a well-known property, and custom property names must start with a lowercase letter and use only lowercase letters, digits, '_', '-' and '.' (at most {CUSTOM_NAME_MAX_BYTES} bytes)"
        ),
    }
}

fn normalize_value(name: &str, value: &Value) -> Result<Option<Value>> {
    match property_definition(name) {
        Some(definition) => definition.normalize(value),
        None => normalize_custom(value).with_context(|| {
            format!(
                "custom property {name} must be text of at most {CUSTOM_VALUE_MAX_BYTES} bytes with no control characters other than newlines and tabs"
            )
        }),
    }
}

fn is_custom_name(name: &str) -> bool {
    name.len() <= CUSTOM_NAME_MAX_BYTES
        && name.starts_with(|character: char| character.is_ascii_lowercase())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

/// Trimmed text, `Some(None)` for an empty value, or `None` when `value`
/// isn't text at all.
fn normalize_text(value: &Value) -> Option<Option<String>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => {
            let text = text.trim();
            Some((!text.is_empty()).then(|| text.to_string()))
        }
        _ => None,
    }
}

/// A positive integer, also accepted as a string such as `"2296"` or
/// `"#2296"`. `0` is a model's placeholder for "none" and removes the value.
fn normalize_number(value: &Value) -> Option<Option<Value>> {
    let number = match value {
        Value::Null => return Some(None),
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => match text.trim().trim_start_matches('#') {
            "" => return Some(None),
            digits => digits.parse::<u64>().ok()?,
        },
        _ => return None,
    };
    Some((number != 0).then(|| Value::from(number)))
}

/// A list of distinct trimmed labels; a single string is one label.
fn normalize_labels(value: &Value) -> Option<Option<Value>> {
    let items = match value {
        Value::Null => return Some(None),
        Value::String(_) => std::slice::from_ref(value),
        Value::Array(items) => items.as_slice(),
        _ => return None,
    };
    let mut labels: Vec<String> = Vec::new();
    for item in items {
        let label = trimmed_label(item)?;
        if !label.is_empty() && !labels.iter().any(|existing| existing == label) {
            labels.push(label.to_string());
        }
    }
    (labels.len() <= SESSION_LABELS_MAX).then(|| (!labels.is_empty()).then(|| Value::from(labels)))
}

/// `item` trimmed, or `None` when it isn't acceptable label text.
fn trimmed_label(item: &Value) -> Option<&str> {
    let label = item.as_str()?.trim();
    (label.chars().count() <= LABEL_MAX_CHARS && !label.chars().any(char::is_control))
        .then_some(label)
}

/// Text, with numbers and booleans taken as their text.
fn normalize_custom(value: &Value) -> Option<Option<Value>> {
    let text = match value {
        Value::Null => return Some(None),
        Value::String(text) => text.trim().to_string(),
        Value::Number(_) | Value::Bool(_) => value.to_string(),
        _ => return None,
    };
    let acceptable = text.len() <= CUSTOM_VALUE_MAX_BYTES
        && !text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'));
    acceptable.then(|| (!text.is_empty()).then_some(Value::String(text)))
}

fn is_owner_repo(text: &str) -> bool {
    text.len() <= OWNER_REPO_MAX_BYTES
        && text
            .split_once('/')
            .is_some_and(|(owner, repo)| is_repo_segment(owner) && is_repo_segment(repo))
}

fn is_repo_segment(segment: &str) -> bool {
    !matches!(segment, "" | "." | "..")
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// An absolute http or https URL with a host and no credentials.
fn is_http_url(value: &str) -> bool {
    is_written_exactly(value)
        && reqwest::Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

/// Whether `value` parses as the URL it spells. URL parsers repair missing
/// slashes and strip whitespace, and a value that only parses after repair
/// shouldn't be stored as given.
fn is_written_exactly(value: &str) -> bool {
    value.len() <= URL_MAX_BYTES
        && !value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '\\'
        })
        && value
            .split_once("://")
            .is_some_and(|(_, rest)| !rest.starts_with('/'))
}

/// The properties recorded for a session; empty when none are.
pub fn session_properties(metadata: &SessionMetadata) -> Result<SessionProperties> {
    let Some(value) = metadata.extensions.get(SESSION_PROPERTIES_NAMESPACE) else {
        return Ok(SessionProperties::default());
    };
    serde_json::from_value(value.clone()).context("decode dev.harnx.session_properties extension")
}

pub(super) fn store_properties(
    metadata: &mut SessionMetadata,
    properties: &SessionProperties,
) -> Result<()> {
    if properties.is_empty() {
        metadata.extensions.remove(SESSION_PROPERTIES_NAMESPACE);
    } else {
        metadata.extensions.insert(
            SESSION_PROPERTIES_NAMESPACE.to_string(),
            serde_json::to_value(properties)?,
        );
    }
    Ok(())
}

impl SessionMetadataStore {
    /// Apply an agent's change to the session's properties. A worker passes
    /// its lease fence so it can't write behind the worker that replaced it.
    /// A change that alters nothing writes nothing.
    pub async fn update_session_properties(
        &self,
        session_id: &str,
        update: &SessionPropertiesUpdate,
        fence_token: Option<u64>,
    ) -> Result<MetadataRecord> {
        update.validate()?;
        let guard = fence_token.map_or_else(PatchGuard::default, PatchGuard::for_worker);
        self.patch_guarded_if_changed(session_id, guard, |metadata| {
            let mut properties = session_properties(metadata)?;
            let changed = update.apply(&mut properties)?;
            if changed {
                store_properties(metadata, &properties)?;
            }
            Ok(changed)
        })
        .await
    }

    /// Record the Web UI address harnx-serve worked out for a session. It
    /// fills in a missing address, and a configured one replaces an address
    /// Harnx recorded earlier, which may have been inferred or configured
    /// differently. An address an agent or person set is left alone. Returns
    /// whether this call changed the address.
    pub async fn record_web_session_url(
        &self,
        session_id: &str,
        url: &str,
        source: PropertySource,
    ) -> Result<bool> {
        let value = property_definition(WEB_SESSION_URL_PROPERTY)
            .context("web_session_url is a well-known property")?
            .normalize(&Value::from(url))?
            .context("web_session_url must not be empty")?;
        let mut recorded = false;
        self.patch_guarded_if_changed(session_id, PatchGuard::default(), |metadata| {
            let mut properties = session_properties(metadata)?;
            recorded = properties
                .0
                .get(WEB_SESSION_URL_PROPERTY)
                .is_none_or(|existing| {
                    source == PropertySource::Configured
                        && existing.source.is_some()
                        && existing.value != value
                });
            if recorded {
                properties.put(WEB_SESSION_URL_PROPERTY, value.clone(), None);
                if let Some(property) = properties.0.get_mut(WEB_SESSION_URL_PROPERTY) {
                    property.source = Some(source);
                }
                store_properties(metadata, &properties)?;
            }
            Ok(recorded)
        })
        .await?;
        Ok(recorded)
    }
}

/// A source this version doesn't know reads as none, so a value a newer
/// Harnx recorded can't make the whole object unreadable.
fn known_source<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PropertySource>, D::Error> {
    let source = Option::<Value>::deserialize(deserializer)?;
    Ok(source.and_then(|source| serde_json::from_value(source).ok()))
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

fn trimmed_labels<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let labels: Vec<String> = null_as_empty(deserializer)?;
    Ok(labels
        .into_iter()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .collect())
}

#[cfg(test)]
#[path = "session_properties_tests.rs"]
mod tests;
