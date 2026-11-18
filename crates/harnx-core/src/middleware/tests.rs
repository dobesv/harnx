use super::*;
use crate::{agent_config::AgentConfig, config_data::ConfigData};

#[test]
fn middleware_defaults_and_all_source_forms() {
    for (source, expected) in [
        (
            "bash: next",
            MiddlewareSourceConfig::Bash {
                bash: "next".into(),
            },
        ),
        (
            "python: call_next()",
            MiddlewareSourceConfig::Python {
                python: "call_next()".into(),
            },
        ),
        (
            "script: hooks/audit.sh",
            MiddlewareSourceConfig::Script {
                script: "hooks/audit.sh".into(),
            },
        ),
        (
            "script: hooks/audit.bash",
            MiddlewareSourceConfig::Script {
                script: "hooks/audit.bash".into(),
            },
        ),
        (
            "script: hooks/audit.py",
            MiddlewareSourceConfig::Script {
                script: "hooks/audit.py".into(),
            },
        ),
    ] {
        let entry: MiddlewareEntry =
            serde_yaml::from_str(&format!("name: audit\nmatch: '.*'\n{source}")).unwrap();
        let MiddlewareEntry::Full(definition) = entry else {
            panic!("expected full entry")
        };
        assert_eq!(definition.source, expected);
        assert_eq!(definition.timeout, Duration::from_secs(30));
        assert_eq!(definition.on_error, MiddlewareOnError::Deny);
        assert!(definition.tools.is_empty());
    }
}

