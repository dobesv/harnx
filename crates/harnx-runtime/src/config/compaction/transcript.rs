//! Sanitized transcript pieces, kept separate until the summarizer budget is known.
//!
//! The summarizer reads the transcript as XML-style elements: the dropped
//! messages inside `<transcript>`, then `<kept_after_summary>` listing what is
//! replayed verbatim after the summary. Each speaker has its own element, so
//! the summarizer can attribute text: an agent's instructions to a delegate
//! sit inside `<tool_call>` and the delegate's echo of them inside
//! `<tool_result>`, never inside `<user>`.

use super::{is_turn_prompt, render_tool_output, truncate_middle};
use fancy_regex::{Captures, Regex};
use harnx_core::message::{Message, MessageContent, MessageContentPart, MessageRole};
use harnx_core::runtime_note::is_reserved_message_id;
use harnx_core::session::ToolOutput;
use harnx_core::text::estimate_token_length;
use std::borrow::Cow;
use std::sync::LazyLock;

#[cfg(test)]
thread_local! {
    pub(super) static FULL_RENDER_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

static DATA_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:data):([^;,\s]*)(?:;[^;,\s]*)*;(?i:base64),([A-Za-z0-9+/_-]*={0,2})").unwrap()
});

/// Count decoded bytes without allocating or decoding a potentially huge payload.
/// Sanitize before taking excerpts: a tail slice alone no longer has a `data:` prefix.
pub(super) fn strip_data_urls(text: &str) -> Cow<'_, str> {
    DATA_URL.replace_all(text, |captures: &fancy_regex::Captures<'_, str>| {
        let mime = if captures[1].is_empty() {
            "text/plain"
        } else {
            &captures[1]
        };
        let bytes = captures[2].trim_end_matches('=').len() * 3 / 4;
        format!("[{mime} data: {bytes} bytes]")
    })
}

/// Our own element names, wherever they appear in message or tool text. Text
/// such as a file of this module, or a tool result quoting `</tool_result>`,
/// must not be able to close an element or open a forged `<user>`. A new
/// element name belongs in this list too.
static ELEMENT_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"<(/?)(transcript|earlier_summary|kept_after_summary|system|user|assistant|runtime_note|tool_call|tool_result)\b",
    )
    .unwrap()
});

fn escape_element_tags(text: &str) -> Cow<'_, str> {
    ELEMENT_TAG.replace_all(text, |captures: &Captures<'_, str>| {
        format!("&lt;{}{}", &captures[1], &captures[2])
    })
}

fn attribute(name: &str, value: &str) -> String {
    let value = strip_data_urls(value)
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace(['\r', '\n'], " ");
    format!(" {name}=\"{value}\"")
}

