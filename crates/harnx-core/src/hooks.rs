use crate::middleware::{MiddlewareOrigin, MiddlewareSourceConfig};
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf, time::Duration};

// --- Event-hook configuration (serialized shape read from config.yaml) ------

/// Guidance shown when a configuration still uses the retired process-hook
/// `hooks:` shape. Kept in one place so every rejection spells it the same way.
pub const LEGACY_HOOK_MIGRATION: &str =
    "legacy process hooks were replaced by in-process scripts: \
    wrap tool calls with `middleware:` (docs/middleware-guide.md), run existing Claude-protocol \
    hook commands through the claude_hook_run compatibility tool from middleware \
    (docs/middleware-claude-hooks.md), or declare lifecycle events as `hooks:` script entries \
    with `bash:`/`python:`/`script:` (docs/hooks-guide.md)";

/// Lifecycle events that can carry event-hook entries. Tool-call events
/// (`PreToolUse`/`PostToolUse`/`PostToolUseFailure`) moved to middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum HookEventKind {
    SessionStart,
    UserPromptSubmit,
    Stop,
    StopFailure,
}

impl HookEventKind {
    pub const ALL: [Self; 4] = [
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::Stop,
        Self::StopFailure,
    ];

    pub fn key(&self) -> &'static str {
        // qual:allow(dry, repeated_matches) reason: "Canonical enum-to-config-key mapping; a lookup table would separate the keys from the event they name."
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
        }
    }

    /// `on_error: deny` is only meaningful where the event's failure can stop
    /// work: blocking a prompt (UserPromptSubmit) or failing the session start.
    pub fn allows_on_error_deny(&self) -> bool {
        matches!(self, Self::SessionStart | Self::UserPromptSubmit)
    }
}

/// Error policy for event hooks. Defaults to `allow`: a failing run's effects
/// are dropped, a warning is logged and later hooks still run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventHookOnError {
    #[default]
    Allow,
    Deny,
}

/// A file's ordered event-hook declarations for one event. Origin is attached
/// by the file/package loader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct EventHooksList {
    pub entries: Vec<EventHookEntry>,
}

/// One event-hook entry: a full script declaration or a disable marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum EventHookEntry {
    Full(EventHookDefinition),
    Disable { name: String, enabled: bool },
}

impl EventHookEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Full(definition) => &definition.name,
            Self::Disable { name, .. } => name,
        }
    }
}

/// Full replacement, never a partial overlay of an earlier declaration.
/// Middleware fields minus `match`, `tools` and the interaction operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventHookDefinition {
    pub name: String,
    #[serde(flatten)]
    pub source: MiddlewareSourceConfig,
    /// Exact HTTP(S) hosts; empty denies all fetch requests.
    pub fetch: Vec<String>,
    #[serde(serialize_with = "serialize_event_hook_timeout")]
    pub timeout: Duration,
    pub on_error: EventHookOnError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventHookEntry {
    name: String,
    bash: Option<String>,
    python: Option<String>,
    script: Option<PathBuf>,
    fetch: Option<Vec<String>>,
    timeout: Option<String>,
    on_error: Option<EventHookOnError>,
    enabled: Option<bool>,
}

const DEFAULT_EVENT_HOOK_TIMEOUT_SECS: u64 = 30;

fn serialize_event_hook_timeout<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&humantime::format_duration(*duration).to_string())
}

impl<'de> Deserialize<'de> for EventHooksList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        parse_event_hooks_list(&value)
            .map_err(|error| D::Error::custom(format!("event hooks: {error}")))
    }
}

fn parse_event_hooks_list(value: &serde_json::Value) -> Result<EventHooksList, String> {
    let entries = value
        .as_array()
        .ok_or_else(|| "expected a list of hook entries".to_string())?;
    let mut parsed = Vec::with_capacity(entries.len());
    let mut names = HashSet::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("<unnamed>");
        if !names.insert(name.to_string()) {
            return Err(format!(
                "hook '{name}': duplicate name in the same configuration level"
            ));
        }
        parsed.push(
            parse_event_hook_entry(entry).map_err(|error| format!("hook '{name}': {error}"))?,
        );
    }
    Ok(EventHooksList { entries: parsed })
}

