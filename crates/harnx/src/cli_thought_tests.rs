use super::*;
use crate::test_utils::capture_output;

fn capture_stream(
    highlight: bool,
    terminal: bool,
    action: impl FnOnce(&mut CliSinkState),
) -> String {
    harnx_core::require_nextest();
    let sink = CliAgentEventSink::new(
        highlight,
        RenderOptions::default(),
        harnx_core::abort::create_abort_signal(),
    );
    let mut state = sink.state.lock().unwrap();
    // dup2 capture isn't a terminal. Override the production TTY snapshot, not
    // the render entry point, so these tests cover the actual dispatch gate.
    state.stdout_is_terminal = terminal;
    capture_output(|| action(&mut state))
}

fn chunk(state: &mut CliSinkState, thought: bool, text: &str) {
    let blocks = vec![ContentBlock::Text(text.into())];
    state.print_model_event(if thought {
        ModelEvent::ThoughtChunk { blocks }
    } else {
        ModelEvent::MessageChunk { blocks }
    });
}

fn label() -> &'static str {
    "\x1b[2mThinking\x1b[0m\n"
}

fn thought(text: &str) -> String {
    format!("\x1b[2;3;90m{text}\x1b[0m")
}

fn end_turn(state: &mut CliSinkState) {
    state.print_turn_event(TurnEvent::Ended {
        outcome: Default::default(),
    });
}

#[test]
fn native_thought_chunks_stream_inline_then_resume_markdown() {
    let options = RenderOptions {
        theme: Some(harnx_render::load_builtin_theme(false).unwrap()),
        ..RenderOptions::default()
    };
    let output = capture_stream(true, true, |state| {
        state.render_options = options.clone();
        chunk(state, true, "reason **plain** ");
        chunk(state, true, "continues");
        chunk(state, false, "answer **bold**\n");
        assert!(state.render.is_some());
        assert!(state.buffer.is_empty());
        end_turn(state);
    });
    let answer = MarkdownRender::init(options)
        .unwrap()
        .render("answer **bold**");
    assert!(answer.contains("\x1b["));
    assert!(!answer.contains("\x1b[2;3;90m"));
    assert_eq!(
        output,
        format!(
            "{}{}{}\n\r{answer}\n",
            label(),
            thought("reason **plain** "),
            thought("continues")
        )
    );
}

#[test]
fn inline_split_tags_never_reach_partial_line_writes() {
    let output = capture_stream(true, true, |state| {
        chunk(state, false, "before<thi");
        assert_eq!(state.buffer, "before");
        chunk(state, false, "nk>\nrea");
        assert!(state.buffer.is_empty());
        chunk(state, false, "son\n</thi");
        chunk(state, false, "nk>answer\n");
        end_turn(state);
    });
    assert_eq!(
        output,
        format!(
            "before\rbefore\n{}{}{}\n\ranswer\n",
            label(),
            thought("rea"),
            thought("son")
        )
    );
    assert!(!output.contains("<thi"));
    assert!(!output.contains("</thi"));
}

#[test]
fn multiple_and_adjacent_thought_blocks_each_get_label() {
    let output = capture_stream(true, true, |state| {
        chunk(
            state,
            false,
            "<think>one</think><thinking>two</thinking>middle",
        );
        chunk(state, false, "<think>three</think>end\n");
        end_turn(state);
    });
    assert_eq!(
        output,
        format!(
            "{}{}\n{}{}\nmiddle\rmiddle\n{}{}\n\rend\n",
            label(),
            thought("one"),
            label(),
            thought("two"),
            label(),
            thought("three")
        )
    );
    assert_eq!(output.matches(label()).count(), 3);
    assert!(!output.contains("<think"));
}

#[test]
fn unclosed_thought_flushes_held_newline_and_resets_next_turn() {
    let output = capture_stream(true, true, |state| {
        chunk(state, false, "<thinking>unfinished\n");
        end_turn(state);
        assert!(state.render.is_none());
        assert!(state.buffer.is_empty());
        chunk(state, true, "new thought");
        chunk(state, false, "next answer\n");
        end_turn(state);
    });
    assert_eq!(
        output,
        format!(
            "{}{}{}{}{}\n\rnext answer\n",
            label(),
            thought("unfinished"),
            thought("\n"),
            label(),
            thought("new thought")
        )
    );
}

#[test]
fn cleanup_drains_partial_prefix_before_closing_markdown_line() {
    let output = capture_stream(true, true, |state| {
        chunk(state, false, "answer <thi");
        state.print_model_event(ModelEvent::Final {
            output: String::new(),
            usage: Default::default(),
        });
        assert!(state.render.is_none());
        assert!(state.buffer.is_empty());
        chunk(state, false, "next\n");
        end_turn(state);
    });
    // Not a completed tag: preserve literal text, then close that same line.
    assert_eq!(output, "answer <thi\n\rnext\n");
}

#[test]
fn thought_newline_is_not_duplicated_on_transition() {
    let output = capture_stream(true, true, |state| {
        chunk(state, true, "reason\n");
        chunk(state, false, "answer\n");
        end_turn(state);
    });
    assert_eq!(
        output,
        format!("{}{}\ranswer\n", label(), thought("reason\n"))
    );
}

#[test]
fn raw_and_non_tty_output_remain_byte_identical() {
    for (highlight, terminal) in [(false, true), (false, false), (true, false)] {
        let output = capture_stream(highlight, terminal, |state| {
            chunk(state, true, "native ");
            chunk(state, true, "thought\n");
            chunk(state, false, "before<thi");
            chunk(state, false, "nk>inline</think><thinking>unclosed\n");
            end_turn(state);
            chunk(state, false, "tail");
            end_turn(state);
            assert!(state.render.is_none());
        });
        assert_eq!(
            output,
            "native thought\nbefore<think>inline</think><thinking>unclosed\ntail\n"
        );
    }
}

#[test]
fn cleanup_flushes_unfinished_closer_in_thought_style() {
    let output = capture_stream(true, true, |state| {
        chunk(state, false, "<think>body\n</thi");
        end_turn(state);
        // Cleanup is idempotent, including the block's separator newline.
        end_turn(state);
        chunk(state, false, "next\n");
        end_turn(state);
    });
    assert_eq!(
        output,
        format!(
            "{}{}{}\n\rnext\n",
            label(),
            thought("body"),
            thought("\n</thi")
        )
    );
}
