//! Public A2A 1.0 discovery cards, built from resolved export metadata.

use a2a_lf::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill};
use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};

use crate::{exports::Export, web_url::inferred_base_url};

fn card(export: &Export, base: &str) -> AgentCard {
    let meta = &export.card_meta;
    AgentCard {
        name: meta.name.clone(),
        description: meta.description.clone(),
        version: meta.version.clone(),
        supported_interfaces: vec![AgentInterface {
            url: format!("{base}/agents/{}", export.public_name),
            protocol_binding: a2a_lf::types::TRANSPORT_PROTOCOL_JSONRPC.into(),
            protocol_version: "1.0".into(),
            tenant: None,
        }],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extended_agent_card: Some(false),
            extensions: None,
        },
        default_input_modes: vec!["text/plain".into(), "application/json".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: export.public_name.clone(),
            name: meta.name.clone(),
            description: meta.description.clone(),
            tags: vec![export.public_name.clone()],
            examples: Some(meta.conversation_starters.clone()),
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

fn card_json(
    card: &AgentCard,
) -> Result<serde_json::Value, a2a_pb::protojson_conv::ProtoJsonPayloadError> {
    let mut json = a2a_pb::protojson_conv::to_value(card)?;
    // ProtoJSON omits empty scalars, but the Agent Card schema requires these
    // fields even when AgentConfig leaves description or version empty.
    json["name"] = card.name.clone().into();
    json["description"] = card.description.clone().into();
    json["version"] = card.version.clone().into();
    if let Some(skills) = json["skills"].as_array_mut() {
        for (json_skill, skill) in skills.iter_mut().zip(&card.skills) {
            json_skill["description"] = skill.description.clone().into();
        }
    }
    Ok(json)
}

/// Separate from RPC identity/version checks: discovery is always public.
pub(crate) fn router(export: &Export, public_base_url: Option<&str>) -> Router {
    let export = export.clone();
    let public_base_url = public_base_url.map(str::to_owned);
    let handler = move |request: Request| {
        let export = export.clone();
        let public_base_url = public_base_url.clone();
        async move { response(&export, public_base_url.as_deref(), &request) }
    };
    Router::new()
        .route("/.well-known/agent-card.json", get(handler.clone()))
        .route("/.well-known/agent.json", get(handler))
}

fn response(export: &Export, configured_base: Option<&str>, request: &Request) -> Response {
    let inferred;
    let base = match configured_base {
        Some(base) => base,
        None => {
            inferred = inferred_base_url(request.headers(), request.uri());
            let Some(base) = inferred.as_deref() else {
                return (StatusCode::BAD_REQUEST, "cannot infer Agent Card base URL")
                    .into_response();
            };
            base
        }
    };
    match card_json(&card(export, base)) {
        Ok(json) => Json(json).into_response(),
        Err(error) => {
            tracing::error!(%error, agent = %export.agent, "failed to serialize Agent Card");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::AgentCardMeta;

    #[test]
    fn agent_card_matches_a2a_1_0_golden_fixture() {
        harnx_core::require_nextest();
        let export = Export {
            public_name: "tools".into(),
            agent: "pkg/agent".into(),
            cluster: None,
            card_meta: AgentCardMeta {
                name: "pkg/agent".into(),
                description: "Package agent".into(),
                version: "2.3.4".into(),
                conversation_starters: vec!["Review the checkout flow".into()],
            },
            lookup_keys: vec![],
        };
        let json = card_json(&card(&export, "https://harnx.example.com")).unwrap();
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/agent_card.json")).unwrap();
        assert_eq!(json, golden);
        let decoded: AgentCard = a2a_pb::protojson_conv::from_value(json).unwrap();
        assert_eq!(decoded, card(&export, "https://harnx.example.com"));
    }
}