fn render_parts(parts: &[MessageContentPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            MessageContentPart::Text { text } => text.clone(),
            MessageContentPart::ImageUrl { image_url } => {
                format!("[image: {}]", image_url.url)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Reply,
    Tool,
    Context,
}

#[derive(Clone)]
struct Section {
    open: String,
    body: String,
    close: String,
    stub: Option<String>,
}

impl Section {
    /// An element whose body is message or tool text: strips inline data and
    /// escapes our own tag names so the body cannot end the element early.
    fn element(tag: &str, attributes: &str, body: &str) -> Self {
        Self::from_sanitized(tag, attributes, &strip_data_urls(body))
    }

    /// Like `element`, for a body whose data URLs were already stripped.
    fn from_sanitized(tag: &str, attributes: &str, body: &str) -> Self {
        Self {
            open: format!("<{tag}{attributes}>\n"),
            body: escape_element_tags(body).into_owned(),
            close: format!("\n</{tag}>"),
            stub: None,
        }
    }

    /// A self-closing element that stands in for content left out.
    fn empty(tag: &str, attributes: &str) -> Self {
        Self {
            open: format!("<{tag}{attributes}/>"),
            body: String::new(),
            close: String::new(),
            stub: None,
        }
    }

    fn render(&self) -> String {
        format!("{}{}{}", self.open, self.body, self.close)
    }

    fn frame_tokens(&self) -> usize {
        estimate_token_length(&self.open) + estimate_token_length(&self.close)
    }

    fn shrink_to_stub(&mut self) -> bool {
        let Some(stub) = self.stub.take() else {
            return false;
        };
        if estimate_token_length(&stub) >= estimate_token_length(&self.render()) {
            return false;
        }
        self.open = stub;
        self.body.clear();
        self.close.clear();
        true
    }
}

#[derive(Clone)]
struct RenderedMessage {
    kind: Kind,
    sections: Vec<Section>,
    tokens: usize,
    omitted: bool,
}

impl RenderedMessage {
    fn from_message(message: &Message, max_tool_chars: usize) -> Self {
        if let MessageContent::ToolCalls(calls) = &message.content {
            return Self::new(Kind::Tool, tool_sections(calls, max_tool_chars));
        }
        let (kind, tag) = message_kind_and_tag(message);
        let body = render_content(&message.content);
        let mut section = Section::element(tag, "", &body);
        if kind == Kind::Tool {
            section.stub = Some(format!(
                "<tool_result{}{}/>",
                attribute("status", "completed"),
                attribute("output", "omitted")
            ));
        }
        Self::new(kind, vec![section])
    }

    fn prior_summary(summary: &str) -> Self {
        Self::new(
            Kind::Context,
            vec![Section::element("earlier_summary", "", summary)],
        )
    }

    fn new(kind: Kind, sections: Vec<Section>) -> Self {
        let mut message = Self {
            kind,
            sections,
            tokens: 0,
            omitted: false,
        };
        message.recount();
        message
    }

    fn render(&self) -> String {
        render_sections(&self.sections)
    }

    fn recount(&mut self) {
        self.tokens = estimate_token_length(&self.render());
    }
}

fn render_sections(sections: &[Section]) -> String {
    sections
        .iter()
        .map(Section::render)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Runtime notes, hook context and timing notices are user-role rows the
/// harness wrote. Only the structured id or the timing receipt identifies
/// them; the `[Runtime note]` text prefix is not trusted.
fn is_runtime_context(message: &Message) -> bool {
    !is_turn_prompt(message) || message.id.as_deref().is_some_and(is_reserved_message_id)
}

fn message_kind_and_tag(message: &Message) -> (Kind, &'static str) {
    match message.role {
        MessageRole::System => (Kind::Context, "system"),
        MessageRole::User if is_runtime_context(message) => (Kind::Context, "runtime_note"),
        MessageRole::User => (Kind::User, "user"),
        MessageRole::Assistant => (Kind::Assistant, "assistant"),
        MessageRole::Tool => (Kind::Tool, "tool_result"),
    }
}

fn render_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Array(parts) => render_parts(parts),
        _ => content.to_text(),
    }
}

fn mark_turn_replies(messages: &mut [RenderedMessage]) {
    let mut has_reply = false;
    for message in messages.iter_mut().rev() {
        match message.kind {
            Kind::User => has_reply = false,
            Kind::Assistant if !has_reply => {
                message.kind = Kind::Reply;
                has_reply = true;
            }
            _ => {}
        }
    }
}

fn tool_status(output: &serde_json::Value) -> &'static str {
    let failed = ["isError", "is_error"]
        .iter()
        .any(|key| output.get(key).and_then(|v| v.as_bool()) == Some(true));
    if failed {
        "error"
    } else {
        "completed"
    }
}

fn tool_sections(calls: &harnx_core::message::MessageContentToolCalls, cap: usize) -> Vec<Section> {
    let mut sections = Vec::new();
    if !calls.text.is_empty() {
        sections.push(Section::element("assistant", "", &calls.text));
    }
    for result in &calls.tool_results {
        let name = attribute("name", &result.call.name);
        let args = serde_json::to_string(&result.call.arguments).unwrap_or_default();
        sections.push(Section::element("tool_call", &name, &args));
        let output = ToolOutput {
            id: result.call.id.clone(),
            name: result.call.name.clone(),
            output: result.output.clone(),
            markdown: result.markdown.clone(),
            content: result.content.clone(),
            user_surfaces: result.user_surfaces.clone(),
            switch_agent: result.switch_agent.clone(),
        };
        let status = attribute("status", tool_status(&result.output));
        // The three-tier renderer already sanitizes before applying its cap.
        let mut section = Section::from_sanitized(
            "tool_result",
            &format!("{name}{status}"),
            &render_tool_output(&output, cap),
        );
        section.stub = Some(format!(
            "<tool_result{name}{status}{}/>",
            attribute("output", "omitted")
        ));
        sections.push(section);
    }
    sections
}

