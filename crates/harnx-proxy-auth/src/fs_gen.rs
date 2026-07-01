use std::path::{Component, Path};

use anyhow::{anyhow, bail, Result};
use jaq_all::{compile_with, data, defs, fmts};
use serde_json::Value;

pub fn eval_and_write_files(
    scripts: &[String],
    vars: &crate::filter::JaqVars,
    temp_file_root: &str,
) -> Result<()> {
    if scripts.is_empty() {
        return Ok(());
    }

    if temp_file_root.is_empty() {
        bail!("--fs requires non-empty $temp_file_root");
    }

    let root = Path::new(temp_file_root);
    let files = eval_fs_scripts(scripts, vars)?;
    write_files(root, &files)
}

fn eval_fs_scripts(
    scripts: &[String],
    vars: &crate::filter::JaqVars,
) -> Result<serde_json::Map<String, Value>> {
    let mut current = Value::Object(serde_json::Map::new());

    for script in scripts {
        current = run_fs_script(script, vars, current)?;
    }

    let Value::Object(map) = current else {
        bail!("jaq fs script must output object, got {current}");
    };

    for (key, value) in &map {
        validate_relative_path(key)?;
        if !value.is_string() {
            bail!("jaq fs script output `{key}` must be a string, got {value}");
        }
    }

    Ok(map)
}

fn run_fs_script(script: &str, vars: &crate::filter::JaqVars, input: Value) -> Result<Value> {
    let filter = compile_with(
        script,
        defs(),
        data::funs().chain(crate::filter::auth_funs()),
        vars.names(),
    )
    .map_err(|errors| anyhow!("jaq fs script compile error: {errors:?}"))?;

    let jaq_input = fmts::read::json::parse_single(input.to_string().as_bytes())
        .map_err(|error| anyhow!("fs script input conversion: {error}"))?;
    let runner = data::Runner::default();
    let mut output = None;

    data::run(
        &runner,
        &filter,
        vars.runtime_vars()?,
        std::iter::once(Ok::<_, String>(jaq_input)),
        |value| Ok::<_, String>(Some(value)),
        |result| {
            // `Exn` borrows from the filter execution; convert to an owned
            // string before it escapes the closure.
            output = Some(result.map_err(crate::filter::exn_to_string));
            Ok(())
        },
    )
    .map_err(|error| anyhow!("jaq fs script execution failed: {error:?}"))?;

    match output {
        Some(Ok(value)) => crate::filter::jaq_value_to_json(value),
        Some(Err(error)) => bail!("jaq fs script runtime error: {error}"),
        // No output (e.g. the filter is `empty`): pass the current fs-map
        // through unchanged rather than discarding accumulated state.
        None => Ok(input),
    }
}

fn write_files(root: &Path, files: &serde_json::Map<String, Value>) -> Result<()> {
    for (relative_path, contents) in files {
        validate_relative_path(relative_path)?;
        let full_path = root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = contents
            .as_str()
            .ok_or_else(|| anyhow!("jaq fs script output `{relative_path}` must be a string"))?;
        std::fs::write(full_path, contents)?;
    }

    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        bail!("jaq fs path must be relative: {path}");
    }

    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("jaq fs path must not contain `..`: {path}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("jaq fs path must be relative: {path}")
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::eval_and_write_files;
    use crate::{filter::JaqVars, sentinel::Sentinels};
    use serde_json::json;
    use tempfile::tempdir;

    fn sentinels() -> Sentinels {
        Sentinels {
            uuid_key: "11111111-2222-3333-4444-555555555555".to_owned(),
            base64_key: "QUJDREVGR0hJSktMTU5PUA==".to_owned(),
            url_base64_key: "QUJDREVGR0hJSktMTU5PUA".to_owned(),
            hex_key: "00112233445566778899aabbccddeeff".to_owned(),
            email: "fake@example.com".to_owned(),
        }
    }

    fn vars(temp_file_root: &str) -> JaqVars {
        JaqVars::new(&sentinels(), temp_file_root.to_owned(), None, Vec::new()).unwrap()
    }

    #[test]
    fn writes_nested_file() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        eval_and_write_files(
            &[r#". + {"foo/bar.txt": "hello"}"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foo/bar.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn chains_multiple_scripts() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        eval_and_write_files(
            &[
                r#". + {"a.txt": "one"}"#.to_owned(),
                r#". + {"b/c.txt": "two"}"#.to_owned(),
            ],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b/c.txt")).unwrap(),
            "two"
        );
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        let err = eval_and_write_files(
            &[r#". + {"../evil": "x"}"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not contain `..`"));
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        let err = eval_and_write_files(
            &[r#". + {"/tmp/evil": "x"}"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be relative"));
    }

    #[test]
    fn rejects_non_string_value() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        let err = eval_and_write_files(
            &[r#". + {"a.txt": 1}"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn temp_file_root_available_in_fs_script() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        eval_and_write_files(
            &[r#". + {"root.txt": $temp_file_root}"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("root.txt")).unwrap(),
            dir.path().to_str().unwrap()
        );
    }

    #[test]
    fn false_if_passes_through_without_writing() {
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        eval_and_write_files(
            &[r#"if false then . + {"a.txt": "x"} end"#.to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        assert!(!dir.path().join("a.txt").exists());
    }

    #[test]
    fn empty_output_preserves_prior_fs_map() {
        // A script that produces no output (`empty`) must pass the accumulated
        // fs-map through unchanged rather than discarding it.
        let dir = tempdir().unwrap();
        let vars = vars(dir.path().to_str().unwrap());
        eval_and_write_files(
            &[r#". + {"keep.txt": "kept"}"#.to_owned(), "empty".to_owned()],
            &vars,
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "kept"
        );
    }

    #[test]
    fn temp_file_root_empty_without_fs() {
        let vars = JaqVars::new(&sentinels(), String::new(), None, Vec::new()).unwrap();
        let output =
            crate::filter::eval_env_scripts(&[r#"{"ROOT": $temp_file_root}"#.to_owned()], &vars)
                .unwrap();
        assert_eq!(output.get("ROOT"), Some(&json!("")));
    }
}
