use super::*;
use crate::config::test_support::{env_lock, EnvGuard};
use harnx_core::script_tool::{ScriptToolOutput, ScriptToolSource};
use harnx_core::tool::ToolDeclaration;
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;

fn definition(name: &str) -> Value {
    json!({
        "name": name,
        "description": "Summarize an issue.",
        "parameters": {
            "type": "object",
            "properties": {"number": {"type": "integer"}},
            "required": ["number"],
            "additionalProperties": false
        },
        "bash": "tool_input"
    })
}

fn write(root: &Path, path: &str, value: &Value) -> PathBuf {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_yaml::to_string(value).unwrap()).unwrap();
    path
}

fn load_error(path: &Path) -> String {
    format!("{:#}", Config::load_from_file(path).unwrap_err())
}

/// `Path::join` keeps `/` inside joined literals, while `read_dir` reports
/// `\` on Windows; error messages carry whichever produced the path. Compare
/// with both sides normalized so the assertions hold on every platform.
fn forward_slashes(text: &str) -> String {
    text.replace('\\', "/")
}

/// Exercise the real loader for both supported sources, including serde errors.
fn assert_invalid_definition(value: &Value, expected: &str) {
    for package in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
        let config_path = write(root, "config.yaml", &json!({}));
        let source = if package {
            write(root, "packages/pkg/script_tools/tool.yaml", value)
        } else {
            write(root, "config.yaml", &json!({"script_tools": [value]}))
        };
        let error = forward_slashes(&load_error(&config_path));
        assert!(
            error.contains(&forward_slashes(source.to_str().unwrap())),
            "{error}"
        );
        assert!(error.contains(value["name"].as_str().unwrap()), "{error}");
        assert!(error.contains(expected), "expected {expected}: {error}");
    }
}

#[test]
fn script_tools_load_global_and_package_definitions_with_compiled_validators() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let global = definition("global_brief");
    let file = write(root, "config.yaml", &json!({"script_tools": [global]}));
    let mut python = definition("package_brief");
    python.as_object_mut().unwrap().remove("bash");
    python["python"] = json!("print(tool_input())");
    python["timeout"] = json!("1m 250ms");
    python["output"] = json!("json");
    python["tools"] = json!(["fs_read", "fs_*", "toolset"]);
    let package_file = write(
        root,
        "packages/pkg/script_tools/different_stem.yaml",
        &python,
    );
    write(
        root,
        "packages/pkg/script_tools/ignored.yml",
        &json!("invalid"),
    );
    write(
        root,
        "packages/pkg/script_tools/nested/ignored.yaml",
        &json!("invalid"),
    );
    fs::create_dir(root.join("packages/pkg/script_tools/directory.yaml")).unwrap();
    let config = Config::load_from_file(&file).unwrap();
    let catalog = config.script_tool_catalog();
    assert_eq!(catalog.len(), 2);
    assert!(!catalog.is_empty());
    assert_eq!(
        catalog
            .iter()
            .map(|t| t.definition().name.as_str())
            .collect::<Vec<_>>(),
        ["global_brief", "package_brief"]
    );
    let global = catalog.get("global_brief").unwrap();
    assert_eq!(global.definition().timeout, Duration::from_secs(300));
    assert_eq!(global.definition().output, ScriptToolOutput::Text);
    assert_eq!(global.location().path, file);
    assert_eq!(global.location().index, Some(0));
    assert_eq!(global.location().package, None);
    assert!(matches!(
        global.definition().source,
        ScriptToolSource::Bash { .. }
    ));
    let packaged = catalog.get("package_brief").unwrap();
    assert_eq!(packaged.location().path, package_file);
    assert_eq!(packaged.location().package.as_deref(), Some("pkg"));
    assert_eq!(packaged.location().index, None);
    assert_eq!(packaged.definition().timeout, Duration::from_millis(60_250));
    assert_eq!(packaged.definition().output, ScriptToolOutput::Json);
    assert!(matches!(
        packaged.definition().source,
        ScriptToolSource::Python { .. }
    ));
    assert!(packaged.validator().is_valid(&json!({"number": 2813})));
    assert!(packaged
        .validate_arguments(&json!({"number": 2813}))
        .is_ok());
    let error = packaged
        .validate_arguments(&json!({"number": "wrong"}))
        .unwrap_err()
        .to_string();
    let error = forward_slashes(&error);
    assert!(
        error.contains("package_brief")
            && error.contains("/number")
            && error.contains(&forward_slashes(package_file.to_str().unwrap())),
        "{error}"
    );
    let cloned = config.clone();
    let forked = config.fork_session_scope();
    assert!(Arc::ptr_eq(
        packaged,
        cloned.script_tool_catalog().get("package_brief").unwrap()
    ));
    assert!(Arc::ptr_eq(
        packaged,
        forked.script_tool_catalog().get("package_brief").unwrap()
    ));
    assert_eq!(config.data.script_tools.len(), 1);
}