fn parse_event_hook_entry(value: &serde_json::Value) -> Result<EventHookEntry, String> {
    if value.get("command").is_some() {
        return Err(format!(
            "legacy `command:` hook entries are no longer supported; {LEGACY_HOOK_MIGRATION}"
        ));
    }
    let raw: RawEventHookEntry =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    crate::middleware::validate_entry_shape(value, &raw.name)?;
    if crate::middleware::disable_entry(value, raw.enabled)? {
        return Ok(EventHookEntry::Disable {
            name: raw.name,
            enabled: false,
        });
    }
    let source = crate::middleware::parse_entry_source(raw.bash, raw.python, raw.script)?;
    let timeout =
        crate::middleware::parse_entry_timeout(raw.timeout, DEFAULT_EVENT_HOOK_TIMEOUT_SECS)?;
    Ok(EventHookEntry::Full(EventHookDefinition {
        name: raw.name,
        source,
        fetch: crate::middleware::parse_fetch_hosts(raw.fetch.unwrap_or_default())?,
        timeout,
        on_error: raw.on_error.unwrap_or_default(),
    }))
}

/// Configuration for lifecycle event hooks (global, per-agent or per
/// tool-server): a map from event name to ordered script entries, plus the
/// `max_resume` bound for `Stop` resumes.
///
/// Strict parsing: unknown keys and the retired legacy shape
/// (`entries:` with `command:` processes) fail configuration load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HooksConfig {
    /// Maximum number of resume iterations a `Stop` hook may trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_resume: Option<u32>,

    #[serde(
        rename = "SessionStart",
        default,
        skip_serializing_if = "is_empty_list"
    )]
    pub session_start: EventHooksList,
    #[serde(
        rename = "UserPromptSubmit",
        default,
        skip_serializing_if = "is_empty_list"
    )]
    pub user_prompt_submit: EventHooksList,
    #[serde(rename = "Stop", default, skip_serializing_if = "is_empty_list")]
    pub stop: EventHooksList,
    #[serde(rename = "StopFailure", default, skip_serializing_if = "is_empty_list")]
    pub stop_failure: EventHooksList,

    /// Declaring file and the directory `script:` paths resolve against.
    /// Attached by the loader, never serialized.
    #[serde(skip)]
    pub origin: Option<MiddlewareOrigin>,
}

fn is_empty_list(list: &EventHooksList) -> bool {
    list.entries.is_empty()
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.max_resume.is_none()
            && is_empty_list(&self.session_start)
            && is_empty_list(&self.user_prompt_submit)
            && is_empty_list(&self.stop)
            && is_empty_list(&self.stop_failure)
    }

    pub fn list(&self, event: HookEventKind) -> &EventHooksList {
        match event {
            HookEventKind::SessionStart => &self.session_start,
            HookEventKind::UserPromptSubmit => &self.user_prompt_submit,
            HookEventKind::Stop => &self.stop,
            HookEventKind::StopFailure => &self.stop_failure,
        }
    }

    /// Merge global and agent hooks: the agent's `max_resume` wins; event
    /// lists are resolved per event by name/override/disable rules at
    /// resolution time (see `Config::resolved_event_hooks`), so this only
    /// serves callers reading `max_resume`.
    pub fn merge(global: &HooksConfig, agent: &HooksConfig) -> HooksConfig {
        let mut merged = agent.clone();
        merged.max_resume = agent.max_resume.or(global.max_resume);
        merged
    }
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        parse_hooks_config(&value).map_err(D::Error::custom)
    }
}

