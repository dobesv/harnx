use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::exec_hook::ExecHookProcess;
use crate::filter::{CompiledFilter, JaqVars};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStageKind {
    Jaq,
    ExecInline,
    ExecFile,
}

pub fn stage_kind(hook: &str) -> HookStageKind {
    if hook.as_bytes().starts_with(b"#!") {
        HookStageKind::ExecInline
    } else if hook.starts_with("/") || hook.starts_with("./") || hook.starts_with("../") {
        HookStageKind::ExecFile
    } else {
        HookStageKind::Jaq
    }
}

pub fn hooks_require_unix(hooks: &[String]) -> bool {
    hooks.iter().any(|hook| {
        matches!(
            stage_kind(hook),
            HookStageKind::ExecInline | HookStageKind::ExecFile
        )
    })
}

pub fn ensure_hook_platform_support(hooks: &[String]) -> Result<()> {
    if hooks_require_unix(hooks) && cfg!(not(unix)) {
        bail!("shebang --hook scripts (resident exec stages) are only supported on Unix");
    }
    Ok(())
}

fn hook_script_source(hook: &str) -> Result<&str> {
    match stage_kind(hook) {
        HookStageKind::ExecInline => Ok(hook),
        HookStageKind::Jaq | HookStageKind::ExecFile => {
            bail!("hook `{hook}` is not an inline exec stage")
        }
    }
}

pub fn build_stages(
    hooks: &[String],
    vars: &Arc<JaqVars>,
    exec_timeout_secs: u64,
) -> Result<Vec<Stage>> {
    ensure_hook_platform_support(hooks)?;
    hooks
        .iter()
        .map(|hook| match stage_kind(hook) {
            HookStageKind::Jaq => Ok(Stage::Jaq {
                filter: Arc::new(crate::filter::compile_with_vars(hook, vars)?),
                vars: vars.clone(),
            }),
            HookStageKind::ExecInline => {
                let script = hook_script_source(hook)?;
                Ok(Stage::Exec(Arc::new(ExecHookProcess::spawn_inline(
                    script,
                    exec_timeout_secs,
                )?)))
            }
            HookStageKind::ExecFile => Ok(Stage::Exec(Arc::new(ExecHookProcess::spawn_path(
                PathBuf::from(hook),
                exec_timeout_secs,
            )?))),
        })
        .collect()
}

pub enum Stage {
    Jaq {
        filter: Arc<CompiledFilter>,
        vars: Arc<JaqVars>,
    },
    Exec(Arc<ExecHookProcess>),
}

pub struct TransformPipeline {
    stages: Vec<Stage>,
}

impl TransformPipeline {
    pub fn new(stages: Vec<Stage>) -> Self {
        Self { stages }
    }

    pub async fn apply(&self, req_json: Value) -> Value {
        let mut current = req_json;
        for stage in &self.stages {
            current = match stage {
                Stage::Jaq { filter, vars } => {
                    match crate::filter::apply_filter_with_vars(filter, current.clone(), vars) {
                        Ok(next) => next,
                        Err(err) => {
                            tracing::warn!(error = %err, "jaq hook stage failed");
                            current
                        }
                    }
                }
                Stage::Exec(process) => process.transform(current.clone()).await,
            };

            if should_short_circuit(&current) {
                break;
            }
        }
        current
    }
}

pub(crate) fn should_short_circuit(value: &Value) -> bool {
    match value {
        Value::Object(map) => has_truthy(map, "respond") || has_truthy(map, "block"),
        _ => false,
    }
}

fn has_truthy(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key)
        .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
}

#[cfg(test)]
mod tests {

    #[cfg(unix)]
    use super::ExecHookProcess;
    use super::{
        build_stages, ensure_hook_platform_support, hook_script_source, hooks_require_unix,
        stage_kind, HookStageKind, Stage, TransformPipeline,
    };
    use crate::filter::{compile_with_vars, JaqVars};
    use crate::sentinel::Sentinels;
    use serde_json::json;
    use std::sync::Arc;
    #[cfg(unix)]
    use tempfile::NamedTempFile;

    #[test]
    fn stage_kind_distinguishes_jaq_inline_and_path_exec_hooks() {
        assert_eq!(
            stage_kind("#!/bin/sh\necho hi\n"),
            HookStageKind::ExecInline
        );
        assert_eq!(
            stage_kind("./example_config/hook.py"),
            HookStageKind::ExecFile
        );
        assert_eq!(
            stage_kind("../example_config/hook.py"),
            HookStageKind::ExecFile
        );
        assert_eq!(stage_kind("/tmp/hook.py"), HookStageKind::ExecFile);
        assert_eq!(stage_kind(".headers.auth = \"x\""), HookStageKind::Jaq);
        assert_eq!(
            stage_kind("if .host == \"x\" then . else . end"),
            HookStageKind::Jaq
        );
        assert_eq!(stage_kind(" #!/bin/sh\necho hi\n"), HookStageKind::Jaq);
        assert_eq!(stage_kind(".foo = 1"), HookStageKind::Jaq);
    }

