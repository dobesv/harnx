use crate::nats_session_metadata::{RunLimitsPolicySource, RunLimitsRecord};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const TIMEOUT_TERMINAL_PREFIX: &str = "harnx:timeout ";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutScope {
    LocalInvocation,
    InheritedDeadline,
    OuterRun,
}

impl TimeoutScope {
    pub(super) fn retry_hint(self, session_id: &str) -> String {
        match self {
            Self::LocalInvocation => format!("Local invocation allowance expired. Inspect saved public results and revise or narrow instructions before continuing the same session id `{session_id}`, only while the outer run remains live. Do not retry unchanged."),
            Self::OuterRun | Self::InheritedDeadline => {
                let reason = match self {
                    Self::OuterRun => "The outer run deadline expired.",
                    _ => "The inherited deadline expired.",
                };
                format!("{reason} Do not retry: autonomous retries cannot renew this deadline. Return to the user to confirm continuation; a new external instruction is required. Inspect available saved public results and artifact references.")
            }
        }
    }
}

/// Derived from the worker's frozen record, not caller timer or role guesses.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutTerminal {
    pub scope: TimeoutScope,
    pub deadline: DateTime<Utc>,
    pub run_id: String,
    pub invocation_id: String,
}

impl TimeoutTerminal {
    pub fn from_record(record: &RunLimitsRecord) -> Option<Self> {
        Some(Self {
            scope: if record.parent_invocation.is_none() {
                TimeoutScope::OuterRun
            } else if matches!(
                record.policy_source,
                RunLimitsPolicySource::InheritedFrom { .. }
            ) {
                TimeoutScope::InheritedDeadline
            } else {
                TimeoutScope::LocalInvocation
            },
            deadline: record.deadline?,
            run_id: record.run_id.as_str().to_owned(),
            invocation_id: record.invocation_id.as_str().to_owned(),
        })
    }

    /// Persist inside the existing Cancel requester label. Cancel remains terminal authority.
    pub fn message(&self) -> String {
        format!(
            "worker deadline: {TIMEOUT_TERMINAL_PREFIX}{}",
            serde_json::to_string(self).expect("timeout JSON")
        )
    }
}

pub fn parse_timeout_terminal(message: &str) -> Option<TimeoutTerminal> {
    let start = message.rfind(TIMEOUT_TERMINAL_PREFIX)?;
    let terminal: TimeoutTerminal =
        serde_json::from_str(&message[start + TIMEOUT_TERMINAL_PREFIX.len()..]).ok()?;
    if terminal.run_id.is_empty() || terminal.invocation_id.is_empty() {
        return None;
    }
    Some(terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nats_session_metadata::CallTimeoutOverride;
    use crate::nats_session_metadata::{InvocationEdgeKind, InvocationIdentity, RunIdentity};
    use harnx_core::config_data::RunLimitsConfig;

    #[test]
    fn worker_timeout_marker_roundtrips_and_scopes_follow_frozen_policy() {
        let now = Utc::now();
        let root = RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            now,
            RunLimitsConfig::default(),
            None,
            CallTimeoutOverride::from_optional(Some(20)),
        )
        .unwrap();
        let child = |seconds| {
            RunLimitsRecord::admit_child(
                &root,
                InvocationIdentity::new(),
                InvocationEdgeKind::Delegation,
                now,
                RunLimitsConfig::default(),
                None,
                CallTimeoutOverride::from_optional(Some(seconds)),
            )
            .unwrap()
        };
        for (record, scope) in [
            (root.clone(), TimeoutScope::OuterRun),
            (child(1), TimeoutScope::LocalInvocation),
            (child(0), TimeoutScope::InheritedDeadline),
        ] {
            let terminal = TimeoutTerminal::from_record(&record).unwrap();
            assert_eq!(terminal.scope, scope);
            assert_eq!(
                parse_timeout_terminal(&format!("worker: {}", terminal.message())),
                Some(terminal)
            );
        }
        for invalid in [
            "parent interrupted",
            "autonomous deadline expired",
            "harnx:timeout {}",
            "harnx:timeout {\"scope\":\"local_invocation\",\"deadline\":\"bad\"}",
        ] {
            assert_eq!(parse_timeout_terminal(invalid), None);
        }
    }
}

