//! Operator-supplied, ordered jaq source patches for a specification.
//!
//! The tool server CLI (T4) will expose this as a repeatable
//! `--spec-patch <JAQ>` argument. This module is the library seam for that
//! flag, which is not wired here. Operation selection is implemented at the
//! library layer in [`crate::select`]; its `--include-operation` CLI flag
//! likewise remains future T4 wiring.
//!
//! Contract, mirroring the plan's CLI-controls recommendation:
//!
//! * Patches are literal argv/config values supplied by the operator: they
//!   are trusted code, exactly like the rest of the server configuration.
//!   Specification or model data is never treated as code. No patch list is
//!   the identity — the document is returned unchanged.
//! * Patches apply in occurrence order to the in-memory parsed document
//!   root, before security/media/reference/normalization passes, and the
//!   effective (patched) source drives both discovery and dispatch
//!   ([`crate::Catalog::build_patched`] normalizes the patched document).
//! * Each stage is an ordinary jaq transform that must produce exactly one
//!   JSON object; the output of stage N is the input of stage N+1. Zero
//!   outputs, multiple outputs, a non-object output, or a runtime error
//!   after the first output all fail the whole patch application atomically
//!   — a failed patch never yields a half-patched document.
//! * The evaluator boundary disables host effects and ambient input: the
//!   `env`, `now`, `debug`, `stderr`, `halt`, `halt_error`,
//!   `localtime`/`strflocaltime` (ambient timezone) and `input`/`inputs`
//!   builtins are removed from the compiled definition and function tables,
//!   so referencing one is a compile failure. Empty globals alone do not
//!   disable builtins — the builtins themselves are removed. No global
//!   variables are declared, so `$name` references never resolve. Module
//!   (`import`/`include`) and data imports are rejected by the loader, which
//!   performs no file or network I/O. A halted execution — unreachable by
//!   construction once `halt` is removed — is still classified as a patch
//!   failure rather than passed to jaq's `unwrap_valr`, which would exit
//!   the host process.
//! * Failures are reported by one-based index and failure category only:
//!   never the expression text, the document content, or the underlying
//!   jaq error chain. Trusted code or not, spec patches may embed operator
//!   values, and a deterministic, secret-free listing must not echo them.
//! * The limits in [`SpecPatchLimits`] are data limits, not fuel: they bound
//!   patch count, per-expression source size and per-stage document
//!   size/depth. Bad trusted code may still hang or exhaust startup; this
//!   evaluator makes no sandbox or resource-isolation claim.

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

use crate::document::SpecDocument;
use crate::error::{SpecError, SpecResult};

type JsonFilter = jaq_core::Filter<data::JustLut<Val>>;

/// Default cap on the number of `--spec-patch` occurrences.
pub const MAX_SPEC_PATCH_COUNT: usize = 32;

/// Default cap on one patch expression's source size, in UTF-8 bytes.
pub const MAX_SPEC_PATCH_EXPRESSION_BYTES: usize = 4096;

/// Default cap on the serialized size of each stage's input and output
/// document, in bytes.
pub const MAX_SPEC_PATCH_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Default cap on the nesting depth of each stage's input and output
/// document.
pub const MAX_SPEC_PATCH_DOCUMENT_DEPTH: usize = 128;

/// Host-effect builtins removed from the compiled function table.
///
/// `env` reads process environment variables (credential-bearing), `now`
/// reads the wall clock (breaks deterministic listings), `debug_empty` and
/// `stderr_empty` write to operator-visible logs, `halt` terminates the host
/// process, and `localtime`/`strflocaltime` depend on the machine's
/// timezone. `input`/`inputs` are not installed by this evaluator but are
/// listed so the exclusion is enforced, not incidental.
const DISABLED_FUNS: [&str; 9] = [
    "env",
    "now",
    "halt",
    "debug_empty",
    "stderr_empty",
    "localtime",
    "strflocaltime",
    "input",
    "inputs",
];