#[test]
fn script_tools_reject_invalid_shapes_with_source_and_name() {
    let _lock = env_lock();
    for (field, value, expected) in [
        ("python", json!("print('both')"), "exactly one"),
        ("typo", json!(true), "unknown field"),
        ("timeout", json!("0s"), "greater than zero"),
        ("timeout", json!(300), "expected a string"),
        ("output", json!("yaml"), "unknown variant"),
    ] {
        let mut tool = definition("invalid_tool");
        tool[field] = value;
        assert_invalid_definition(&tool, expected);
    }
    let mut neither = definition("neither");
    neither.as_object_mut().unwrap().remove("bash");
    assert_invalid_definition(&neither, "exactly one");
}

#[test]
fn script_tools_reject_invalid_names_with_source() {
    let _lock = env_lock();
    for name in [
        "",
        "bad.name",
        "bad name",
        "pkg/name",
        "café",
        &"x".repeat(65),
    ] {
        assert_invalid_definition(&definition(name), "name must");
    }
}

#[test]
fn script_tools_reject_reserved_definition_names() {
    let _lock = env_lock();
    for name in [
        "bash",
        "switch_agent",
        "ask_user",
        "foo_session_handoff",
        "_session_handoff",
    ] {
        assert_invalid_definition(&definition(name), "reserved tool name");
    }
    for (name, _) in BUILTIN_TOOLS {
        assert_invalid_definition(&definition(name), "reserved tool name");
    }
}

#[test]
fn script_tools_reject_invalid_json_schemas_at_load_time() {
    let _lock = env_lock();
    for schema in [
        json!({}),
        json!({"type": "array"}),
        json!({"type": ["object", "null"]}),
        json!(true),
    ] {
        let mut tool = definition("bad_schema");
        tool["parameters"] = schema;
        assert_invalid_definition(&tool, "top-level type: object");
    }
    for schema in [
        json!({"type": "object", "required": "not an array"}),
        json!({"type": "object", "properties": {"number": {"type": "nonsense"}}}),
        json!({"type": "object", "properties": {"number": {"pattern": "["}}}),
        json!({"type": "object", "$ref": "#/$defs/missing"}),
    ] {
        let mut tool = definition("bad_schema");
        tool["parameters"] = schema;
        assert_invalid_definition(&tool, "invalid parameters JSON Schema");
    }
}

#[test]
fn script_tools_schema_refs_never_fetch_http_or_host_files() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let local_schema = temp.path().join("schema.json");
    fs::write(&local_schema, r#"{"type":"integer"}"#).unwrap();
    let local_schema_path = local_schema.to_string_lossy().replace('\\', "/");
    let local_schema_uri = if local_schema_path.starts_with('/') {
        format!("file://{local_schema_path}")
    } else {
        format!("file:///{local_schema_path}")
    };
    for (reference, disabled_feature) in [
        ("https://example.invalid/schema".to_owned(), "resolve-http"),
        (local_schema_uri, "resolve-file"),
    ] {
        let mut tool = definition("external_schema");
        tool["parameters"] =
            json!({"type": "object", "properties": {"number": {"$ref": reference}}});
        // A network/parse failure isn't enough: retrieval must be disabled.
        assert_invalid_definition(&tool, &format!("`{disabled_feature}` feature"));
    }
}