/// What the summarizer sees of a message replayed after its summary: who
/// spoke and what they said, with tool arguments and results left out.
fn kept_sections(message: &Message, cap: usize) -> Vec<(Kind, Section)> {
    let excerpt = |text: &str| truncate_middle(&strip_data_urls(text), cap);
    if let MessageContent::ToolCalls(calls) = &message.content {
        let mut sections = Vec::new();
        if !calls.text.is_empty() {
            sections.push((
                Kind::Assistant,
                Section::from_sanitized("assistant", "", &excerpt(&calls.text)),
            ));
        }
        sections.extend(calls.tool_results.iter().map(|result| {
            let name = attribute("name", &result.call.name);
            (Kind::Tool, Section::empty("tool_call", &name))
        }));
        return sections;
    }
    match message_kind_and_tag(message) {
        (Kind::Tool, tag) => vec![(Kind::Tool, Section::empty(tag, ""))],
        (kind, tag) => {
            let body = excerpt(&render_content(&message.content));
            vec![(kind, Section::from_sanitized(tag, "", &body))]
        }
    }
}

const KEPT_OPEN: &str = "<kept_after_summary>\n";
const KEPT_CLOSE: &str = "\n</kept_after_summary>";

/// Render the replayed messages within `limit` tokens. Excerpting bodies can't
/// shrink element tags, so a round of many tool calls can overflow `limit` by
/// itself; the fewest elements that make the block fit are then left out.
fn render_kept(sections: &[(Kind, Section)], limit: usize) -> String {
    if sections.is_empty() {
        return "<kept_after_summary/>".into();
    }
    let order = kept_omission_order(sections);
    let render = |omitted: usize| {
        let mut shown = vec![true; sections.len()];
        order[..omitted]
            .iter()
            .for_each(|&index| shown[index] = false);
        let shown: Vec<&Section> = sections
            .iter()
            .zip(shown)
            .filter_map(|((_, section), show)| show.then_some(section))
            .collect();
        let full = wrap_kept(&shown, omitted, None);
        if estimate_token_length(&full) <= limit {
            full
        } else {
            wrap_kept(&shown, omitted, Some(limit))
        }
    };
    let all = render(0);
    if estimate_token_length(&all) <= limit {
        return all;
    }
    // Search for the fewest omissions that fit. Each candidate is measured,
    // so the result stays within `limit` unless even the bare marker doesn't.
    let (mut low, mut high) = (1, sections.len());
    let mut best = render(high);
    while low < high {
        let omitted = low + (high - low) / 2;
        let candidate = render(omitted);
        if estimate_token_length(&candidate) <= limit {
            best = candidate;
            high = omitted;
        } else {
            low = omitted + 1;
        }
    }
    best
}

/// The order elements are left out of the kept block in: tool calls first,
/// then other messages, then user prompts, each oldest first. The user's
/// prompt is what the summarizer most needs.
fn kept_omission_order(sections: &[(Kind, Section)]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sections.len()).collect();
    // The sort is stable, so each group stays oldest first.
    order.sort_by_key(|&index| {
        let kind = sections[index].0;
        (kind != Kind::Tool, kind == Kind::User)
    });
    order
}

