//! OSC 8 hyperlinks for the URL interaction modal (decision 6).
//!
//! ratatui 0.30 has no hyperlink API: its buffer drops control characters, so
//! an escape sequence embedded in a span never reaches the terminal. The link
//! is instead rendered as plain text (always visible — that is the fallback)
//! wrapped in two sentinel characters from the Private Use Area. The
//! terminal backend writes through [`HyperlinkWriter`], which rewrites the
//! bytes between the sentinels into an OSC 8 hyperlink span when enabled, or
//! strips the sentinels when the terminal cannot be assumed to support
//! hyperlinks. Either way the URL text itself stays on screen.

use std::io::{self, Write};

/// Sentinel marking the start of a hyperlink span.
pub(crate) const LINK_OPEN: char = '\u{E000}';
/// Sentinel marking the end of a hyperlink span.
pub(crate) const LINK_CLOSE: char = '\u{E001}';

const OPEN_UTF8: [u8; 3] = [0xEE, 0x80, 0x80];
const CLOSE_UTF8: [u8; 3] = [0xEE, 0x80, 0x81];

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const CSI_FINAL_MIN: u8 = 0x40;
const CSI_FINAL_MAX: u8 = 0x7E;
const BOX_DRAWING_START: char = '\u{2500}';
const BOX_DRAWING_END: char = '\u{259F}';

/// The URL line of a `url` interaction modal: plain-text URL wrapped in the
/// hyperlink sentinels.
pub(crate) fn url_span(url: &str) -> ratatui::text::Line<'static> {
    let sanitized: String = url
        .chars()
        .filter(|&c| c != LINK_OPEN && c != LINK_CLOSE)
        .collect();
    ratatui::text::Line::from(format!("{LINK_OPEN}{sanitized}{LINK_CLOSE}"))
}

/// Terminal writer that turns sentinel-delimited spans into OSC 8 hyperlinks.
///
/// Bytes between an open and close sentinel are buffered until the close
/// sentinel arrives, then written as `OSC 8 ;; <url> <url-text> OSC 8 ;;`
/// (when enabled) or verbatim (when disabled). Everything else passes through
/// unchanged. A span that never closes (its line was clipped by the modal
/// area) is flushed as plain text by [`Write::flush`], bounded by
/// [`MAX_PENDING`].
pub(crate) struct HyperlinkWriter<W: Write> {
    inner: W,
    enabled: bool,
    /// Buffered span bytes after an open sentinel; `None` outside a span.
    pending: Option<Vec<u8>>,
    /// A partial sentinel sequence cut in half by a write boundary.
    carry: Vec<u8>,
}

/// A span larger than this lost its close sentinel to clipping; flush it
/// verbatim instead of buffering it forever.
const MAX_PENDING: usize = 8 * 1024;

impl<W: Write> HyperlinkWriter<W> {
    pub(crate) fn new(inner: W, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            pending: None,
            carry: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_inner(self) -> W {
        self.inner
    }

    fn rewrite(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let combined = combine(&self.carry, chunk);
        self.carry.clear();
        let bytes = combined.as_slice();
        let mut start = 0;
        for index in 0..bytes.len() {
            if !is_sentinel_lead(bytes[index]) {
                continue;
            }
            match sentinel_at(bytes, index) {
                Some(sentinel) => {
                    self.emit_between(&bytes[start..index], out);
                    self.on_sentinel(sentinel, out);
                    start = index + SENTINEL_BYTES;
                }
                None => {
                    // A truncated sentinel at the chunk's tail may complete
                    // on the next write; park it in the carry.
                    if let Some(tail) = truncated_sentinel_tail(bytes, index) {
                        self.emit_between(&bytes[start..index], out);
                        self.carry.extend_from_slice(tail);
                        return;
                    }
                }
            }
        }
        self.emit_between(&bytes[start..], out);
        self.flush_oversized_pending(out);
    }

    /// Text inside a span belongs to the pending buffer, so the close
    /// sentinel can re-emit it as the OSC 8 label; text before an open
    /// sentinel (or outside any span) goes straight out.
    fn emit_between(&mut self, between: &[u8], out: &mut Vec<u8>) {
        if let Some(pending) = &mut self.pending {
            pending.extend_from_slice(between);
        } else {
            out.extend_from_slice(between);
        }
    }

    fn on_sentinel(&mut self, sentinel: Sentinel, out: &mut Vec<u8>) {
        match sentinel {
            Sentinel::Open => {
                // Nested open sentinels: keep the first span only.
                self.pending.get_or_insert_with(Vec::new);
            }
            Sentinel::Close => self.close_span(out),
        }
    }

    fn close_span(&mut self, out: &mut Vec<u8>) {
        match self.pending.take() {
            Some(span) if self.enabled => write_osc8_link(out, &span),
            Some(span) => out.extend_from_slice(&span),
            // A stray close without an open: drop the sentinel.
            None => {}
        }
    }

    /// A span larger than [`MAX_PENDING`] lost its close sentinel to
    /// clipping; emit it verbatim instead of buffering it forever.
    fn flush_oversized_pending(&mut self, out: &mut Vec<u8>) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.len() > MAX_PENDING)
        {
            let span = self.pending.take().unwrap_or_default();
            out.extend_from_slice(&span);
        }
    }
}