#[test]
fn script_tools_local_refs_and_schema_defaults_are_preserved_without_injection() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let mut tool = definition("local_schema");
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "$defs": {"number": {"type": "integer", "minimum": 1}},
        "properties": {"number": {"$ref": "#/$defs/number", "default": 2813}}
    });
    tool["parameters"] = schema.clone();
    let file = write(temp.path(), "config.yaml", &json!({"script_tools": [tool]}));
    let config = Config::load_from_file(&file).unwrap();
    let resolved = config.script_tool_catalog().get("local_schema").unwrap();
    let arguments = json!({});
    resolved.validate_arguments(&arguments).unwrap();
    assert_eq!(arguments, json!({}));
    assert_eq!(resolved.definition().parameters.as_value(), &schema);
    assert!(resolved.validate_arguments(&json!({"number": 0})).is_err());
    assert!(resolved
        .validate_arguments(&json!({"number": 2813}))
        .is_ok());
}

#[test]
fn script_tools_duplicates_name_both_sources() {
    let _lock = env_lock();
    for pair in [
        ["config.yaml", "packages/pkg/script_tools/one.yaml"],
        [
            "packages/a/script_tools/one.yaml",
            "packages/b/script_tools/two.yaml",
        ],
        [
            "packages/pkg/script_tools/one.yaml",
            "packages/pkg/script_tools/two.yaml",
        ],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
        let file = write(root, "config.yaml", &json!({}));
        for path in pair {
            let value = if path == "config.yaml" {
                json!({"script_tools": [definition("duplicate")]})
            } else {
                definition("duplicate")
            };
            write(root, path, &value);
        }
        let error = forward_slashes(&load_error(&file));
        for expected in ["duplicate script tool 'duplicate'", pair[0], pair[1]] {
            assert!(error.contains(expected), "{error}");
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let file = write(
        temp.path(),
        "config.yaml",
        &json!({"script_tools": [definition("duplicate"), definition("duplicate")]}),
    );
    let error = load_error(&file);
    let error = forward_slashes(&error);
    let file_label = forward_slashes(file.to_str().unwrap());
    for expected in [
        file_label.as_str(),
        "script_tools[0]",
        "script_tools[1]",
        "duplicate",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

/// A symlink alias of a package root must load its definitions once;
/// loading both would reject the duplicated names and abort startup.
#[test]
#[cfg(unix)]
fn script_tools_symlinked_package_roots_load_once() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", &json!({}));
    write(
        root,
        "packages/pkg/script_tools/one.yaml",
        &definition("aliased"),
    );
    std::os::unix::fs::symlink(root.join("packages/pkg"), root.join("packages/alias")).unwrap();
    let config = Config::load_from_file(&file).unwrap();
    assert_eq!(
        config
            .script_tool_catalog()
            .iter()
            .map(|tool| tool.definition().name.as_str())
            .collect::<Vec<_>>(),
        ["aliased"]
    );
    assert_eq!(
        config
            .script_tool_catalog()
            .get("aliased")
            .unwrap()
            .location()
            .package
            .as_deref(),
        Some("alias"),
        "the first entry in sorted-name order wins"
    );
}

#[test]
fn script_tools_reject_literal_control_tools_and_allow_globs() {
    let _lock = env_lock();
    for selector in [
        "bash",
        "switch_agent",
        "ask_user",
        "foo_session_handoff",
        "switch.agent",
        "literal.self",
        " literal_self ",
    ] {
        let mut tool = definition("literal_self");
        tool["tools"] = json!([selector]);
        assert_invalid_definition(&tool, "names an excluded tool");
    }
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let mut tool = definition("glob_tool");
    tool["tools"] = json!([
        "*",
        "bash*",
        "*_session_handoff",
        "glob_*",
        "[ab]*",
        "{bash,ask_user}",
        "session_history"
    ]);
    let file = write(temp.path(), "config.yaml", &json!({"script_tools": [tool]}));
    assert_eq!(
        Config::load_from_file(&file)
            .unwrap()
            .script_tool_catalog()
            .len(),
        1
    );
}

#[test]
fn script_tools_reject_forward_and_cross_source_script_selectors() {
    let _lock = env_lock();
    for global_caller in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
        let mut caller = definition("caller");
        caller["tools"] = json!(["callee"]);
        let file = write(
            root,
            "config.yaml",
            &json!({"script_tools": [if global_caller { caller.clone() } else { definition("callee") }]}),
        );
        let package_file = write(
            root,
            "packages/pkg/script_tools/tool.yaml",
            &if global_caller {
                definition("callee")
            } else {
                caller
            },
        );
        let error = load_error(&file);
        let error = forward_slashes(&error);
        let source = if global_caller { &file } else { &package_file };
        for expected in [
            "caller",
            "callee",
            &forward_slashes(source.to_str().unwrap()),
            "names an excluded tool",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }
}

#[test]
fn script_tools_load_packages_without_global_config_or_with_dynamic_provider() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    write(
        temp.path(),
        "packages/pkg/script_tools/tool.yaml",
        &definition("packaged"),
    );
    let file = temp.path().join("config.yaml");
    let config = Config::load_from_file(&file).unwrap();
    assert!(config.script_tool_catalog().get("packaged").is_some());
    let dynamic = Config::load_dynamic("openai:gpt-4o").unwrap();
    assert!(dynamic.script_tool_catalog().get("packaged").is_some());
}

// ---------------------------------------------------------------------
// Accepted view (decision 12) and inner-allowlist resolution (decision 5).
// ---------------------------------------------------------------------

fn in_memory_config(definitions: &[Value]) -> Config {
    let definitions: Vec<harnx_core::script_tool::ScriptToolDefinition> = definitions
        .iter()
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .collect();
    let mut config = Config::default();
    config
        .set_script_tool_definitions(&definitions)
        .expect("in-memory script tools resolve");
    config
}

fn nats_declaration(name: &str) -> ToolDeclaration {
    serde_json::from_value(json!({
        "name": name,
        "description": "Probe",
        "parameters": {"type": "object"},
    }))
    .unwrap()
}

fn seed_nats_declarations(config: &mut Config, names: &[&str]) {
    *config.nats_tool_declarations.write() =
        names.iter().map(|name| nats_declaration(name)).collect();
}

fn accepted_names(config: &Config) -> Vec<String> {
    config
        .accepted_script_tools()
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect()
}

/// Declarations of the accepted view, mirroring the round snapshot's path.
fn accepted_declarations(config: &Config) -> Vec<ToolDeclaration> {
    config
        .accepted_script_tools()
        .iter()
        .map(|tool| crate::config::script_tools::script_tool_declaration(tool.definition()))
        .collect()
}

#[test]
fn accepted_view_excludes_base_tool_name_collisions() {
    let mut config = in_memory_config(&[definition("wrap"), definition("probe_run")]);
    seed_nats_declarations(&mut config, &["probe_run", "probe_aux"]);
    // `wrap` survives; `probe_run` collides with the NATS tool of the same
    // name and leaves the accepted view entirely.
    assert_eq!(accepted_names(&config), vec!["wrap"]);
    let declarations = accepted_declarations(&config);
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0].name, "wrap");
    assert_eq!(
        declarations[0].parameters.as_value()["properties"]["number"]["type"],
        "integer",
        "the definition's schema must be the advertised schema"
    );
}

