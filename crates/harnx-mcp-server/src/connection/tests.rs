use super::*;
use serde_json::json;

fn declaration(name: &str) -> ToolDeclaration {
    serde_json::from_value(json!({"name":name, "description":"provider schema", "parameters":{"type":"object", "properties":{}}})).unwrap()
}

fn names(
    config: &Config,
    package: Option<&str>,
    selectors: &[&str],
    declarations: &[ToolDeclaration],
) -> Vec<String> {
    selected_tools(
        config,
        &ToolReservationView {
            package: package.map(str::to_owned),
            use_tools: selectors.iter().map(|s| (*s).to_owned()).collect(),
        },
        declarations,
    )
    .unwrap()
    .into_iter()
    .map(|t| t.name.to_string())
    .collect()
}

#[test]
fn whitelist_uses_exact_agent_names_not_provider_raw_aliases_or_servers() {
    let config = Config::default();
    let declarations = [
        declaration("fs_read"),
        declaration("fs_write"),
        declaration("pkg__fs_read"),
    ];
    assert!(names(&config, None, &["read", "fs"], &declarations).is_empty());
    assert_eq!(
        names(&config, None, &["fs_read"], &declarations),
        ["fs_read"]
    );
    assert_eq!(
        names(&config, None, &["fs_*"], &declarations),
        ["fs_read", "fs_write"]
    );
    assert_eq!(
        names(&config, None, &["pkg__fs_read"], &declarations),
        ["pkg__fs_read"]
    );
    assert!(names(&config, None, &["missing_*"], &declarations).is_empty());
}

#[test]
fn package_naming_aliases_and_cache_are_connection_local() {
    let mut config = Config::default();
    config.toolsets.insert(
        "chosen".to_owned(),
        vec![
            "fs_read".to_owned(),
            "other__fs_write".to_owned(),
            "session_history".to_owned(),
        ],
    );
    config
        .nats_tool_declarations
        .write()
        .push(declaration("old_tool"));
    let same_package = [declaration("fs_read"), declaration("other__fs_write")];
    let cross_package = [declaration("pkg__fs_read"), declaration("fs_write")];
    assert_eq!(
        names(&config, Some("pkg"), &["chosen"], &same_package),
        ["fs_read", "other__fs_write"]
    );
    assert_eq!(
        names(&config, None, &["*"], &cross_package),
        ["pkg__fs_read", "fs_write"]
    );
    assert_eq!(config.nats_tool_declarations.read()[0].name, "old_tool");
    assert!(names(
        &config,
        None,
        &["old_tool", "session_history", "agent_session_handoff"],
        &cross_package
    )
    .is_empty());
}

#[test]
fn provider_annotations_and_schema_survive_selection() {
    let mut d = declaration("fs_read");
    d.read_only_hint = Some(true);
    d.idempotent_hint = Some(false);
    let tools = selected_tools(
        &Config::default(),
        &ToolReservationView {
            package: None,
            use_tools: vec!["*".to_owned()],
        },
        &[d],
    )
    .unwrap();
    assert_eq!(tools[0].description.as_deref(), Some("provider schema"));
    assert_eq!(tools[0].input_schema.get("type"), Some(&json!("object")));
    let annotations = tools[0].annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.idempotent_hint, Some(false));
}

#[test]
fn disabled_tool_use_remains_empty() {
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            tool_use: false,
            ..Default::default()
        },
        ..Config::default()
    };
    assert!(names(&config, None, &["*"], &[declaration("fs_read")]).is_empty());
}