/// The kind of sentinel starting at a byte position.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sentinel {
    Open,
    Close,
}

/// Both sentinels are three-byte UTF-8 sequences; the lead byte is shared.
const SENTINEL_BYTES: usize = 3;

fn is_sentinel_lead(byte: u8) -> bool {
    byte == OPEN_UTF8[0]
}

fn combine(carry: &[u8], chunk: &[u8]) -> Vec<u8> {
    let mut combined = Vec::with_capacity(carry.len() + chunk.len());
    combined.extend_from_slice(carry);
    combined.extend_from_slice(chunk);
    combined
}

fn sentinel_at(bytes: &[u8], index: usize) -> Option<Sentinel> {
    let rest = bytes.get(index..index + SENTINEL_BYTES)?;
    if rest == OPEN_UTF8 {
        Some(Sentinel::Open)
    } else if rest == CLOSE_UTF8 {
        Some(Sentinel::Close)
    } else {
        None
    }
}

/// The bytes from `index` when they are a truncated sentinel prefix at the
/// chunk's tail: the sequence is incomplete here but may complete on the
/// next write.
fn truncated_sentinel_tail(bytes: &[u8], index: usize) -> Option<&[u8]> {
    let tail = &bytes[index..];
    if tail.len() >= SENTINEL_BYTES {
        return None;
    }
    let is_open_prefix = tail == &OPEN_UTF8[..tail.len()];
    let is_close_prefix = tail == &CLOSE_UTF8[..tail.len()];
    (is_open_prefix || is_close_prefix).then_some(tail)
}

/// OSC 8: open with the URL as the target, re-emit the span text as the
/// link label, then close the link.
///
/// When a URL wraps across terminal rows, Ratatui emits cursor repositioning
/// sequences (`\x1b[row;colH`) and modal border characters (`│`) between the
/// text segments of each row. We split the span into URL segments and
/// intervening terminal sequences, assemble the full target URL, and emit an
/// OSC 8 hyperlink for each row segment while preserving the cursor movements
/// and borders between rows.
fn write_osc8_link(out: &mut Vec<u8>, span: &[u8]) {
    let Ok(span_str) = std::str::from_utf8(span) else {
        out.extend_from_slice(span);
        return;
    };
    let elements = parse_span_elements(span_str);
    match extract_target_url(&elements) {
        Some(target) => emit_osc8_elements(out, &elements, &target),
        None => out.extend_from_slice(span),
    }
}

fn extract_target_url(elements: &[SpanElement<'_>]) -> Option<String> {
    let mut target = String::new();
    for elem in elements {
        if elem.is_url {
            target.push_str(elem.text);
        }
    }
    harnx_toolset::ToolInputRequired::validate_url(&target).ok()?;
    Some(target)
}

fn emit_osc8_elements(out: &mut Vec<u8>, elements: &[SpanElement<'_>], target: &str) {
    for elem in elements {
        if elem.is_url {
            out.extend_from_slice(b"\x1b]8;;");
            out.extend_from_slice(target.as_bytes());
            out.extend_from_slice(b"\x1b\\");
            out.extend_from_slice(elem.text.as_bytes());
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        } else {
            out.extend_from_slice(elem.text.as_bytes());
        }
    }
}

struct SpanElement<'a> {
    is_url: bool,
    text: &'a str,
}

fn parse_span_elements(span: &str) -> Vec<SpanElement<'_>> {
    let mut elements = Vec::new();
    let bytes = span.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == ESC {
            let esc_len = escape_sequence_len(&bytes[i..]);
            let start = i;
            i += esc_len;
            elements.push(SpanElement {
                is_url: false,
                text: &span[start..i],
            });
        } else {
            let start = i;
            let Some(c) = span[i..].chars().next() else {
                break;
            };
            let is_url = !is_non_url_char(c);
            i += c.len_utf8();
            while i < bytes.len() && bytes[i] != ESC {
                let Some(next_c) = span[i..].chars().next() else {
                    break;
                };
                if !is_non_url_char(next_c) != is_url {
                    break;
                }
                i += next_c.len_utf8();
            }
            elements.push(SpanElement {
                is_url,
                text: &span[start..i],
            });
        }
    }
    elements
}