/// Prelude definitions removed alongside the disabled natives — the `jq`
/// surface names that wrap them.
const DISABLED_DEFS: [&str; 5] = ["stderr", "debug", "halt", "halt_error", "input"];

/// Data limits for spec patches. These bound data, not execution: a patch is
/// trusted operator code, and no sandbox or fuel guarantee is made for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecPatchLimits {
    /// Maximum number of patch expressions.
    pub max_patch_count: usize,
    /// Maximum source size of one patch expression, in UTF-8 bytes.
    pub max_expression_bytes: usize,
    /// Maximum serialized size of each stage's input and output document.
    pub max_document_bytes: usize,
    /// Maximum nesting depth of each stage's input and output document.
    pub max_document_depth: usize,
}

impl Default for SpecPatchLimits {
    fn default() -> Self {
        Self {
            max_patch_count: MAX_SPEC_PATCH_COUNT,
            max_expression_bytes: MAX_SPEC_PATCH_EXPRESSION_BYTES,
            max_document_bytes: MAX_SPEC_PATCH_DOCUMENT_BYTES,
            max_document_depth: MAX_SPEC_PATCH_DOCUMENT_DEPTH,
        }
    }
}

/// How one stage failed. Sanitized by construction: carries no expression,
/// document, or jaq error-chain content.
enum PatchFailure {
    Compile,
    Runtime,
    Halt,
    NoOutput,
    MultipleOutputs,
    NotJsonObject,
    NotValidJson,
}

impl PatchFailure {
    fn detail(&self) -> String {
        match self {
            PatchFailure::Compile => "did not compile".to_string(),
            PatchFailure::Runtime => "failed during evaluation".to_string(),
            PatchFailure::Halt => "attempted to halt; patches cannot halt the server".to_string(),
            PatchFailure::NoOutput => {
                "produced no output; expected exactly one JSON object".to_string()
            }
            PatchFailure::MultipleOutputs => {
                "produced multiple outputs; expected exactly one JSON object".to_string()
            }
            PatchFailure::NotJsonObject => "produced output that is not a JSON object".to_string(),
            PatchFailure::NotValidJson => "produced a value that is not valid JSON".to_string(),
        }
    }
}

/// Compile one patch expression with host effects removed from the tables.
///
/// Any parse, load, or compile problem — malformed syntax, a reference to a
/// disabled builtin or an undeclared `$global`, an `import`/`include`, or a
/// data import — collapses to `Err(())`: the caller reports the index and a
/// compile-failure category, nothing else.
fn compile_patch(expression: &str) -> Result<JsonFilter, ()> {
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs())
        .filter(|def| !DISABLED_DEFS.contains(&def.name));
    let loader = Loader::new(defs);
    // The loader performs no file or network I/O: its module reader always
    // fails, so `import`/`include` cannot pull in outside code or data.
    let modules = loader
        .load(
            &arena,
            File {
                code: expression,
                path: (),
            },
        )
        .or(Err(()))?;
    // Module imports (`import "…" as f;` / `include "…";`) fail at load:
    // the loader's module reader always refuses. Data imports
    // (`import "…" as $name;`) are not resolved by `load`, so walk them with
    // jaq's own import walker and refuse each — the probe callback never
    // reads any file.
    jaq_core::load::import(&modules, |_| Err("data imports are rejected".to_string()))
        .or(Err(()))?;
    let funs = jaq_core::funs::<data::JustLut<Val>>()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs())
        .filter(|fun| !DISABLED_FUNS.contains(&fun.0));
    Compiler::<_, data::JustLut<Val>>::default()
        .with_funs(funs)
        .with_global_vars(std::iter::empty::<&str>())
        .compile(modules)
        .or(Err(()))
}

/// Convert `serde_json::Value` → `Val` by round-tripping through JSON text.
fn json_to_val(value: &Value) -> Val {
    let json = value.to_string();
    jaq_json::read::parse_single(json.as_bytes()).unwrap_or(Val::Null)
}

