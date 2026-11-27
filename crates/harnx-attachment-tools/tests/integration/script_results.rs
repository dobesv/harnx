//! Script-style results: what a `bash` script's inner `attachment_read` and
//! `attachment_create` calls see when they ask for a structured result.

use super::{first_text, media_url, put_media, tool_session, TestContext, HASH_A};
use serde_json::{json, Value};

/// An attachment with `bytes` stored as `mime`, callable in both styles.
struct ScriptAttachment {
    ctx: TestContext,
    url: String,
}

impl ScriptAttachment {
    async fn start(bytes: &[u8], mime: &str) -> Option<Self> {
        let ctx = TestContext::start().await?;
        let url = media_url(HASH_A);
        put_media(&ctx.store, &url, bytes, mime)
            .await
            .expect("seed attachment object store");
        Some(Self {
            url: url.to_string(),
            ctx,
        })
    }

    async fn read(&self, selectors: Value) -> Value {
        let mut args = json!({ "url": self.url });
        if let Value::Object(fields) = selectors {
            for (key, value) in fields {
                args[key] = value;
            }
        }
        self.ctx
            .invoke_structured("attachment_read", args, None)
            .await
    }
}

fn structured(result: &Value) -> &Value {
    result
        .get("structuredContent")
        .filter(|value| value.is_object())
        .unwrap_or_else(|| panic!("structured content missing: {result}"))
}

/// Byte paging through the structured fields reconstructs multibyte content
/// exactly, and stdout text carries no paging notices.
#[tokio::test]
async fn structured_byte_paging_reconstructs_multibyte_content() {
    let original = "héllo 日本語 🌍\nline two\n".repeat(9);
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let mut text = String::new();
    let mut offset = 0;
    for _ in 0..=original.len() {
        let result = stored
            .read(json!({ "byte_offset": offset, "byte_limit": 7 }))
            .await;
        assert_eq!(result["isError"], false, "{result}");
        let structured = structured(&result);
        let page = structured["text"].as_str().expect("text field");
        assert!(!page.contains("next_byte_offset"), "stdout must stay clean");
        assert!(
            !page.contains("bytes left"),
            "paging notices belong in notes"
        );
        text.push_str(page);
        assert_eq!(structured["byte_offset"], json!(offset));
        assert_eq!(structured["total_bytes"], json!(original.len()));
        if structured["end_of_content"] == json!(true) {
            assert_eq!(structured["next_byte_offset"], Value::Null);
            break;
        }
        let next = structured["next_byte_offset"]
            .as_u64()
            .expect("next offset") as usize;
        assert!(next > offset, "paging must advance");
        let note = structured["notes"][0].as_str().expect("continuation note");
        assert!(
            note.contains(&format!("byte_offset={next}")),
            "note must name the next offset: {note}"
        );
        offset = next;
    }
    assert_eq!(text, original);
}

/// Line mode keeps stdout clean: no in-band truncation markers, cut-offs and
/// continuation hints are notes, and following the notes reassembles the text.
#[tokio::test]
async fn structured_line_mode_reconstructs_through_continuation_notes() {
    // Multibyte lines force the byte cap to cut between lines.
    let original = "γρήγορη αλεπού\n".repeat(60);
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let mut text = String::new();
    let mut offset = 1;
    for _ in 0..=original.lines().count() {
        let result = stored
            .read(json!({ "offset": offset, "max_output_bytes": 100 }))
            .await;
        assert_eq!(result["isError"], false, "{result}");
        let structured = structured(&result);
        let page = structured["text"].as_str().expect("text field");
        assert!(!page.contains("[truncated"), "no markers in stdout: {page}");
        text.push_str(page);
        let notes = structured["notes"].as_array().expect("notes");
        let Some(note) = notes.iter().find_map(Value::as_str) else {
            break;
        };
        let next = note
            .split("Use offset=")
            .nth(1)
            .expect("continuation note")
            .split_whitespace()
            .next()
            .expect("numeric offset")
            .parse::<usize>()
            .expect("numeric offset");
        assert_eq!(next, offset + page.lines().count());
        offset = next;
    }
    assert_eq!(text, original);
}

