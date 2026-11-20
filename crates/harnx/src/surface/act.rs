//! Submitting one surface action over the durable acceptance path and
//! reporting the outcome (plan T5.1 Part B).

use anyhow::{Context, Result};
use harnx_core::interaction::response::build_data;
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};
use harnx_runtime::NatsSession;
use serde_json::{Map, Value};

use crate::cli::SurfaceActArgs;
use crate::response_inputs::{insert_data_value, json_values};

/// The CLI message for one surface action outcome. The bool is the exit code's
/// success flag; outcomes are never a process failure.
fn outcome_message(outcome: SurfaceActionOutcome, id: &str) -> (bool, String) {
    if let Some(rejection) = rejection_message(&outcome) {
        return (false, rejection.to_string());
    }
    let accepted = outcome.is_applied();
    let message = match &outcome {
        SurfaceActionOutcome::Applied => {
            format!("Accepted: recorded the action for surface '{id}'.")
        }
        SurfaceActionOutcome::AlreadyApplied => {
            format!("Already applied: an identical action for '{id}' was recorded earlier.")
        }
        SurfaceActionOutcome::Conflicting => format!(
            "Rejected: a different action for '{id}' was already accepted \
             (conflicting). Nothing was recorded."
        ),
        SurfaceActionOutcome::UnknownSurface => format!(
            "Rejected: no published surface matches '{id}' at that sequence \
             (unknown_surface). Nothing was recorded."
        ),
        // `submit_surface_action` consumes Pending acknowledgements internally;
        // keep this mapping defensive if the public outcome contract changes.
        SurfaceActionOutcome::Pending => {
            "Pending: the worker is still processing this action. Check the session \
             for its outcome before submitting again."
                .to_string()
        }
        _ => "Rejected: this surface action outcome is not supported by the CLI.".to_string(),
    };
    (accepted, message)
}

/// The static rejection texts, keyed by outcome; `None` for outcomes formatted
/// directly by `outcome_message`.
fn rejection_message(outcome: &SurfaceActionOutcome) -> Option<&'static str> {
    match outcome {
        SurfaceActionOutcome::UndeclaredAction => Some(
            "Rejected: the action is not a declared button on this surface \
             (undeclared_action). Nothing was recorded.",
        ),
        SurfaceActionOutcome::InvalidVisibility => Some(
            "Rejected: the button declares an unknown visibility \
             (invalid_visibility). Nothing was recorded.",
        ),
        SurfaceActionOutcome::InvalidTool => Some(
            "Rejected: the declared target tool is malformed (invalid_tool). \
             Nothing was executed or recorded.",
        ),
        SurfaceActionOutcome::InvalidData => Some(
            "Rejected: the data does not match the surface's bound inputs \
             (invalid_data). Nothing was recorded.",
        ),
        SurfaceActionOutcome::UnauthorizedTool => Some(
            "Rejected: the declared target tool is not available in this session \
             (unauthorized_tool). Nothing was executed or recorded.",
        ),
        SurfaceActionOutcome::ToolFailed => Some(
            "Rejected: the declared target tool ran and failed (tool_failed). \
             Nothing was recorded; a retry re-runs the tool.",
        ),
        SurfaceActionOutcome::LeaseLost => Some(
            "Rejected: the worker lost its session lease (lease_lost). The action \
             outcome is uncertain: a direct tool may have run, and the action may \
             or may not be recorded. Check the session before retrying.",
        ),
        SurfaceActionOutcome::AppendFailed => Some(
            "Rejected: the action record could not be confirmed after conflicts \
             or a handler panic (append_failed). The action outcome is uncertain: \
             a direct tool may have run, and the action may or may not be recorded. \
             Check the session before retrying.",
        ),
        SurfaceActionOutcome::Pending => None,
        SurfaceActionOutcome::Applied
        | SurfaceActionOutcome::AlreadyApplied
        | SurfaceActionOutcome::Conflicting
        | SurfaceActionOutcome::UnknownSurface => None,
    }
}

/// Build the response through the shared T4.2 contract: input values keyed by
/// binding, then `build_data` binds them to the declared button.
pub(super) fn build_surface_response(
    payload: &Value,
    action: &str,
    data: &[String],
    data_json: Option<&str>,
) -> Result<InteractionResponse> {
    let mut values = match data_json {
        Some(raw) => json_values(payload, raw)?,
        None => Map::new(),
    };
    for pair in data {
        insert_data_value(payload, pair, &mut values)?;
    }
    let data = build_data(payload, action, &values).map_err(anyhow::Error::msg)?;
    Ok(InteractionResponse {
        action: action.to_string(),
        data,
        note: None,
    })
}