/// Convert `Val` → `serde_json::Value` via the `Display` impl (which writes
/// valid JSON).
fn val_to_json(value: &Val) -> Option<Value> {
    serde_json::from_str(&value.to_string()).ok()
}

/// Classify an execution exception without ever exiting the process.
///
/// `halt` is removed from the function table, so a halt exception is
/// unreachable by construction; classifying it defensively means a future
/// jaq builtin can terminate at most this patch application, never the host
/// (jaq's own `unwrap_valr` exits the process on halt).
fn classify_exn<E>(exn: jaq_core::Exn<E>) -> PatchFailure {
    if exn.get_halt().is_ok() {
        PatchFailure::Halt
    } else {
        PatchFailure::Runtime
    }
}

/// Run one compiled stage: exactly one output, and no error may follow it.
fn run_patch(filter: &JsonFilter, input: Value) -> Result<Value, PatchFailure> {
    let input_val = json_to_val(&input);
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut outputs = filter.id.run((ctx, input_val));
    let first = match outputs.next() {
        Some(Ok(value)) => value,
        Some(Err(exn)) => return Err(classify_exn(exn)),
        None => return Err(PatchFailure::NoOutput),
    };
    // Looking past the first output also catches a late runtime error, and
    // stops as soon as a second output exists so an unbounded stream cannot
    // stall the host on a stream that never yields a second value.
    match outputs.next() {
        Some(Ok(_)) => Err(PatchFailure::MultipleOutputs),
        Some(Err(exn)) => Err(classify_exn(exn)),
        None => val_to_json(&first).ok_or(PatchFailure::NotValidJson),
    }
}

/// Nesting depth of a JSON value: objects and arrays count one level each.
// qual:allow(iosp) — recursion over the value tree necessarily interleaves
// the per-variant logic with the recursive calls.
fn document_depth(value: &Value) -> usize {
    fn depth(value: &Value) -> usize {
        match value {
            Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
            Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
            _ => 1,
        }
    }
    depth(value)
}

fn patch_error(stage: usize, total: usize, detail: impl std::fmt::Display) -> SpecError {
    SpecError::spec_patch(format!("patch {stage} of {total} {detail}"))
}

/// Enforce the per-stage data limits on one side of a stage boundary.
// qual:allow(iosp) — two bounded checks and their two error renders; the
// check/render split would push the stage/total/role triple through two
// more helpers without removing any logic.
fn ensure_document_within(
    value: &Value,
    limits: SpecPatchLimits,
    stage: usize,
    total: usize,
    role: &str,
) -> SpecResult<()> {
    let bytes = serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| patch_error(stage, total, format!("{role} is not valid JSON")))?;
    if bytes > limits.max_document_bytes {
        return Err(patch_error(
            stage,
            total,
            format!(
                "{role} exceeds the per-stage document size limit \
                 ({bytes} bytes; limit {} bytes)",
                limits.max_document_bytes
            ),
        ));
    }
    let depth = document_depth(value);
    if depth > limits.max_document_depth {
        return Err(patch_error(
            stage,
            total,
            format!(
                "{role} exceeds the per-stage document depth limit \
                 (depth {depth}; limit {})",
                limits.max_document_depth
            ),
        ));
    }
    Ok(())
}