#[test]
fn accepted_view_recomputes_when_nats_declarations_refresh() {
    let mut config = in_memory_config(&[definition("wrap")]);
    assert_eq!(accepted_names(&config), vec!["wrap"]);
    // A later base-tool discovery that takes the name removes the script tool
    // from both declaration selection and dispatch.
    seed_nats_declarations(&mut config, &["wrap"]);
    assert!(accepted_names(&config).is_empty());
    assert!(accepted_declarations(&config).is_empty());
}

#[test]
fn inner_declarations_exclude_script_and_control_tools_even_under_globs() {
    let mut wrap = definition("wrap");
    wrap["tools"] = json!(["probe_*"]);
    let mut helper = definition("probe_helper");
    helper["tools"] = json!([]);
    let mut config = in_memory_config(&[wrap, helper]);
    seed_nats_declarations(&mut config, &["probe_run", "probe_aux"]);
    let config = &config;
    let wrap = config.script_tool_catalog().get("wrap").unwrap();
    let names: Vec<String> = config
        .script_tool_inner_declarations(&wrap.definition().tools, None)
        .iter()
        .map(|declaration| declaration.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["probe_run", "probe_aux"],
        "globs skip the script tool the glob also matches, and keep base tools"
    );

    // Literal selectors for excluded tools were rejected at load time; a
    // literal naming an unknown tool simply resolves to nothing.
    let unknown = config.script_tool_catalog().get("probe_helper").unwrap();
    assert!(config
        .script_tool_inner_declarations(&unknown.definition().tools, None)
        .is_empty());
    assert!(config
        .script_tool_inner_declarations(&["missing_tool".to_string()], None)
        .is_empty());
    // Control tools are never callable even if a resolution would match.
    *config.nats_tool_declarations.write() = vec![nats_declaration("ask_user")];
    assert!(config
        .script_tool_inner_declarations(&["ask_user".to_string()], None)
        .is_empty());
}

