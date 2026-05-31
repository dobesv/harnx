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

pub(crate) fn search_recursive(
    base: &Path,
    dir: &Path,
    regex: &Regex,
    context_lines: usize,
    max_results: usize,
    include_glob: Option<&glob::Pattern>,
    results: &mut Vec<String>,
) {
    if results.len() >= max_results {
        return;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if results.len() >= max_results {
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
                search_recursive(
                    base,
                    &path,
                    regex,
                    context_lines,
                    max_results,
                    include_glob,
                    results,
                );
            }
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        if let Some(glob) = include_glob {
            if !glob.matches(&name) {
                continue;
            }
        }

        search_file(base, &path, regex, context_lines, max_results, results);
    }
}

pub(crate) fn search_file(
    base: &Path,
    file: &Path,
    regex: &Regex,
    context_lines: usize,
    max_results: usize,
    results: &mut Vec<String>,
) {
    if results.len() >= max_results {
        return;
    }

    let _metadata = match std::fs::metadata(file) {
        Ok(metadata) if metadata.len() <= SEARCH_FILE_MAX_BYTES => metadata,
        _ => return,
    };

    let content = match std::fs::read(file) {
        Ok(content) => content,
        Err(_) => return,
    };

    if is_binary_content(&content) {
        return;
    }

    let text = String::from_utf8_lossy(&content);
    let lines = text.lines().collect::<Vec<_>>();
    let relative = file
        .strip_prefix(base)
        .unwrap_or(file)
        .display()
        .to_string();

    let mut match_indices = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if results.len() + match_indices.len() >= max_results {
            break;
        }
        if let Ok(true) = regex.is_match(line) {
            match_indices.push(index);
        }
    }

    if match_indices.is_empty() {
        return;
    }

    let mut blocks = Vec::<(usize, usize)>::new();
    for &index in &match_indices {
        let start = index.saturating_sub(context_lines);
        let end = (index + context_lines).min(lines.len().saturating_sub(1));
        if let Some(last) = blocks.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        blocks.push((start, end));
    }

    for (block_index, (start, end)) in blocks.iter().enumerate() {
        if block_index > 0 {
            results.push("--".to_string());
        }
        for (index, line_content) in lines[*start..=*end].iter().enumerate() {
            let abs_index = start + index;
            let line = truncate_line(line_content, GREP_MAX_LINE_LENGTH);
            let line_number = abs_index + 1;
            let sep = if match_indices.contains(&abs_index) {
                ":"
            } else {
                "-"
            };
            results.push(format!(
                "{}{}{}{} {}",
                relative, sep, line_number, sep, line
            ));
            if results.len() >= max_results {
                return;
            }
        }
    }
}
