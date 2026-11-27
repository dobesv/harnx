//! What `harnx validate` takes from its command line besides the files to
//! check: tool names to treat as valid, strict mode and findings to allow.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

use super::report::Code;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckSettings {
    /// Tool name globs to treat as valid.
    pub ignore_tools: Vec<String>,
    /// Fail on warnings too.
    pub strict: bool,
    /// Findings to drop before counting.
    pub allow: Vec<AllowRule>,
}

/// Drops findings with `code`, only in files matching `file` when it is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowRule {
    pub code: Code,
    /// Glob over the finding's path relative to the checked directory.
    pub file: Option<String>,
}

/// `CODE` or `CODE:GLOB`, as `--allow` takes it. An unknown code or a glob
/// that doesn't compile is an error.
impl FromStr for AllowRule {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (code, file) = match value.split_once(':') {
            Some((code, glob)) => (code, Some(glob)),
            None => (value, None),
        };
        let code = Code::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == code)
            .ok_or_else(|| {
                let known: Vec<&str> = Code::ALL.iter().map(Code::as_str).collect();
                anyhow!(
                    "unknown finding code '{code}'; expected one of {}",
                    known.join(", ")
                )
            })?;
        if let Some(glob) = file {
            compile_file_glob(glob)
                .map_err(|error| anyhow!("invalid file glob '{glob}': {error}"))?;
        }
        Ok(Self {
            code,
            file: file.map(str::to_string),
        })
    }
}

/// The `ignore_tools` globs, compiled. One that doesn't compile stops the
/// check, like any other bad command-line value.
pub(super) fn ignore_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).with_context(|| format!("Invalid tool name glob '{pattern}'"))?;
        builder.add(glob);
    }
    Ok(builder.build()?)
}

/// An `--allow` file glob, compiled so that `*` stops at `/`.
pub(super) fn compile_file_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
}
