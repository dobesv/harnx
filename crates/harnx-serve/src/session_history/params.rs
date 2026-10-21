use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryParams {
    pub before_compaction: Option<u64>,
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum Selector {
    Before(u64),
    Range { from: u64, to: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Window {
    pub version: u8,
    pub scope: (String, String, String),
    pub created_at: String,
    pub as_of_seq: u64,
    pub selector: Selector,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub window: Window,
    pub offset: usize,
    pub limit: usize,
}

pub(super) enum Request {
    Initial { selector: Selector, limit: usize },
    Continue(Cursor),
}

impl HistoryParams {
    pub(super) fn parse(self) -> Result<Request> {
        if let Some(value) = self.cursor.as_deref() {
            return self.continue_request(value);
        }
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        validate_limit(limit)?;
        Ok(Request::Initial {
            selector: self.selector()?,
            limit,
        })
    }

    fn continue_request(&self, value: &str) -> Result<Request> {
        ensure!(
            self.before_compaction
                .or(self.from_seq)
                .or(self.to_seq)
                .is_none(),
            "cursor cannot be combined with sequence selectors"
        );
        let cursor = Cursor::decode(value)?;
        ensure!(
            self.limit.is_none_or(|limit| limit == cursor.limit),
            "cursor limit cannot change"
        );
        Ok(Request::Continue(cursor))
    }

    fn selector(&self) -> Result<Selector> {
        match (self.before_compaction, self.from_seq, self.to_seq) {
            (Some(seq), None, None) if seq > 0 => Ok(Selector::Before(seq)),
            (None, Some(from), Some(to)) if from > 0 && from <= to => {
                Ok(Selector::Range { from, to })
            }
            _ => bail!("provide either before_compaction > 0 or 0 < from_seq <= to_seq"),
        }
    }
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("history cursor serializes"))
    }

    fn decode(value: &str) -> Result<Self> {
        ensure!(value.len() <= 4096, "history cursor is too long");
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .context("invalid history cursor encoding")?;
        let cursor: Self = serde_json::from_slice(&bytes).context("invalid history cursor")?;
        ensure!(
            cursor.window.version == 1,
            "unsupported history cursor version"
        );
        validate_limit(cursor.limit)?;
        ensure!(
            cursor.offset.is_multiple_of(cursor.limit),
            "invalid history cursor offset"
        );
        Ok(cursor)
    }
}

fn validate_limit(limit: usize) -> Result<()> {
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_LIMIT}"
    );
    Ok(())
}
