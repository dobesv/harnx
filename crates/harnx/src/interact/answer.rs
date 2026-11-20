//! Planning one interaction answer: exact identity, shared-contract response
//! building, and classification of a lost-reply re-run. Submission and
//! outcome reporting live in `submit`; re-run classification helpers in
//! `rerun`.

use anyhow::{anyhow, bail, Result};
use harnx_core::interaction::{
    response::{build_data, validate_response},
    InteractionResponse,
};
use harnx_runtime::interactions::PendingInteraction;
use serde_json::Map;

use super::rerun;
use super::submit;
use super::SessionInteractions;
use crate::response_inputs::{insert_data_value, json_values};

/// What `harnx interact answer` does with the session's current state.
enum AnswerPlan {
    /// The request is still pending: submit this response for it. Boxed —
    /// `PendingInteraction` is large and `Rerun` is one bool.
    Submit(Box<SubmitPlan>),
    /// The request is no longer pending: its recorded response already
    /// answered it, and `identical` says whether this answer matches.
    Rerun { identical: bool },
}

struct SubmitPlan {
    target: PendingInteraction,
    response: InteractionResponse,
}

/// Plan the answer before touching NATS: exact identity, shared-contract
/// response building, and classification of a lost-reply re-run.
fn answer_plan(
    snapshot: &SessionInteractions,
    args: &crate::cli::InteractAnswerArgs,
) -> Result<AnswerPlan> {
    if snapshot
        .pending
        .iter()
        .any(|item| item.interaction_id == args.interaction_id)
    {
        let target = answer_target(&snapshot.pending, &args.interaction_id, args.request_seq)?;
        let response = build_response(
            target,
            &args.action,
            &args.data,
            args.data_json.as_deref(),
            args.note.clone(),
        )?;
        return Ok(AnswerPlan::Submit(Box::new(SubmitPlan {
            target: target.clone(),
            response,
        })));
    }
    // A re-run after the reply was lost (or a racing frontend answered first)
    // no longer sees the request as pending; compare against the recorded
    // response so it reports `Already applied` or `conflicting`, not "unknown".
    let Some((target, recorded)) =
        rerun::answered_request(&snapshot.history, &args.interaction_id, args.request_seq)
    else {
        bail!(
            "interaction '{}' is not pending (stale, already answered, or \
             unknown); run `harnx interact list` to see pending interactions",
            args.interaction_id
        );
    };
    let response = build_response(
        &target,
        &args.action,
        &args.data,
        args.data_json.as_deref(),
        args.note.clone(),
    )?;
    Ok(AnswerPlan::Rerun {
        identical: response == *recorded,
    })
}

/// The pending request to answer: exact identity, never "whatever is pending".
pub(super) fn answer_target<'a>(
    pending: &'a [PendingInteraction],
    interaction_id: &str,
    request_seq: Option<u64>,
) -> Result<&'a PendingInteraction> {
    let matching: Vec<&PendingInteraction> = pending
        .iter()
        .filter(|item| item.interaction_id == interaction_id)
        .collect();
    match (matching.as_slice(), request_seq) {
        ([], _) => bail!(
            "interaction '{interaction_id}' is not pending (stale, already answered, or \
             unknown); run `harnx interact list` to see pending interactions"
        ),
        ([one], None) => Ok(one),
        ([one], Some(seq)) => {
            anyhow::ensure!(
                one.request_seq == seq,
                "request sequence {seq} does not match pending request sequence {} of '{interaction_id}'",
                one.request_seq
            );
            Ok(one)
        }
        (many, None) => bail!(
            "{} pending requests share interaction id '{interaction_id}'; pass --request-seq",
            many.len()
        ),
        (many, Some(seq)) => many
            .iter()
            .copied()
            .find(|item| item.request_seq == seq)
            .ok_or_else(|| {
                anyhow!(
                    "request sequence {seq} matches none of the {len} pending requests of \
                     '{interaction_id}'",
                    len = many.len()
                )
            }),
    }
}

/// Build the response through the shared contract: `--data`/`--data-json`
/// values are keyed by input binding, `build_data` fills defaults and resolves
/// the action's declared context, and `validate_response` produces the same
/// rejections durable acceptance would.
pub(super) fn build_response(
    target: &PendingInteraction,
    action: &str,
    data: &[String],
    data_json: Option<&str>,
    note: Option<String>,
) -> Result<InteractionResponse> {
    let payload = &target.request.payload;
    let data = match target.request.kind.as_str() {
        "confirm" => {
            anyhow::ensure!(
                action == "approve" || action == "deny",
                "confirm interactions accept only 'approve' or 'deny', not '{action}'"
            );
            Map::new()
        }
        "a2ui" => {
            let mut values = match data_json {
                Some(raw) => json_values(payload, raw)?,
                None => Map::new(),
            };
            for pair in data {
                insert_data_value(payload, pair, &mut values)?;
            }
            build_data(payload, action, &values).map_err(anyhow::Error::msg)?
        }
        kind => bail!("interaction kind '{kind}' cannot be answered from the CLI"),
    };
    let response = InteractionResponse {
        action: action.to_string(),
        data,
        note,
    };
    if target.request.kind == "a2ui" {
        validate_response(payload, &response)
            .map_err(|error| anyhow!("invalid response: {error}"))?;
    }
    Ok(response)
}

pub(super) async fn run_answer(args: &crate::cli::InteractAnswerArgs) -> Result<u8> {
    let config = crate::init_frontend_config(crate::config::WorkingMode::Cmd, true).await?;
    let snapshot = super::read_pending(&config, &args.agent, &args.session).await?;
    match answer_plan(&snapshot, args)? {
        AnswerPlan::Submit(plan) => {
            submit::submit_answer(config, args, plan.target, plan.response).await
        }
        AnswerPlan::Rerun { identical } => rerun::report_rerun(&args.interaction_id, identical),
    }
}
