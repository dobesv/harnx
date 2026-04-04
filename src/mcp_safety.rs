use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const READ_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const WRITE_MAX_BYTES: usize = 100 * 1024 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;
pub const DEFAULT_GREP_LIMIT: usize = 100;
pub const DEFAULT_FIND_LIMIT: usize = 1000;
pub const DEFAULT_LS_LIMIT: usize = 500;
pub const LS_SCAN_HARD_LIMIT: usize = 20_000;
pub const SEARCH_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;

pub fn path_to_file_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let s = s.replace('\\', "/");
    let encoded = percent_encode_path(&s);
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let path_str = uri
        .strip_prefix("file://localhost")
        .or_else(|| uri.strip_prefix("file://"))?;
    let decoded = percent_decode(path_str);
    // On Windows, file:///C:/foo produces path_str="/C:/foo" — strip the leading slash.
    #[cfg(windows)]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|s| s.len() >= 2 && s.as_bytes()[1] == b':')
        .unwrap_or(&decoded)
        .to_string();
    Some(PathBuf::from(decoded))
}

fn percent_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            // RFC 3986 unreserved + '/' (path separator) + ':' (drive letter)
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX_UPPER[(b >> 4) as usize] as char);
                out.push(HEX_UPPER[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut iter = input.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next().and_then(hex_val);
            let lo = iter.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                bytes.push(h << 4 | l);
            } else {
                bytes.push(b'%');
            }
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncateOpts {
    pub head_lines: usize,
    pub tail_lines: usize,
    pub line_head_bytes: usize,
    pub line_tail_bytes: usize,
    pub max_output_bytes: usize,
    pub marker: Option<String>,
}

impl Default for TruncateOpts {
    fn default() -> Self {
        Self {
            head_lines: 25,
            tail_lines: 75,
            line_head_bytes: 500,
            line_tail_bytes: 1500,
            max_output_bytes: 16_384,
            marker: None,
        }
    }
}

#[cfg(test)]
pub fn truncate_head(content: String, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let total_lines = count_lines(&content);

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content,
            truncated: false,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
        };
    }

    let mut line_count = 0usize;
    let mut byte_pos = 0usize;
    let bytes = content.as_bytes();

    while byte_pos < bytes.len() && line_count < max_lines {
        let next_nl = bytes[byte_pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| byte_pos + p);

        let line_end_with_nl = match next_nl {
            Some(nl) => nl + 1,
            None => bytes.len(),
        };

        if line_end_with_nl > max_bytes {
            if line_count == 0 {
                let valid = clamp_boundary_left(&content, max_bytes.min(bytes.len()));
                if valid > 0 {
                    line_count = 1;
                    byte_pos = valid;
                }
            }
            break;
        }

        byte_pos = line_end_with_nl;
        line_count += 1;
    }

    let mut truncated_content = content;
    truncated_content.truncate(byte_pos);

    TruncationResult {
        content: truncated_content,
        truncated: true,
        total_lines,
        total_bytes,
        output_lines: line_count,
        output_bytes: byte_pos,
    }
}

pub fn sanitize_output_text(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\u{FFFD}' {
            continue;
        }
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        out.push(ch);
    }
    out
}