fn is_non_url_char(c: char) -> bool {
    c.is_whitespace()
        || c.is_ascii_control()
        || (BOX_DRAWING_START..=BOX_DRAWING_END).contains(&c)
        || c == LINK_OPEN
        || c == LINK_CLOSE
        || matches!(c, '<' | '>' | '"' | '\\' | '^' | '`' | '{' | '|' | '}')
}

fn escape_sequence_len(bytes: &[u8]) -> usize {
    if bytes.first() != Some(&ESC) {
        return 0;
    }
    match bytes.get(1) {
        Some(b'[') => csi_sequence_len(bytes),
        Some(b']') => osc_sequence_len(bytes),
        Some(_) => 2,
        None => 1,
    }
}

fn csi_sequence_len(bytes: &[u8]) -> usize {
    let mut len = 2;
    while len < bytes.len() {
        let b = bytes[len];
        len += 1;
        if (CSI_FINAL_MIN..=CSI_FINAL_MAX).contains(&b) {
            break;
        }
    }
    len
}

fn osc_sequence_len(bytes: &[u8]) -> usize {
    let mut len = 2;
    while len < bytes.len() {
        if bytes[len] == BEL {
            return len + 1;
        }
        if bytes[len] == ESC && bytes.get(len + 1) == Some(&b'\\') {
            return len + 2;
        }
        len += 1;
    }
    len
}

