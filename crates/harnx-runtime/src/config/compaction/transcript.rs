//! Sanitized transcript pieces, kept separate until the summarizer budget is known.

use super::{is_turn_prompt, render_tool_output};
use fancy_regex::Regex;
use harnx_core::message::{Message, MessageContent, MessageContentPart, MessageRole};
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
    heading: String,
    body: String,
    stub: Option<String>,
}

impl Section {
    fn new(heading: impl Into<String>, body: &str) -> Self {
        Self::from_sanitized(heading, strip_data_urls(body).into_owned())
    }

    fn from_sanitized(heading: impl Into<String>, body: String) -> Self {
        Self {
            heading: heading.into(),
            body,
            stub: None,
        }
    }

    fn render(&self) -> String {
        format!("{}{}", self.heading, self.body)
    }

    fn shrink_to_stub(&mut self) -> bool {
        let Some(stub) = self.stub.take() else {
            return false;
        };
        if estimate_token_length(&stub) >= estimate_token_length(&self.render()) {
            return false;
        }
        self.heading.clear();
        self.body = stub;
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
        let (kind, label) = message_kind_and_label(message);
        let body = render_content(&message.content);
        let mut section = Section::new(format!("── {label} ──\n"), &body);
        if kind == Kind::Tool {
            section.stub = Some("[tool: completed; output omitted]".into());
        }
        Self::new(kind, vec![section])
    }

    fn prior_summary(summary: &str) -> Self {
        let heading = format!(
            "{}Earlier compaction summary:\n",
            crate::config::session::RUNTIME_NOTE_PREFIX
        );
        Self::new(
            Kind::Context,
            vec![Section::new(heading, &format!("{summary}\n\n---"))],
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
        self.sections
            .iter()
            .map(Section::render)
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn recount(&mut self) {
        self.tokens = estimate_token_length(&self.render());
    }
}

fn message_kind_and_label(message: &Message) -> (Kind, &'static str) {
    match message.role {
        MessageRole::System => (Kind::Context, "system"),
        MessageRole::User if is_turn_prompt(message) => (Kind::User, "user"),
        MessageRole::User => (Kind::Context, "user"),
        MessageRole::Assistant => (Kind::Assistant, "assistant"),
        MessageRole::Tool => (Kind::Tool, "tool result"),
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

fn tool_sections(calls: &harnx_core::message::MessageContentToolCalls, cap: usize) -> Vec<Section> {
    let mut sections = Vec::new();
    if !calls.text.is_empty() {
        sections.push(Section::new("── assistant ──\n", &calls.text));
    }
    for result in &calls.tool_results {
        let name = strip_data_urls(&result.call.name).replace(['\r', '\n'], " ");
        let args = serde_json::to_string(&result.call.arguments).unwrap_or_default();
        sections.push(Section::new(format!("── tool call: {name} ──\n"), &args));
        let output = ToolOutput {
            id: result.call.id.clone(),
            name: result.call.name.clone(),
            output: result.output.clone(),
            markdown: result.markdown.clone(),
            content: result.content.clone(),
            switch_agent: result.switch_agent.clone(),
        };
        let failed = ["isError", "is_error"]
            .iter()
            .any(|key| result.output.get(key).and_then(|v| v.as_bool()) == Some(true));
        let status = if failed { "error" } else { "completed" };
        // The three-tier renderer already sanitizes before applying its cap.
        let mut section =
            Section::from_sanitized("── tool result ──\n", render_tool_output(&output, cap));
        section.stub = Some(format!("[{name}: {status}; output omitted]"));
        sections.push(section);
    }
    sections
}

/// Rendering is independent of the session split and can be reused across model fallbacks.
#[derive(Clone)]
pub(crate) struct Transcript {
    messages: Vec<RenderedMessage>,
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
        Self { messages: rendered }
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

struct FittingTranscript {
    transcript: Transcript,
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
        Self {
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
        sections.join("\n\n")
    }

    fn fits(&self) -> bool {
        // Reduction loops must not render the whole transcript on each mutation.
        // Transcript::fit checks the joined estimator at stage boundaries.
        self.tokens + estimate_token_length(&self.markers()) <= self.cached_budget
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
        let marker_tokens = estimate_token_length(&self.markers());
        let message = &mut self.transcript.messages[index];
        let allowance = self
            .cached_budget
            .saturating_sub(self.tokens - message.tokens + marker_tokens);
        // Tool rounds have been removed by stage (b); user/reply/context each
        // have one section, so labels and omission counts remain intact.
        for section in &mut message.sections {
            let body_budget = allowance.saturating_sub(estimate_token_length(&section.heading));
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
        estimate_token_length(&kept.join("\n\n")) <= self.budget
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
        for index in 0..self.transcript.messages.len() {
            if self.fits() {
                return;
            }
            if self.protected(index) {
                self.excerpt(index);
            }
        }
    }

    fn omit_oldest_users(&mut self) {
        // User omissions are the final fallback. Exhaust old reply/context
        // excerpts first, so their markers don't evict user prompts.
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
