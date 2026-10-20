use super::render_tool_output;
use super::transcript::{render_transcript, strip_data_urls, Transcript};
use harnx_core::message::{
    ImageUrl, Message, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole,
};
use harnx_core::session::ToolOutput;
use harnx_core::text::estimate_token_length;
use harnx_core::tool::{ToolCall, ToolResult};
use serde_json::json;

fn msg(role: MessageRole, text: impl Into<String>) -> Message {
    Message::new(role, MessageContent::Text(text.into()))
}

struct ToolFixture<'a> {
    name: &'a str,
    body: &'a str,
    failed: bool,
}

fn tool(fixture: ToolFixture<'_>) -> Message {
    Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls {
            text: "checking".into(),
            thought: None,
            sequence: false,
            tool_results: vec![ToolResult::new(
                ToolCall::new(
                    fixture.name.into(),
                    json!({"path": "/tmp/check"}),
                    None,
                    None,
                ),
                json!({"_meta": {"harnx.dev/compaction": fixture.body}, "isError": fixture.failed}),
            )],
        }),
    )
}

fn fit(messages: &[Message], budget: usize) -> String {
    let result = Transcript::new(messages, 2000, None).fit(budget);
    assert!(
        estimate_token_length(&result) <= budget,
        "budget {budget}: {result}"
    );
    result
}

struct ExpectedFragments<'a> {
    kept: &'a [&'a str],
    removed: &'a [&'a str],
}

fn assert_fragments(text: &str, expected: ExpectedFragments<'_>) {
    for fragment in expected.kept {
        assert!(text.contains(fragment), "missing {fragment:?} in {text}");
    }
    for fragment in expected.removed {
        assert!(
            !text.contains(fragment),
            "unexpected {fragment:?} in {text}"
        );
    }
}

#[test]
fn data_urls_count_decoded_bytes_and_leave_references_alone() {
    let text = "before ![](data:image/png;base64,AQIDBA==) data:audio/wav;base64,AQI= data:application/pdf;base64,AQID data:;base64,AQ== after cid:abc https://example.com";
    assert_eq!(strip_data_urls(text), "before ![]([image/png data: 4 bytes]) [audio/wav data: 2 bytes] [application/pdf data: 3 bytes] [text/plain data: 1 bytes] after cid:abc https://example.com");
}

#[test]
fn data_url_variants_preserve_mime_and_decoded_length() {
    let cases = [
        (
            "data:text/plain;charset=utf-8;base64,AQID",
            "[text/plain data: 3 bytes]",
        ),
        ("data:image/png;base64,", "[image/png data: 0 bytes]"),
        ("data:text/plain,unencoded", "data:text/plain,unencoded"),
        ("DATA:image/png;BASE64,AQ-_", "[image/png data: 3 bytes]"),
    ];
    for (input, expected) in cases {
        assert_eq!(strip_data_urls(input), expected, "{input}");
    }
}

#[test]
fn every_text_role_image_part_and_tool_argument_is_sanitized() {
    let data = "data:image/png;base64,AQIDBA==";
    let mut messages: Vec<_> = [
        MessageRole::System,
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::Tool,
    ]
    .into_iter()
    .map(|role| msg(role, data))
    .collect();
    messages.push(Message::new(
        MessageRole::User,
        MessageContent::Array(vec![
            MessageContentPart::Text { text: data.into() },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: data.into() },
            },
        ]),
    ));
    let mut call = tool(ToolFixture {
        name: "read",
        body: data,
        failed: false,
    });
    if let MessageContent::ToolCalls(calls) = &mut call.content {
        calls.text = data.into();
        calls.tool_results[0].call.arguments = json!({"image": data});
    }
    messages.push(call);
    let rendered = render_transcript(&messages, 2000);
    assert!(!rendered.contains("AQID"));
    assert!(!rendered.contains(";base64,"));
    assert_eq!(rendered.matches("[image/png data: 4 bytes]").count(), 9);
}

fn padded_data_url() -> String {
    let payload = format!("data:image/png;base64,{}", "AQID".repeat(2000));
    let text = format!("head {}{payload} tail", "padding ".repeat(40));
    // Truncating first would leave a tail with base64 but no data-URL prefix.
    assert!(super::truncate_middle(&text, 100).contains("AQID"));
    assert!(strip_data_urls(&text).chars().count() > 100);
    text
}

enum ExpectedToolLimit<'a> {
    Uncapped(&'a str),
    Truncated,
}

