//! `ls`, `find` and `grep` results shaped like `ls -1`, `find` and `grep -n`
//! for a bash script. The model-facing text stays in the handlers.

use harnx_core::safety::{format_size, DEFAULT_LS_LIMIT, LS_SCAN_HARD_LIMIT};
use serde_json::{json, Value};
use std::path::Path;

use crate::script_result::script_result;
use crate::server::SearchLine;
use crate::summary::{find_summary, ls_summary, search_summary, SearchTruncation};

/// What a `grep -n` row adds to the path and text: the line number, two
/// separators and the newline.
const GREP_ROW_OVERHEAD: usize = 16;

/// What to put before a path found under `searched` so a script can pass it
/// back as a `path` argument: the caller's own `path` argument, or the
/// absolute search directory when that is not the current directory. `None`
/// leaves paths relative.
pub(crate) fn path_prefix(given: Option<&str>, searched: &Path) -> Option<String> {
    if let Some(given) = given {
        return Some(given.to_string());
    }
    let current = std::env::current_dir().ok()?;
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    (!same(&current, searched)).then(|| without_verbatim_prefix(&searched.display().to_string()))
}

/// `path` without the `\\?\` (or `\\?\UNC\`) prefix Windows puts on a
/// canonical path. Appending `/rel` to a verbatim path gives one no file tool
/// can open, and glob patterns cannot use it either.
pub(crate) fn without_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Make found paths relative to `searched`. `collect_found_paths` cannot
/// strip a verbatim search root from glob output built on the plain form, so
/// strip the plain form here.
pub(crate) fn strip_search_root(paths: &mut [String], searched: &Path) {
    let plain = without_verbatim_prefix(&searched.display().to_string());
    for path in paths {
        if let Ok(rel) = Path::new(path.as_str()).strip_prefix(&plain) {
            *path = rel.display().to_string();
        }
    }
}

/// Whether `path` is absolute. Drive letters and backslashes only mean a
/// root on Windows; on Unix `c:/x` is a relative path under a directory `c:`.
fn is_absolute(windows: bool, path: &str) -> bool {
    match path.as_bytes() {
        [b'/', ..] => true,
        [b'\\', ..] => windows,
        [drive, b':', b'/' | b'\\', ..] => windows && drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// `rel` under `prefix`, joined with `/` without doubling a trailing `/` or
/// `\`. An absolute `rel` is already complete and an empty prefix adds
/// nothing. A verbatim prefix is made readable first, since `/` is not a
/// separator after `\\?\`.
fn prefixed(prefix: Option<&str>, rel: &str) -> String {
    prefixed_on(cfg!(windows), prefix, rel)
}

fn prefixed_on(windows: bool, prefix: Option<&str>, rel: &str) -> String {
    match prefix {
        Some(prefix) if !prefix.is_empty() && !is_absolute(windows, rel) => {
            let prefix = without_verbatim_prefix(prefix);
            format!("{}/{rel}", prefix.trim_end_matches(['/', '\\']))
        }
        _ => rel.to_string(),
    }
}

/// How many leading items fit in `cap` bytes of text.
fn items_within(sizes: impl Iterator<Item = usize>, cap: usize) -> usize {
    let mut total = 0;
    sizes
        .take_while(|size| {
            total += size;
            total <= cap
        })
        .count()
}

fn cap_note(kept: usize, total: usize, noun: &str, cap: usize) -> String {
    format!(
        "stopped at {kept} of {total} {noun} to stay within {}",
        format_size(cap)
    )
}

/// One line per item in `field`, plus notes. Items past `cap` bytes of text
/// are dropped with a note.
fn script_list(
    (field, noun): (&str, &str),
    mut items: Vec<String>,
    (mut notes, summary): (Vec<String>, String),
    cap: usize,
) -> rmcp::model::CallToolResult {
    let total = items.len();
    let kept = items_within(items.iter().map(|item| item.len() + 1), cap);
    if kept < total {
        items.truncate(kept);
        notes.push(cap_note(kept, total, noun, cap));
    }
    script_result(json!({ field: items, "notes": notes }), summary)
}

pub(crate) struct LsListing<'a> {
    pub path: &'a str,
    pub entries: Vec<String>,
    pub scan_count: usize,
    pub limit_reached: bool,
}

pub(crate) fn script_ls(listing: LsListing<'_>, cap: usize) -> rmcp::model::CallToolResult {
    let LsListing {
        path,
        entries,
        scan_count,
        limit_reached,
    } = listing;
    let mut notes = Vec::new();
    if limit_reached {
        notes.push(format!(
            "stopped at {DEFAULT_LS_LIMIT} entries. Use fs_find for a targeted search"
        ));
    }
    if scan_count >= LS_SCAN_HARD_LIMIT {
        notes.push(format!(
            "scan stopped at {LS_SCAN_HARD_LIMIT} entries to prevent excessive I/O"
        ));
    }
    let summary = if entries.is_empty() {
        format!("{path} (empty directory)")
    } else {
        ls_summary(path, entries.len(), scan_count, limit_reached)
    };
    script_list(("entries", "entries"), entries, (notes, summary), cap)
}

pub(crate) fn script_find(
    paths: Vec<String>,
    max_results: usize,
    limit_reached: bool,
    (prefix, cap): (Option<&str>, usize),
) -> rmcp::model::CallToolResult {
    let paths = paths
        .iter()
        .map(|path| prefixed(prefix, path))
        .collect::<Vec<_>>();
    let mut notes = Vec::new();
    if limit_reached {
        notes.push(format!(
            "stopped at {max_results} results. Use a more specific pattern"
        ));
    }
    let summary = if paths.is_empty() {
        "No files found matching pattern".to_string()
    } else {
        find_summary(paths.len(), max_results, limit_reached)
    };
    script_list(("paths", "paths"), paths, (notes, summary), cap)
}

pub(crate) struct GrepCut<'a> {
    pub location: &'a str,
    pub max_results: usize,
    pub limit_reached: bool,
    /// Whether the search found a match before any cap was applied.
    pub found_match: bool,
    /// Put before every path, see [`path_prefix`].
    pub path_prefix: Option<&'a str>,
}