pub fn truncate_output(s: &str, opts: &TruncateOpts) -> String {
    let mut out = sanitize_output_text(s);
    if out.is_empty() {
        return out;
    }

    let opts = normalize_opts(opts.clone());

    let line_threshold = opts.line_head_bytes.saturating_add(opts.line_tail_bytes);
    if line_threshold > 0 {
        let mut lines = split_lines_preserve_newlines(&out);
        for line in &mut lines {
            let had_newline = line.ends_with('\n');
            let body = if had_newline {
                &line[..line.len() - 1]
            } else {
                line.as_str()
            };
            if body.len() > line_threshold {
                let head_len = clamp_boundary_left(body, opts.line_head_bytes.min(body.len()));
                let tail_start =
                    clamp_boundary_right(body, body.len().saturating_sub(opts.line_tail_bytes));
                let removed = body
                    .len()
                    .saturating_sub(head_len)
                    .saturating_sub(body.len().saturating_sub(tail_start));

                let marker = if let Some(ref m) = opts.marker {
                    m.clone()
                } else {
                    format!("... [truncated: {removed} bytes removed from line] ...")
                };

                let mut rebuilt = String::with_capacity(
                    head_len + marker.len() + (body.len() - tail_start) + usize::from(had_newline),
                );
                rebuilt.push_str(&body[..head_len]);
                rebuilt.push_str(&marker);
                rebuilt.push_str(&body[tail_start..]);
                if had_newline {
                    rebuilt.push('\n');
                }
                *line = rebuilt;
            }
        }
        out = lines.concat();
    }

    let mut lines = split_lines_preserve_newlines(&out);
    let total_window = opts.head_lines.saturating_add(opts.tail_lines);
    if total_window > 0 && lines.len() > total_window {
        let removed = lines.len() - total_window;
        let mut kept = Vec::with_capacity(total_window + 1);
        if opts.head_lines > 0 {
            kept.extend(lines.iter().take(opts.head_lines).cloned());
        }

        let marker = if let Some(ref m) = opts.marker {
            format!("{m}\n")
        } else {
            format!("... [truncated: {removed} lines removed. Use head_lines/tail_lines to show more] ...\n")
        };
        kept.push(marker);

        if opts.tail_lines > 0 {
            kept.extend(lines.iter().skip(lines.len() - opts.tail_lines).cloned());
        }
        lines = kept;
        out = lines.concat();
    }

    if out.len() > opts.max_output_bytes {
        let approx_marker = if let Some(ref m) = opts.marker {
            m.clone()
        } else {
            format!(
                "\n... [truncated: {} bytes removed, showing first {} + last {} bytes. Use max_output_bytes to increase limit] ...\n",
                out.len(),
                opts.max_output_bytes / 4,
                opts.max_output_bytes
                    .saturating_sub(opts.max_output_bytes / 4)
            )
        };

        let marker_budget = approx_marker
            .len()
            .min(opts.max_output_bytes.saturating_sub(1));
        let mut remaining_budget = opts.max_output_bytes.saturating_sub(marker_budget);
        if remaining_budget < 2 {
            remaining_budget = 2;
        }

        let mut head_bytes = remaining_budget / 4;
        let mut tail_bytes = remaining_budget - head_bytes;
        head_bytes = head_bytes.min(out.len());
        tail_bytes = tail_bytes.min(out.len().saturating_sub(head_bytes));

        let head_end = clamp_boundary_left(&out, head_bytes);
        let tail_start = clamp_boundary_right(&out, out.len().saturating_sub(tail_bytes));
        let removed = out
            .len()
            .saturating_sub(head_end)
            .saturating_sub(out.len().saturating_sub(tail_start));

        let marker = if let Some(ref m) = opts.marker {
            m.clone()
        } else {
            format!(
                "\n... [truncated: {removed} bytes removed, showing first {} + last {} bytes. Use max_output_bytes to increase limit] ...\n",
                head_end,
                out.len() - tail_start
            )
        };

        out = format!("{}{}{}", &out[..head_end], marker, &out[tail_start..]);
    }

    out
}

fn normalize_opts(mut opts: TruncateOpts) -> TruncateOpts {
    if opts.max_output_bytes == 0 {
        opts.max_output_bytes = 1;
    }
    opts
}

fn split_lines_preserve_newlines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split_inclusive('\n').map(ToOwned::to_owned).collect()
}

fn clamp_boundary_left(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn clamp_boundary_right(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        bytecount(s, b'\n') + usize::from(!s.ends_with('\n'))
    }
}

#[cfg(test)]
fn bytecount(s: &str, needle: u8) -> usize {
    s.as_bytes().iter().filter(|&&b| b == needle).count()
}