/// Explicit head/tail caps drop the middle of the selection and say so, and
/// a script without them gets whole lines from the start, never a silent
/// head/tail window.
#[tokio::test]
async fn structured_line_mode_head_tail_caps_and_no_silent_window() {
    let original = "alpha\nbêta\nthree\nquatre\nfive\n".to_string();
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };

    let both = stored
        .read(json!({ "head_lines": 1, "tail_lines": 1 }))
        .await;
    assert_eq!(structured(&both)["text"], "alpha\nfive\n");
    assert_eq!(
        structured(&both)["notes"],
        json!(["3 lines left out"]),
        "a two-sided cut cannot offer a single offset"
    );

    // Without head/tail the default window (250/750 lines in model mode)
    // must not hide the middle from a script.
    let long = "filler\n".repeat(1200) + "buried\n";
    let Some(tall) = ScriptAttachment::start(long.as_bytes(), "text/plain").await else {
        return;
    };
    let whole = tall.read(json!({})).await;
    let text = structured(&whole)["text"].as_str().expect("text field");
    assert_eq!(text, long, "script mode has no silent line window");
    assert_eq!(structured(&whole)["notes"], json!([]));
}

/// grep keeps matching lines out of stdout's way and reports what it dropped.
#[tokio::test]
async fn structured_line_mode_grep_filters_with_a_note() {
    let original = "keep a\nskip\nkeep b\nskip\nkeep c\n";
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "grep": "^keep" })).await;
    assert_eq!(structured(&result)["text"], "keep a\nkeep b\nkeep c\n");
    assert_eq!(
        structured(&result)["notes"],
        json!(["grep left out 2 of 5 lines"])
    );
}

/// A single line longer than the cap shows its start, cut at a character
/// boundary, with the offset that skips past it.
#[tokio::test]
async fn structured_line_mode_oversized_line_cuts_at_a_boundary() {
    let original = "日本語".repeat(200);
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "max_output_bytes": 20 })).await;
    let structured = structured(&result);
    let text = structured["text"].as_str().expect("text field");
    assert_eq!(text, "日本語".repeat(2), "cut at a character boundary");
    let note = structured["notes"][0].as_str().expect("note");
    assert!(note.contains("longer than 20B"), "{note}");
    assert!(
        note.contains("Use offset=2 to skip the rest of it"),
        "{note}"
    );
}

/// Images and binary errors keep their direct-call shape in script mode:
/// images externalize to attachments instead of becoming structured text.
#[tokio::test]
async fn structured_reads_keep_image_and_binary_shapes() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    )
    .expect("decode test PNG");
    put_media(&ctx.store, &url, &png, "image/png")
        .await
        .expect("put PNG");

    let image = ctx
        .invoke_structured("attachment_read", json!({ "url": url.to_string() }), None)
        .await;
    assert_eq!(image["isError"], false, "{image}");
    assert_eq!(image["content"][0]["type"], "image");
    assert!(image.get("structuredContent").is_none(), "{image}");

    let binary_url = media_url("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
    put_media(
        &ctx.store,
        &binary_url,
        &[0, 1, 2, 3],
        "application/octet-stream",
    )
    .await
    .expect("put binary attachment");
    let binary = ctx
        .invoke_structured(
            "attachment_read",
            json!({ "url": binary_url.to_string() }),
            None,
        )
        .await;
    assert_eq!(binary["isError"], true, "{binary}");
    assert!(binary.get("structuredContent").is_none(), "{binary}");
    assert!(first_text(&binary).contains("application/octet-stream"));
}