#[test]
fn inner_selectors_follow_selection_by_name_glob_and_toolset() {
    let mut wrap = definition("wrap");
    wrap["tools"] = json!(["fs_read"]);
    let mut glob = definition("globbed");
    glob["tools"] = json!(["fs_write"]);
    let mut member = definition("member");
    member["tools"] = json!(["fs_list"]);
    let mut unselected = definition("unselected");
    unselected["tools"] = json!(["never_started"]);
    let mut config = in_memory_config(&[wrap, glob, member, unselected]);
    config.toolsets.insert(
        "toolset".to_string(),
        vec!["member".to_string(), "other".to_string()],
    );
    for selectors in [
        vec!["wrap".to_string()],
        vec!["glob*".to_string()],
        vec!["toolset".to_string()],
    ] {
        let selectors_for = config.script_tool_inner_selectors_for(&selectors);
        let expected = match selectors[0].as_str() {
            "wrap" => "fs_read",
            "glob*" => "fs_write",
            _ => "fs_list",
        };
        assert_eq!(
            selectors_for,
            vec![expected.to_string()],
            "selectors {selectors:?} must pull in only their script tool's tools"
        );
    }
    // Unselected script tools never start servers for the tools they wrap.
    assert!(config
        .script_tool_inner_selectors_for(&["wrap".to_string()])
        .iter()
        .all(|selector| selector != "never_started"));
    assert!(config.script_tool_inner_selectors_for(&[]).is_empty());
    // A dotted selector selects the sanitized script-tool name at runtime
    // (`collect_selected_tool_names`), so startup must resolve it too.
    let mut dotted = definition("wrap_brief");
    dotted["tools"] = json!(["fs_stat"]);
    let dotted_config = in_memory_config(&[dotted]);
    assert_eq!(
        dotted_config.script_tool_inner_selectors_for(&["wrap.brief".to_string()]),
        vec!["fs_stat".to_string()],
        "a dotted selector must match the sanitized script-tool name"
    );
}
