//! Detects a model streaming the same text over and over, in its answer or
//! its thinking. The check is pure, so the streaming handler, the
//! non-streaming path and the session replay reach the same verdicts.

use super::{RepetitionSource, RepetitionTerminal};
use serde::Serialize;
use std::collections::VecDeque;

/// A repeated tail must cover at least this many characters. Short exact
/// repetition (a divider, a run of zeros) is normal output; this much is not,
/// and waiting for it costs only about 500 tokens.
pub const OUTPUT_REPEAT_MIN_COVER: usize = 2_000;
/// The tail must also hold at least this many back-to-back copies of its
/// unit, so a long block that appears twice is never enough.
pub const OUTPUT_REPEAT_MIN_COPIES: usize = 4;
/// The longest repeated unit looked for.
pub const OUTPUT_REPEAT_MAX_UNIT: usize = 2_000;
/// New characters between checks while streaming.
const CHECK_EVERY: usize = 256;
/// Characters kept per channel: room for four copies of the longest unit.
const TAIL: usize = OUTPUT_REPEAT_MAX_UNIT * OUTPUT_REPEAT_MIN_COPIES;
/// How much of the unit a note or message quotes.
const EXCERPT_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
    Answer,
    Thinking,
}

impl OutputChannel {
    fn source(self) -> RepetitionSource {
        match self {
            Self::Answer => RepetitionSource::Answer,
            Self::Thinking => RepetitionSource::Thinking,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Answer => "reply",
            Self::Thinking => "reasoning",
        }
    }
}

/// Where a channel's text was found repeating.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepeatedTail {
    /// The last copy of the repeated unit.
    pub unit: String,
    /// How many characters at the end of the text repeat `unit`.
    pub covered: usize,
    /// Characters read when the repeat was found.
    pub at_char: usize,
}

/// Incremental detector for one channel of one response.
#[derive(Debug, Default)]
pub struct OutputRepeatDetector {
    tail: VecDeque<char>,
    since_check: usize,
    seen: usize,
}

impl OutputRepeatDetector {
    /// Feed the next chunk. Checks every [`CHECK_EVERY`] characters, so the
    /// verdict does not depend on how the stream was chunked.
    pub fn push(&mut self, chunk: &str) -> Option<RepeatedTail> {
        chunk.chars().find_map(|ch| self.push_char(ch))
    }

    /// Process one character and check if a repeat was found.
    fn push_char(&mut self, ch: char) -> Option<RepeatedTail> {
        if self.tail.len() == TAIL {
            self.tail.pop_front();
        }
        self.tail.push_back(ch);
        self.seen += 1;
        self.since_check += 1;
        if self.since_check < CHECK_EVERY {
            return None;
        }
        self.since_check = 0;
        self.check()
    }

    /// Check whatever arrived since the last check, at the end of a complete
    /// text.
    pub fn finish(&mut self) -> Option<RepeatedTail> {
        if self.since_check == 0 {
            return None;
        }
        self.since_check = 0;
        self.check()
    }

    fn check(&mut self) -> Option<RepeatedTail> {
        let (unit, covered) = periodic_tail(self.tail.make_contiguous())?;
        Some(RepeatedTail {
            unit,
            covered,
            at_char: self.seen,
        })
    }
}

/// Check a complete text the way a stream of it would have been checked.
pub fn detect_in_text(text: &str) -> Option<RepeatedTail> {
    let mut detector = OutputRepeatDetector::default();
    detector.push(text).or_else(|| detector.finish())
}

/// Check a complete text, for replies that did not stream.
pub fn check_output(channel: OutputChannel, text: &str) -> Result<(), RepetitiveOutput> {
    match detect_in_text(text) {
        Some(tail) => Err(RepetitiveOutput::new(channel, tail)),
        None => Ok(()),
    }
}

/// The smallest unit the tail repeats, if it meets the rule, and how much of
/// the tail it covers. For a unit length `p`, `p + z[p]` over the reversed
/// tail is the length of the tail's longest `p`-periodic suffix.
fn periodic_tail(tail: &[char]) -> Option<(String, usize)> {
    if tail.len() < OUTPUT_REPEAT_MIN_COVER {
        return None;
    }
    let reversed: Vec<char> = tail.iter().rev().copied().collect();
    let z = z_function(&reversed);
    let longest = OUTPUT_REPEAT_MAX_UNIT.min(reversed.len() - 1);
    (1..=longest).find_map(|unit| {
        let covered = unit + z[unit];
        (covered >= OUTPUT_REPEAT_MIN_COVER.max(OUTPUT_REPEAT_MIN_COPIES * unit))
            .then(|| (tail[tail.len() - unit..].iter().collect(), covered))
    })
}