fn parse_hooks_config(value: &serde_json::Value) -> Result<HooksConfig, String> {
    let fields = value
        .as_object()
        .ok_or_else(|| "hooks must be a map of event names to hook entries".to_string())?;
    let mut config = HooksConfig::default();
    for (key, entry) in fields {
        match key.as_str() {
            "max_resume" => {
                config.max_resume = serde_json::from_value(entry.clone())
                    .map_err(|e| format!("max_resume: {e}"))?;
            }
            "entries" => {
                return Err(format!(
                    "legacy `hooks:` `entries:` are no longer supported; {LEGACY_HOOK_MIGRATION}"
                ));
            }
            event => {
                let kind = HookEventKind::ALL
                    .iter()
                    .find(|kind| kind.key() == event)
                    .copied()
                    .ok_or_else(|| {
                        format!(
                            "unknown hooks key '{event}': expected max_resume or one of {}",
                            HookEventKind::ALL
                                .iter()
                                .map(|kind| kind.key())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                let list: EventHooksList =
                    serde_json::from_value(entry.clone()).map_err(|e| e.to_string())?;
                validate_on_error(&list, kind)?;
                // qual:allow(dry, repeated_matches) reason: "Distributes one parsed list to the field the event names; a helper would hide the four-event layout the config documents."
                match kind {
                    HookEventKind::SessionStart => config.session_start = list,
                    HookEventKind::UserPromptSubmit => config.user_prompt_submit = list,
                    HookEventKind::Stop => config.stop = list,
                    HookEventKind::StopFailure => config.stop_failure = list,
                }
            }
        }
    }
    Ok(config)
}

/// `on_error: deny` is a configuration error on events whose failure cannot
/// stop work (D7).
fn validate_on_error(list: &EventHooksList, event: HookEventKind) -> Result<(), String> {
    if event.allows_on_error_deny() {
        return Ok(());
    }
    for entry in &list.entries {
        if let EventHookEntry::Full(definition) = entry {
            if definition.on_error == EventHookOnError::Deny {
                return Err(format!(
                    "hook '{}': on_error: deny is only valid for SessionStart and \
                     UserPromptSubmit hooks",
                    definition.name
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        EventHookEntry, EventHookOnError, EventHooksList, HookEventKind, HooksConfig,
        LEGACY_HOOK_MIGRATION,
    };

    #[test]
    fn hooks_config_parses_event_map() {
        let yaml = r#"
max_resume: 3
SessionStart:
  - name: greet
    bash: |
      add_context "hello"
UserPromptSubmit:
  - name: policy
    python: |
      add_context("checked")
    timeout: 5s
Stop:
  - name: keep-going
    bash: |
      resume "keep going"
StopFailure:
  - name: log-failure
    bash: |
      event_input
"#;

        let config: HooksConfig = serde_yaml::from_str(yaml).expect("parse hooks config");

        assert_eq!(config.max_resume, Some(3));
        assert_eq!(
            config
                .session_start
                .entries
                .iter()
                .map(EventHookEntry::name)
                .collect::<Vec<_>>(),
            vec!["greet"]
        );
        let EventHookEntry::Full(definition) = &config.session_start.entries[0] else {
            panic!("expected a full entry");
        };
        assert!(matches!(
            definition.source,
            crate::middleware::MiddlewareSourceConfig::Bash { .. }
        ));
        let EventHookEntry::Full(policy) = &config.user_prompt_submit.entries[0] else {
            panic!("expected a full entry");
        };
        assert_eq!(policy.timeout, std::time::Duration::from_secs(5));
        assert_eq!(policy.on_error, EventHookOnError::Allow);
    }

    #[test]
    fn hooks_config_round_trips_every_event_as_a_list() {
        let mut value = serde_json::json!({"max_resume": 2});
        for event in HookEventKind::ALL {
            value[event.key()] = serde_json::json!([
                {"name": "shell", "bash": "event_input", "timeout": "5s"},
                {"name": "python", "python": "event_input()"},
                {"name": "file", "script": "hooks/audit.py"},
                {"name": "disabled", "enabled": false}
            ]);
        }
        let config: HooksConfig = serde_json::from_value(value).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        for event in HookEventKind::ALL {
            assert!(json[event.key()].is_array(), "{}: {json}", event.key());
        }
        assert_eq!(serde_json::from_value::<HooksConfig>(json).unwrap(), config);
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert_eq!(serde_yaml::from_str::<HooksConfig>(&yaml).unwrap(), config);
    }

    #[test]
    fn hooks_config_defaults_are_allow_and_thirty_seconds() {
        let yaml = "Stop:\n  - name: s\n    bash: |\n      resume \"again\"\n";
        let config: HooksConfig = serde_yaml::from_str(yaml).expect("parse hooks config");
        let EventHookEntry::Full(definition) = &config.stop.entries[0] else {
            panic!("expected a full entry");
        };
        assert_eq!(definition.timeout, std::time::Duration::from_secs(30));
        assert_eq!(definition.on_error, EventHookOnError::Allow);
    }

    #[test]
    fn hooks_config_merges_max_resume_agent_first() {
        let global: HooksConfig = serde_yaml::from_str("max_resume: 5\n").unwrap();
        let agent: HooksConfig = serde_yaml::from_str("max_resume: 3\n").unwrap();
        assert_eq!(HooksConfig::merge(&global, &agent).max_resume, Some(3));
        assert_eq!(
            HooksConfig::merge(&global, &HooksConfig::default()).max_resume,
            Some(5)
        );
    }

    #[test]
    fn legacy_entries_key_fails_with_migration_guidance() {
        let yaml = "entries:\n  - command: hook-server --event Stop\n";
        let error =
            serde_yaml::from_str::<HooksConfig>(yaml).expect_err("legacy entries must fail");
        let message = error.to_string();
        assert!(message.contains("entries"), "{message}");
        assert!(message.contains("middleware"), "{message}");
    }

    #[test]
    fn legacy_command_entry_fails_with_migration_guidance() {
        let yaml = r#"
Stop:
  - command: /path/to/hook.sh
"#;
        let error =
            serde_yaml::from_str::<HooksConfig>(yaml).expect_err("legacy command entry must fail");
        let message = error.to_string();
        assert!(message.contains("command"), "{message}");
        assert!(message.contains(LEGACY_HOOK_MIGRATION), "{message}");
    }

    #[test]
    fn unknown_hooks_key_and_unknown_entry_field_fail() {
        let error = serde_yaml::from_str::<HooksConfig>("PreToolUse: []")
            .expect_err("tool events are not event hooks");
        assert!(error.to_string().contains("PreToolUse"), "{error}");

        let yaml = "Stop:\n  - name: s\n    bash: |\n      resume \"x\"\n    match: \".*\"\n";
        let error = serde_yaml::from_str::<HooksConfig>(yaml)
            .expect_err("middleware-only fields must fail");
        assert!(error.to_string().contains("match"), "{error}");
    }

    #[test]
    fn on_error_deny_rejected_for_stop_and_stop_failure() {
        let yaml = "Stop:\n  - name: s\n    bash: |\n      resume \"x\"\n    on_error: deny\n";
        let error = serde_yaml::from_str::<HooksConfig>(yaml)
            .expect_err("deny on Stop must fail config load");
        assert!(error.to_string().contains("on_error"), "{error}");

        let yaml =
            "StopFailure:\n  - name: s\n    bash: |\n      event_input\n    on_error: deny\n";
        assert!(serde_yaml::from_str::<HooksConfig>(yaml).is_err());

        for event in ["SessionStart", "UserPromptSubmit"] {
            let yaml = format!(
                "{event}:\n  - name: s\n    bash: |\n      event_input\n    on_error: deny\n"
            );
            assert!(
                serde_yaml::from_str::<HooksConfig>(&yaml).is_ok(),
                "deny is valid for {event}"
            );
        }
    }

    #[test]
    fn duplicate_names_and_disable_entries_validate() {
        let yaml = r#"
UserPromptSubmit:
  - name: a
    bash: |
      event_input
  - name: a
    bash: |
      event_input
"#;
        let error = serde_yaml::from_str::<HooksConfig>(yaml).expect_err("duplicate names fail");
        assert!(error.to_string().contains("duplicate"), "{error}");

        let yaml = "Stop:\n  - name: a\n    enabled: false\n    bash: |\n      resume \"x\"\n";
        assert!(serde_yaml::from_str::<HooksConfig>(yaml).is_err());

        let yaml = "Stop:\n  - name: a\n    enabled: false\n";
        let config: HooksConfig = serde_yaml::from_str(yaml).expect("bare disable entry parses");
        assert!(matches!(
            config.stop.entries[0],
            EventHookEntry::Disable { .. }
        ));

        let yaml = "Stop:\n  - name: a\n    enabled: false\n    extra: 1\n";
        assert!(serde_yaml::from_str::<HooksConfig>(yaml).is_err());
    }

    #[test]
    fn event_hook_timeout_serializes_as_human_duration() {
        let mut config = super::HooksConfig::default();
        config
            .stop
            .entries
            .push(EventHookEntry::Full(super::EventHookDefinition {
                name: "keep-going".into(),
                source: crate::middleware::MiddlewareSourceConfig::Bash {
                    bash: "resume \"continue\"".into(),
                },
                fetch: vec![],
                timeout: std::time::Duration::from_millis(1500),
                on_error: super::EventHookOnError::Allow,
            }));
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("timeout: 1s 500ms"), "{yaml}");

        let mut output = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut output);
        super::serialize_event_hook_timeout(
            &std::time::Duration::from_millis(1500),
            &mut serializer,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&output).unwrap(),
            "1s 500ms"
        );
    }

    #[test]
    fn hooks_config_default_is_empty() {
        let config = HooksConfig::default();
        assert!(config.max_resume.is_none());
        assert!(config.is_empty());
        assert!(config.list(HookEventKind::Stop).entries.is_empty());
    }

    #[test]
    fn event_hooks_list_requires_script_entries() {
        let error = serde_yaml::from_str::<EventHooksList>("- name: s\n")
            .expect_err("entry without a source fails");
        assert!(error.to_string().contains("exactly one"), "{error}");

        let list: EventHooksList =
            serde_yaml::from_str("- name: s\n  bash: |\n    event_input\n").unwrap();
        assert_eq!(list.entries.len(), 1);
    }
}