fn assert_tool_tier_limit(expected: ExpectedToolLimit<'_>, rendered: &str) {
    match expected {
        ExpectedToolLimit::Uncapped(text) => {
            // Explicit compaction summaries remain uncapped by the per-tool limit.
            assert_eq!(rendered, strip_data_urls(text));
        }
        ExpectedToolLimit::Truncated => {
            assert!(rendered.contains("…[truncated]…"), "{rendered}");
            assert!(rendered.chars().count() <= 100);
        }
    }
}

#[test]
fn all_three_tool_tiers_strip_before_truncating_so_tails_do_not_leak() {
    let text = padded_data_url();
    let cases = [
        (
            json!({"_meta": {"harnx.dev/compaction": text}}),
            ExpectedToolLimit::Uncapped(&text),
        ),
        (
            json!({"content": [{"type": "text", "text": text, "annotations": {"audience": ["user"]}}]}),
            ExpectedToolLimit::Truncated,
        ),
        (json!({"result": text}), ExpectedToolLimit::Truncated),
    ];
    for (output, expected_limit) in cases {
        let out = ToolOutput {
            id: None,
            name: "read".into(),
            output,
            markdown: None,
            content: vec![],
            switch_agent: None,
        };
        let rendered = render_tool_output(&out, 100);
        assert_fragments(
            &rendered,
            ExpectedFragments {
                kept: &["[image/png data: 6000 bytes]", "tail"],
                removed: &["AQID"],
            },
        );
        assert_tool_tier_limit(expected_limit, &rendered);
    }
}

#[test]
fn fitting_transcript_is_unchanged() {
    let messages = [
        msg(MessageRole::User, "question"),
        msg(MessageRole::Assistant, "answer"),
    ];
    assert_eq!(fit(&messages, 100), render_transcript(&messages, 2000));
}

#[test]
fn tool_results_shrink_oldest_first_with_name_and_status() {
    let old_body = "old result ".repeat(100);
    let new_body = "new result ".repeat(100);
    let messages = [
        msg(MessageRole::User, "question"),
        tool(ToolFixture {
            name: "old",
            body: &old_body,
            failed: true,
        }),
        tool(ToolFixture {
            name: "new",
            body: &new_body,
            failed: false,
        }),
        msg(MessageRole::Assistant, "answer"),
    ];
    let rendered = fit(&messages, 340);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &["[old: error; output omitted]", &new_body],
            removed: &[&old_body, "earlier messages omitted"],
        },
    );
    let smaller = fit(&messages, 100);
    assert!(
        smaller.contains("[new: completed; output omitted]"),
        "{smaller}"
    );
}

#[test]
fn middle_messages_drop_oldest_first_before_user_excerpts() {
    let messages = [
        msg(MessageRole::User, "question"),
        msg(MessageRole::Assistant, "old commentary ".repeat(100)),
        msg(MessageRole::Assistant, "new commentary ".repeat(30)),
        msg(MessageRole::Assistant, "final answer"),
    ];
    let rendered = fit(&messages, 110);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &[
                "[1 earlier messages omitted]",
                "new commentary",
                "final answer",
            ],
            removed: &["old commentary", "chars omitted"],
        },
    );
}

#[test]
fn huge_tool_arguments_drop_with_the_whole_middle_round() {
    let mut round = tool(ToolFixture {
        name: "write",
        body: "ok",
        failed: false,
    });
    if let MessageContent::ToolCalls(calls) = &mut round.content {
        calls.tool_results[0].call.arguments = json!({"content": "large argument ".repeat(1000)});
    }
    let messages = [
        msg(MessageRole::User, "request"),
        round,
        msg(MessageRole::Assistant, "answer"),
    ];
    let rendered = fit(&messages, 70);
    assert!(rendered.contains("[1 earlier messages omitted]"));
    assert!(!rendered.contains("tool call"));
    assert!(rendered.contains("request") && rendered.contains("answer"));
}

#[test]
fn user_prompts_alone_over_budget_are_excerpted_oldest_first() {
    let old = format!("old-head {}old-tail", "many words ".repeat(300));
    let middle = "middle prompt";
    let latest = "latest prompt";
    let messages = [
        msg(MessageRole::User, &old),
        msg(MessageRole::User, middle),
        msg(MessageRole::User, latest),
    ];
    let rendered = fit(&messages, 90);
    assert_eq!(rendered.matches("── user ──").count(), 3);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &["old-head", "old-tail", "chars omitted", middle, latest],
            removed: &["user prompts omitted"],
        },
    );
}