impl<W: Write> Write for HyperlinkWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = Vec::with_capacity(buf.len());
        self.rewrite(buf, &mut out);
        if !out.is_empty() {
            self.inner.write_all(&out)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // A frame is a complete render; an unclosed span here lost its close
        // sentinel to clipping. Emit what we hold as plain text so nothing is
        // ever swallowed.
        if let Some(span) = self.pending.take() {
            self.inner.write_all(&span)?;
        }
        if !self.carry.is_empty() {
            let carry = std::mem::take(&mut self.carry);
            self.inner.write_all(&carry)?;
        }
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(enabled: bool, chunks: &[&[u8]]) -> Vec<u8> {
        let mut writer = HyperlinkWriter::new(Vec::new(), enabled);
        for chunk in chunks {
            writer.write_all(chunk).unwrap();
        }
        writer.flush().unwrap();
        writer.into_inner()
    }

    fn span_bytes(url: &str) -> Vec<u8> {
        format!("{LINK_OPEN}{url}{LINK_CLOSE}").into_bytes()
    }

    #[test]
    fn wraps_a_complete_span_with_osc8_when_enabled() {
        let input = span_bytes("https://example.com/connect");
        let out = run(true, &[&input]);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            text.contains("\u{1b}]8;;https://example.com/connect\u{1b}\\"),
            "{text:?}"
        );
        assert!(
            text.ends_with("\u{1b}]8;;\u{1b}\\"),
            "the link closes after the label: {text:?}"
        );
        assert!(
            !out.windows(3).any(|w| w == OPEN_UTF8),
            "sentinels never reach the terminal"
        );
        assert!(!out.windows(3).any(|w| w == CLOSE_UTF8));
    }

    #[test]
    fn disabled_writer_strips_sentinels_but_keeps_plain_text() {
        let input = span_bytes("https://example.com/connect");
        let out = run(false, &[&input]);
        assert_eq!(out, b"https://example.com/connect");
    }

    #[test]
    fn split_writes_still_match_a_complete_span() {
        let input = span_bytes("https://example.com/x");
        let split = input.len() / 2;
        // Force several awkward boundaries, including mid-sentinel cuts.
        for cut in [split, 1, 2, 3, 4, input.len() - 1] {
            let out = run(true, &[&input[..cut], &input[cut..]]);
            let text = String::from_utf8_lossy(&out).into_owned();
            assert!(
                text.contains("\u{1b}]8;;https://example.com/x\u{1b}\\"),
                "cut at {cut}: {text:?}"
            );
            assert!(
                !out.windows(3).any(|w| w == OPEN_UTF8),
                "cut at {cut}: {text:?}"
            );
        }
    }

    #[test]
    fn bytes_outside_spans_pass_through_unchanged() {
        let out = run(true, &[b"\x1b[2;3Hplain text"]);
        assert_eq!(out, b"\x1b[2;3Hplain text");
    }

    #[test]
    fn clipped_close_sentinel_flushes_plain_text_on_flush() {
        // Open with content but no close: flush() must not swallow the text.
        let mut input = span_bytes("https://example.com/long");
        input.truncate(input.len() - 3); // drop the close sentinel
        let out = run(true, &[&input]);
        assert_eq!(out, b"https://example.com/long");
    }

    #[test]
    fn oversized_pending_span_is_flushed_verbatim() {
        let long_url = format!("https://example.com/{}", "x".repeat(MAX_PENDING + 16));
        let mut input = span_bytes(&long_url);
        input.truncate(input.len() - 3); // never closed
        let out = run(true, &[&input]);
        assert_eq!(out, long_url.as_bytes());
    }

    fn render_modal_to_writer(
        url: &str,
        width: u16,
        height: u16,
        enabled: bool,
    ) -> (String, Vec<u8>) {
        use ratatui::backend::CrosstermBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let out = Arc::new(Mutex::new(Vec::new()));
        let writer = HyperlinkWriter::new(SharedWriter(out.clone()), enabled);
        let backend = CrosstermBackend::new(writer);
        let options = TerminalOptions {
            viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, width, height)),
        };
        let mut terminal = Terminal::with_options(backend, options).unwrap();

        let config = crate::tests::test_config();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let tui = rt.block_on(crate::types::Tui::init(&config)).unwrap();

        let mut modal = crate::interaction_confirmation::InteractionModal {
            generation: 1,
            pending: crate::interaction_confirmation::PendingConfirmation {
                key: crate::interaction_confirmation::InteractionKey {
                    id: "test".into(),
                    request_seq: 1,
                },
                message: "Connect Jira".into(),
                kind: "url".into(),
                payload: serde_json::json!({ "url": url }),
                pending_count: 0,
            },
            child: None,
            a2ui: None,
            note: ratatui_textarea::TextArea::default(),
            scroll: 0,
            submitting: false,
            error: None,
            last_key_at: std::time::Instant::now(),
            decision_guard_until: None,
        };

        terminal
            .draw(|f| {
                tui.render_interaction_modal(
                    f,
                    ratatui::layout::Rect::new(0, 0, width, height),
                    &mut modal,
                );
            })
            .unwrap();
        drop(terminal);
        let bytes = out.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (text, bytes)
    }

    #[test]
    fn wrapped_url_at_several_widths_produces_full_url_targets() {
        let full_url = "https://example.com/oauth/authorize?client_id=12345&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback&state=xyz";
        for width in [25, 35, 45, 60, 80, 120] {
            let (text, _) = render_modal_to_writer(full_url, width, 18, true);
            let osc8_prefix = "\u{1b}]8;;";
            let osc8_suffix = "\u{1b}\\";
            let mut link_count = 0;
            for part in text.split(osc8_prefix).skip(1) {
                if let Some(target) = part.split(osc8_suffix).next() {
                    if !target.is_empty() {
                        assert_eq!(
                            target, full_url,
                            "width {width}: target must equal the full uncorrupted URL"
                        );
                        link_count += 1;
                    }
                }
            }
            assert!(
                link_count >= 1,
                "width {width}: must emit at least one OSC 8 link"
            );
            if width <= 60 {
                assert!(
                    link_count > 1,
                    "width {width}: expected URL to wrap across multiple rows, got {link_count}"
                );
            }
            assert!(
                !text.contains(LINK_OPEN),
                "width {width}: LINK_OPEN must not leak"
            );
            assert!(
                !text.contains(LINK_CLOSE),
                "width {width}: LINK_CLOSE must not leak"
            );
        }
    }

    #[test]
    fn disabled_hyperlinks_at_several_widths_keeps_plain_text_without_osc8() {
        let full_url = "https://example.com/oauth/authorize?client_id=12345&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback&state=xyz";
        for width in [25, 35, 45, 60, 80, 120] {
            let (text, _) = render_modal_to_writer(full_url, width, 18, false);
            assert!(
                !text.contains("\u{1b}]8;;"),
                "width {width}: disabled writer must not emit OSC 8"
            );
            assert!(
                !text.contains(LINK_OPEN),
                "width {width}: LINK_OPEN must not leak"
            );
            assert!(
                !text.contains(LINK_CLOSE),
                "width {width}: LINK_CLOSE must not leak"
            );
            assert!(
                text.contains("https://example.com/"),
                "width {width}: URL text must be visible as plain text"
            );
        }
    }

    #[test]
    fn synthetic_wrapped_span_with_various_border_and_cursor_escapes() {
        let full_url = "https://example.com/oauth/flow?client=foo&redirect=bar";
        let chunk = format!(
            "{LINK_OPEN}https://example.com/oauth/\x1b[;m│\x1b[10;1H│\x1b[;mflow?client=foo&redirect=bar{LINK_CLOSE}"
        );
        let out = run(true, &[chunk.as_bytes()]);
        let text = String::from_utf8_lossy(&out).into_owned();
        let expected_target = format!("\u{1b}]8;;{full_url}\u{1b}\\");
        let matches: Vec<_> = text.match_indices(&expected_target).collect();
        assert_eq!(
            matches.len(),
            2,
            "must produce exactly 2 OSC 8 links: {text:?}"
        );
        assert!(
            text.contains("│\x1b[10;1H│"),
            "intervening escapes must be preserved outside links"
        );
    }

    fn assert_unwrapped_link(url: &str) {
        let input = span_bytes(url);
        let out = run(true, &[&input]);
        let text = String::from_utf8_lossy(&out).into_owned();
        let expected = format!("\u{1b}]8;;{url}\u{1b}\\");
        assert!(text.contains(&expected), "{text:?}");
    }

    fn assert_wrapped_link(url: &str, width: u16) {
        let (text, _) = render_modal_to_writer(url, width, 18, true);
        let osc8_target = format!("\u{1b}]8;;{url}\u{1b}\\");
        let matches: Vec<_> = text.match_indices(&osc8_target).collect();
        assert!(
            matches.len() > 1,
            "wrapped URL must produce multiple OSC 8 segments with full URL: {text}"
        );
        for part in text.split("\u{1b}]8;;").skip(1) {
            if let Some(target) = part.split("\u{1b}\\").next() {
                if !target.is_empty() {
                    assert_eq!(target, url, "target must equal the full URL");
                }
            }
        }
    }

    #[test]
    fn non_ascii_host_unwrapped_produces_valid_hyperlink() {
        assert_unwrapped_link("https://münchen.de/x");
    }

    #[test]
    fn non_ascii_host_wrapped_produces_full_url_targets() {
        assert_wrapped_link(
            "https://münchen.de/oauth/authorize?client_id=12345&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback&state=xyz",
            30,
        );
    }

    #[test]
    fn non_ascii_path_unwrapped_produces_valid_hyperlink() {
        assert_unwrapped_link("https://example.com/über/uns");
    }

    #[test]
    fn non_ascii_path_wrapped_produces_full_url_targets() {
        assert_wrapped_link(
            "https://example.com/über/uns/oauth/authorize?client_id=12345&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback&state=xyz",
            30,
        );
    }

    #[test]
    fn remote_http_degrades_safely_to_plain_text() {
        let remote_url = "http://example.com/connect/abc";
        let input = span_bytes(remote_url);
        let out = run(true, &[&input]);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            !text.contains("\u{1b}]8;;"),
            "unwrapped remote http must not emit OSC 8"
        );
        assert_eq!(text, remote_url);

        let (modal_text, _) = render_modal_to_writer(remote_url, 30, 18, true);
        assert!(
            !modal_text.contains("\u{1b}]8;;"),
            "wrapped remote http must not emit OSC 8"
        );
        assert!(
            modal_text.contains("http://example.com/"),
            "remote http text must remain visible as plain text"
        );
    }

    #[test]
    fn invalid_target_degrades_safely_to_plain_text() {
        let bad = "javascript:alert(1)";
        let input = span_bytes(bad);
        let out = run(true, &[&input]);
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(
            !text.contains("\u{1b}]8;;"),
            "must not emit OSC 8 for invalid scheme"
        );
        assert_eq!(text, bad);
    }

    #[test]
    fn loopback_http_url_produces_valid_hyperlink() {
        assert_unwrapped_link("http://127.0.0.1:8080/callback?code=xyz");
    }

    #[test]
    fn url_span_strips_embedded_sentinels() {
        let line = url_span(&format!("https://example.com/{LINK_OPEN}test{LINK_CLOSE}"));
        assert_eq!(
            line.to_string(),
            String::from_utf8_lossy(&span_bytes("https://example.com/test"))
        );
    }

    #[test]
    fn url_span_marks_the_link_boundaries() {
        let line = url_span("https://example.com/connect");
        assert_eq!(
            line.to_string(),
            String::from_utf8_lossy(&span_bytes("https://example.com/connect"))
        );
    }
}
