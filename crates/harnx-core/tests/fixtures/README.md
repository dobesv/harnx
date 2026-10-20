# Shared compaction transcript

`compaction-history.json` is the single fixture for transcript parity tests in
Rust and web. It's already the JSON export: an array of `[physical_seq, entry]`
pairs using `SessionLogEntry`'s serde format. Import this file directly rather
than maintaining a second web copy.

| Sequences | Meaning |
| --- | --- |
| 1–4 | First prompt/reply, turn receipt, manual compaction request |
| 5–6 | First `Compress`, then receipt; nothing kept |
| 7–14 | Latest prompt, middle tool round, tail tool round, reply, receipts |
| 15–20 | Second `Compress`, re-logged prompt + tail + reply, receipt |
| 21–23 | Later prompt/reply and turn receipt |

Second compaction keeps three model messages (four log entries): original
sequences 7, 10–12 become 16–19. The middle round at 8–9 is not kept. This is
C2's `KeepPlan { prompt_idx: Some(0), tail_from: 2 }` over the four model messages
in the second window. The fixture records the log shape, not a token-budget test.

Expected message/tool-entry sequences, with markers in parentheses:

- Default: `(15), 16, 17, 18, 19, 21, 22`.
- Before 15: `(5), 7, 8, 9, 10, 11, 12`.
- Before 5: `1, 2`.

`harnx_core::transcript_view` returns control entries too; adapters decide how to
render them. `TurnEnd` is removed by canonical mutation resolution. The marker
at 15 has archive bounds 6–14 and earlier marker 5. The marker at 5 has bounds
1–4 and no earlier marker. These bounds describe archived windows, including
originals later re-logged, not only the messages passed to the summarizer.
`CompactionMarker::archive_range()` returns these bounds, or `None` for an empty
archive.

Both stored summaries include the real runtime recovery-note suffix.
`CompactionMarker.summary` strips only that suffix for display; the raw text
remains in each source `Compress.prompt`. Its recovery note counts summarized
model messages, so the second note covers one message at 8–8 (the tool round),
not both physical entries 8–9 or the whole archive.

`kept_count` is optional metadata, never a row-selection rule. The adjacent
receipt proves `Some(0)` at 5. At 15 it remains `None`: nonempty spans don't
carry a durable kept count, even though the fixture's construction is known.

## Rust

From another crate's `tests/it/` module:

```rust
let entries: Vec<(u64, harnx_core::session::SessionLogEntry)> =
    serde_json::from_str(include_str!(
        "../../../harnx-core/tests/fixtures/compaction-history.json"
    ))?;
let view = harnx_core::transcript_view::default_view(&entries)?;
let earlier = harnx_core::transcript_view::before_compaction(&entries, 15, None)?;
let page = harnx_core::transcript_view::before_compaction(&entries, 15, Some(7..=12))?;
```

## Web

From a test under `web/src/__tests__/`:

```typescript
import entries from '../../../crates/harnx-core/tests/fixtures/compaction-history.json';
```

Backend view types are serializable as `{ compaction, entries }`. D2–D4 tests
can use this raw fixture to construct their adapter-specific snapshots. No UI
snapshot format or HTTP API is introduced by D1.
