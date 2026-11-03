//! Bound a `ToolResults` session-log entry to the broker's payload ceiling.
//!
//! The tool server bounds each terminal reply by its JSON encoding, but the
//! worker stores the whole round as one YAML entry: block-scalar indentation
//! and the sum of many parallel results can exceed the broker's `max_payload`
//! even when every reply passed its own check (#2555). Before appending, the
//! serialized entry is measured; when it does not fit, the biggest outputs
//! are degraded first into the same recoverable size error the tool server
//! produces, with the original retained as a `cid:media:` attachment.

use anyhow::Result;
use async_nats::jetstream;
use harnx_core::cid_url::SessionRef;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_toolset_server::reply_limits::{
    full_size_error_message, minimal_size_error_message, short_size_error_message,
    SessionAttachments,
};

mod passes;

/// Reserved for the wire around the measured payload: the publish subject,
/// JetStream headers (`Nats-Msg-Id`, expected-tail) and protocol framing.
/// The payload itself is measured exactly, so this only covers the envelope.
pub(crate) const ENVELOPE_HEADROOM: usize = 1024;

/// Whether `entry` still fits the connected broker's payload ceiling.
/// Non-`ToolResults` entries are reported as fitting: they have no
/// degradable form, so their rejection stays between the broker and the
/// append that produced it.
pub(crate) fn entry_fits(js: &jetstream::Context, entry: &SessionLogEntry) -> Result<bool> {
    if !matches!(entry, SessionLogEntry::ToolResults { .. }) {
        return Ok(true);
    }
    fits_budget(payload_budget(js), entry)
}

fn payload_budget(js: &jetstream::Context) -> usize {
    js.client()
        .server_info()
        .max_payload
        .saturating_sub(ENVELOPE_HEADROOM)
}

fn fits_budget(budget: usize, entry: &SessionLogEntry) -> Result<bool> {
    Ok(serde_yaml::to_string(entry)?.len() <= budget)
}

/// Degrade oversized `ToolResults` outputs until the serialized entry fits.
pub(crate) struct ToolResultsBudget<'a> {
    retention: Option<SessionAttachments<'a>>,
    budget: usize,
}

impl<'a> ToolResultsBudget<'a> {
    pub(crate) fn new(
        js: &'a jetstream::Context,
        owner: Option<SessionRef>,
        replicas: usize,
    ) -> Self {
        Self {
            retention: owner.map(|owner| SessionAttachments::for_owner(Some(owner), js, replicas)),
            budget: payload_budget(js),
        }
    }

    #[cfg(test)]
    fn without_retention(budget: usize) -> Self {
        Self {
            retention: None,
            budget,
        }
    }

    /// Degrade the biggest outputs first until the entry fits. A fitting
    /// entry, and any entry that is not a `ToolResults` round, is returned
    /// unchanged; a non-`ToolResults` rejection keeps its typed broker error.
    pub(crate) async fn bound(&self, entry: &SessionLogEntry) -> Result<SessionLogEntry> {
        let SessionLogEntry::ToolResults { results, timestamp } = entry else {
            return Ok(entry.clone());
        };
        let total = serialized_len(entry)?;
        if total <= self.budget {
            return Ok(entry.clone());
        }
        let mut passes = passes::BudgetPasses::new(results, total)?;
        passes.retain_full_messages(self).await?;
        // Full errors can grow small outputs; every candidate may still try
        // shorter forms. Each pass keeps only strictly smaller replacements.
        passes.apply_fallback(self.budget, |url| url.map(short_size_error_message))?;
        // Unretained outputs have no short reference form, so their minimal
        // message is produced only by this last pass.
        passes.apply_fallback(self.budget, |url| {
            Some(minimal_size_error_message(url.is_some()))
        })?;
        passes.finish(*timestamp)
    }