#[cfg(test)]
mod result_tests {
    use super::*;
    use crate::{synthesize_terminated_result, PublicProgress, TerminationInputs, TerminationKind};

    #[test]
    fn scoped_timeout_serialization_preserves_ids_usage_and_requires_external_continuation() {
        for scope in [
            TimeoutScope::LocalInvocation,
            TimeoutScope::OuterRun,
            TimeoutScope::InheritedDeadline,
        ] {
            let timeout = TimeoutTerminal {
                scope,
                deadline: Utc::now(),
                run_id: "run".into(),
                invocation_id: "invocation".into(),
            };
            let progress = PublicProgress::default();
            let result = synthesize_terminated_result(TerminationInputs {
                kind: TerminationKind::Timeout,
                timeout: Some(timeout.clone()),
                public_progress: Some(&progress),
                session_id: "child",
                usage: &Default::default(),
                thinking_excerpt: None,
                budget: None,
                repetition: None,
            });
            let json = result.termination_json();
            assert_eq!(json["kind"], "timeout");
            assert_eq!(json["session_id"], "child");
            assert_eq!(json["run_id"], "run");
            assert_eq!(json["invocation_id"], "invocation");
            assert_eq!(json["public_progress"]["available"], false);
            assert_eq!(json["thinking_excerpt"], serde_json::Value::Null);
            assert_eq!(json["usage"]["budgeted"], 0);
            assert_eq!(
                crate::parse_worker_terminal(&timeout.message())
                    .unwrap()
                    .timeout(),
                Some(timeout)
            );
            assert!(result.response.contains("Public progress unavailable"));
            if scope == TimeoutScope::LocalInvocation {
                assert!(result
                    .termination
                    .retry_hint
                    .contains("outer run remains live"));
                assert!(result
                    .termination
                    .retry_hint
                    .contains("Do not retry unchanged"));
            } else {
                assert!(result
                    .termination
                    .retry_hint
                    .contains("new external instruction"));
                assert!(result
                    .termination
                    .retry_hint
                    .contains("autonomous retries cannot renew"));
                assert!(result.response.contains("Do not retry:"));
                assert!(result
                    .response
                    .contains("Return to the user to confirm continuation"));
                let reason = if scope == TimeoutScope::OuterRun {
                    "The outer run deadline expired."
                } else {
                    "The inherited deadline expired."
                };
                assert!(result.response.contains(reason));
                assert!(json["retry_hint"].as_str().unwrap().contains(reason));
            }
        }
    }

    #[test]
    fn timeout_surfaces_streaming_public_output_without_thinking_or_nested_progress() {
        use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent, NullSink};
        let sink = crate::InvocationBufferingSink::new(std::sync::Arc::new(NullSink));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("Saved cid:media:test/session/".into())],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("a".repeat(64) + " ")],
        }));
        let progress = sink.public_progress();
        let result = synthesize_terminated_result(TerminationInputs {
            kind: TerminationKind::Timeout,
            timeout: None,
            public_progress: Some(&progress),
            session_id: "child",
            usage: &Default::default(),
            thinking_excerpt: None,
            budget: None,
            repetition: None,
        });
        assert!(
            result
                .termination
                .public_progress
                .as_ref()
                .unwrap()
                .available
        );
        assert!(result
            .termination
            .public_progress
            .as_ref()
            .unwrap()
            .references
            .iter()
            .any(|r| r == &format!("cid:media:test/session/{}", "a".repeat(64))));
        assert_eq!(result.termination.thinking_excerpt, None);
        assert!(result
            .response
            .contains(&format!("Saved cid:media:test/session/{}", "a".repeat(64))));
    }
}
