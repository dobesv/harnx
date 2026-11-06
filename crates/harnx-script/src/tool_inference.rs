//! Bounded static inference of command names from an operator Bash script.
//!
//! `harnx call bash` (#2630) uses this to pick candidate inner tools before
//! execution. The pinned BashKit parser does the walking — functions,
//! pipelines, conditionals, loops and command substitutions are all covered
//! by its analysis. The script is never executed here, and inference stays
//! bounded by the parser's own depth/fuel/node limits.

use serde::{Deserialize, Serialize};

/// Statically known command names for one script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptToolInference {
    /// Distinct literal command names in first-seen order.
    ///
    /// Interpreter builtins stay in the list: a registered tool can replace a
    /// builtin, so eligibility is decided against the real tool inventory by
    /// the caller, not here.
    pub names: Vec<String>,
    /// The script hides commands from static analysis: a dynamic command
    /// name, interpreter re-entry (`eval`/`source`/nested `bash`/`sh`), or a
    /// truncated analysis walk. Callers must use this to stay conservative;
    /// it never justifies widening selection to every tool.
    pub opaque: bool,
    /// Distinct wrapper commands (`env`, `sudo`, `timeout`, ...) that run
    /// another command in their own arguments. The wrapped name is not
    /// inferred, so it may be missing from `names`.
    pub wrappers: Vec<String>,
}

/// Infer literal command names using the pinned BashKit parser.
///
/// A script that does not parse is a hard error: callers must surface it, not
/// turn it into an empty or wildcard selection.
pub fn infer_script_tools(script: &str) -> Result<ScriptToolInference, String> {
    let analysis = bashkit::analysis::analyze(script)
        .map_err(|error| format!("cannot analyze bash script: {error}"))?;
    Ok(ScriptToolInference {
        names: analysis
            .command_names()
            .into_iter()
            .map(String::from)
            .collect(),
        opaque: analysis.is_opaque(),
        wrappers: analysis
            .command_wrappers()
            .into_iter()
            .map(String::from)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(script: &str) -> Vec<String> {
        infer_script_tools(script).unwrap().names
    }

    #[test]
    fn infers_literal_and_quoted_names() {
        assert_eq!(names("fs_read '/tmp/a b'"), ["fs_read"]);
        assert_eq!(names("fs_write \"x y\""), ["fs_write"]);
    }

    #[test]
    fn deduplicates_names_in_first_seen_order() {
        assert_eq!(
            names("fs_read a | fs_write b; fs_read c"),
            ["fs_read", "fs_write"]
        );
    }

    #[test]
    fn infers_nested_functions_conditionals_and_substitutions() {
        let script = concat!(
            "deploy() { fs_read /a; }\n",
            "if true; then deploy; fi\n",
            "echo \"result: $(fs_stat /b)\"\n"
        );
        let inference = infer_script_tools(script).unwrap();
        // Nested substitution commands precede the containing command, and
        // function bodies are walked in source order.
        assert_eq!(
            inference.names,
            ["fs_read", "true", "deploy", "fs_stat", "echo"]
        );
        assert!(!inference.opaque);
    }

    #[test]
    fn dynamic_command_names_flag_opaque() {
        let inference = infer_script_tools("$name --flag").unwrap();
        assert!(inference.names.is_empty());
        assert!(inference.opaque);
    }

    #[test]
    fn wrappers_report_their_own_name_without_the_wrapped_command() {
        let inference = infer_script_tools("env FOO=1 fs_read /a").unwrap();
        assert_eq!(inference.names, ["env"]);
        assert_eq!(inference.wrappers, ["env"]);
        assert!(!inference.opaque);
    }

    #[test]
    fn interpreter_reentry_is_opaque() {
        let inference = infer_script_tools("eval 'fs_read /a'").unwrap();
        assert_eq!(inference.names, ["eval"]);
        assert!(inference.opaque);
    }

    #[test]
    fn parse_failures_are_errors_not_empty_selections() {
        for script in ["if then", "echo 'unterminated"] {
            let error = infer_script_tools(script).unwrap_err();
            assert!(
                error.starts_with("cannot analyze bash script:"),
                "actual: {error}"
            );
        }
    }

    #[test]
    fn assignment_only_script_names_nothing() {
        let inference = infer_script_tools("x=1").unwrap();
        assert!(inference.names.is_empty());
        assert!(!inference.opaque);
    }

    #[test]
    fn truncated_analysis_is_flagged_opaque() {
        let script: String = (0..5000).map(|i| format!("tool{i} x\n")).collect();
        let inference = infer_script_tools(&script).unwrap();
        assert!(inference.names.len() < 5000);
        assert!(inference.opaque, "truncated analysis must flag opacity");
    }
}