    /// Replace one output with its recoverable size error, retaining the
    /// original as a session attachment when a trusted owner is attached.
    async fn degrade(
        &self,
        output: &ToolOutput,
        retained: &mut Option<String>,
    ) -> Result<ToolOutput> {
        let original_bytes = serde_json::to_vec(output)?;
        *retained = match &self.retention {
            Some(attachments) => match attachments.retain(&original_bytes).await {
                Ok(url) => url,
                Err(error) => {
                    // The append must still close the round: failing it
                    // here would wedge the session exactly as before
                    // #2555, just one step later.
                    log::warn!("tool result not retained while bounding entry: {error:#}");
                    None
                }
            },
            None => None,
        };
        Ok(size_error_output(
            output,
            full_size_error_message(original_bytes.len(), retained.as_deref()),
        ))
    }
}

/// The same recoverable shape the engine builds for a failed tool call, so a
/// degraded result is indistinguishable from one the tool server bounded.
fn size_error_output(original: &ToolOutput, message: String) -> ToolOutput {
    ToolOutput {
        id: original.id.clone(),
        name: original.name.clone(),
        output: serde_json::json!({"is_error": true, "error": message}),
        markdown: None,
        content: Vec::new(),
        switch_agent: original.switch_agent.clone(),
    }
}

fn serialized_len<T: serde::Serialize>(value: &T) -> Result<usize> {
    Ok(serde_yaml::to_string(value)?.len())
}

// Measure at the same sequence nesting as the durable entry so block-scalar
// indentation counts too. The fixed wrapper cancels in size differences.
fn serialized_output_len(output: &ToolOutput) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct ResultRow<'a> {
        results: &'a [ToolOutput],
    }
    serialized_len(&ResultRow {
        results: std::slice::from_ref(output),
    })
}

fn replace_if_smaller(
    output: &mut ToolOutput,
    replacement: ToolOutput,
    size: &mut usize,
    total: &mut usize,
) -> Result<bool> {
    let replacement_size = serialized_output_len(&replacement)?;
    if replacement_size >= *size {
        return Ok(false);
    }
    *total -= *size - replacement_size;
    *size = replacement_size;
    *output = replacement;
    Ok(true)
}