/// `z[i]` is the length of the longest common prefix of `s` and `s[i..]`.
fn z_function(s: &[char]) -> Vec<usize> {
    let n = s.len();
    let mut z = vec![0; n];
    let (mut left, mut right) = (0, 0);
    for i in 1..n {
        if i < right {
            z[i] = (right - i).min(z[i - left]);
        }
        while i + z[i] < n && s[z[i]] == s[i + z[i]] {
            z[i] += 1;
        }
        if i + z[i] > right {
            left = i;
            right = i + z[i];
        }
    }
    z
}

/// The error a response fails with when its output starts repeating. The
/// retry layer recognizes it through [`find_repetitive_output`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepetitiveOutput {
    pub channel: OutputChannel,
    pub unit: String,
}

impl RepetitiveOutput {
    pub fn new(channel: OutputChannel, tail: RepeatedTail) -> Self {
        Self {
            channel,
            unit: tail.unit,
        }
    }

    fn excerpt(&self) -> String {
        let mut excerpt: String = self.unit.chars().take(EXCERPT_CHARS).collect();
        if self.unit.chars().count() > EXCERPT_CHARS {
            excerpt.push('…');
        }
        excerpt
    }

    /// The note sent with the retried request. It names what repeated
    /// without replaying the loop, which would only feed it.
    pub fn retry_note(&self) -> String {
        let subject = match self.channel {
            OutputChannel::Answer => "Your previous reply",
            OutputChannel::Thinking => "Your reasoning",
        };
        format!(
            "[harnx] {subject} was stopped because it began repeating the same text (\"{}\"). \
             Continue without repeating it.",
            self.excerpt()
        )
    }

    /// The stop that ends the turn when every attempt repeated.
    pub fn terminal(&self) -> RepetitionTerminal {
        RepetitionTerminal::output(self.channel.source())
    }
}

impl std::fmt::Display for RepetitiveOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model's {} started repeating the same text (\"{}\")",
            self.channel.noun(),
            self.excerpt()
        )
    }
}

impl std::error::Error for RepetitiveOutput {}

