// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".svn",
    ".hg",
    ".venv",
    "venv",
];

/// One output line of a content search, kept structured so a script can read
/// the fields and a model gets grep-style text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchLine {
    Row {
        path: String,
        line: usize,
        text: String,
        is_match: bool,
    },
    BlockBreak,
}

impl SearchLine {
    pub(crate) fn model_text(&self) -> String {
        match self {
            Self::Row {
                path,
                line,
                text,
                is_match,
            } => {
                let sep = if *is_match { ":" } else { "-" };
                format!("{path}{sep}{line}{sep} {text}")
            }
            Self::BlockBreak => "--".to_string(),
        }
    }
}

pub(crate) fn walk_dir_flat(dir: &Path, entries: &mut Vec<String>, scan_count: &mut usize) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if *scan_count >= LS_SCAN_HARD_LIMIT {
            break;
        }
        *scan_count += 1;

        if let Ok(entry) = entry {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    entries.push(format!("{name}/"));
                } else {
                    entries.push(name);
                }
            }
        }
    }
}

pub(crate) fn walk_dir_recursive(
    base: &Path,
    dir: &Path,
    entries: &mut Vec<String>,
    scan_count: &mut usize,
) {
    if *scan_count >= LS_SCAN_HARD_LIMIT {
        return;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if *scan_count >= LS_SCAN_HARD_LIMIT {
            break;
        }
        *scan_count += 1;

        if let Ok(entry) = entry {
            let path = entry.path();
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .display()
                .to_string();

            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        entries.push(format!("{relative}/"));
                        walk_dir_recursive(base, &path, entries, scan_count);
                    }
                } else {
                    entries.push(relative);
                }
            }
        }
    }
}

/// What `grep` searches with, whether `path` names a directory or one file.
pub(crate) struct GrepSearch<'a> {
    pub(crate) regex: &'a Regex,
    pub(crate) include_glob: Option<&'a glob::Pattern>,
    pub(crate) context_lines: usize,
    pub(crate) max_results: usize,
}

/// Walk a directory that is already open.
pub(crate) fn search_recursive(
    search: &GrepSearch<'_>,
    base: &Path,
    read_dir: std::fs::ReadDir,
    results: &mut Vec<SearchLine>,
) {
    for entry in read_dir {
        if results.len() >= search.max_results {
            return;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                search_subdir(search, base, &path, results);
            }
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        if search.include_glob.is_some_and(|glob| !glob.matches(&name)) {
            continue;
        }

        if let Some(content) = load_walked_file(&path) {
            search_file(search, base, &path, &content, results);
        }
    }
}

/// Descend into a directory met while walking; one that can't be opened is
/// skipped.
fn search_subdir(search: &GrepSearch<'_>, base: &Path, dir: &Path, results: &mut Vec<SearchLine>) {
    let opened = std::fs::read_dir(dir);
    opened
        .into_iter()
        .for_each(|read_dir| search_recursive(search, base, read_dir, results));
}

/// Read a file met while walking a directory. One that is unreadable, over
/// the size limit or binary is skipped, as `grep -rs` does.
fn load_walked_file(file: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(file).ok()?;
    if metadata.len() > SEARCH_FILE_MAX_BYTES {
        return None;
    }
    let content = std::fs::read(file).ok()?;
    (!is_binary_content(&content)).then_some(content)
}

/// Search the loaded content of `file`, naming rows relative to `base`.
pub(crate) fn search_file(
    search: &GrepSearch<'_>,
    base: &Path,
    file: &Path,
    content: &[u8],
    results: &mut Vec<SearchLine>,
) {
    let (regex, context_lines, max_results) =
        (search.regex, search.context_lines, search.max_results);
    if results.len() >= max_results {
        return;
    }

    let text = String::from_utf8_lossy(content);
    let lines = text.lines().collect::<Vec<_>>();
    let relative = file
        .strip_prefix(base)
        .unwrap_or(file)
        .display()
        .to_string();

    let room = max_results.saturating_sub(results.len());
    let match_indices = matching_lines(regex, &lines, room);

    if match_indices.is_empty() {
        return;
    }

    let blocks = context_blocks(&match_indices, context_lines, lines.len());

    for (block_index, (start, end)) in blocks.iter().enumerate() {
        if block_index > 0 {
            results.push(SearchLine::BlockBreak);
        }
        for (index, line_content) in lines[*start..=*end].iter().enumerate() {
            let abs_index = start + index;
            let line = truncate_line(line_content, GREP_MAX_LINE_LENGTH);
            let line_number = abs_index + 1;
            results.push(SearchLine::Row {
                path: relative.clone(),
                line: line_number,
                text: line,
                is_match: match_indices.contains(&abs_index),
            });
            if results.len() >= max_results {
                return;
            }
        }
    }
}

/// Merge the context windows around each match into inclusive line ranges.
fn context_blocks(
    matches: &[usize],
    context_lines: usize,
    line_count: usize,
) -> Vec<(usize, usize)> {
    let mut blocks = Vec::<(usize, usize)>::new();
    for &index in matches {
        let start = index.saturating_sub(context_lines);
        let end = (index + context_lines).min(line_count.saturating_sub(1));
        if let Some(last) = blocks.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        blocks.push((start, end));
    }
    blocks
}

/// Indices of the first `room` lines the regex matches.
fn matching_lines(regex: &Regex, lines: &[&str], room: usize) -> Vec<usize> {
    let matching = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| matches!(regex.is_match(line), Ok(true)));
    matching.map(|(index, _)| index).take(room).collect()
}
