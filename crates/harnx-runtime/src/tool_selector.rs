//! Matching `use_tools` selectors against tool names.

use globset::{GlobBuilder, GlobMatcher};

/// Characters that make a selector a glob instead of a tool name. `\` counts
/// because globset reads it as an escape on Unix.
const GLOB_SYNTAX: [char; 7] = ['*', '?', '[', ']', '{', '}', '\\'];

/// A `use_tools` selector compiled once and matched against many tool names.
///
/// Compiling a glob builds a regex, which costs far more than matching one.
/// Tool selection runs for every model request and tool round, against every
/// registered tool, so each selector is compiled once per pass rather than
/// once per candidate. A selector without glob syntax compares by equality,
/// which is all its compiled glob would match.
pub(crate) enum ToolSelector {
    Name(String),
    Glob(GlobMatcher),
    /// A malformed glob, which matches nothing.
    Invalid,
}

impl ToolSelector {
    /// `literal_separator` is passed to [`GlobBuilder::literal_separator`]:
    /// whether `*` and `?` stop at `/`.
    pub(crate) fn new(selector: &str, literal_separator: bool) -> Self {
        if !selector.contains(GLOB_SYNTAX) {
            return Self::Name(selector.to_string());
        }
        GlobBuilder::new(selector)
            .literal_separator(literal_separator)
            .build()
            .map_or(Self::Invalid, |glob| Self::Glob(glob.compile_matcher()))
    }

    pub(crate) fn is_match(&self, name: &str) -> bool {
        match self {
            Self::Name(selector) => selector == name,
            Self::Glob(matcher) => matcher.is_match(name),
            Self::Invalid => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolSelector;

    fn matching(selector: &str, literal_separator: bool, names: &[&str]) -> Vec<String> {
        let selector = ToolSelector::new(selector, literal_separator);
        names
            .iter()
            .filter(|name| selector.is_match(name))
            .map(|name| name.to_string())
            .collect()
    }

    const NAMES: &[&str] = &[
        "fs_read",
        "fs_read_all",
        "xfs_read",
        "fs_write",
        "bash_exec",
        "pytheas_session_prompt",
    ];

    #[test]
    fn plain_name_matches_only_the_identical_tool() {
        assert_eq!(matching("fs_read", true, NAMES), vec!["fs_read"]);
    }

    #[test]
    fn star_selects_by_pattern() {
        assert_eq!(
            matching("fs_*", true, NAMES),
            vec!["fs_read", "fs_read_all", "fs_write"]
        );
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert_eq!(matching("?fs_read", true, NAMES), vec!["xfs_read"]);
    }

    #[test]
    fn character_class_selects_listed_characters() {
        assert_eq!(matching("fs_[w]*", true, NAMES), vec!["fs_write"]);
    }

    #[test]
    fn brace_alternation_selects_each_alternative() {
        assert_eq!(
            matching("{bash,pytheas}_*", true, NAMES),
            vec!["bash_exec", "pytheas_session_prompt"]
        );
    }

    // globset only treats `\` as an escape where it is not a path separator.
    #[cfg(unix)]
    #[test]
    fn backslash_escapes_like_a_glob() {
        assert_eq!(matching(r"fs\_read", true, NAMES), vec!["fs_read"]);
    }

    #[test]
    fn malformed_glob_matches_nothing() {
        assert!(matching("fs_[read", true, NAMES).is_empty());
        assert!(matching("{fs_read", true, NAMES).is_empty());
    }

    #[test]
    fn literal_separator_controls_whether_star_crosses_slashes() {
        let names = ["pkg/tool", "pkg_tool"];
        assert_eq!(matching("pkg*", true, &names), vec!["pkg_tool"]);
        assert_eq!(
            matching("pkg*", false, &names),
            vec!["pkg/tool", "pkg_tool"]
        );
    }
}
