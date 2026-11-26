//! Findings and the report that collects them: suppression by `--allow`
//! rules, `--strict` promotion, counting and rendering.

use std::path::{Path, PathBuf};

use globset::GlobMatcher;
use serde::{Deserialize, Serialize, Serializer};

use super::settings::{compile_file_glob, AllowRule, CheckSettings};

/// How bad a finding is. Errors fail the run; warnings fail it only in strict
/// mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Stable identifier of each check, so CI scripts and `allow` rules can pick
/// findings out without matching message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Code {
    ParseError,
    UnknownKey,
    UnknownClientType,
    EmptyConfig,
    SilentDrop,
    AgentLoad,
    ModelUnresolved,
    ModelNotInCatalog,
    AccessAgentUnmatched,
    EnvVarUnset,
    CommandNotFound,
    UnknownTool,
    ToolUnverified,
    GlobMatchesNothing,
    AgentNotFound,
    FrontMatterNotAtStart,
    ModelsOverrideVersion,
    EnvLineInvalid,
}

impl Code {
    /// Every code, in the order the documentation lists them.
    pub const ALL: [Code; 18] = [
        Self::ParseError,
        Self::UnknownKey,
        Self::UnknownClientType,
        Self::SilentDrop,
        Self::AgentLoad,
        Self::ModelUnresolved,
        Self::AgentNotFound,
        Self::UnknownTool,
        Self::ModelNotInCatalog,
        Self::AccessAgentUnmatched,
        Self::EnvVarUnset,
        Self::CommandNotFound,
        Self::ToolUnverified,
        Self::GlobMatchesNothing,
        Self::FrontMatterNotAtStart,
        Self::ModelsOverrideVersion,
        Self::EnvLineInvalid,
        Self::EmptyConfig,
    ];

    /// The code as serialized, e.g. `unknown-key`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParseError => "parse-error",
            Self::UnknownKey => "unknown-key",
            Self::UnknownClientType => "unknown-client-type",
            Self::EmptyConfig => "empty-config",
            Self::SilentDrop => "silent-drop",
            Self::AgentLoad => "agent-load",
            Self::ModelUnresolved => "model-unresolved",
            Self::ModelNotInCatalog => "model-not-in-catalog",
            Self::AccessAgentUnmatched => "access-agent-unmatched",
            Self::EnvVarUnset => "env-var-unset",
            Self::CommandNotFound => "command-not-found",
            Self::UnknownTool => "unknown-tool",
            Self::ToolUnverified => "tool-unverified",
            Self::GlobMatchesNothing => "glob-matches-nothing",
            Self::AgentNotFound => "agent-not-found",
            Self::FrontMatterNotAtStart => "front-matter-not-at-start",
            Self::ModelsOverrideVersion => "models-override-version",
            Self::EnvLineInvalid => "env-line-invalid",
        }
    }

    /// Errors are what harnx rejects or silently drops at runtime; warnings
    /// are what may be fine where the check can't see the whole picture (an
    /// unset secret in CI, a rule written ahead of its agent).
    pub fn default_severity(&self) -> Severity {
        match self {
            Self::ParseError
            | Self::UnknownKey
            | Self::UnknownClientType
            | Self::SilentDrop
            | Self::AgentLoad
            | Self::ModelUnresolved
            | Self::UnknownTool
            | Self::AgentNotFound => Severity::Error,
            Self::EmptyConfig
            | Self::ModelNotInCatalog
            | Self::AccessAgentUnmatched
            | Self::EnvVarUnset
            | Self::CommandNotFound
            | Self::ToolUnverified
            | Self::GlobMatchesNothing
            | Self::FrontMatterNotAtStart
            | Self::ModelsOverrideVersion
            | Self::EnvLineInvalid => Severity::Warning,
        }
    }
}

/// One problem found in the checked directory.
///
/// `message` stands on its own in the text output, which prints it after the
/// file and line: a finding located by `key_path` names the key in its
/// message too (`unknown key a.b`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: Code,
    /// Relative to the checked directory.
    #[serde(serialize_with = "serialize_file")]
    pub file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    pub message: String,
}