#[test]
fn old_final_replies_are_excerpted_and_latest_pair_is_verbatim() {
    let messages = [
        msg(MessageRole::User, "old prompt"),
        msg(MessageRole::Assistant, "old answer ".repeat(500)),
        msg(MessageRole::User, "latest prompt"),
        msg(MessageRole::Assistant, "latest answer"),
    ];
    let rendered = fit(&messages, 75);
    assert_eq!(rendered.matches("── assistant ──").count(), 2);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &[
                "chars omitted",
                "old prompt",
                "latest prompt",
                "latest answer",
            ],
            removed: &[],
        },
    );
}

#[test]
fn huge_latest_reply_keeps_small_prompt_with_or_without_older_history() {
    let prompt = "Please review the parser changes and explain which inputs fail validation without changing any files or running commands.";
    for old_count in [0, 40] {
        let mut messages: Vec<_> = (0..old_count)
            .map(|i| {
                msg(
                    MessageRole::User,
                    format!("old-{i} {}tail", "history ".repeat(40)),
                )
            })
            .collect();
        messages.push(msg(MessageRole::User, prompt));
        messages.push(msg(MessageRole::Assistant, "large reply ".repeat(1000)));
        let rendered = fit(&messages, 120);
        assert!(rendered.contains(prompt), "{rendered}");
        assert_eq!(rendered.matches("chars omitted").count(), 1);
        assert_eq!(rendered.matches("── user ──").count(), 1);
    }
}

#[test]
fn huge_latest_prompt_keeps_small_reply_after_older_history_is_omitted() {
    let reply = "The parser change is complete and all requested validation checks passed without modifying any other files in the repository.";
    let mut messages: Vec<_> = (0..40)
        .map(|i| {
            msg(
                MessageRole::User,
                format!("old-{i} {}tail", "history ".repeat(40)),
            )
        })
        .collect();
    messages.push(msg(MessageRole::User, "large prompt ".repeat(1000)));
    messages.push(msg(MessageRole::Assistant, reply));
    let rendered = fit(&messages, 120);
    assert!(rendered.contains(reply), "{rendered}");
    assert_eq!(rendered.matches("chars omitted").count(), 1);
    assert!(rendered.contains("[40 earlier user prompts omitted]"));
}

#[test]
fn latest_pair_is_excerpted_only_as_last_resort_and_unicode_counts_chars() {
    let prompt = format!("始{}終", "漢字 ".repeat(200));
    let reply = format!("答{}末", "返答 ".repeat(200));
    let messages = [
        msg(MessageRole::User, &prompt),
        msg(MessageRole::Assistant, &reply),
    ];
    let rendered = fit(&messages, 50);
    assert!(
        rendered.contains("始[… 600 chars omitted …]終"),
        "{rendered}"
    );
    assert!(rendered.contains("答") && rendered.ends_with('末'));
    assert_eq!(rendered.matches("chars omitted").count(), 2);
}

#[test]
fn pathological_prompt_count_omits_oldest_but_keeps_latest_pair_in_full() {
    let mut messages: Vec<_> = (0..200)
        .map(|i| {
            msg(
                MessageRole::User,
                format!("prompt-{i} {}end-{i}", "body ".repeat(50)),
            )
        })
        .collect();
    messages.push(msg(MessageRole::User, "latest question must stay intact"));
    messages.push(msg(MessageRole::Assistant, "latest reply must stay intact"));
    let rendered = fit(&messages, 60);
    assert_eq!(rendered.matches("earlier user prompts omitted]").count(), 1);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &[
                "latest question must stay intact",
                "latest reply must stay intact",
            ],
            removed: &["prompt-0"],
        },
    );
}

#[test]
fn prior_summary_is_sanitized_and_budgeted_too() {
    let prior = format!("prior {} data:image/png;base64,AQID", "work ".repeat(500));
    let messages = [
        msg(MessageRole::User, "latest prompt"),
        msg(MessageRole::Assistant, "reply"),
    ];
    let rendered = Transcript::new(&messages, 2000, Some(&prior)).fit(100);
    assert!(estimate_token_length(&rendered) <= 100);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &[
                "Earlier compaction summary:",
                "chars omitted",
                "latest prompt",
            ],
            removed: &["AQID"],
        },
    );
}

#[test]
fn budget_sweep_terminates_and_fits_even_when_users_alone_exceed() {
    let mut messages = Vec::new();
    for i in 0..12 {
        messages.push(msg(
            MessageRole::User,
            format!("q{i} {}tail{i}", "word ".repeat(80)),
        ));
        messages.push(tool(ToolFixture {
            name: "read",
            body: &"output ".repeat(100),
            failed: i % 2 == 0,
        }));
        messages.push(msg(
            MessageRole::Assistant,
            format!("a{i} {}end{i}", "reply ".repeat(70)),
        ));
    }
    for budget in (40..=600).step_by(7) {
        let rendered = fit(&messages, budget);
        assert!(rendered.contains("── user ──"));
        assert!(rendered.contains("── assistant ──"));
    }
}