fn match_item(path: &str, line: usize, text: &str, is_match: bool) -> Value {
    let mut item = json!({ "path": path, "line": line, "text": text });
    if !is_match {
        item["context"] = json!(true);
    }
    item
}

pub(crate) fn script_grep(
    lines: Vec<SearchLine>,
    cut: GrepCut<'_>,
    cap: usize,
) -> rmcp::model::CallToolResult {
    let rows = lines
        .iter()
        .filter_map(|line| match line {
            SearchLine::Row {
                path,
                line,
                text,
                is_match,
            } => Some((prefixed(cut.path_prefix, path), *line, text, *is_match)),
            SearchLine::BlockBreak => None,
        })
        .collect::<Vec<_>>();
    // grep's status reflects what it found, not what fit in the output.
    let exit_code = i32::from(!cut.found_match);
    let total = rows.len();
    let kept = items_within(
        rows.iter()
            .map(|(path, _, text, _)| path.len() + text.len() + GREP_ROW_OVERHEAD),
        cap,
    );
    let mut notes = Vec::new();
    if cut.limit_reached {
        notes.push(format!(
            "stopped at {} matches. Refine the pattern for more specific results",
            cut.max_results
        ));
    }
    if kept < total {
        notes.push(cap_note(kept, total, "matches", cap));
    }
    let matches = rows[..kept]
        .iter()
        .map(|(path, line, text, is_match)| match_item(path, *line, text, *is_match))
        .collect::<Vec<_>>();
    let summary = if !cut.found_match {
        "No matches found".to_string()
    } else {
        search_summary(
            cut.location,
            &SearchTruncation {
                match_count: kept,
                max_results: cut.max_results,
                limit_reached: cut.limit_reached,
                output_bytes: 0,
                raw_bytes: 0,
                byte_truncated: false,
            },
        )
    };
    script_result(
        json!({ "matches": matches, "notes": notes, "exit_code": exit_code }),
        summary,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(result: &rmcp::model::CallToolResult) -> &Value {
        result.structured_content.as_ref().unwrap()
    }

    #[test]
    fn prefix_joins_with_one_slash() {
        assert_eq!(prefixed(Some("src"), "a.rs"), "src/a.rs");
        assert_eq!(prefixed(Some("src/"), "a.rs"), "src/a.rs");
        assert_eq!(prefixed(Some("/"), "a.rs"), "/a.rs");
        assert_eq!(prefixed(None, "a.rs"), "a.rs");
    }

    #[test]
    fn prefix_leaves_absolute_paths_alone() {
        for abs in ["/x/a.txt", r"C:\x\a.txt", "C:/x/a.txt", r"\\?\C:\x\a.txt"] {
            assert_eq!(prefixed_on(true, Some("src"), abs), abs);
        }
        assert_eq!(prefixed_on(false, Some("src"), "/x/a.txt"), "/x/a.txt");
    }

    #[test]
    fn drive_and_backslash_paths_are_relative_on_unix() {
        assert_eq!(prefixed_on(false, Some("src"), "c:/x.txt"), "src/c:/x.txt");
        assert_eq!(prefixed_on(false, Some("src"), r"\x"), r"src/\x");
        assert_eq!(prefixed_on(true, Some("src"), "c:/x.txt"), "c:/x.txt");
        assert_eq!(prefixed_on(true, Some("src"), r"\x"), r"\x");
    }

    #[test]
    fn prefix_handles_empty_and_backslash_prefixes() {
        assert_eq!(prefixed(Some(""), "a.rs"), "a.rs");
        assert_eq!(prefixed(Some(r"src\"), "a.rs"), "src/a.rs");
        assert_eq!(prefixed(Some(r"C:\x\"), "a.rs"), r"C:\x/a.rs");
        assert_eq!(prefixed(Some(r"\\?\C:\x"), "a.rs"), r"C:\x/a.rs");
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(without_verbatim_prefix(r"\\?\C:\x"), r"C:\x");
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\host\share"),
            r"\\host\share"
        );
        assert_eq!(without_verbatim_prefix("/x/y"), "/x/y");
        assert_eq!(without_verbatim_prefix("C:/x"), "C:/x");
    }

    #[test]
    fn ls_stops_at_the_byte_cap_with_a_note() {
        let entries = ["aaaa", "bbbb", "cccc"].map(String::from).to_vec();
        let listing = LsListing {
            path: "d",
            entries,
            scan_count: 3,
            limit_reached: false,
        };
        let result = script_ls(listing, 10);
        assert_eq!(fields(&result)["entries"], json!(["aaaa", "bbbb"]));
        assert_eq!(
            fields(&result)["notes"],
            json!(["stopped at 2 of 3 entries to stay within 10B"])
        );
    }

    #[test]
    fn find_stops_at_the_byte_cap_with_a_note() {
        let paths = ["aaaa", "bbbb", "cccc"].map(String::from).to_vec();
        let result = script_find(paths, 10, false, (None, 10));
        assert_eq!(fields(&result)["paths"], json!(["aaaa", "bbbb"]));
        assert_eq!(
            fields(&result)["notes"],
            json!(["stopped at 2 of 3 paths to stay within 10B"])
        );
    }

    #[test]
    fn grep_stops_at_the_byte_cap_but_keeps_the_exit_code() {
        let row = |line| SearchLine::Row {
            path: "p".into(),
            line,
            text: "t".into(),
            is_match: true,
        };
        let cut = GrepCut {
            location: "workspace",
            max_results: 10,
            limit_reached: false,
            found_match: true,
            path_prefix: None,
        };
        let cap = 2 * (1 + 1 + GREP_ROW_OVERHEAD);
        let result = script_grep(vec![row(1), row(2), row(3)], cut, cap);
        assert_eq!(fields(&result)["matches"].as_array().unwrap().len(), 2);
        assert_eq!(fields(&result)["exit_code"], json!(0));
        assert_eq!(
            fields(&result)["notes"],
            json!([format!("stopped at 2 of 3 matches to stay within {cap}B")])
        );
    }
}