/// The kept block with `omitted` elements noted as left out. Under a `limit`,
/// every shown body gets an equal share, excerpted from both ends.
fn wrap_kept(sections: &[&Section], omitted: usize, limit: Option<usize>) -> String {
    let marker = (omitted > 0).then(|| format!("[{omitted} replayed elements not listed]"));
    let share = limit.map(|limit| {
        let fixed = estimate_token_length(&format!("{KEPT_OPEN}{KEPT_CLOSE}"))
            + marker.as_deref().map_or(0, estimate_token_length);
        limit.saturating_sub(fixed) / sections.len().max(1)
    });
    let bodies = sections.iter().map(|section| {
        let mut section = (*section).clone();
        if let Some(share) = share {
            section.body =
                excerpt_text(&section.body, share.saturating_sub(section.frame_tokens()));
        }
        section.render()
    });
    format!(
        "{KEPT_OPEN}{}{KEPT_CLOSE}",
        marker
            .into_iter()
            .chain(bodies)
            .collect::<Vec<_>>()
            .join("\n\n")
    )
}

/// Rendering is independent of the session split and can be reused across model fallbacks.
#[derive(Clone)]
pub(crate) struct Transcript {
    messages: Vec<RenderedMessage>,
    kept: Vec<(Kind, Section)>,
    max_tool_chars: usize,
}

impl Transcript {
    /// Render borrowed selections without cloning the dropped messages into a temporary list.
    pub(crate) fn new<'a>(
        messages: impl IntoIterator<Item = &'a Message>,
        max_tool_chars: usize,
        prior: Option<&str>,
    ) -> Self {
        let mut rendered: Vec<_> = prior
            .filter(|s| !s.is_empty())
            .map(RenderedMessage::prior_summary)
            .into_iter()
            .collect();
        rendered.extend(
            messages
                .into_iter()
                .map(|message| RenderedMessage::from_message(message, max_tool_chars)),
        );
        mark_turn_replies(&mut rendered);
        Self {
            messages: rendered,
            kept: Vec::new(),
            max_tool_chars,
        }
    }

    /// The messages compaction keeps verbatim after the summary. The
    /// summarizer sees them as context, so it can attribute requests and
    /// avoid repeating what will be replayed anyway.
    pub(crate) fn with_kept<'a>(mut self, kept: impl IntoIterator<Item = &'a Message>) -> Self {
        self.kept = kept
            .into_iter()
            .flat_map(|message| kept_sections(message, self.max_tool_chars))
            .collect();
        self
    }

    /// Apply lossy stages only as needed. The source remains intact for a retry
    /// on a different model, which may have a larger context window.
    pub(crate) fn fit(&self, budget: usize) -> String {
        let mut fitting = FittingTranscript::new(self.clone(), budget);
        let mut rendered = fitting.render();
        if estimate_token_length(&rendered) <= budget {
            return rendered;
        }
        let mut cached_budget = budget;
        'fit: loop {
            fitting.cached_budget = cached_budget;
            let stages: [fn(&mut FittingTranscript); 5] = [
                FittingTranscript::shrink_tools,
                FittingTranscript::drop_middle,
                FittingTranscript::excerpt_older,
                FittingTranscript::excerpt_latest,
                FittingTranscript::omit_oldest_users,
            ];
            for stage in stages {
                stage(&mut fitting);
                rendered = fitting.render();
                let exact = estimate_token_length(&rendered);
                if exact <= budget {
                    return rendered;
                }
                if fitting.fits() && cached_budget > 0 {
                    // Per-message rounding usually overcounts, but the estimator's
                    // f32 accumulation can undercount too. Refit from the source
                    // with headroom rather than nesting excerpts or accepting overflow.
                    let correction = (exact - budget).max(cached_budget / 100 + 1);
                    cached_budget = cached_budget.saturating_sub(correction);
                    fitting = FittingTranscript::new(self.clone(), budget);
                    continue 'fit;
                }
            }
            return rendered;
        }
    }
}

#[cfg(test)]
pub fn render_transcript(messages: &[Message], max_tool_chars: usize) -> String {
    FittingTranscript::new(Transcript::new(messages, max_tool_chars, None), usize::MAX).render()
}

const TRANSCRIPT_OPEN: &str = "<transcript>\n";
const TRANSCRIPT_CLOSE: &str = "\n</transcript>\n\n";