fn build_surface_submission(
    args: &SurfaceActArgs,
    payload: &Value,
    publish_seq: u64,
) -> Result<SurfaceActionSubmission> {
    let response =
        build_surface_response(payload, &args.action, &args.data, args.data_json.as_deref())?;
    let user_message = args
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(str::to_owned);
    Ok(SurfaceActionSubmission {
        surface_id: args.surface_id.clone(),
        publish_seq,
        response,
        user_message,
    })
}

/// Attach the session the surface belongs to, like `harnx interact answer`.
async fn attach_session(
    config: crate::config::Config,
    args: &SurfaceActArgs,
) -> Result<NatsSession> {
    crate::session_attach::attach_session(config, &args.agent, &args.session).await
}

pub(super) async fn run_act(args: &SurfaceActArgs) -> Result<u8> {
    let config = crate::init_frontend_config(crate::config::WorkingMode::Cmd, true).await?;
    let entries =
        crate::session_attach::session_entries(&config, &args.agent, &args.session).await?;
    let (publish_seq, target) =
        super::resolve_target(&entries, &args.surface_id, args.publish_seq)?;
    let submission = build_surface_submission(args, &target.payload, publish_seq)?;
    // An already-answered surface is classified by the durable path, so an
    // identical retry reports `already_applied` (success), not an error.
    let session = attach_session(config, args).await?;
    let outcome = session
        .submit_surface_action(&submission)
        .await
        .context("submitting the surface action failed")?;
    let (accepted, message) = outcome_message(outcome, &args.surface_id);
    if accepted {
        println!("{message}");
        Ok(0)
    } else {
        eprintln!("{message}");
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_messages_map_to_exit_flags() {
        let (ok, _) = outcome_message(SurfaceActionOutcome::Applied, "s");
        assert!(ok);
        let (ok, _) = outcome_message(SurfaceActionOutcome::AlreadyApplied, "s");
        assert!(ok);
        for outcome in [
            SurfaceActionOutcome::Conflicting,
            SurfaceActionOutcome::UnknownSurface,
            SurfaceActionOutcome::UndeclaredAction,
            SurfaceActionOutcome::InvalidVisibility,
            SurfaceActionOutcome::InvalidTool,
            SurfaceActionOutcome::InvalidData,
            SurfaceActionOutcome::UnauthorizedTool,
            SurfaceActionOutcome::ToolFailed,
            SurfaceActionOutcome::LeaseLost,
            SurfaceActionOutcome::AppendFailed,
            SurfaceActionOutcome::Pending,
        ] {
            let (ok, message) = outcome_message(outcome, "deploy-1");
            assert!(!ok, "message: {message}");
        }

        for outcome in [
            SurfaceActionOutcome::LeaseLost,
            SurfaceActionOutcome::AppendFailed,
        ] {
            let (_, message) = outcome_message(outcome, "deploy-1");
            assert!(message.contains("direct tool may have run"), "{message}");
            assert!(
                message.contains("Check the session before retrying"),
                "{message}"
            );
        }

        let (ok, message) = outcome_message(SurfaceActionOutcome::Pending, "deploy-1");
        assert!(!ok);
        assert!(message.contains("still processing"), "{message}");
        assert!(message.contains("before submitting again"), "{message}");
    }

    #[test]
    fn caller_builds_submission_with_trimmed_user_message() {
        let args = SurfaceActArgs {
            agent: "agent".into(),
            session: "session".into(),
            surface_id: "deploy-1".into(),
            publish_seq: Some(7),
            action: "ship".into(),
            data: vec!["name=rel-9".into()],
            data_json: None,
            note: Some("  deploy rel-9  ".into()),
        };
        let payload = serde_json::json!({
            "version": "1.0",
            "createSurface": {
                "surfaceId": "deploy-1",
                "components": [
                    {"id": "root", "component": "Card", "child": "col"},
                    {"id": "col", "component": "Column", "children": ["name", "ship"]},
                    {"id": "name", "component": "TextField", "label": "Name",
                     "value": {"@path": "/name"}},
                    {"id": "ship", "component": "Button",
                     "action": {"event": {"name": "ship"}}}
                ]
            }
        });

        let submission = build_surface_submission(&args, &payload, 7).unwrap();
        assert_eq!(submission.surface_id, "deploy-1");
        assert_eq!(submission.publish_seq, 7);
        assert_eq!(submission.response.action, "ship");
        assert_eq!(submission.response.data["name"], "rel-9");
        assert_eq!(submission.response.note, None);
        assert_eq!(submission.user_message.as_deref(), Some("deploy rel-9"));

        let blank_note = SurfaceActArgs {
            note: Some("  ".into()),
            ..args
        };
        let submission = build_surface_submission(&blank_note, &payload, 7).unwrap();
        assert_eq!(submission.user_message, None);
    }
}