/// Apply the ordered patch list to a JSON value tree. No patches is the
/// identity.
// qual:allow(iosp) — the stage pipeline itself: the limit guards must stay
// interleaved with the compile/run calls so each stage reports its own
// failure index before the next stage starts.
fn apply_value_patches(
    root: &Value,
    patches: &[String],
    limits: SpecPatchLimits,
) -> SpecResult<Value> {
    if patches.len() > limits.max_patch_count {
        return Err(SpecError::spec_patch(format!(
            "too many patches ({}; limit {})",
            patches.len(),
            limits.max_patch_count
        )));
    }
    let total = patches.len();
    let mut current = root.clone();
    for (index, expression) in patches.iter().enumerate() {
        let stage = index + 1;
        let expression_bytes = expression.len();
        if expression_bytes > limits.max_expression_bytes {
            return Err(patch_error(
                stage,
                total,
                format!(
                    "exceeds the expression size limit \
                     ({expression_bytes} bytes; limit {} bytes)",
                    limits.max_expression_bytes
                ),
            ));
        }
        ensure_document_within(&current, limits, stage, total, "input")?;
        let filter = compile_patch(expression)
            .map_err(|_| patch_error(stage, total, PatchFailure::Compile.detail()))?;
        let output = run_patch(&filter, current)
            .map_err(|failure| patch_error(stage, total, failure.detail()))?;
        if !output.is_object() {
            return Err(patch_error(
                stage,
                total,
                PatchFailure::NotJsonObject.detail(),
            ));
        }
        ensure_document_within(&output, limits, stage, total, "output")?;
        current = output;
    }
    Ok(current)
}

/// Compile-check a patch list without a document: the count, source-size and
/// compile categories, but not runtime or cardinality failures, can be
/// validated before a specification is loaded. The server (T4) uses this
/// for offline configuration validation.
// qual:api — offline configuration validation seam for the T4 server CLI;
// tests exercise the same entry point.
pub fn validate_spec_patches(patches: &[String], limits: SpecPatchLimits) -> SpecResult<()> {
    if patches.len() > limits.max_patch_count {
        return Err(SpecError::spec_patch(format!(
            "too many patches ({}; limit {})",
            patches.len(),
            limits.max_patch_count
        )));
    }
    let total = patches.len();
    for (index, expression) in patches.iter().enumerate() {
        let stage = index + 1;
        let expression_bytes = expression.len();
        if expression_bytes > limits.max_expression_bytes {
            return Err(patch_error(
                stage,
                total,
                format!(
                    "exceeds the expression size limit \
                     ({expression_bytes} bytes; limit {} bytes)",
                    limits.max_expression_bytes
                ),
            ));
        }
        compile_patch(expression)
            .map_err(|_| patch_error(stage, total, PatchFailure::Compile.detail()))?;
    }
    Ok(())
}

/// The sanitized failure category for a patched document that fails
/// re-validation: variant shape only, never the error's message, context or
/// reference fields, so no patched document content can be echoed.
fn revalidation_category(error: &SpecError) -> &'static str {
    match error {
        SpecError::Unsupported { .. } => {
            "left a document that uses a specification feature this crate does not support"
        }
        SpecError::Invalid { .. } => {
            "left a document that is not a structurally valid specification"
        }
        SpecError::UnresolvedRef { .. } => {
            "left a document with a reference that cannot be resolved"
        }
        _ => "left a document that is not a usable specification",
    }
}

