//! Incremental counterpart of `harnx_core::text::split_think_blocks`.
//!
//! Runs before the CLI's partial-line writes, so even a tag split across model
//! chunks never reaches stdout. Only a possible tag suffix and one thought LF
//! are held back; the rest streams immediately. No nesting or code-fence rules.

use harnx_core::text::THINK_TAG_PAIRS;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Text(String),
    Thought(String),
    /// Separate adjacent blocks even when there is no text between them.
    ThoughtEnd,
}

#[derive(Debug, Default)]
pub(crate) struct StreamingThinkSegmenter {
    pending: String,
    closer: Option<&'static str>,
    trim_leading_newline: bool,
}

impl StreamingThinkSegmenter {
    pub(crate) fn segment(&mut self, input: &str) -> Vec<Segment> {
        self.pending.push_str(input);
        let mut segments = Vec::new();
        while !self.pending.is_empty() {
            self.trim_opening_newline();
            if let Some(tag) = self.next_tag() {
                self.consume_tag(tag, &mut segments);
            } else {
                let mut end = self.pending.len() - self.tag_suffix_len();
                // This LF may be the single newline immediately before a closer.
                if self.closer.is_some() && self.pending[..end].ends_with('\n') {
                    end -= 1;
                }
                let body = self.pending.drain(..end).collect();
                self.emit(body, &mut segments);
                break;
            }
        }
        segments
    }

    fn consume_tag(
        &mut self,
        (pos, open, close): (usize, &'static str, &'static str),
        segments: &mut Vec<Segment>,
    ) {
        let mut body: String = self.pending.drain(..pos).collect();
        if self.closer.is_some() {
            if body.ends_with('\n') {
                body.pop();
            }
            self.emit(body, segments);
            self.pending.drain(..close.len());
            self.closer = None;
            segments.push(Segment::ThoughtEnd);
        } else {
            self.emit(body, segments);
            self.pending.drain(..open.len());
            self.closer = Some(close);
            self.trim_leading_newline = true;
        }
    }

    /// Release unresolved prefixes as literal content in their current style.
    /// An unclosed thought keeps its trailing LF, matching the full-text splitter.
    pub(crate) fn flush(&mut self) -> Vec<Segment> {
        let mut segments = Vec::new();
        self.trim_opening_newline();
        let body = std::mem::take(&mut self.pending);
        self.emit(body, &mut segments);
        if self.closer.take().is_some() {
            segments.push(Segment::ThoughtEnd);
        }
        self.trim_leading_newline = false;
        segments
    }

    fn trim_opening_newline(&mut self) {
        if self.trim_leading_newline && !self.pending.is_empty() {
            if self.pending.starts_with('\n') {
                self.pending.remove(0);
            }
            self.trim_leading_newline = false;
        }
    }

    fn next_tag(&self) -> Option<(usize, &'static str, &'static str)> {
        match self.closer {
            Some(close) => self.pending.find(close).map(|pos| (pos, "", close)),
            None => THINK_TAG_PAIRS
                .iter()
                .filter_map(|&(open, close)| self.pending.find(open).map(|pos| (pos, open, close)))
                .min_by_key(|&(pos, _, _)| pos),
        }
    }

    fn tag_suffix_len(&self) -> usize {
        let tags: Vec<_> = match self.closer {
            Some(close) => vec![close],
            None => THINK_TAG_PAIRS.iter().map(|&(open, _)| open).collect(),
        };
        tags.into_iter()
            .flat_map(|tag| (1..tag.len()).map(move |len| &tag[..len]))
            .filter(|prefix| self.pending.ends_with(prefix))
            .map(str::len)
            .max()
            .unwrap_or(0)
    }

    fn emit(&self, body: String, segments: &mut Vec<Segment>) {
        if !body.is_empty() {
            segments.push(if self.closer.is_some() {
                Segment::Thought(body)
            } else {
                Segment::Text(body)
            });
        }
    }
}

#[cfg(test)]
#[path = "streaming_think_segmenter_tests.rs"]
mod tests;