struct FittingTranscript {
    transcript: Transcript,
    /// The rendered `<kept_after_summary>` block. It takes at most half the
    /// budget and stays fixed while the stages shrink the dropped messages.
    kept: String,
    /// Tokens of the transcript wrapper and the kept block.
    frame_tokens: usize,
    budget: usize,
    cached_budget: usize,
    tokens: usize,
    latest_user: Option<usize>,
    latest_reply: Option<usize>,
    omitted_messages: usize,
    omitted_users: usize,
}

impl FittingTranscript {
    fn new(transcript: Transcript, budget: usize) -> Self {
        let latest = |kind| transcript.messages.iter().rposition(|m| m.kind == kind);
        let kept = render_kept(&transcript.kept, budget / 2);
        Self {
            frame_tokens: estimate_token_length(&format!(
                "{TRANSCRIPT_OPEN}{TRANSCRIPT_CLOSE}{kept}"
            )),
            kept,
            budget,
            cached_budget: budget,
            tokens: transcript.messages.iter().map(|m| m.tokens).sum(),
            latest_user: latest(Kind::User),
            latest_reply: latest(Kind::Reply),
            transcript,
            omitted_messages: 0,
            omitted_users: 0,
        }
    }

    fn markers(&self) -> String {
        omission_markers(self.omitted_messages, self.omitted_users)
    }

    fn render(&self) -> String {
        #[cfg(test)]
        FULL_RENDER_COUNT.with(|count| count.set(count.get() + 1));
        let mut sections = Vec::new();
        let markers = self.markers();
        if !markers.is_empty() {
            // Counts cover the whole transcript, not only its first omitted gap.
            sections.push(markers);
        }
        sections.extend(
            self.transcript
                .messages
                .iter()
                .filter(|message| !message.omitted)
                .map(RenderedMessage::render),
        );
        format!(
            "{TRANSCRIPT_OPEN}{}{TRANSCRIPT_CLOSE}{}",
            sections.join("\n\n"),
            self.kept
        )
    }

    fn fits(&self) -> bool {
        // Reduction loops must not render the whole transcript on each mutation.
        // Transcript::fit checks the joined estimator at stage boundaries.
        self.tokens + estimate_token_length(&self.markers()) + self.frame_tokens
            <= self.cached_budget
    }

    fn recount(&mut self, index: usize) {
        let message = &mut self.transcript.messages[index];
        self.tokens -= message.tokens;
        message.recount();
        self.tokens += message.tokens;
    }

    fn protected(&self, index: usize) -> bool {
        self.latest_user == Some(index) || self.latest_reply == Some(index)
    }

    fn shrink_tools(&mut self) {
        for index in 0..self.transcript.messages.len() {
            if self.fits() {
                return;
            }
            self.shrink_message_tools(index);
        }
    }

    fn shrink_message_tools(&mut self, index: usize) {
        for part in 0..self.transcript.messages[index].sections.len() {
            if self.fits() {
                return;
            }
            self.shrink_tool_section(index, part);
        }
    }

    fn shrink_tool_section(&mut self, index: usize, part: usize) {
        if self.transcript.messages[index].sections[part].shrink_to_stub() {
            self.recount(index);
        }
    }

    fn omit(&mut self, index: usize) {
        let message = &mut self.transcript.messages[index];
        message.omitted = true;
        self.tokens -= message.tokens;
        if message.kind == Kind::User {
            self.omitted_users += 1;
        } else {
            self.omitted_messages += 1;
        }
    }

    fn drop_middle(&mut self) {
        for index in 0..self.transcript.messages.len() {
            if self.fits() {
                return;
            }
            let kind = self.transcript.messages[index].kind;
            if !self.protected(index) && matches!(kind, Kind::Assistant | Kind::Tool) {
                self.omit(index);
            }
        }
    }

    fn excerpt(&mut self, index: usize) {
        let fixed_tokens = estimate_token_length(&self.markers()) + self.frame_tokens;
        let message = &mut self.transcript.messages[index];
        let allowance = self
            .cached_budget
            .saturating_sub(self.tokens - message.tokens + fixed_tokens);
        // Tool rounds have been removed by stage (b); user/reply/context each
        // have one section, so their tags and omission counts remain intact.
        for section in &mut message.sections {
            let body_budget = allowance.saturating_sub(section.frame_tokens());
            section.body = excerpt_text(&section.body, body_budget);
        }
        self.recount(index);
    }