pub fn validate_path(path_str: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let path = Path::new(path_str);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    let canonical = resolved
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path '{}': {}", path_str, e))?;

    if roots.is_empty() {
        return Err("No roots configured — all filesystem access is denied".to_string());
    }

    for root in roots {
        if let Ok(canonical_root) = root.canonicalize() {
            if canonical.starts_with(&canonical_root) {
                return Ok(canonical);
            }
        }
    }

    Err(format!(
        "Path '{}' is outside allowed roots: [{}]",
        path_str,
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

pub fn validate_write_path(path_str: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let path = Path::new(path_str);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    if resolved.exists() {
        return validate_path(path_str, roots);
    }

    let parent = resolved
        .parent()
        .ok_or_else(|| format!("Cannot determine parent directory for '{}'", path_str))?;

    if !parent.exists() {
        let mut ancestor = parent.to_path_buf();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| format!("No existing ancestor for '{}'", path_str))?
                .to_path_buf();
        }
        let canonical_ancestor = ancestor
            .canonicalize()
            .map_err(|e| format!("Cannot resolve ancestor: {}", e))?;

        if roots.is_empty() {
            return Err("No roots configured — all filesystem access is denied".to_string());
        }
        let in_root = roots.iter().any(|root| {
            root.canonicalize()
                .map(|cr| canonical_ancestor.starts_with(&cr))
                .unwrap_or(false)
        });
        if !in_root {
            return Err(format!("Path '{}' is outside allowed roots", path_str));
        }
        return Ok(resolved);
    }

    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("Cannot resolve parent directory: {}", e))?;

    if roots.is_empty() {
        return Err("No roots configured — all filesystem access is denied".to_string());
    }
    let in_root = roots.iter().any(|root| {
        root.canonicalize()
            .map(|cr| canonical_parent.starts_with(&cr))
            .unwrap_or(false)
    });
    if !in_root {
        return Err(format!("Path '{}' is outside allowed roots", path_str));
    }

    Ok(canonical_parent.join(resolved.file_name().unwrap_or_default()))
}

pub fn is_binary_content(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8192);
    bytes[..check_len].contains(&0)
}

pub fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