#[test]
fn impossible_budget_keeps_minimum_latest_prompt_instead_of_failing() {
    let messages = [
        msg(MessageRole::User, "earlier ".repeat(100)),
        msg(MessageRole::User, format!("始{}終", "word ".repeat(100))),
    ];
    let rendered = Transcript::new(&messages, 2000, None).fit(0);
    assert!(rendered.contains("[1 earlier user prompts omitted]"));
    assert!(rendered.contains("始[… 500 chars omitted …]終"));
}

#[test]
fn old_reply_excerpts_cannot_evict_user_prompts_that_fit() {
    let mut messages = Vec::new();
    for i in 0..4 {
        messages.push(msg(MessageRole::User, format!("q{i}")));
        messages.push(msg(MessageRole::Assistant, "long answer ".repeat(100)));
    }
    messages.push(msg(MessageRole::User, "latest question"));
    messages.push(msg(MessageRole::Assistant, "latest answer"));
    let rendered = fit(&messages, 35);
    assert!(!rendered.contains("user prompts omitted"), "{rendered}");
    for i in 0..4 {
        assert!(rendered.contains(&format!("q{i}")), "{rendered}");
    }
    assert!(rendered.contains("latest question") && rendered.contains("latest answer"));
}

#[test]
fn latest_pair_that_exactly_fits_with_omission_marker_stays_verbatim() {
    let mut messages: Vec<_> = (0..200)
        .map(|_| msg(MessageRole::User, "old prompt ".repeat(20)))
        .collect();
    messages.push(msg(MessageRole::User, "latest question"));
    messages.push(msg(MessageRole::Assistant, "latest answer"));
    let expected = "[200 earlier user prompts omitted]\n\n── user ──\nlatest question\n\n── assistant ──\nlatest answer";
    let rendered = fit(&messages, estimate_token_length(expected));
    assert_eq!(rendered, expected);
}

#[test]
fn five_thousand_messages_fit_ten_thousand_tokens() {
    let messages: Vec<_> = (0..5000)
        .map(|index| {
            let role = if index % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            msg(
                role,
                format!("message {index}: {}end {index}", "context word ".repeat(20)),
            )
        })
        .collect();
    let transcript = Transcript::new(&messages, 2000, None);
    super::transcript::FULL_RENDER_COUNT.with(|count| count.set(0));
    let started = std::time::Instant::now();
    let rendered = transcript.fit(10_000);
    let renders = super::transcript::FULL_RENDER_COUNT.with(|count| count.get());
    eprintln!(
        "5000 messages, 10000-token budget: fit took {:?}, {renders} full renders",
        started.elapsed()
    );
    // Assert work, not wall time, so a busy runner cannot make this flaky.
    assert!(
        renders <= 12,
        "expected stage-boundary renders, got {renders}"
    );
    assert!(estimate_token_length(&rendered) <= 10_000);
    assert_fragments(
        &rendered,
        ExpectedFragments {
            kept: &[
                &messages[4998].content.to_text(),
                &messages[4999].content.to_text(),
            ],
            removed: &[],
        },
    );
}

#[test]
fn exact_fit_is_unchanged_despite_per_message_rounding() {
    let messages: Vec<_> = (0..200)
        .map(|_| msg(MessageRole::User, "question"))
        .collect();
    let expected = render_transcript(&messages, 2000);
    let budget = estimate_token_length(&expected);
    let rounded = rounded_transcript_tokens(&messages);
    assert!(
        rounded > budget,
        "fixture must exercise per-message rounding"
    );
    assert_eq!(fit(&messages, budget), expected);
}

#[test]
fn stage_validation_corrects_cached_underestimates() {
    let messages: Vec<_> = (0..50)
        .map(|_| msg(MessageRole::User, "word ".repeat(19)))
        .collect();
    let rendered = fit(&messages, underestimated_budget(&messages));
    assert_eq!(rendered.matches("── user ──").count(), messages.len());
    assert!(rendered.contains("chars omitted"));
    assert!(rendered.ends_with(&messages.last().unwrap().content.to_text()));
}

fn underestimated_budget(messages: &[Message]) -> usize {
    let rounded = rounded_transcript_tokens(messages);
    let exact = estimate_token_length(&render_transcript(messages, 2000));
    assert!(
        rounded < exact,
        "fixture must exercise f32 accumulation: {rounded} < {exact}"
    );
    rounded
}

fn rounded_transcript_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            estimate_token_length(&render_transcript(std::slice::from_ref(message), 2000))
        })
        .sum()
}