#[test]
fn middleware_strict_errors_name_the_entry() {
    for (fields, diagnostic) in [
        ("match: '.*'\nbash: next\nextra: true", "unknown field"),
        (
            "match: '.*'\nbash: next\npython: call_next()",
            "exactly one",
        ),
        ("match: '.*'\nbash: next\nscript: audit.sh", "exactly one"),
        ("match: '.*'", "exactly one"),
        ("bash: next", "requires match"),
        ("match: '['\nbash: next", "invalid match regex"),
        ("match: '.*'\nscript: audit.js", "extension"),
        (
            "match: '.*'\nbash: next\non_error: ignore",
            "unknown variant",
        ),
        (
            "match: '.*'\nbash: next\ntimeout: tomorrow",
            "invalid timeout",
        ),
        ("match: '.*'\nbash: next\ntimeout: 0s", "greater than zero"),
        ("match: '.*'\nbash: next\npython: null", "must not be null"),
    ] {
        let error = serde_yaml::from_str::<MiddlewareEntry>(&format!("name: audit\n{fields}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("middleware 'audit'"), "{error}");
        assert!(error.contains(diagnostic), "{error}");
    }
}

#[test]
fn middleware_disable_is_only_name_and_enabled_false() {
    let entry: MiddlewareEntry = serde_yaml::from_str("name: audit\nenabled: false").unwrap();
    assert!(matches!(
        entry,
        MiddlewareEntry::Disable { enabled: false, .. }
    ));
    for field in [
        "match: '.*'",
        "bash: next",
        "script: missing.sh",
        "tools: []",
        "timeout: 30s",
        "on_error: deny",
    ] {
        let error = serde_yaml::from_str::<MiddlewareEntry>(&format!(
            "name: audit\nenabled: false\n{field}"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("audit"), "{error}");
        assert!(error.contains("only name and enabled"), "{error}");
    }
}

#[test]
fn middleware_duplicate_names_rejected_at_each_level() {
    let entries = "[{name: audit, match: '.*', bash: next}, {name: audit, enabled: false}]";
    let global = serde_yaml::from_str::<ConfigData>(&format!("middleware: {entries}")).unwrap_err();
    assert!(global.to_string().contains("audit"));
    assert!(global.to_string().contains("duplicate name"));
    let agent =
        AgentConfig::from_markdown("test", &format!("---\nmiddleware: {entries}\n---\nPrompt"))
            .unwrap_err();
    assert!(agent.to_string().contains("duplicate name"));
}

#[test]
fn middleware_roundtrips_agent_frontmatter_without_dropping_it() {
    let markdown = "---\nmiddleware:\n  - name: audit\n    match: '(?<=bash_)exec$'\n    python: call_next()\n    tools: [fs_*, '@files']\n    timeout: 1m 500ms\n    on_error: allow\n  - name: gate\n    enabled: false\n---\nPrompt";
    let agent = AgentConfig::from_markdown("test", markdown).unwrap();
    let exported = agent.export().unwrap();
    assert!(exported.starts_with("---\n"));
    let reparsed = AgentConfig::from_markdown("test", &exported).unwrap();
    assert_eq!(agent.middleware, reparsed.middleware);
    let MiddlewareEntry::Full(definition) = &agent.middleware.entries[0] else {
        panic!("full entry")
    };
    assert_eq!(definition.timeout, Duration::from_millis(60_500));
    assert_eq!(definition.on_error, MiddlewareOnError::Allow);
}

#[test]
fn middleware_leaves_legacy_hooks_permissive_and_unchanged() {
    let legacy = "hooks:\n  max_resume: 3\n  entries:\n    - command: legacy-hook --event PreToolUse\n      status_message: Checking\n      async: true\n      old_extra_key: accepted\n";
    let global: ConfigData = serde_yaml::from_str(legacy).unwrap();
    let combined: ConfigData = serde_yaml::from_str(&format!(
        "{legacy}middleware: [{{name: audit, match: '.*', bash: next}}]\n"
    ))
    .unwrap();
    assert_eq!(global.hooks, combined.hooks);
    let agent = AgentConfig::from_markdown("test", &format!("---\n{legacy}---\nPrompt")).unwrap();
    assert_eq!(agent.hooks(), global.hooks.as_ref());
}

fn resolved() -> ResolvedMiddleware {
    ResolvedMiddleware {
        name: "audit".into(),
        matcher: Regex::new(".*").unwrap(),
        source: MiddlewareSource::Bash("next".into()),
        tools: vec!["fs_*".into()],
        fetch: vec![],
        timeout: Duration::from_secs(30),
        on_error: MiddlewareOnError::Deny,
        origin: MiddlewareOrigin {
            file: "config.yaml".into(),
            base_dir: ".".into(),
        },
    }
}

#[test]
fn middleware_version_covers_every_effective_setting_and_chain_order() {
    let original = resolved();
    let version = ResolvedMiddlewareChain::new(vec![original.clone()]).version;
    let mut changed = Vec::new();
    let mut entry = original.clone();
    entry.name.push('2');
    changed.push(entry);
    let mut entry = original.clone();
    entry.matcher = Regex::new("^bash_exec$").unwrap();
    changed.push(entry);
    let mut entry = original.clone();
    entry.source = MiddlewareSource::Bash("next; echo changed".into());
    changed.push(entry);
    let mut entry = original.clone();
    entry.source = MiddlewareSource::Python("next".into());
    changed.push(entry);
    let mut entry = original.clone();
    entry.tools.push("time_*".into());
    changed.push(entry);
    let mut entry = original.clone();
    entry.timeout += Duration::from_nanos(1);
    changed.push(entry);
    let mut entry = original.clone();
    entry.on_error = MiddlewareOnError::Allow;
    changed.push(entry);
    for entry in changed {
        assert_ne!(version, ResolvedMiddlewareChain::new(vec![entry]).version);
    }
    assert_eq!(
        version,
        ResolvedMiddlewareChain::new(vec![original.clone()]).version
    );
    let mut moved = original.clone();
    moved.origin.file = "elsewhere.yaml".into();
    assert_eq!(version, ResolvedMiddlewareChain::new(vec![moved]).version);
    let mut second = original.clone();
    second.name = "second".into();
    assert_ne!(
        ResolvedMiddlewareChain::new(vec![original.clone(), second.clone()]).version,
        ResolvedMiddlewareChain::new(vec![second, original]).version
    );
}

#[test]
fn middleware_serialize_timeout_preserves_subsecond_precision() {
    let duration = Duration::from_millis(60_500);
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut output);
    serialize_timeout(&duration, &mut serializer).unwrap();
    assert_eq!(
        serde_json::from_slice::<String>(&output).unwrap(),
        "1m 500ms"
    );
}

#[test]
fn middleware_fetch_config_roundtrip_and_version() {
    let mut value = serde_json::json!({"name":"audit", "match":".*", "bash":"next", "fetch":["Example.COM", "[::1]"]});
    let entry: MiddlewareEntry = serde_json::from_value(value.clone()).unwrap();
    let MiddlewareEntry::Full(definition) = &entry else {
        panic!("expected full")
    };
    assert_eq!(definition.fetch, ["example.com", "[::1]"]);
    let encoded = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        serde_json::from_value::<MiddlewareEntry>(encoded).unwrap(),
        entry
    );
    for invalid in [
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(["*.example.com"]),
        serde_json::json!(["https://example.com"]),
    ] {
        value["fetch"] = invalid;
        let error = serde_json::from_value::<MiddlewareEntry>(value.clone()).unwrap_err();
        assert!(error.to_string().contains("middleware 'audit'"), "{error}");
    }
    let original = resolved();
    let version = ResolvedMiddlewareChain::new(vec![original.clone()]).version;
    let mut changed = original;
    changed.fetch = vec!["example.com".into()];
    assert_ne!(
        version,
        ResolvedMiddlewareChain::new(vec![changed.clone()]).version
    );
    let with_fetch = ResolvedMiddlewareChain::new(vec![changed.clone()]).version;
    changed.fetch.push("other.example.com".into());
    assert_ne!(
        with_fetch,
        ResolvedMiddlewareChain::new(vec![changed]).version
    );
}