/// Errors keep the same shape in script mode: a recoverable error result the
/// script layer prints on stderr with exit status 1.
#[tokio::test]
async fn structured_reads_keep_error_shapes() {
    let Some(stored) = ScriptAttachment::start("日本語".as_bytes(), "text/plain").await else {
        return;
    };
    let mid_character = stored
        .read(json!({ "byte_offset": 1, "byte_limit": 5 }))
        .await;
    assert_eq!(mid_character["isError"], true, "{mid_character}");
    assert!(mid_character.get("structuredContent").is_none());
    assert!(first_text(&mid_character).contains("not at a valid UTF-8 character boundary"));

    let conflicting = stored
        .read(json!({ "byte_offset": 0, "head_lines": 1 }))
        .await;
    assert_eq!(conflicting["isError"], true, "{conflicting}");
    assert!(first_text(&conflicting).contains("cannot combine"));
}

/// `attachment_create` answers a script with the URL as fields instead of a
/// line to parse, and keeps the confirmation line on stdout.
#[tokio::test]
async fn structured_create_returns_url_fields() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let created = ctx
        .invoke_structured(
            "attachment_create",
            json!({ "content": "structured", "mime_type": "text/plain" }),
            Some(tool_session()),
        )
        .await;
    assert_eq!(created["isError"], false, "{created}");
    let structured = structured(&created);
    let url = structured["url"].as_str().expect("url field");
    assert!(url.starts_with("cid:media:"));
    assert_eq!(structured["mime_type"], "text/plain");
    // The mapped message is the shell's stdout line: newline-terminated so a
    // `while read` loop or a following command sees a complete record.
    assert_eq!(
        structured["message"],
        format!("Created attachment: {url}\n")
    );
    // The stored attachment reads back through the plain path unchanged.
    let read = ctx
        .invoke("attachment_read", json!({ "url": url }), None)
        .await;
    assert_eq!(first_text(&read), "structured");
}

/// `grep` matches the line without its terminator, like the model path's
/// `lines()` view, but emits the original bytes: `^foo$` finds `foo\n`,
/// `foo\r\n` and an unterminated final `foo`, and a lone CR stays part of
/// the matched text instead of being trimmed.
#[tokio::test]
async fn structured_grep_matches_without_line_terminators() {
    let terminated = "foo\nfoo\r\nfoo";
    let Some(stored) = ScriptAttachment::start(terminated.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "grep": "^foo$" })).await;
    assert_eq!(structured(&result)["text"], terminated, "{result}");
    assert_eq!(structured(&result)["notes"], json!([]), "{result}");

    let lone_cr = "foo\rbar\nskip\n";
    let Some(cr) = ScriptAttachment::start(lone_cr.as_bytes(), "text/plain").await else {
        return;
    };
    let result = cr.read(json!({ "grep": "foo\rbar$" })).await;
    assert_eq!(structured(&result)["text"], "foo\rbar\n", "{result}");
    assert_eq!(
        structured(&result)["notes"],
        json!(["grep left out 1 of 2 lines"]),
        "{result}"
    );

    // EOF: an unterminated final record ending in a lone CR keeps that CR —
    // it is content, not a terminator. `^foo\r$` selects it (positive) and
    // `^foo$` must not (negative), with the returned bytes exact either way.
    let eof_cr = "foo\nfoo\r";
    let Some(eof) = ScriptAttachment::start(eof_cr.as_bytes(), "text/plain").await else {
        return;
    };
    let result = eof.read(json!({ "grep": "^foo\\r$" })).await;
    assert_eq!(structured(&result)["text"], "foo\r", "{result}");
    assert_eq!(
        structured(&result)["notes"],
        json!(["grep left out 1 of 2 lines"]),
        "{result}"
    );
    let result = eof.read(json!({ "grep": "^foo$" })).await;
    assert_eq!(structured(&result)["text"], "foo\n", "{result}");
    assert_eq!(
        structured(&result)["notes"],
        json!(["grep left out 1 of 2 lines"]),
        "{result}"
    );
}

/// A binding `limit` returns only the requested lines from the selection,
/// with no cutoff note — the truncation is the caller's own choice.
#[tokio::test]
async fn structured_line_mode_limit_returns_the_requested_lines() {
    let original = "one\ntwo\nthree\nfour\nfive\n";
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "offset": 2, "limit": 2 })).await;
    assert_eq!(structured(&result)["text"], "two\nthree\n", "{result}");
    assert_eq!(structured(&result)["notes"], json!([]), "{result}");
}

