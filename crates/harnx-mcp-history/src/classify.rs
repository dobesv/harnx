use std::ffi::OsStr;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotDecision {
    ReadOnly,
    Targeted(Vec<PathBuf>),
    FullSnapshot,
}

#[derive(Clone, Copy)]
struct CommandRule {
    command: &'static str,
    subcommand: Option<&'static str>,
    classify: fn(&[&str], &Path) -> SnapshotDecision,
}

static RULES: &[CommandRule] = &[
    CommandRule {
        command: "git",
        subcommand: None,
        classify: classify_git,
    },
    CommandRule {
        command: "cargo",
        subcommand: Some("check"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "cargo",
        subcommand: Some("test"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "cargo",
        subcommand: Some("clippy"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "cargo",
        subcommand: Some("build"),
        classify: classify_cargo_build,
    },
    CommandRule {
        command: "ls",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "find",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "cat",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "head",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "tail",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "wc",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "stat",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "file",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "du",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "df",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "grep",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "rg",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "ag",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "ack",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "echo",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "printf",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "date",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "pwd",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "which",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "type",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "curl",
        subcommand: None,
        classify: classify_curl,
    },
    CommandRule {
        command: "wget",
        subcommand: None,
        classify: classify_wget,
    },
    CommandRule {
        command: "docker",
        subcommand: Some("ps"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "docker",
        subcommand: Some("images"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "docker",
        subcommand: Some("inspect"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "docker",
        subcommand: Some("logs"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "kubectl",
        subcommand: Some("get"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "kubectl",
        subcommand: Some("describe"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "kubectl",
        subcommand: Some("logs"),
        classify: classify_read_only,
    },
    CommandRule {
        command: "jq",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "gh",
        subcommand: None,
        classify: classify_gh,
    },
    CommandRule {
        command: "diff",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "sort",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "uniq",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "cut",
        subcommand: None,
        classify: classify_read_only,
    },
    CommandRule {
        command: "awk",
        subcommand: None,
        classify: classify_awk,
    },
    CommandRule {
        command: "sed",
        subcommand: None,
        classify: classify_sed,
    },
    CommandRule {
        command: "cp",
        subcommand: None,
        classify: classify_cp,
    },
    CommandRule {
        command: "mv",
        subcommand: None,
        classify: classify_mv,
    },
    CommandRule {
        command: "ln",
        subcommand: None,
        classify: classify_ln,
    },
    CommandRule {
        command: "touch",
        subcommand: None,
        classify: classify_touch,
    },
    CommandRule {
        command: "truncate",
        subcommand: None,
        classify: classify_truncate,
    },
    CommandRule {
        command: "chmod",
        subcommand: None,
        classify: classify_chmod,
    },
    CommandRule {
        command: "chown",
        subcommand: None,
        classify: classify_chown,
    },
    CommandRule {
        command: "npm",
        subcommand: Some("install"),
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "npm",
        subcommand: Some("ci"),
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "pip",
        subcommand: Some("install"),
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "make",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "cmake",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "ninja",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "rm",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "bash",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "sh",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "zsh",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    CommandRule {
        command: "dash",
        subcommand: None,
        classify: classify_full_snapshot,
    },
    #[cfg(test)]
    CommandRule {
        command: "panic-rule",
        subcommand: None,
        classify: classify_panic_rule,
    },
];

pub fn classify_command(raw: &str, cwd: &Path) -> SnapshotDecision {
    panic::catch_unwind(AssertUnwindSafe(|| classify_command_inner(raw, cwd)))
        .unwrap_or(SnapshotDecision::FullSnapshot)
}

fn classify_command_inner(raw: &str, cwd: &Path) -> SnapshotDecision {
    if raw.trim().is_empty() || contains_opaque_substitution(raw) {
        return SnapshotDecision::FullSnapshot;
    }

    let mut decision = SnapshotDecision::ReadOnly;
    for segment in split_compound(raw) {
        let segment_decision = classify_single(&segment, cwd);
        decision = merge_decisions(decision, segment_decision);
        if matches!(decision, SnapshotDecision::FullSnapshot) {
            return SnapshotDecision::FullSnapshot;
        }
    }

    decision
}

fn classify_single(raw_segment: &str, cwd: &Path) -> SnapshotDecision {
    let trimmed = raw_segment.trim();
    if trimmed.is_empty() {
        return SnapshotDecision::FullSnapshot;
    }

    let redirect_targets = extract_redirect_targets(trimmed)
        .into_iter()
        .map(|target| cwd.join(target))
        .collect::<Vec<_>>();

    let argv_owned = match shell_words::split(trimmed) {
        Ok(argv) if !argv.is_empty() => argv,
        _ => return SnapshotDecision::FullSnapshot,
    };
    let argv = argv_owned.iter().map(String::as_str).collect::<Vec<_>>();
    let (command, subcommand, rest) = normalize_argv(&argv);
    if command.is_empty() {
        return SnapshotDecision::FullSnapshot;
    }

    let mut decision = SnapshotDecision::FullSnapshot;
    for rule in RULES {
        if rule.command != command {
            continue;
        }
        let matched = match rule.subcommand {
            Some(expected) => subcommand == Some(expected),
            None => true,
        };
        if matched {
            decision = (rule.classify)(rest, cwd);
            break;
        }
    }

    if redirect_targets.is_empty() {
        decision
    } else {
        merge_decisions(decision, SnapshotDecision::Targeted(redirect_targets))
    }
}

fn extract_redirect_targets(raw: &str) -> Vec<String> {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut targets = Vec::new();
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut stage_start = true;

    while index < chars.len() {
        let ch = chars[index];

        if ch == '\'' && !in_double {
            in_single = !in_single;
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            index += 1;
            continue;
        }
        if in_single || in_double {
            index += 1;
            continue;
        }

        if stage_start {
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            let token_start = index;
            while index < chars.len()
                && !chars[index].is_whitespace()
                && chars[index] != '|'
                && chars[index] != ';'
                && chars[index] != '>'
            {
                index += 1;
            }
            let token = chars[token_start..index].iter().collect::<String>();
            if token == "tee" {
                while index < chars.len() && chars[index].is_whitespace() {
                    index += 1;
                }
                let file_start = index;
                while index < chars.len()
                    && !chars[index].is_whitespace()
                    && chars[index] != '|'
                    && chars[index] != ';'
                    && chars[index] != '>'
                {
                    index += 1;
                }
                let file = chars[file_start..index].iter().collect::<String>();
                if !file.is_empty() && !file.starts_with('-') {
                    targets.push(file);
                }
            }
            stage_start = false;
            continue;
        }

        if ch == '|' || ch == ';' {
            stage_start = true;
            index += 1;
            continue;
        }

        if ch == '>' {
            index += 1;
            if index < chars.len() && chars[index] == '>' {
                index += 1;
            }
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            let file_start = index;
            while index < chars.len()
                && !chars[index].is_whitespace()
                && chars[index] != '|'
                && chars[index] != ';'
            {
                index += 1;
            }
            let file = chars[file_start..index].iter().collect::<String>();
            if !file.is_empty() {
                targets.push(file);
            }
            continue;
        }

        index += 1;
    }

    targets
}

fn split_compound(raw: &str) -> Vec<String> {
    if contains_opaque_substitution(raw) {
        return vec![raw.to_string()];
    }

    let chars = raw.chars().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];
        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            index += 1;
            continue;
        }
        if !in_single && !in_double {
            if ch == ';' {
                push_segment(&mut segments, &mut current);
                index += 1;
                continue;
            }
            if ch == '&' && chars.get(index + 1) == Some(&'&') {
                push_segment(&mut segments, &mut current);
                index += 2;
                continue;
            }
            if ch == '|' && chars.get(index + 1) == Some(&'|') {
                push_segment(&mut segments, &mut current);
                index += 2;
                continue;
            }
            if ch == '|' {
                push_segment(&mut segments, &mut current);
                index += 1;
                continue;
            }
        }
        current.push(ch);
        index += 1;
    }

    push_segment(&mut segments, &mut current);
    if segments.is_empty() {
        vec![raw.trim().to_string()]
    } else {
        segments
    }
}

fn normalize_argv<'a>(argv: &'a [&'a str]) -> (&'a str, Option<&'a str>, &'a [&'a str]) {
    let mut index = 0;
    while index < argv.len() {
        match argv[index] {
            "sudo" | "time" | "nice" | "nohup" => {
                index += 1;
            }
            "env" => {
                index += 1;
                while index < argv.len() && argv[index].contains('=') {
                    index += 1;
                }
            }
            _ => break,
        }
    }

    if index >= argv.len() {
        return ("", None, &[]);
    }

    let base = basename(argv[index]);
    let rest = &argv[index + 1..];
    let subcommand = rest.iter().copied().find(|arg| !arg.starts_with('-'));

    (base, subcommand, rest)
}

fn merge_decisions(left: SnapshotDecision, right: SnapshotDecision) -> SnapshotDecision {
    match (left, right) {
        (SnapshotDecision::FullSnapshot, _) | (_, SnapshotDecision::FullSnapshot) => {
            SnapshotDecision::FullSnapshot
        }
        (SnapshotDecision::ReadOnly, SnapshotDecision::ReadOnly) => SnapshotDecision::ReadOnly,
        (SnapshotDecision::ReadOnly, SnapshotDecision::Targeted(paths))
        | (SnapshotDecision::Targeted(paths), SnapshotDecision::ReadOnly) => {
            SnapshotDecision::Targeted(dedup_paths(paths))
        }
        (SnapshotDecision::Targeted(mut left), SnapshotDecision::Targeted(right)) => {
            left.extend(right);
            SnapshotDecision::Targeted(dedup_paths(left))
        }
    }
}

fn classify_read_only(_: &[&str], _: &Path) -> SnapshotDecision {
    SnapshotDecision::ReadOnly
}

fn classify_full_snapshot(_: &[&str], _: &Path) -> SnapshotDecision {
    SnapshotDecision::FullSnapshot
}

fn classify_git(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    match argv.first().copied() {
        Some("status") | Some("log") | Some("diff") | Some("show") | Some("branch")
        | Some("tag") | Some("remote") | Some("fetch") | Some("ls-files") | Some("blame")
        | Some("shortlog") | Some("describe") | Some("rev-parse") => SnapshotDecision::ReadOnly,
        Some("restore") => classify_git_restore(argv, cwd),
        Some("add") => classify_git_add(argv, cwd),
        Some("rm") => classify_git_rm(argv, cwd),
        Some("switch") | Some("checkout") | Some("merge") | Some("rebase") | Some("pull")
        | Some("reset") => SnapshotDecision::FullSnapshot,
        Some("stash") => match argv
            .iter()
            .copied()
            .filter(|arg| !arg.starts_with('-'))
            .nth(1)
        {
            Some("list") => SnapshotDecision::ReadOnly,
            Some("pop") | Some("apply") => SnapshotDecision::FullSnapshot,
            _ => SnapshotDecision::FullSnapshot,
        },
        _ => SnapshotDecision::FullSnapshot,
    }
}

fn classify_cargo_build(argv: &[&str], _: &Path) -> SnapshotDecision {
    if argv.contains(&"--dry-run") {
        SnapshotDecision::ReadOnly
    } else {
        SnapshotDecision::FullSnapshot
    }
}

fn classify_curl(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let mut index = 0;
    while index < argv.len() {
        match argv[index] {
            "-X" => {
                if let Some(method) = argv.get(index + 1) {
                    let method = method.to_ascii_uppercase();
                    if matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH") {
                        return SnapshotDecision::FullSnapshot;
                    }
                }
                index += 2;
                continue;
            }
            arg if arg.starts_with("-X") => {
                let method = arg[2..].to_ascii_uppercase();
                if matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH") {
                    return SnapshotDecision::FullSnapshot;
                }
            }
            "--data" | "-d" | "--data-raw" | "--data-binary" | "--upload-file" | "-T" => {
                return SnapshotDecision::FullSnapshot;
            }
            "-o" | "--output" => {
                let Some(path) = argv.get(index + 1) else {
                    return SnapshotDecision::FullSnapshot;
                };
                let path = Path::new(path);
                let path = if path.is_absolute() {
                    PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                return SnapshotDecision::Targeted(vec![path]);
            }
            arg if arg.starts_with("-o") && arg.len() > 2 => {
                let path = Path::new(&arg[2..]);
                let path = if path.is_absolute() {
                    PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                return SnapshotDecision::Targeted(vec![path]);
            }
            arg if arg.starts_with("--output=") => {
                let path = Path::new(&arg[9..]);
                let path = if path.is_absolute() {
                    PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                return SnapshotDecision::Targeted(vec![path]);
            }
            _ => {}
        }
        index += 1;
    }
    SnapshotDecision::ReadOnly
}

fn classify_wget(argv: &[&str], _: &Path) -> SnapshotDecision {
    if argv.contains(&"--spider") {
        SnapshotDecision::ReadOnly
    } else {
        SnapshotDecision::FullSnapshot
    }
}

fn classify_gh(argv: &[&str], _: &Path) -> SnapshotDecision {
    let mut positional = argv.iter().copied().filter(|arg| !arg.starts_with('-'));
    match (positional.next(), positional.next()) {
        (Some("issue"), Some("view" | "list")) | (Some("pr"), Some("view" | "list")) => {
            SnapshotDecision::ReadOnly
        }
        _ => SnapshotDecision::FullSnapshot,
    }
}

fn classify_sed(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let has_print_only = argv.contains(&"-n");
    let has_in_place = argv.iter().any(|arg| *arg == "-i" || arg.starts_with("-i"));

    if has_in_place {
        return last_positional_path(argv, cwd)
            .map_or(SnapshotDecision::FullSnapshot, single_target);
    }
    if has_print_only {
        SnapshotDecision::ReadOnly
    } else {
        SnapshotDecision::FullSnapshot
    }
}

fn classify_awk(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let mut index = 0;
    while index < argv.len() {
        if argv[index] == "-i" && argv.get(index + 1) == Some(&"inplace") {
            return last_positional_path(argv, cwd)
                .map_or(SnapshotDecision::FullSnapshot, single_target);
        }
        index += 1;
    }
    SnapshotDecision::ReadOnly
}

fn classify_cp(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    last_positional_path(argv, cwd).map_or(SnapshotDecision::FullSnapshot, single_target)
}

fn classify_mv(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = positional_paths(argv, cwd);
    if paths.len() < 2 {
        return SnapshotDecision::FullSnapshot;
    }
    SnapshotDecision::Targeted(dedup_paths(vec![
        paths[0].clone(),
        paths[paths.len() - 1].clone(),
    ]))
}

fn classify_ln(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = positional_paths(argv, cwd);
    if paths.len() < 2 {
        return SnapshotDecision::FullSnapshot;
    }
    single_target(paths[paths.len() - 1].clone())
}

fn classify_touch(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    all_positional_targets(argv, cwd)
}

fn classify_truncate(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    last_positional_path(argv, cwd).map_or(SnapshotDecision::FullSnapshot, single_target)
}

fn classify_chmod(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = positionals_after_flags(argv, cwd, 1);
    if paths.is_empty() {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

fn classify_chown(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = positionals_after_flags(argv, cwd, 1);
    if paths.is_empty() {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

fn classify_git_restore(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = git_paths_after_subcommand(argv, cwd);
    if paths.is_empty() {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

fn classify_git_add(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    if has_any_flag(argv, &["-A", "--all", "-p", "--patch", "-u", "--update"]) {
        return SnapshotDecision::FullSnapshot;
    }

    let raw_paths = raw_positionals_after_flags(argv, 1);
    if raw_paths.is_empty() {
        return SnapshotDecision::FullSnapshot;
    }

    let paths = raw_paths
        .iter()
        .map(|arg| cwd.join(arg))
        .collect::<Vec<_>>();
    if raw_paths.iter().any(|arg| *arg == "." || *arg == "..")
        || paths.iter().any(|path| path.is_dir())
    {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

fn classify_git_rm(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    if has_any_flag(argv, &["-r", "--recursive"]) {
        return SnapshotDecision::FullSnapshot;
    }

    let paths = positionals_after_flags(argv, cwd, 1);
    if paths.is_empty() {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

#[cfg(test)]
fn classify_panic_rule(_: &[&str], _: &Path) -> SnapshotDecision {
    panic!("panic rule hit");
}

fn contains_opaque_substitution(raw: &str) -> bool {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut in_single = false;
    let mut in_double = false;
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];
        if ch == '\'' && !in_double {
            in_single = !in_single;
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            index += 1;
            continue;
        }
        if !in_single {
            if ch == '`' {
                return true;
            }
            if ch == '$' && chars.get(index + 1) == Some(&'(') {
                return true;
            }
        }
        if !in_single && !in_double && ch == '<' && chars.get(index + 1) == Some(&'(') {
            return true;
        }
        index += 1;
    }

    false
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    current.clear();
}

fn basename(command: &str) -> &str {
    let path = Path::new(command);
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or(command);
    name.strip_suffix(".exe").unwrap_or(name)
}

fn positional_paths(argv: &[&str], cwd: &Path) -> Vec<PathBuf> {
    argv.iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| *arg != ">" && *arg != ">>" && *arg != "|")
        .map(|arg| cwd.join(arg))
        .collect()
}

fn positionals_after_flags(argv: &[&str], cwd: &Path, skip: usize) -> Vec<PathBuf> {
    raw_positionals_after_flags(argv, skip)
        .into_iter()
        .map(|arg| cwd.join(arg))
        .collect()
}

fn raw_positionals_after_flags<'a>(argv: &'a [&'a str], skip: usize) -> Vec<&'a str> {
    argv.iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| *arg != ">" && *arg != ">>" && *arg != "|")
        .skip(skip)
        .collect()
}

fn has_any_flag(argv: &[&str], flags: &[&str]) -> bool {
    argv.iter().any(|arg| {
        // Exact match handles long-form flags (--all, --recursive) and standalone short flags (-A, -r)
        if flags.contains(arg) {
            return true;
        }
        // Combined short-flag bundles like `-rf` or `-Ap`: check each char after the leading `-`,
        // but only for true short bundles (start with single `-` and not `--`).
        if let Some(stripped) = arg.strip_prefix('-') {
            if !stripped.is_empty() && !arg.starts_with("--") {
                for ch in stripped.chars() {
                    let candidate = format!("-{ch}");
                    if flags.iter().any(|f| *f == candidate) {
                        return true;
                    }
                }
            }
        }
        false
    })
}

fn last_positional_path(argv: &[&str], cwd: &Path) -> Option<PathBuf> {
    let mut skip_next_redirect_target = false;
    for arg in argv.iter().rev().copied() {
        if skip_next_redirect_target {
            skip_next_redirect_target = false;
            continue;
        }
        if arg == ">" || arg == ">>" {
            skip_next_redirect_target = true;
            continue;
        }
        if !arg.starts_with('-') && arg != "|" {
            return Some(cwd.join(arg));
        }
    }
    None
}

fn git_paths_after_subcommand(argv: &[&str], cwd: &Path) -> Vec<PathBuf> {
    if let Some(separator) = argv.iter().position(|arg| *arg == "--") {
        return argv[separator + 1..]
            .iter()
            .map(|arg| cwd.join(arg))
            .collect();
    }

    positionals_after_flags(argv, cwd, 1)
}

fn all_positional_targets(argv: &[&str], cwd: &Path) -> SnapshotDecision {
    let paths = positional_paths(argv, cwd);
    if paths.is_empty() {
        SnapshotDecision::FullSnapshot
    } else {
        SnapshotDecision::Targeted(dedup_paths(paths))
    }
}

fn single_target(path: PathBuf) -> SnapshotDecision {
    SnapshotDecision::Targeted(vec![path])
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/tmp/project")
    }

    #[test]
    fn extract_redirects_and_tee_targets() {
        assert_eq!(
            extract_redirect_targets("echo foo > out.txt"),
            vec!["out.txt"]
        );
        assert_eq!(
            extract_redirect_targets("echo foo >> out.txt"),
            vec!["out.txt"]
        );
        assert_eq!(
            extract_redirect_targets("cmd | tee file.txt"),
            vec!["file.txt"]
        );
        assert_eq!(
            extract_redirect_targets("echo '>' > out.txt"),
            vec!["out.txt"]
        );
        assert!(extract_redirect_targets("echo '>'").is_empty());
    }

    #[test]
    fn split_compound_handles_quotes_and_opaque_cases() {
        assert_eq!(
            split_compound("git status && cp a b"),
            vec!["git status", "cp a b"]
        );
        assert_eq!(
            split_compound("echo 'a|b' | tee out.txt"),
            vec!["echo 'a|b'", "tee out.txt"]
        );
        assert_eq!(split_compound("echo $(date)"), vec!["echo $(date)"]);
    }

    #[test]
    fn normalize_argv_skips_wrappers_and_normalizes_binary() {
        let argv = ["sudo", "git", "status"];
        assert_eq!(
            normalize_argv(&argv),
            ("git", Some("status"), &["status"][..])
        );

        let argv = ["env", "FOO=bar", "/usr/bin/cat", "file.txt"];
        assert_eq!(
            normalize_argv(&argv),
            ("cat", Some("file.txt"), &["file.txt"][..])
        );

        let argv = ["C:/bin/grep.exe", "needle", "file.txt"];
        assert_eq!(
            normalize_argv(&argv),
            ("grep", Some("needle"), &["needle", "file.txt"][..])
        );
    }

    #[test]
    fn read_only_command_families_classify_read_only() {
        let cwd = cwd();
        for command in [
            "git status",
            "git stash list",
            "cargo check",
            "cargo test",
            "cargo clippy",
            "cargo build --dry-run",
            "ls -la",
            "find src -name '*.rs'",
            "cat file.txt",
            "head -n 1 file.txt",
            "tail -n 1 file.txt",
            "wc -l file.txt",
            "stat file.txt",
            "file file.txt",
            "du -sh .",
            "df -h",
            "grep foo file.txt",
            "rg foo",
            "ag foo",
            "ack foo",
            "echo hello",
            "printf '%s' hi",
            "date",
            "pwd",
            "which git",
            "type git",
            "curl https://example.com",
            "wget --spider https://example.com",
            "docker ps",
            "docker images",
            "docker inspect alpine",
            "docker logs container",
            "kubectl get pods",
            "kubectl describe pod demo",
            "kubectl logs demo",
            "jq . file.json",
            "gh issue view 1",
            "gh issue list",
            "gh pr view 2",
            "gh pr list",
            "diff a b",
            "sort file.txt",
            "uniq file.txt",
            "cut -d, -f1 data.csv",
            "awk '{print $1}' file.txt",
            "sed -n 'p' file.txt",
            "sudo git status",
            "/usr/bin/cat file.txt",
        ] {
            assert_eq!(
                classify_command(command, &cwd),
                SnapshotDecision::ReadOnly,
                "{command}"
            );
        }
    }

    #[test]
    fn targeted_command_families_return_expected_paths() {
        let cwd = cwd();
        assert_eq!(
            classify_command("cp src.rs dst.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("dst.rs")])
        );
        assert_eq!(
            classify_command("mv old.rs new.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("old.rs"), cwd.join("new.rs")])
        );
        assert_eq!(
            classify_command("ln -s src dst", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("dst")])
        );
        assert_eq!(
            classify_command("touch a b", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("a"), cwd.join("b")])
        );
        assert_eq!(
            classify_command("truncate -s 0 file.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt")])
        );
        assert_eq!(
            classify_command("chmod 644 file.txt other.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt"), cwd.join("other.txt")])
        );
        assert_eq!(
            classify_command("chown root file.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt")])
        );
        assert_eq!(
            classify_command("sed -i 's/x/y/' path.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("path.rs")])
        );
        assert_eq!(
            classify_command("sed -i.bak 's/x/y/' path.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("path.rs")])
        );
        assert_eq!(
            classify_command("awk -i inplace '{print $1}' path.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("path.rs")])
        );
        assert_eq!(
            classify_command("git restore file.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt")])
        );
        assert_eq!(
            classify_command("git restore -- file.txt other.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt"), cwd.join("other.txt")])
        );
        assert_eq!(
            classify_command("git add file.txt other.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt"), cwd.join("other.txt")])
        );
        assert_eq!(
            classify_command("git add src/lib.rs", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("src/lib.rs")])
        );
        assert_eq!(
            classify_command("git rm file.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("file.txt")])
        );
        assert_eq!(
            classify_command("echo hello > out.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("out.txt")])
        );
        assert_eq!(
            classify_command("echo hello >> out.txt", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("out.txt")])
        );
        assert_eq!(
            classify_command("curl -o output.json https://example.com", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("output.json")])
        );
        assert_eq!(
            classify_command("curl --output report.txt https://example.com", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("report.txt")])
        );
        assert_eq!(
            classify_command("curl --output=/tmp/data.json https://api.example.com", &cwd),
            SnapshotDecision::Targeted(vec![PathBuf::from("/tmp/data.json")])
        );
    }

    #[test]
    fn full_snapshot_triggers_classify_full_snapshot() {
        let cwd = cwd();
        for command in [
            "git switch main",
            "git checkout main",
            "git merge branch",
            "git rebase main",
            "git stash pop",
            "git stash apply",
            "git pull",
            "git reset --hard",
            "git add .",
            "git add -A",
            "npm install",
            "npm ci",
            "pip install requests",
            "make all",
            "cmake ..",
            "ninja",
            "cargo build",
            "curl -X POST https://example.com",
            "curl --data foo=bar https://example.com",
            "wget https://example.com/file",
            "gh issue create",
            "rm -rf target",
            "unknown_binary --args",
            "",
            "   ",
            "bash -c 'something'",
            "bash script.sh",
            "sh script.sh",
            "zsh script.sh",
            "dash script.sh",
            "echo $(date)",
            "echo `date`",
            "cat <(echo hi)",
            "sed 's/x/y/' file.txt",
        ] {
            assert_eq!(
                classify_command(command, &cwd),
                SnapshotDecision::FullSnapshot,
                "{command}"
            );
        }
        // Combined short-flag bundles must trigger FullSnapshot
        assert_eq!(
            classify_command("git rm -rf src", &cwd),
            SnapshotDecision::FullSnapshot
        );
        assert_eq!(
            classify_command("git add -Ap", &cwd),
            SnapshotDecision::FullSnapshot
        );
        assert_eq!(
            classify_command("malformed 'quote", &cwd),
            SnapshotDecision::FullSnapshot
        );
        assert_eq!(
            classify_command(r#"echo "$(touch file)""#, &cwd),
            SnapshotDecision::FullSnapshot
        );
    }

    #[test]
    fn single_quoted_command_substitution_is_literal() {
        let cwd = cwd();
        assert_eq!(
            classify_command("echo '$(not a command)'", &cwd),
            SnapshotDecision::ReadOnly
        );
    }

    #[test]
    fn compound_merging_works() {
        let cwd = cwd();
        assert_eq!(
            classify_command("git status && cp a b", &cwd),
            SnapshotDecision::Targeted(vec![cwd.join("b")])
        );
        assert_eq!(
            classify_command("cp a b && git checkout main", &cwd),
            SnapshotDecision::FullSnapshot
        );
    }

    #[test]
    fn panic_in_rule_fails_open() {
        let cwd = cwd();
        assert_eq!(
            classify_command("panic-rule", &cwd),
            SnapshotDecision::FullSnapshot
        );
    }
}
