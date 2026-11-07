//! Bounded physical reads over the durable JetStream session log (plan #1550
//! Task 4): one-page scans with record and byte caps, scan progress that
//! survives purged ranges, and single-entry lookup for continuation reads.
//! Every read here is pure — nothing creates, appends or repairs.

use super::{deserialize_entry, NatsSessionLog, READ_TIMEOUT};
use anyhow::{Context, Result};
use async_nats::jetstream::{self, stream::LastRawMessageErrorKind};
use bytes::Bytes;
use harnx_core::session::SessionLogEntry;
use tokio::time::timeout;

/// A single page reads at most this many records.
pub const SCAN_RECORD_LIMIT: usize = 1_000;

/// A single page reads at most this many raw payload bytes. A single entry
/// can still exceed the remaining budget (it is bounded by the broker's own
/// max payload), so the cap stops the scan *after* including the entry that
/// crosses the limit.
pub const SCAN_BYTE_LIMIT: usize = 8 * 1024 * 1024;

/// One physically bounded page of durable log entries, ascending by durable
/// sequence. Returned by [`NatsSessionLog::load_window_async`]; the sequence
/// is the source index every bounded-read continuation refers to.
#[derive(Debug, Default)]
pub struct LogWindow {
    /// Entries in ascending durable-sequence order, source index retained.
    pub entries: Vec<(u64, SessionLogEntry)>,
    /// Highest durable sequence this scan examined (included entry or
    /// confirmed gap), when the stream had any message in the requested
    /// range.
    pub scanned_through: Option<u64>,
    /// True when the scan stopped on a cap while the requested range extends
    /// beyond the scanned position, so the next page resumes at
    /// `scanned_through + 1` without gaps or duplicates.
    pub capped: bool,
    /// Raw payload bytes counted towards [`SCAN_BYTE_LIMIT`] so far.
    scanned_bytes: usize,
}

/// One durable entry the scan accepted, with its raw payload size for
/// byte-cap accounting.
struct ScannedEntry {
    sequence: u64,
    payload_len: usize,
    entry: SessionLogEntry,
}

impl LogWindow {
    /// Record a scanned entry. `capped` flips on when the byte cap is crossed
    /// while the requested range continues past this sequence.
    fn push(&mut self, scanned: ScannedEntry, range_end: u64) {
        self.scanned_bytes += scanned.payload_len;
        let sequence = scanned.sequence;
        self.entries.push((scanned.sequence, scanned.entry));
        self.scanned_through = Some(sequence);
        self.capped = self.scanned_bytes >= SCAN_BYTE_LIMIT && sequence < range_end;
    }
}

impl NatsSessionLog {
    /// Read one physically bounded page of durable entries with sequence in
    /// `[index_min, index_max]` (inclusive; `None` = unbounded). Never loads
    /// the whole log: the scan stops after [`SCAN_RECORD_LIMIT`] records or
    /// [`SCAN_BYTE_LIMIT`] raw payload bytes, whichever comes first, and the
    /// returned [`LogWindow`] says whether the log continues beyond the
    /// scanned position. Pure read; no activation or replay side effects.
    pub async fn load_window_async(
        &self,
        index_min: Option<u64>,
        index_max: Option<u64>,
    ) -> Result<LogWindow> {
        let Some(mut stream) = self.open_stream_for_read().await? else {
            return Ok(LogWindow::default());
        };
        match self.scan_bounds(&mut stream, index_min, index_max).await? {
            Some(range) => self.scan_window(&stream, range).await,
            None => Ok(LogWindow::default()),
        }
    }

    /// Fetch one durable entry by its exact stream sequence (the bounded
    /// read's continuation selector). `None` when the sequence is absent:
    /// missing stream, purged range or a gap. Pure read.
    pub async fn load_entry_async(&self, index: u64) -> Result<Option<SessionLogEntry>> {
        let Some(stream) = self.open_stream_for_read().await? else {
            return Ok(None);
        };
        match fetch_scan_entry(&stream, index, &self.session_id).await? {
            ScanFetch::Missing => Ok(None),
            ScanFetch::Entry { sequence, payload } => {
                window_entry(&payload, sequence, &self.session_id).map(Some)
            }
        }
    }

    /// The stream's actual bounds intersected with the requested inclusive
    /// range. `None` when the stream holds no message in the range.
    async fn scan_bounds(
        &self,
        stream: &mut jetstream::stream::Stream,
        index_min: Option<u64>,
        index_max: Option<u64>,
    ) -> Result<Option<ScanRange>> {
        let info = stream.info().await.with_context(|| {
            format!(
                "Failed to inspect JetStream log stream '{}' for session '{}'",
                self.stream_name, self.session_id
            )
        })?;
        if info.state.messages == 0 {
            return Ok(None);
        }
        let start = index_min.unwrap_or(1).max(info.state.first_sequence).max(1);
        let end = index_max.unwrap_or(u64::MAX).min(info.state.last_sequence);
        Ok((end >= start).then_some(ScanRange { start, end }))
    }

    /// Walk the inclusive range under the scan caps. Purged sequences are
    /// confirmed gaps, not errors, and still advance scan progress.
    async fn scan_window(
        &self,
        stream: &jetstream::stream::Stream,
        range: ScanRange,
    ) -> Result<LogWindow> {
        let mut window = LogWindow::default();
        for seq in range.start..=range.end {
            if window.entries.len() >= SCAN_RECORD_LIMIT {
                window.capped = true;
                break;
            }
            match fetch_scan_entry(stream, seq, &self.session_id).await? {
                ScanFetch::Missing => window.scanned_through = Some(seq),
                ScanFetch::Entry { sequence, payload } => {
                    let entry = window_entry(&payload, sequence, &self.session_id)?;
                    window.push(
                        ScannedEntry {
                            sequence,
                            payload_len: payload.len(),
                            entry,
                        },
                        range.end,
                    );
                    if window.capped {
                        break;
                    }
                }
            }
        }
        Ok(window)
    }
}

/// Inclusive durable-sequence range a scan should walk.
struct ScanRange {
    start: u64,
    end: u64,
}

/// Outcome of looking up one durable sequence during a scan.
enum ScanFetch {
    Entry {
        sequence: u64,
        payload: Bytes,
    },
    /// The sequence was removed (retention/limits), leaving a gap.
    Missing,
}

async fn fetch_scan_entry(
    stream: &jetstream::stream::Stream,
    seq: u64,
    session_id: &str,
) -> Result<ScanFetch> {
    match timeout(READ_TIMEOUT, stream.get_raw_message(seq)).await {
        Ok(Ok(raw)) => Ok(ScanFetch::Entry {
            sequence: raw.sequence,
            payload: raw.payload,
        }),
        Ok(Err(error)) if error.kind() == LastRawMessageErrorKind::NoMessageFound => {
            Ok(ScanFetch::Missing)
        }
        Ok(Err(error)) => Err(error).with_context(|| {
            format!(
                "Failed to read JetStream session log for session '{session_id}' at stream sequence {seq}"
            )
        }),
        Err(_) => Err(anyhow::anyhow!(
            "Timed out after {READ_TIMEOUT:?} reading JetStream session log for session \
             '{session_id}' at stream sequence {seq}"
        )),
    }
}

fn window_entry(payload: &[u8], sequence: u64, session_id: &str) -> Result<SessionLogEntry> {
    deserialize_entry(payload).with_context(|| {
        format!(
            "Failed to deserialize session log entry for session '{session_id}' \
             at stream sequence {sequence}"
        )
    })
}