// qual:allow(dry, boilerplate) reason: "the setters are the builder API every check pass uses"
impl Finding {
    pub fn new(severity: Severity, code: Code, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            file: None,
            line: None,
            key_path: None,
            message: message.into(),
        }
    }

    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: Code, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    /// A finding with the code's default severity.
    pub fn of(code: Code, message: impl Into<String>) -> Self {
        Self::new(code.default_severity(), code, message)
    }

    pub fn in_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn at_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn at_key(mut self, key_path: impl Into<String>) -> Self {
        self.key_path = Some(key_path.into());
        self
    }

    /// `file` with `/` separators on every platform, so output and `allow`
    /// globs read the same everywhere.
    pub fn file_display(&self) -> Option<String> {
        self.file.as_deref().map(slash_path)
    }

    /// `error[unknown-key]: config.yaml:3: message`, rustc-style, so the
    /// code an `allow` rule needs is on the line.
    fn render_text(&self) -> String {
        let severity = format!("{}[{}]", self.severity.as_str(), self.code.as_str());
        let Some(file) = self.file_display() else {
            return format!("{severity}: {}", self.message);
        };
        match self.line {
            Some(line) => format!("{severity}: {file}:{line}: {}", self.message),
            None => format!("{severity}: {file}: {}", self.message),
        }
    }
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// qual:allow(test_quality, untested) reason: "reached through serde's serialize_with, which the call graph doesn't follow; json_shape covers it"
fn serialize_file<S: Serializer>(file: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error> {
    file.as_deref().map(slash_path).serialize(serializer)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub files_checked: usize,
    pub errors: usize,
    pub warnings: usize,
    /// Findings an `allow` rule removed.
    pub suppressed: usize,
    /// Tool references the check couldn't confirm or rule out.
    pub tools_unverified: usize,
    /// Tool references matched by `ignore_tools`.
    pub tools_ignored: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub summary: Summary,
}

impl Report {
    pub fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }

    /// Apply the settings and count what's left: `allow` rules drop findings
    /// first, then `strict` turns the remaining warnings into errors.
    pub fn finish(mut self, settings: &CheckSettings) -> Report {
        let rules: Vec<AllowMatcher> = settings.allow.iter().map(AllowMatcher::new).collect();
        let before = self.findings.len();
        self.findings
            .retain(|finding| !rules.iter().any(|rule| rule.matches(finding)));
        self.summary.suppressed += before - self.findings.len();
        if settings.strict {
            for finding in &mut self.findings {
                finding.severity = Severity::Error;
            }
        }
        self.summary.errors = self.count(Severity::Error);
        self.summary.warnings = self.count(Severity::Warning);
        self
    }

    fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .count()
    }

    /// `1` when any error remains, else `0`.
    pub fn exit_code(&self) -> u8 {
        u8::from(
            self.findings
                .iter()
                .any(|finding| finding.severity == Severity::Error),
        )
    }

    /// One line per finding, then the summary line.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for finding in &self.findings {
            out.push_str(&finding.render_text());
            out.push('\n');
        }
        out.push_str(&self.summary.render_text());
        out.push('\n');
        out
    }

    pub fn render_json(&self) -> String {
        let report = JsonReport {
            findings: &self.findings,
            summary: &self.summary,
        };
        // Plain data with string keys: serializing can't fail.
        serde_json::to_string_pretty(&report).unwrap_or_default()
    }
}

/// The JSON output's shape: `{ "findings": [...], "summary": {...} }`.
#[derive(Serialize)]
struct JsonReport<'a> {
    findings: &'a [Finding],
    summary: &'a Summary,
}

impl Summary {
    fn render_text(&self) -> String {
        let mut parts = vec![
            counted(self.files_checked, "file checked", "files checked"),
            counted(self.errors, "error", "errors"),
            counted(self.warnings, "warning", "warnings"),
        ];
        if self.suppressed > 0 {
            parts.push(format!("{} suppressed", self.suppressed));
        }
        parts.push(counted(
            self.tools_unverified,
            "tool reference unverified",
            "tool references unverified",
        ));
        if self.tools_ignored > 0 {
            parts.push(counted(
                self.tools_ignored,
                "tool reference ignored",
                "tool references ignored",
            ));
        }
        parts.join(", ")
    }
}

fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// An `allow` rule with its file glob compiled. `*` stops at `/`, so
/// `nats_servers/*` covers that directory's files and not deeper ones.
struct AllowMatcher<'a> {
    rule: &'a AllowRule,
    /// `None` when the rule names no file. `Some(None)` when its glob doesn't
    /// compile: parsing `--allow` rejects those, and one built by hand
    /// matches nothing rather than everything.
    file: Option<Option<GlobMatcher>>,
}

impl<'a> AllowMatcher<'a> {
    fn new(rule: &'a AllowRule) -> Self {
        let file = rule
            .file
            .as_deref()
            .map(|pattern| compile_file_glob(pattern).ok());
        Self { rule, file }
    }

    // qual:allow(iosp) — two short guards around one glob match; splitting them apart hides the rule.
    fn matches(&self, finding: &Finding) -> bool {
        if finding.code != self.rule.code {
            return false;
        }
        let Some(glob) = &self.file else {
            return true;
        };
        match (glob, finding.file_display()) {
            (Some(glob), Some(file)) => glob.is_match(file),
            _ => false,
        }
    }
}