/// Lower bound for a full size-error replacement, before retention. The real
/// byte count and any attachment URL are checked after building the message.
fn degraded_len(output: &ToolOutput) -> Result<usize> {
    serialized_output_len(&size_error_output(output, full_size_error_message(0, None)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::session::SessionLogEntry;
    use serde_json::json;

    fn output(id: &str, value: serde_json::Value) -> ToolOutput {
        ToolOutput {
            id: Some(id.to_string()),
            name: "echo".to_string(),
            output: value,
            markdown: Some("stale markdown".to_string()),
            content: Vec::new(),
            switch_agent: None,
        }
    }

    fn tool_results(results: Vec<ToolOutput>) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results,
            timestamp: None,
        }
    }

    fn small(id: &str) -> ToolOutput {
        output(id, json!({"ok": true}))
    }

    fn big(id: &str, bytes: usize) -> ToolOutput {
        output(id, json!({"blob": "x".repeat(bytes)}))
    }

    async fn bound(budget: usize, entry: &SessionLogEntry) -> SessionLogEntry {
        ToolResultsBudget::without_retention(budget)
            .bound(entry)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fitting_entry_and_other_entry_types_pass_through() {
        let entry = tool_results(vec![small("a")]);
        assert_eq!(bound(1 << 20, &entry).await, entry);
        // An oversized non-ToolResults entry is not degradable; it must reach
        // the broker unchanged so the rejection stays typed.
        let message = SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("x".repeat(4096)),
            timestamp: None,
            fence_token: None,
        };
        assert_eq!(bound(16, &message).await, message);
    }

    #[tokio::test]
    async fn biggest_output_degrades_and_smaller_siblings_survive() {
        let entry = tool_results(vec![small("small"), big("big", 4096)]);
        let budget = serialized_len(&entry).unwrap() - 2048;
        let bounded = bound(budget, &entry).await;
        let SessionLogEntry::ToolResults { results, .. } = &bounded else {
            unreachable!()
        };
        assert_eq!(results[0], small("small"), "small result stays intact");
        let error = results[1].output.clone();
        assert_eq!(error["is_error"], true);
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("exceeded storage/transport size limit"),
            "{error}"
        );
        // The degraded result carries no bulk of its own.
        assert_eq!(results[1].markdown, None);
        assert!(results[1].content.is_empty());
        assert!(fits_budget(budget, &bounded).unwrap());
    }

    #[tokio::test]
    async fn degraded_output_keeps_call_identity_and_switch() {
        let mut original = big("call-1", 4096);
        original.switch_agent = Some(harnx_core::tool::SwitchAgentData {
            agent: "worker".to_string(),
            prompt: "hand off".to_string(),
            session_id: None,
            tool_call_id: None,
        });
        let degraded = size_error_output(&original, "too large".to_string());
        assert_eq!(degraded.id, original.id);
        assert_eq!(degraded.name, original.name);
        assert_eq!(degraded.switch_agent, original.switch_agent);
    }

    #[tokio::test]
    async fn sum_of_fitting_results_degrades_only_what_is_needed() {
        // Each result fits alone; only together do they exceed the budget.
        let entry = tool_results(vec![big("a", 3000), big("b", 1000), big("c", 1000)]);
        let budget = serialized_len(&entry).unwrap() - 1500;
        let bounded = bound(budget, &entry).await;
        let SessionLogEntry::ToolResults { results, .. } = &bounded else {
            unreachable!()
        };
        assert_eq!(results[1], entry_results(&entry)[1]);
        assert_eq!(results[2], entry_results(&entry)[2]);
        assert_eq!(results[0].output["is_error"], true);
        assert!(fits_budget(budget, &bounded).unwrap());
    }

    #[tokio::test]
    async fn small_outputs_can_skip_full_messages_and_fit_with_minimal_fallbacks() {
        let original = (0..512)
            .map(|bytes| {
                let mut output = big("a", bytes);
                output.name = "echo-output".into();
                output
            })
            .find(|output| serialized_output_len(output).unwrap() == 205)
            .expect("205-byte output fixture");
        let entry = tool_results(vec![original.clone(); 100]);
        let minimal = tool_results(vec![
            size_error_output(
                &original,
                minimal_size_error_message(false)
            );
            100
        ]);
        assert_eq!(serialized_len(&entry).unwrap(), 19_628);
        assert_eq!(serialized_len(&minimal).unwrap(), 17_428);
        assert!(degraded_len(&original).unwrap() > serialized_output_len(&original).unwrap());
        let budget = 18_128;
        let bounded = bound(budget, &entry).await;
        assert!(
            fits_budget(budget, &bounded).unwrap(),
            "shorter messages must rescue the round even when full messages would grow it"
        );
        assert!(
            entry_results(&bounded)
                .iter()
                .any(|output| output == &original),
            "stop degrading once the round fits"
        );
    }

    fn entry_results(entry: &SessionLogEntry) -> &[ToolOutput] {
        let SessionLogEntry::ToolResults { results, .. } = entry else {
            unreachable!()
        };
        results
    }

    #[tokio::test]
    async fn tiny_outputs_are_never_grown_by_degradation() {
        // Many small results overflow only by count; a size error is bigger
        // than the result it replaces, so nothing may be degraded.
        let entry = tool_results((0..200).map(|i| small(&format!("r{i}"))).collect());
        let budget = serialized_len(&entry).unwrap() / 2;
        let bounded = bound(budget, &entry).await;
        assert_eq!(bounded, entry, "degradation cannot shrink any result");
    }

    #[tokio::test]
    async fn unretained_fallbacks_kick_in_when_the_message_itself_is_too_big() {
        // Without a trusted owner there is no cid:media reference, so the
        // fallback chain ends at the minimal unretained message. Pick a
        // budget that the full size error exceeds but the minimal one fits.
        let original = big("big", 4096);
        let full = tool_results(vec![size_error_output(
            &original,
            full_size_error_message(0, None),
        )]);
        let minimal = tool_results(vec![size_error_output(
            &original,
            minimal_size_error_message(false),
        )]);
        let budget = serialized_len(&minimal).unwrap() + 8;
        assert!(budget < serialized_len(&full).unwrap());

        let bounded = bound(budget, &tool_results(vec![original])).await;
        let SessionLogEntry::ToolResults { results, .. } = &bounded else {
            unreachable!()
        };
        assert_eq!(
            results[0].output,
            json!({"is_error": true, "error": minimal_size_error_message(false)}),
            "the minimal message must replace the full one"
        );
        assert!(
            fits_budget(budget, &bounded).unwrap(),
            "the minimal message must fit the budget"
        );
    }

    #[tokio::test]
    async fn last_resort_without_retention_stays_honest_even_when_nothing_fits() {
        // No retained URL means pass 2 cannot help; only pass 3 writes the
        // minimal message, so changing its retention flag must fail here.
        let original = big("big", 4096);
        let bounded = bound(0, &tool_results(vec![original.clone(), small("small")])).await;
        let results = entry_results(&bounded);
        assert_eq!(
            results[0],
            size_error_output(&original, minimal_size_error_message(false))
        );
        assert_eq!(results[1], small("small"));
    }

    #[tokio::test]
    async fn real_byte_count_must_not_grow_a_boundary_output() {
        // The zero-byte lower bound omits digits present in the real message.
        let original = (0..512)
            .map(|len| big("boundary", len))
            .find(|output| {
                let size = serialized_output_len(output).unwrap();
                let actual = size_error_output(
                    output,
                    full_size_error_message(serde_json::to_vec(output).unwrap().len(), None),
                );
                degraded_len(output).unwrap() < size
                    && serialized_output_len(&actual).unwrap() >= size
            })
            .expect("a boundary between the lower bound and the real replacement");
        let full = size_error_output(
            &original,
            full_size_error_message(serde_json::to_vec(&original).unwrap().len(), None),
        );
        let mut candidate = original.clone();
        let mut size = serialized_output_len(&original).unwrap();
        let mut total = serialized_len(&tool_results(vec![original.clone()])).unwrap();
        let initial_total = total;
        assert!(!replace_if_smaller(&mut candidate, full, &mut size, &mut total).unwrap());
        assert_eq!(candidate, original);
        assert_eq!(total, initial_total);
        assert_eq!(size, serialized_output_len(&original).unwrap());
        let entry = tool_results(vec![original.clone()]);
        let minimal = tool_results(vec![size_error_output(
            &original,
            minimal_size_error_message(false),
        )]);
        let budget = serialized_len(&minimal).unwrap();
        assert_eq!(bound(budget, &entry).await, minimal);
    }

    #[test]
    fn running_total_matches_full_entry_serialization() {
        let variants = [
            json!({"out": "x".repeat(4096)}),
            json!({"listing": "  indented\n\nquoted: 'yes'\n漢字\n".repeat(256)}),
            json!({"nested": [{"a": [null, true, 17], "b": "line\nline\n".repeat(256)}]}),
        ];
        let mut entry = tool_results(
            variants
                .into_iter()
                .enumerate()
                .map(|(i, output)| {
                    let mut result = small(&format!("result-{i}"));
                    result.output = output;
                    result
                })
                .collect(),
        );
        let mut total = serialized_len(&entry).unwrap();
        for i in 0..entry_results(&entry).len() {
            let SessionLogEntry::ToolResults { results, .. } = &mut entry else {
                unreachable!()
            };
            let mut size = serialized_output_len(&results[i]).unwrap();
            let replacement = size_error_output(&results[i], minimal_size_error_message(false));
            assert!(
                replace_if_smaller(&mut results[i], replacement, &mut size, &mut total).unwrap()
            );
            assert_eq!(total, serialized_len(&entry).unwrap());
        }
    }
}