/// Apply the ordered patch list to a parsed specification, returning the
/// effective source document that normalization must consume.
///
/// The patched root is re-validated as a specification, so a patch that
/// breaks the document shape fails here instead of producing a catalog from
/// a half-patched source. No patches returns the document unchanged, and
/// with no patches the re-validation error is the caller's ordinary
/// document diagnostic. With patches applied, the re-validation failure is
/// reported as a sanitized patch failure — final stage index and failure
/// category only — because the patched document may embed operator
/// secrets from the patch list, and its content must not be echoed.
// qual:allow(iosp) — the identity branch and the sanitized branch are one
// seam: whether the document was patched decides the error contract, so
// the guard and the re-validation call paths belong together.
pub fn apply_spec_patches(
    document: &SpecDocument,
    patches: &[String],
    limits: SpecPatchLimits,
) -> SpecResult<SpecDocument> {
    let root = apply_value_patches(&document.root, patches, limits)?;
    if patches.is_empty() {
        return SpecDocument::from_value(root);
    }
    // The patched document may embed operator secrets from the patch list,
    // so its re-validation error must not echo document content. Report
    // the final stage's index and a failure category only, like every
    // other patch failure.
    SpecDocument::from_value(root).map_err(|error| {
        let total = patches.len();
        patch_error(total, total, revalidation_category(&error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(value: Value) -> SpecDocument {
        SpecDocument::from_value(value).unwrap()
    }

    fn minimal() -> Value {
        json!({"swagger": "2.0", "paths": {}, "info": {"title": "T", "version": "1"}})
    }

    fn apply(root: &Value, patches: &[&str]) -> SpecResult<Value> {
        apply_limited(root, patches, SpecPatchLimits::default())
    }

    fn apply_limited(root: &Value, patches: &[&str], limits: SpecPatchLimits) -> SpecResult<Value> {
        let owned: Vec<String> = patches.iter().map(|patch| patch.to_string()).collect();
        apply_value_patches(root, &owned, limits)
    }

    #[test]
    fn empty_patch_list_is_identity() {
        harnx_core::require_nextest();
        let root = minimal();
        assert_eq!(apply(&root, &[]).unwrap(), root);
        let document = document(root.clone());
        let patched = apply_spec_patches(&document, &[], SpecPatchLimits::default()).unwrap();
        assert_eq!(patched.root, root);
    }

    /// A patch that plants a sentinel and breaks the document: the
    /// re-validation failure must report the stage index and a failure
    /// category only, in every rendered format — never the document
    /// content the patch wrote.
    #[test]
    fn patched_document_revalidation_failure_is_sanitized() {
        use std::error::Error as StdError;
        harnx_core::require_nextest();
        const SENTINEL: &str = "s3cr3t-patch-sentinel-value";
        let document = document(minimal());
        // Unknown Swagger version: `from_value` would echo the version
        // string, so the sentinel rides exactly the echo path.
        let patches = vec![format!(r#".swagger = "2.0-{SENTINEL}""#)];
        let error =
            apply_spec_patches(&document, &patches, SpecPatchLimits::default()).unwrap_err();
        assert!(matches!(error, SpecError::SpecPatch(_)), "{error:?}");
        let rendered = error.to_string();
        assert!(rendered.contains("patch 1 of 1"), "{rendered}");
        assert!(
            rendered.contains("uses a specification feature this crate does not support"),
            "the safe failure category must be preserved: {rendered}"
        );
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        // The Debug render and the error chain carry the same sanitized
        // payload; nothing deeper may retain the document.
        let debugged = format!("{error:?}");
        assert!(debugged.contains("patch 1 of 1"), "{debugged}");
        assert!(!debugged.contains(SENTINEL), "{debugged}");
        assert!(
            StdError::source(&error).is_none(),
            "no error chain may carry the patched document"
        );
    }

    #[test]
    fn patched_structurally_invalid_document_is_a_sanitized_patch_failure() {
        harnx_core::require_nextest();
        const SENTINEL: &str = "s3cr3t-struct-sentinel";
        let document = document(minimal());
        // Removing `paths` makes the document structurally invalid; the
        // sentinel rides along in an unrelated field the patch also sets.
        let patches = vec![
            "del(.paths)".to_string(),
            format!(r#".info.title = "{SENTINEL}""#),
        ];
        let error =
            apply_spec_patches(&document, &patches, SpecPatchLimits::default()).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("patch 2 of 2"), "{rendered}");
        assert!(
            rendered.contains("not a structurally valid specification"),
            "{rendered}"
        );
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(!format!("{error:?}").contains(SENTINEL), "{error:?}");
        // The no-patch identity keeps ordinary document diagnostics.
        let source_error = SpecDocument::from_value(serde_json::json!({"x": 1})).unwrap_err();
        assert!(
            !matches!(source_error, SpecError::SpecPatch(_)),
            "{source_error}"
        );
    }

    #[test]
    fn patches_apply_in_order_and_depend_on_each_other() {
        harnx_core::require_nextest();
        let output = apply(
            &minimal(),
            &[".info.title = \"x\"", ".info.version = .info.title"],
        )
        .unwrap();
        assert_eq!(output["info"]["title"], json!("x"));
        assert_eq!(output["info"]["version"], json!("x"));
    }

    #[test]
    fn full_jaq_transforms_remain_available() {
        harnx_core::require_nextest();
        let root = json!({"swagger": "2.0", "paths": {}, "list": [3, 1, 2]});
        let output = apply(
            &root,
            &[
                ".list |= sort",
                ".extra = {\"a\": 1} | .extra |= with_entries(.value += 1)",
                ".names = [.list[] | tostring] | .names |= join(\",\")",
            ],
        )
        .unwrap();
        assert_eq!(output["list"], json!([1, 2, 3]));
        assert_eq!(output["extra"], json!({"a": 2}));
        assert_eq!(output["names"], json!("1,2,3"));
    }

    #[test]
    fn key_order_survives_patching() {
        harnx_core::require_nextest();
        let root = json!({"swagger": "2.0", "paths": {}, "zeta": 1, "alpha": 2});
        let keys: Vec<String> = root.as_object().unwrap().keys().cloned().collect();
        let output = apply(&root, &[".zeta = 9", ".new = true"]).unwrap();
        assert_eq!(
            output
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            [&keys[..], &["new".to_string()][..]].concat()
        );
    }

    #[test]
    fn malformed_expression_fails_without_leaking_it() {
        harnx_core::require_nextest();
        let error = apply(&minimal(), &[".info.title = "])
            .unwrap_err()
            .to_string();
        assert!(error.contains("patch 1 of 1"), "{error}");
        assert!(error.contains("did not compile"), "{error}");
        assert!(!error.contains(".info.title"), "{error}");
    }

    #[test]
    fn zero_multiple_nonobject_and_late_errors_fail_sanitized() {
        harnx_core::require_nextest();
        for (patches, detail) in [
            (vec!["empty"], "produced no output"),
            (vec!["., ."], "produced multiple outputs"),
            (vec![".swagger"], "not a JSON object"),
            (vec!["error(\"boom-secret\")"], "failed during evaluation"),
            (
                vec!["., error(\"late-secret\")"],
                "failed during evaluation",
            ),
        ] {
            let error = apply(&minimal(), &patches).unwrap_err().to_string();
            assert!(error.contains("patch 1 of 1"), "{error}");
            assert!(error.contains(detail), "{error}");
            assert!(!error.contains("secret"), "{error}");
            assert!(!error.contains("boom"), "{error}");
            assert!(!error.contains("late"), "{error}");
        }
    }

    #[test]
    fn document_content_never_reaches_error_text() {
        harnx_core::require_nextest();
        // The first patch plants a sentinel inside the document; the second
        // fails at runtime. The failure text must carry the patch index and
        // category only — neither the sentinel nor the expression.
        let error = apply(&minimal(), &[".paths = \"TOKEN-SECRET\"", "error(.paths)"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("patch 2 of 2"), "{error}");
        assert!(error.contains("failed during evaluation"), "{error}");
        assert!(!error.contains("TOKEN-SECRET"), "{error}");
        assert!(!error.contains("error(.paths)"), "{error}");
    }

    #[test]
    fn host_effect_builtins_fail_to_compile() {
        harnx_core::require_nextest();
        for expression in [
            "now",
            "now | tostring",
            "env",
            "$ENV",
            ".x = env.PATH",
            "debug",
            "debug(.paths)",
            "stderr",
            "halt",
            "halt(0)",
            "halt_error",
            "halt_error(1)",
            "input",
            "inputs",
            "localtime",
            "strflocaltime(\"%Y\")",
            "$undeclared",
        ] {
            let Err(error) = apply(&minimal(), &[expression]) else {
                panic!("{expression:?} must be rejected");
            };
            let error = error.to_string();
            assert!(error.contains("did not compile"), "{expression:?}: {error}");
            assert!(!error.contains(expression), "{expression:?}: {error}");
        }
    }

    #[test]
    fn a_halt_is_classified_as_patch_failure_not_process_exit() {
        harnx_core::require_nextest();
        // halt/1 and its wrapper defs are removed, so this only proves the
        // compile-time rejection; the Exn::Halt classification is defensive.
        assert!(apply(&minimal(), &["halt", "."]).is_err());
        assert!(apply(&minimal(), &["halt(3)"]).is_err());
    }

    #[test]
    fn module_and_data_imports_are_rejected() {
        harnx_core::require_nextest();
        for expression in [
            "include \"sibling\"; .",
            "import \"helper\" as helper; .",
            "import \"data.json\" as $data; .",
        ] {
            let Err(error) = apply(&minimal(), &[expression]) else {
                panic!("{expression:?} must be rejected");
            };
            let error = error.to_string();
            assert!(error.contains("did not compile"), "{expression:?}: {error}");
            assert!(!error.contains("sibling"), "{error}");
            assert!(!error.contains("helper"), "{error}");
        }
    }

    #[test]
    fn limits_are_enforced() {
        harnx_core::require_nextest();
        let many: Vec<String> = (0..33).map(|_| ".".to_string()).collect();
        let error = apply_value_patches(&minimal(), &many, SpecPatchLimits::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("too many patches (33; limit 32)"), "{error}");

        let big = format!(".a = {}", "1 + ".repeat(3000));
        let error = apply(&minimal(), &[&big]).unwrap_err().to_string();
        assert!(error.contains("expression size limit"), "{error}");
        assert!(!error.contains("1 + 1"), "{error}");

        // Tiny document-byte limit: the stage input already exceeds it.
        let tiny_bytes = SpecPatchLimits {
            max_document_bytes: 10,
            ..SpecPatchLimits::default()
        };
        let error = apply_limited(&minimal(), &["."], tiny_bytes)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("input exceeds the per-stage document size"),
            "{error}"
        );

        // Tiny depth limit: a patch whose output nests one level too deep.
        let tiny_depth = SpecPatchLimits {
            max_document_depth: 2,
            ..SpecPatchLimits::default()
        };
        let root = json!({"swagger": "2.0", "paths": {}});
        let error = apply_limited(&root, &[".deep = {\"in\": {\"deeper\": 1}}"], tiny_depth)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("output exceeds the per-stage document depth"),
            "{error}"
        );
        assert!(error.contains("depth 4; limit 2"), "{error}");
    }

    #[test]
    fn document_size_limit_bounds_a_growing_patch() {
        harnx_core::require_nextest();
        let limits = SpecPatchLimits {
            max_document_bytes: 1024,
            ..SpecPatchLimits::default()
        };
        let error = apply_limited(&minimal(), &[".big = [range(0; 2000)]"], limits)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("output exceeds the per-stage document size"),
            "{error}"
        );
    }

    #[test]
    fn broken_patched_document_fails_revalidation() {
        harnx_core::require_nextest();
        let document = document(minimal());
        let error = apply_spec_patches(
            &document,
            &[".paths = 1".to_string()],
            SpecPatchLimits::default(),
        )
        .unwrap_err()
        .to_string();
        // Re-validation of a patched document is a sanitized patch
        // failure — index and category only, no document echo.
        assert!(error.contains("spec patch failed"), "{error}");
        assert!(error.contains("patch 1 of 1"), "{error}");
        assert!(
            error.contains("not a structurally valid specification"),
            "{error}"
        );
    }

    #[test]
    fn validate_compiles_without_a_document() {
        harnx_core::require_nextest();
        let owned = vec![".".to_string(), ".info.title |= ascii_upcase".to_string()];
        validate_spec_patches(&owned, SpecPatchLimits::default()).expect("valid patches");
        let broken = vec![".a = ".to_string()];
        assert!(validate_spec_patches(&broken, SpecPatchLimits::default()).is_err());
        let host = vec!["now".to_string()];
        assert!(validate_spec_patches(&host, SpecPatchLimits::default()).is_err());
    }
}