    fn excerpt_older(&mut self) {
        for index in 0..self.transcript.messages.len() {
            if self.fits() {
                return;
            }
            if !self.protected(index) && !self.transcript.messages[index].omitted {
                self.excerpt(index);
            }
        }
    }

    fn latest_pair_fits(&self) -> bool {
        let mut users = 0;
        let mut messages = 0;
        let mut kept = Vec::new();
        for (index, message) in self.transcript.messages.iter().enumerate() {
            if self.protected(index) {
                kept.push(message.render());
            } else if message.kind == Kind::User {
                users += 1;
            } else {
                messages += 1;
            }
        }
        let markers = omission_markers(messages, users);
        if !markers.is_empty() {
            kept.insert(0, markers);
        }
        // Only render the protected pair, not a clone of all the discarded history.
        estimate_token_length(&kept.join("\n\n")) + self.frame_tokens <= self.budget
    }

    fn excerpt_latest(&mut self) {
        if self.fits() {
            return;
        }
        // Don't clip a latest pair that fits by itself just to retain old excerpt
        // markers. The final fallback can collapse those older prompts instead.
        if self.latest_pair_fits() {
            return;
        }
        // Free older excerpts before spending the latest pair's allowance.
        self.omit_oldest_users();
        for index in self.largest_protected_first() {
            if self.fits() {
                return;
            }
            self.excerpt(index);
        }
    }

    fn largest_protected_first(&self) -> Vec<usize> {
        let mut indices: Vec<_> = [self.latest_user, self.latest_reply]
            .into_iter()
            .flatten()
            .collect();
        // A large reply must not force a small prompt down to its minimum excerpt.
        indices.sort_by_key(|&index| std::cmp::Reverse(self.transcript.messages[index].tokens));
        indices
    }

    fn omit_oldest_users(&mut self) {
        // Exhaust old reply/context excerpts before omitting users. Use this
        // fallback before clipping the latest pair, or when minimal excerpts don't fit.
        self.omit_oldest_matching(|kind| kind != Kind::User);
        self.omit_oldest_matching(|kind| kind == Kind::User);
    }

    fn can_omit(&self, index: usize) -> bool {
        !self.transcript.messages[index].omitted && !self.protected(index)
    }

    fn omit_oldest_matching(&mut self, matches: fn(Kind) -> bool) {
        for index in 0..self.transcript.messages.len() {
            if self.fits() {
                return;
            }
            if self.can_omit(index) && matches(self.transcript.messages[index].kind) {
                self.omit(index);
            }
        }
    }
}

fn omission_markers(messages: usize, users: usize) -> String {
    let mut markers = Vec::new();
    if messages > 0 {
        markers.push(format!("[{messages} earlier messages omitted]"));
    }
    if users > 0 {
        markers.push(format!("[{users} earlier user prompts omitted]"));
    }
    markers.join("\n")
}

/// Keep at least one character at each end, even when the marker itself cannot
/// fit. The caller warns for an irreducible request rather than returning an error.
fn excerpt_text(text: &str, budget: usize) -> String {
    let tokens = estimate_token_length(text);
    if tokens <= budget {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 2 {
        return text.to_owned();
    }
    let excerpt = |keep: usize| {
        let head: String = chars[..keep.div_ceil(2)].iter().collect();
        let tail: String = chars[chars.len() - keep / 2..].iter().collect();
        format!("{head}[… {} chars omitted …]{tail}", chars.len() - keep)
    };
    let mut best = excerpt(2);
    if estimate_token_length(&best) >= tokens {
        return text.to_owned();
    }
    if budget == 0 {
        return best;
    }
    let (mut low, mut high) = (2, chars.len() - 1);
    while low <= high {
        let keep = low + (high - low) / 2;
        let candidate = excerpt(keep);
        if estimate_token_length(&candidate) <= budget {
            best = candidate;
            low = keep + 1;
        } else {
            high = keep - 1;
        }
    }
    best
}