/// The repeat anywhere in `error`'s chain; the call path wraps stream errors
/// in context.
pub fn find_repetitive_output(error: &anyhow::Error) -> Option<&RepetitiveOutput> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RepetitiveOutput>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_guard::{parse_repetition_terminal, RepetitionStop};

    fn repeated(unit: &str, total_chars: usize) -> String {
        unit.chars().cycle().take(total_chars).collect()
    }

    /// The next value of a small linear congruential generator: enough to
    /// make varied, reproducible test text.
    fn next_random(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff;
        *state >> 16
    }

    /// Letters with no short period, so the only repetition in a test's text
    /// is the one the test builds.
    fn noise(len: usize, seed: u32) -> String {
        let mut state = seed;
        (0..len)
            .map(|_| char::from(b'a' + (next_random(&mut state) % 26) as u8))
            .collect()
    }

    /// Filler prose, the way a lorem ipsum generator makes it: sentences of 6
    /// to 17 words drawn from a small vocabulary.
    fn prose(chars: usize, seed: u32) -> String {
        const WORDS: [&str; 24] = [
            "the", "model", "reads", "a", "file", "then", "writes", "tests", "for", "each",
            "change", "it", "makes", "to", "code", "while", "checking", "results", "before",
            "moving", "on", "next", "step", "plan",
        ];
        let mut state = seed;
        let mut text = String::new();
        while text.len() < chars {
            let words = 6 + next_random(&mut state) as usize % 12;
            let sentence: Vec<&str> = (0..words)
                .map(|_| WORDS[next_random(&mut state) as usize % WORDS.len()])
                .collect();
            text.push_str(&sentence.join(" "));
            text.push_str(". ");
        }
        text
    }

    /// Long output that repeats a shape without repeating its text: ordinary
    /// replies at their most regular.
    fn ordinary_samples() -> Vec<(&'static str, String)> {
        let table: String = std::iter::once("| name | value | note |\n|---|---|---|\n".to_string())
            .chain((0..200).map(|i| format!("| item{i} | {} | row {i} |\n", i * 7)))
            .collect();
        let list: String = (1..=300)
            .map(|i| format!("{i}. Step number {i} of the plan\n"))
            .collect();
        let code = format!(
            "```rust\n{}```\n",
            (0..200)
                .map(|i| format!("let x{i} = compute({i});\n"))
                .collect::<String>()
        );
        let json = format!(
            "[{}]",
            (0..150)
                .map(|i| format!("{{\"id\": {i}, \"status\": \"ok\", \"retries\": 0}}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let log: String = (0..200)
            .map(|i| {
                format!(
                    "2026-09-30T12:{:02}:{:02}Z INFO worker: heartbeat sent\n",
                    i / 60,
                    i % 60
                )
            })
            .collect();
        let prose = prose(20_000, 3);
        let mixed = format!("{prose}\n\n{table}\n{list}\n{code}\n{json}\n\n{log}");
        vec![
            ("prose", prose),
            ("table", table),
            ("list", list),
            ("code", code),
            ("json", json),
            ("log", log),
            ("mixed", mixed),
        ]
    }

    fn fed_in_chunks(text: &str, chunk: usize) -> Option<RepeatedTail> {
        let mut detector = OutputRepeatDetector::default();
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            let piece: String = piece.iter().collect();
            if let Some(hit) = detector.push(&piece) {
                return Some(hit);
            }
        }
        detector.finish()
    }

    #[test]
    fn a_short_unit_is_caught_after_two_thousand_characters() {
        assert!(detect_in_text(&repeated("the ", 1_999)).is_none());
        let hit = detect_in_text(&repeated("the ", 2_100)).expect("the the the …");
        assert_eq!(hit.unit, "the ");
        assert_eq!((hit.covered, hit.at_char), (2_048, 2_048));
    }

    #[test]
    fn a_long_unit_needs_four_copies() {
        // (unit length, seed, characters that stop short of four copies,
        // characters that make four). The second row is the longest unit
        // looked for, whose four copies fill the whole 8,000-character tail.
        let cases = [
            (1_501, 7, 1_501 * 3 + 200, 1_501 * 4 + 10),
            (2_000, 13, 7_999, 8_000),
        ];
        for (len, seed, short, enough) in cases {
            let unit = noise(len, seed);
            assert!(
                detect_in_text(&repeated(&unit, short)).is_none(),
                "unit of {len}: fewer than four copies"
            );
            let hit = detect_in_text(&repeated(&unit, enough))
                .unwrap_or_else(|| panic!("unit of {len}: four copies"));
            assert_eq!(hit.unit.chars().count(), len);
            assert_eq!((hit.covered, hit.at_char), (enough, enough));
        }
    }

    #[test]
    fn a_unit_longer_than_the_limit_is_not_looked_for() {
        let unit = noise(2_001, 11);
        assert!(detect_in_text(&repeated(&unit, 2_001 * 4)).is_none());
    }

    #[test]
    fn whitespace_floods_count() {
        let unit = |text: &str| detect_in_text(text).map(|hit| hit.unit);
        assert_eq!(unit(&" ".repeat(2_500)).as_deref(), Some(" "));
        assert_eq!(unit(&"\n".repeat(2_500)).as_deref(), Some("\n"));
    }

    #[test]
    fn a_loop_after_legitimate_text_is_still_caught() {
        let text = format!(
            "Here is my analysis of the module.\n\n{}",
            repeated("I will now re-read the file. ", 2_400)
        );
        let hit = detect_in_text(&text).expect("the loop after the prefix");
        assert_eq!(hit.unit.chars().count(), 29);
    }

    #[test]
    fn a_loop_after_more_text_than_the_tail_holds_is_still_caught() {
        // The detector keeps 8,000 characters, so this prefix fills the tail
        // and every later character pushes an older one out.
        let prefix = prose(12_000, 5);
        let prefix_chars = prefix.chars().count();
        assert!(
            detect_in_text(&prefix).is_none(),
            "the prefix alone is fine"
        );
        let unit = "I will now re-read the file. ";
        let text = format!("{prefix}{}", repeated(unit, 2_400));
        let hit = detect_in_text(&text).expect("the loop after the long prefix");
        assert_eq!(hit.unit.chars().count(), unit.chars().count());
        // Caught at the first check after the repeat reaches 2,000 characters.
        assert!(
            hit.at_char < prefix_chars + 2_000 + CHECK_EVERY,
            "found at character {} after a prefix of {prefix_chars}",
            hit.at_char
        );
    }

    #[test]
    fn multi_byte_text_is_counted_in_characters() {
        assert!(detect_in_text(&repeated("繰り返し🙂", 1_999)).is_none());
        let hit = detect_in_text(&repeated("繰り返し🙂", 2_100)).expect("CJK and emoji loop");
        // Found at character 2,048, partway through a copy of the unit.
        assert_eq!(hit.unit, "し🙂繰り返");
        assert_eq!(hit.at_char, 2_048);
    }

    #[test]
    fn the_verdict_does_not_depend_on_chunk_sizes() {
        let looping = format!("intro text. {}", repeated("again and again ", 3_000));
        let varied: String = (0..400)
            .map(|i| format!("line {i} says something new\n"))
            .collect();
        for chunk in [1, 7, 256, 1_000, 10_000] {
            assert_eq!(
                fed_in_chunks(&looping, chunk).map(|hit| hit.at_char),
                Some(2_048),
                "chunk {chunk}"
            );
            assert!(fed_in_chunks(&varied, chunk).is_none(), "chunk {chunk}");
        }
    }

    #[test]
    fn ordinary_output_is_not_flagged() {
        for (name, sample) in ordinary_samples() {
            assert!(sample.len() > 2_000, "{name} is too short to test anything");
            assert!(detect_in_text(&sample).is_none(), "{name} was flagged");
        }
    }

    #[test]
    fn repetitive_output_names_the_channel_in_notes_and_stops() {
        let answer = RepetitiveOutput::new(
            OutputChannel::Answer,
            RepeatedTail {
                unit: "the ".into(),
                covered: 2_048,
                at_char: 2_048,
            },
        );
        assert_eq!(
            answer.retry_note(),
            "[harnx] Your previous reply was stopped because it began repeating the same text \
             (\"the \"). Continue without repeating it."
        );
        assert_eq!(
            answer.terminal().sentence(),
            "Stopped: the model's reply kept repeating the same text."
        );
        let thinking = RepetitiveOutput::new(
            OutputChannel::Thinking,
            RepeatedTail {
                unit: "x".repeat(79) + "yz",
                covered: 2_048,
                at_char: 2_048,
            },
        );
        assert_eq!(
            thinking.retry_note(),
            format!(
                "[harnx] Your reasoning was stopped because it began repeating the same text \
                 (\"{}y…\"). Continue without repeating it.",
                "x".repeat(79)
            )
        );
        assert_eq!(
            thinking.terminal().sentence(),
            "Stopped: the model's reasoning kept repeating the same text."
        );
    }

    #[test]
    fn the_output_stop_marker_round_trips_with_its_source() {
        let stop = RepetitionStop(RepetitionTerminal::output(RepetitionSource::Thinking));
        let persisted = format!("{:#}", anyhow::Error::new(stop).context("worker turn"));
        assert!(persisted.ends_with("harnx:repetition {\"source\":\"thinking\"}"));
        let parsed = parse_repetition_terminal(&persisted).unwrap();
        assert_eq!(parsed.source, RepetitionSource::Thinking);
        assert_eq!((parsed.tool, parsed.count), (None, None));
    }

    #[test]
    fn a_repeat_is_found_through_error_context() {
        let err = anyhow::Error::new(RepetitiveOutput {
            channel: OutputChannel::Answer,
            unit: "a".into(),
        })
        .context("Failed to call chat-completions api");
        assert_eq!(
            find_repetitive_output(&err).map(|repeat| repeat.channel),
            Some(OutputChannel::Answer)
        );
        assert!(find_repetitive_output(&anyhow::anyhow!("boom")).is_none());
        assert!(check_output(OutputChannel::Answer, "fine").is_ok());
        assert!(check_output(OutputChannel::Thinking, &" ".repeat(3_000)).is_err());
    }
}
