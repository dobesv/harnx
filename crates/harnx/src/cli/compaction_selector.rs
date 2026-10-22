use anyhow::{bail, Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_core::transcript_view::{before_compaction, default_view, TranscriptView};
use std::num::NonZeroU64;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionSelector {
    Sequence(NonZeroU64),
    Back(NonZeroU64),
}

impl FromStr for CompactionSelector {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split_whitespace().collect();
        match parts.as_slice() {
            [seq] => Ok(Self::Sequence(positive_number(seq)?)),
            [count, "back"] => Ok(Self::Back(positive_number(count)?)),
            _ => bail!("expected a compaction sequence or quoted \"N back\" (e.g. \"1 back\")"),
        }
    }
}

impl CompactionSelector {
    pub fn view(self, entries: &[(u64, SessionLogEntry)]) -> Result<TranscriptView> {
        match self {
            Self::Sequence(seq) => before_compaction(entries, seq.get(), None),
            Self::Back(count) => view_back(entries, count.get()),
        }
    }
}

fn positive_number(value: &str) -> Result<NonZeroU64> {
    value
        .parse()
        .context("expected a positive compaction sequence or quoted \"N back\" (e.g. \"1 back\")")
}

fn view_back(entries: &[(u64, SessionLogEntry)], count: u64) -> Result<TranscriptView> {
    let mut view = default_view(entries)?;
    for depth in 0..count {
        let Some(marker) = view.compaction else {
            bail!(
                "cannot go {count} compactions back: only {depth} available from the current view"
            );
        };
        // Follow the historical chain, not raw Compress entries from rewound branches.
        view = before_compaction(entries, marker.compaction_seq, None)?;
    }
    Ok(view)
}

#[cfg(test)]
mod tests;