/// An invalid regex keeps the model path's compatibility behavior: the call
/// succeeds with the unfiltered selection, and the note says grep was not
/// applied instead of failing silently or loudly.
#[tokio::test]
async fn structured_line_mode_invalid_regex_keeps_the_selection() {
    let original = "one\ntwo\nthree\n";
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "grep": "([unclosed" })).await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(structured(&result)["text"], original, "{result}");
    let notes = structured(&result)["notes"].as_array().expect("notes");
    let note = notes
        .iter()
        .filter_map(Value::as_str)
        .find(|note| note.contains("grep not applied"))
        .expect("fallback note");
    assert!(note.contains("invalid regex"), "{note}");
}

/// Head/tail windows under a byte cap report the truth: a two-sided window
/// says how many retained lines it emitted and offers no continuation
/// offset (the kept blocks are disjoint), and names its oversized first
/// line at its head position, not the tail block's. A tail-only window is a
/// contiguous run, so it names the absolute line it stopped at and the
/// offset that continues from there.
#[tokio::test]
async fn structured_window_cap_notes_are_truthful() {
    let original: String = (1..=10).map(|n| format!("line{n:02}\n")).collect();
    let Some(stored) = ScriptAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };

    // Two-sided: 7 bytes per line; the cap fits exactly the head block plus
    // the first line of the tail block, so the emitted prefix ends on
    // line09.
    let result = stored
        .read(json!({ "head_lines": 3, "tail_lines": 2, "max_output_bytes": 28 }))
        .await;
    assert_eq!(
        structured(&result)["text"],
        "line01\nline02\nline03\nline09\n",
        "{result}"
    );
    assert_eq!(
        structured(&result)["notes"],
        json!([
            "5 lines left out",
            "output stopped after 4 of 5 retained lines to stay within 28B"
        ]),
        "{result}"
    );

    // Two-sided with an oversized first line: the window starts at its
    // head, so the oversized line is line 1, with no offset hint across
    // the disjoint blocks.
    let big_first = format!("{}\n", "日本語".repeat(50)) + &"a\n".repeat(9);
    let Some(big) = ScriptAttachment::start(big_first.as_bytes(), "text/plain").await else {
        return;
    };
    let result = big
        .read(json!({ "head_lines": 1, "tail_lines": 1, "max_output_bytes": 20 }))
        .await;
    assert_eq!(
        structured(&result)["text"],
        "日本語".repeat(2),
        "cut at a character boundary: {result}"
    );
    assert_eq!(
        structured(&result)["notes"],
        json!([
            "8 lines left out",
            "line 1 is longer than 20B; showing its first 18 bytes"
        ]),
        "{result}"
    );

    // Tail-only: the retained run starts at line 9; the cap emits one line
    // and names the absolute stop line plus the continuing offset, both in
    // the caller's line space.
    let result = stored
        .read(json!({ "tail_lines": 2, "max_output_bytes": 7 }))
        .await;
    assert_eq!(structured(&result)["text"], "line09\n", "{result}");
    assert_eq!(
        structured(&result)["notes"],
        json!([
            "8 lines left out",
            "output stopped at line 10 of 10 to stay within 7B. Use offset=10 to continue"
        ]),
        "{result}"
    );

    // Offset past one: both note numbers are absolute lines, so the stop
    // line and the denominator share the caller's coordinate space — the
    // selection spans lines 6..=10 and the cap stops before line 8.
    let result = stored
        .read(json!({ "offset": 6, "max_output_bytes": 14 }))
        .await;
    assert_eq!(structured(&result)["text"], "line06\nline07\n", "{result}");
    assert_eq!(
        structured(&result)["notes"],
        json!(["output stopped at line 8 of 10 to stay within 14B. Use offset=8 to continue"]),
        "{result}"
    );
}