    #[test]
    fn hooks_require_unix_only_when_exec_stage_present() {
        assert!(!hooks_require_unix(&[
            ".".to_string(),
            ".headers.auth = \"x\"".to_string()
        ]));
        assert!(hooks_require_unix(&[
            ".headers.auth = \"x\"".to_string(),
            "#!/bin/sh\necho hi\n".to_string()
        ]));
        assert!(hooks_require_unix(&["./hook.py".to_string()]));
    }

    #[test]
    fn ensure_platform_support_allows_jaq() {
        assert!(ensure_hook_platform_support(&[".".to_string()]).is_ok());
    }

    #[test]
    fn empty_hooks_build_empty_pipeline() {
        let vars = test_vars();
        let stages = build_stages(&[], &vars, 30).unwrap();
        assert!(stages.is_empty());
    }

    #[test]
    fn jaq_hooks_map_one_to_one_in_order() {
        let vars = test_vars();
        let hooks = vec![
            ".headers[\"x-a\"] = \"1\"".to_string(),
            ".headers[\"x-b\"] = .headers[\"x-a\"]".to_string(),
        ];

        let stages = build_stages(&hooks, &vars, 30).unwrap();

        assert_eq!(stages.len(), hooks.len());
        assert!(stages
            .iter()
            .all(|stage| matches!(stage, Stage::Jaq { .. })));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_hooks_map_one_to_one_in_order() {
        let vars = test_vars();
        let hooks = vec![
            ".headers[\"x-a\"] = \"1\"".to_string(),
            r##"#!/bin/sh
printf 'READY
'
while IFS= read -r line; do printf '%s
' "$line"; done
"##
            .to_string(),
            ".headers[\"x-b\"] = \"2\"".to_string(),
            ".headers[\"x-c\"] = \"3\"".to_string(),
        ];

        let stages = build_stages(&hooks, &vars, 1).unwrap();

        assert_eq!(stages.len(), 4);
        assert!(matches!(stages[0], Stage::Jaq { .. }));
        assert!(matches!(stages[1], Stage::Exec(..)));
        assert!(matches!(stages[2], Stage::Jaq { .. }));
        assert!(matches!(stages[3], Stage::Jaq { .. }));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_stages_reads_exec_scripts_from_relative_and_absolute_paths() {
        let vars = test_vars();
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("nested");
        let child = nested.join("child");
        std::fs::create_dir_all(&child).unwrap();

        use std::os::unix::fs::PermissionsExt;

        let rel_script = nested.join("rel-hook.py");
        std::fs::write(&rel_script, python_exec_script("mutate")).unwrap();
        let abs_script = base.path().join("abs-hook.py");
        std::fs::write(&abs_script, python_exec_script("mutate")).unwrap();
        let parent_script = nested.join("parent-hook.py");
        std::fs::write(&parent_script, python_exec_script("mutate")).unwrap();
        for path in [&rel_script, &abs_script, &parent_script] {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&child).unwrap();
        let hooks = vec![
            "../rel-hook.py".to_string(),
            abs_script.display().to_string(),
            "../parent-hook.py".to_string(),
        ];
        let stages = build_stages(&hooks, &vars, 1).unwrap();
        std::env::set_current_dir(cwd).unwrap();

        assert_eq!(stages.len(), 3);
        assert!(stages.iter().all(|stage| matches!(stage, Stage::Exec(..))));

        let pipeline = TransformPipeline::new(stages);
        let result = pipeline
            .apply(json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/start"}))
            .await;
        assert_eq!(result["headers"]["x-exec"], "ok");
    }

    #[cfg(unix)]
    #[test]
    fn build_stages_rejects_non_executable_exec_path() {
        let vars = test_vars();
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "print('hi')\n").unwrap();
        let hook = file.path().display().to_string();

        let err = build_stages(std::slice::from_ref(&hook), &vars, 1)
            .err()
            .expect("non-executable file should fail");
        assert_eq!(
            err.to_string(),
            format!("--hook path {hook} is not executable")
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_stages_accepts_executable_exec_path_without_shebang() {
        use std::os::unix::fs::PermissionsExt;

        let vars = test_vars();
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "print('hi')\n").unwrap();
        let mut perms = std::fs::metadata(file.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(file.path(), perms).unwrap();
        let hook = file.path().display().to_string();

        let stages = build_stages(std::slice::from_ref(&hook), &vars, 1).unwrap();
        assert!(matches!(stages.as_slice(), [Stage::Exec(_)]));
    }

    #[test]
    fn hook_script_source_preserves_inline_scripts() {
        let script = "#!/bin/sh\necho hi\n";
        let source = hook_script_source(script).unwrap();
        assert_eq!(source, script);
    }

    #[cfg(unix)]
    fn python_exec_script(mode: &str) -> String {
        format!(
            "#!/usr/bin/env python3\nimport json, sys, time\nprint(\"READY\", flush=True)\nfor line in sys.stdin:\n    line = line.strip()\n    if not line:\n        continue\n    msg = json.loads(line)\n    req_id = msg.pop(\"id\")\n    if \"{mode}\" == \"timeout\":\n        time.sleep(2.0)\n    else:\n        msg.setdefault(\"headers\", {{}})[\"x-exec\"] = \"ok\"\n        msg[\"path\"] = msg.get(\"path\", \"\") + \"-exec\"\n        time.sleep(0.1)\n    msg[\"id\"] = req_id\n    print(json.dumps(msg), flush=True)\n"
        )
    }

    fn test_vars() -> Arc<JaqVars> {
        Arc::new(JaqVars::new(&Sentinels::generate(), String::new(), None, vec![]).unwrap())
    }

    fn jaq_stage(expr: &str, vars: &Arc<JaqVars>) -> Stage {
        Stage::Jaq {
            filter: Arc::new(compile_with_vars(expr, vars).unwrap()),
            vars: vars.clone(),
        }
    }

    #[tokio::test]
    async fn sequential_jaq_pipeline_matches_equivalent_single_filter() {
        let vars = test_vars();
        let input = json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/"});
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(r#".headers["x-first"] = "1""#, &vars),
            jaq_stage(r#".headers["x-second"] = .headers["x-first"]"#, &vars),
        ]);
        let combined = crate::filter::compile_with_vars(
            r#".headers["x-first"] = "1" | .headers["x-second"] = .headers["x-first"]"#,
            &vars,
        )
        .unwrap();

        let sequential = pipeline.apply(input.clone()).await;
        let combined = crate::filter::apply_filter_with_vars(&combined, input, &vars).unwrap();

        assert_eq!(sequential, combined);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_stage_isolated_between_requests() {
        let exec =
            Arc::new(ExecHookProcess::spawn_inline(&python_exec_script("mutate"), 1).unwrap());
        let pipeline = TransformPipeline::new(vec![Stage::Exec(exec)]);

        let first = pipeline
            .apply(json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/a"}))
            .await;
        let second = pipeline
            .apply(json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/b"}))
            .await;

        assert_eq!(first["path"], "/a-exec");
        assert_eq!(second["path"], "/b-exec");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_jaq_and_exec_pipeline_applies_in_declared_order() {
        let vars = test_vars();
        let exec =
            Arc::new(ExecHookProcess::spawn_inline(&python_exec_script("mutate"), 1).unwrap());
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(
                r#".headers["x-order"] = "jaq-1" | .path = .path + "-jaq1""#,
                &vars,
            ),
            Stage::Exec(exec),
            jaq_stage(
                r#".headers["x-order"] = .headers["x-order"] + "+jaq-2" | .path = .path + "-jaq2""#,
                &vars,
            ),
        ]);

        let result = pipeline
            .apply(json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/start"}))
            .await;

        assert_eq!(result["path"], "/start-jaq1-exec-jaq2");
        assert_eq!(result["headers"]["x-order"], "jaq-1+jaq-2");
        assert_eq!(result["headers"]["x-exec"], "ok");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_timeout_passes_through_and_later_stages_still_run() {
        let vars = test_vars();
        let exec =
            Arc::new(ExecHookProcess::spawn_inline(&python_exec_script("timeout"), 1).unwrap());
        let input = json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/slow"});
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(r#".headers["x-before-timeout"] = "set""#, &vars),
            Stage::Exec(exec),
            jaq_stage(r#".headers["x-after-timeout"] = "still-ran""#, &vars),
        ]);

        let result = pipeline.apply(input).await;

        assert_eq!(result["path"], "/slow");
        assert_eq!(result["headers"]["x-before-timeout"], "set");
        assert_eq!(result["headers"]["x-after-timeout"], "still-ran");
        assert!(result["headers"].get("x-exec").is_none());
    }

    #[tokio::test]
    async fn respond_stage_short_circuits_later_stages() {
        let vars = test_vars();
        let input = json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/"});
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(
                r#".respond = {status: 201, headers: {"content-type": "application/json"}, body: "{}"}"#,
                &vars,
            ),
            jaq_stage(
                r#".respond.body = "clobbered" | .headers["x-after"] = "ran""#,
                &vars,
            ),
        ]);

        let result = pipeline.apply(input).await;

        assert_eq!(result["respond"]["status"], 201);
        assert_eq!(result["respond"]["body"], "{}");
        assert!(result["headers"].get("x-after").is_none());
    }

    #[tokio::test]
    async fn block_stage_short_circuits_later_stages() {
        let vars = test_vars();
        let input = json!({"headers": {}, "host": "example.com", "method": "GET", "path": "/"});
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(r#".block = "stop now""#, &vars),
            jaq_stage(r#".block = false | .headers["x-after"] = "ran""#, &vars),
        ]);

        let result = pipeline.apply(input).await;

        assert_eq!(result["block"], "stop now");
        assert!(result["headers"].get("x-after").is_none());
    }
}