pub fn truncate_line(line: &str, max_len: usize) -> String {
    if line.len() <= max_len {
        return line.to_string();
    }
    let mut end = max_len;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harnx-mcp-safety-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn truncate_head_no_truncation() {
        let r = truncate_head("hello\nworld\n".to_string(), 100, 100_000);
        assert!(!r.truncated);
        assert_eq!(r.total_lines, 2);
        assert_eq!(r.total_bytes, 12);
        assert_eq!(r.content, "hello\nworld\n");
    }

    #[test]
    fn truncate_head_by_lines() {
        let r = truncate_head("a\nb\nc\nd\n".to_string(), 2, 100_000);
        assert!(r.truncated);
        assert_eq!(r.output_lines, 2);
        assert_eq!(r.content, "a\nb\n");
    }

    #[test]
    fn truncate_head_by_bytes() {
        let r = truncate_head("abcdefghij".to_string(), 100, 5);
        assert!(r.truncated);
        assert_eq!(r.output_bytes, 5);
    }

    #[test]
    fn is_binary_detects_nulls() {
        assert!(is_binary_content(b"hello\x00world"));
        assert!(!is_binary_content(b"hello world"));
    }

    #[test]
    fn truncate_line_short() {
        assert_eq!(truncate_line("short", 10), "short");
    }

    #[test]
    fn truncate_line_long() {
        let long = "a".repeat(100);
        let result = truncate_line(&long, 10);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 13);
    }

    #[test]
    fn sanitize_output_text_strips_control_chars_and_replacement() {
        let input = "ok\u{0000}\u{0008}\t\n\rbad\u{FFFD}done";
        assert_eq!(sanitize_output_text(input), "ok\t\n\rbaddone");
    }

    #[test]
    fn truncate_output_truncates_long_line() {
        let input = format!("{}\n", "x".repeat(20));
        let output = truncate_output(
            &input,
            &TruncateOpts {
                line_head_bytes: 4,
                line_tail_bytes: 4,
                max_output_bytes: 1_000,
                ..Default::default()
            },
        );
        assert!(output.contains("[truncated: 12 bytes removed from line]"));
        assert!(output.starts_with("xxxx"));
        assert!(output.contains("xxxx\n"));
    }

    #[test]
    fn truncate_output_truncates_middle_lines() {
        let input = (1..=10)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_output(
            &input,
            &TruncateOpts {
                head_lines: 2,
                tail_lines: 3,
                max_output_bytes: 1_000,
                ..Default::default()
            },
        );
        assert!(
            output.contains("[truncated: 5 lines removed. Use head_lines/tail_lines to show more]")
        );
        assert!(output.contains("line-1"));
        assert!(output.contains("line-2"));
        assert!(output.contains("line-8"));
        assert!(output.contains("line-10"));
    }

    #[test]
    fn truncate_output_caps_total_bytes() {
        let input = (1..=200)
            .map(|n| format!("line-{n:03}: {}", "z".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_output(
            &input,
            &TruncateOpts {
                head_lines: 200,
                tail_lines: 200,
                max_output_bytes: 200,
                ..Default::default()
            },
        );
        assert!(output.contains("[truncated:"));
        assert!(output.contains("showing first"));
        assert!(output.starts_with("line-001"));
        assert_ne!(output, input);
    }

    #[test]
    fn test_truncate_output_per_line() {
        let input = format!("prefix-{}-suffix\n", "x".repeat(64));
        let output = truncate_output(
            &input,
            &TruncateOpts {
                line_head_bytes: 8,
                line_tail_bytes: 8,
                max_output_bytes: 1_000,
                ..Default::default()
            },
        );

        assert!(output.starts_with("prefix-x"));
        assert!(output.contains("[truncated:"));
        assert!(output.contains("-suffix"));
    }

    #[test]
    fn test_truncate_output_line_count() {
        let input = (1..=8)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_output(
            &input,
            &TruncateOpts {
                head_lines: 2,
                tail_lines: 2,
                max_output_bytes: 1_000,
                ..Default::default()
            },
        );

        assert!(output.contains("line-1"));
        assert!(output.contains("line-2"));
        assert!(output.contains("line-7"));
        assert!(output.contains("line-8"));
        assert!(
            output.contains("[truncated: 4 lines removed. Use head_lines/tail_lines to show more]")
        );
    }

    #[test]
    fn test_truncate_output_total_bytes() {
        let input = (1..=40)
            .map(|n| format!("line-{n:02}: {}", "a".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_output(
            &input,
            &TruncateOpts {
                head_lines: 100,
                tail_lines: 100,
                max_output_bytes: 200,
                ..Default::default()
            },
        );

        assert!(output.contains("showing first"));
        assert!(output.contains("Use max_output_bytes to increase limit"));
        assert!(output.starts_with("line-01"));
        assert_ne!(output, input);
    }

    #[test]
    fn test_validate_path_within_root() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path.join("inside.txt");
        std::fs::write(&file_path, "ok").unwrap();

        let validated = validate_path(
            &file_path.to_string_lossy(),
            std::slice::from_ref(&temp_dir.path),
        )
        .unwrap();

        assert_eq!(validated, file_path.canonicalize().unwrap());
    }

    #[test]
    fn test_validate_path_outside_root() {
        let root_dir = TestDir::new();
        let outside_dir = TestDir::new();
        let outside_file = outside_dir.path.join("outside.txt");
        std::fs::write(&outside_file, "nope").unwrap();

        let error = validate_path(
            &outside_file.to_string_lossy(),
            std::slice::from_ref(&root_dir.path),
        )
        .unwrap_err();

        assert!(error.contains("outside allowed roots"));
    }

    #[test]
    fn test_validate_path_no_roots_denies_access() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path.join("anywhere.txt");
        std::fs::write(&file_path, "ok").unwrap();

        let err = validate_path(&file_path.to_string_lossy(), &[]).unwrap_err();
        assert!(err.contains("No roots configured"));
    }

    #[test]
    fn test_validate_write_path_new_file() {
        let temp_dir = TestDir::new();
        let new_file = temp_dir.path.join("nested").join("new.txt");

        let validated = validate_write_path(
            &new_file.to_string_lossy(),
            std::slice::from_ref(&temp_dir.path),
        )
        .unwrap();

        assert_eq!(validated, new_file);
    }

    #[test]
    fn file_uri_round_trip_simple() {
        let path = Path::new("/home/user/project/file.txt");
        let uri = path_to_file_uri(path);
        assert_eq!(uri, "file:///home/user/project/file.txt");
        assert_eq!(file_uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn file_uri_round_trip_spaces() {
        let path = Path::new("/home/user/my project/file name.txt");
        let uri = path_to_file_uri(path);
        assert!(
            uri.contains("%20"),
            "spaces should be percent-encoded: {uri}"
        );
        assert_eq!(file_uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn file_uri_round_trip_hash_and_percent() {
        let path = Path::new("/tmp/100%done#notes.txt");
        let uri = path_to_file_uri(path);
        assert!(uri.contains("%25"), "literal % should be encoded: {uri}");
        assert!(uri.contains("%23"), "# should be encoded: {uri}");
        assert_eq!(file_uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn file_uri_rejects_non_file_scheme() {
        assert!(file_uri_to_path("https://example.com/foo").is_none());
        assert!(file_uri_to_path("../../secrets").is_none());
        assert!(file_uri_to_path("").is_none());
    }
}
