//! Guards the request patches shipped in `models.yaml`.
//!
//! Patches run through jaq at request time and a failing one used to be
//! skipped with only a `warn!` in the debug log, so a broken expression could
//! ship unnoticed while every request for that model went out unpatched.

use harnx_client::client::ALL_PROVIDER_MODELS;
use harnx_core::model::{Model, ModelType};
use serde_json::{json, Value};

/// A request envelope shaped like `RequestData::to_json_value`, holding the
/// fields the shipped patches read or delete.
fn request_envelope(model: &Model) -> Value {
    json!({
        "url": "https://example.invalid/v1/messages",
        "headers": {"authorization": "Bearer test"},
        "body": {
            "model": model.real_name(),
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 4096,
            "temperature": 0.5,
            "top_p": 0.9,
            "stream": true
        }
    })
}

#[test]
fn every_shipped_patch_evaluates_without_error() {
    let mut checked = 0;
    for provider in ALL_PROVIDER_MODELS.iter() {
        for model in Model::from_config(&provider.provider, &provider.models) {
            let Some(patches) = model.patches() else {
                continue;
            };
            let input = request_envelope(&model);
            harnx_core::jaq::eval_filters_strict(patches, input).unwrap_or_else(|err| {
                panic!(
                    "patches for model {} failed to evaluate: {err:#}",
                    model.id()
                )
            });
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no models.yaml patches were checked — the test is not exercising anything"
    );
}

/// The effort aliases exist to send an effort level; assert the patched body
/// actually carries it, so a patch that evaluates but writes nothing is caught.
#[test]
fn effort_aliases_set_effort_in_the_request_body() {
    let mut checked = 0;
    for provider in ALL_PROVIDER_MODELS.iter() {
        for model in Model::from_config(&provider.provider, &provider.models) {
            let Some(patches) = model.patches() else {
                continue;
            };
            let effort_key = match patches.iter().find_map(|p| effort_path(p)) {
                Some(key) => key,
                None => continue,
            };
            let patched = harnx_core::jaq::eval_filters_strict(patches, request_envelope(&model))
                .unwrap_or_else(|err| panic!("patches for {} failed: {err:#}", model.id()));

            let effort = patched["body"][effort_key]["effort"].as_str();
            assert!(
                effort.is_some(),
                "model {} patch left .body.{effort_key}.effort unset: {patched}",
                model.id()
            );
            assert_eq!(
                model.model_type(),
                ModelType::Chat,
                "effort patches only make sense on chat models"
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no effort-setting aliases found in models.yaml"
    );
}

#[test]
fn package_openai_models_use_responses_without_sampling_parameters() {
    let openai = ALL_PROVIDER_MODELS
        .iter()
        .find(|provider| provider.provider == "openai")
        .expect("OpenAI provider catalog");
    for name in [
        "gpt-5.6-luna",
        "gpt-5.6-terra",
        "gpt-5.6-sol",
        "gpt-5.6-sol:high",
        "gpt-5.6-sol:max",
        "gpt-6-astra",
        "gpt-6-astra:max",
    ] {
        let model = Model::from_config("openai", &openai.models)
            .into_iter()
            .find(|model| model.name() == name)
            .unwrap_or_else(|| panic!("missing OpenAI model alias {name}"));
        assert_eq!(model.endpoint(), Some("responses"), "{name}");
        let patched = harnx_core::jaq::eval_filters_strict(
            model.patches().expect("OpenAI model patch"),
            request_envelope(&model),
        )
        .unwrap_or_else(|error| panic!("patches for {name} failed: {error:#}"));
        assert!(patched["body"].get("temperature").is_none(), "{name}");
        assert!(patched["body"].get("top_p").is_none(), "{name}");
        if let Some((_, effort)) = name.rsplit_once(':') {
            assert_eq!(patched["body"]["reasoning"]["effort"], effort, "{name}");
        }
    }
}

#[test]
fn fable_5_1_uses_adaptive_thinking_and_an_output_limit() {
    let claude = ALL_PROVIDER_MODELS
        .iter()
        .find(|provider| provider.provider == "claude")
        .expect("Claude catalog");
    for name in ["claude-fable-5-1", "claude-fable-5-1:max"] {
        let model = Model::from_config("claude", &claude.models)
            .into_iter()
            .find(|model| model.name() == name)
            .expect("Fable model");
        assert_eq!(model.real_name(), "claude-fable-5-1");
        assert_eq!(model.max_tokens_param(), Some(128000));
        let mut envelope = request_envelope(&model);
        envelope["headers"]["anthropic-beta"] = json!("files-api-2025-04-14");
        let patched = harnx_core::jaq::eval_filters_strict(
            model.patches().expect("Fable request patch"),
            envelope,
        )
        .expect("valid Fable patch");
        assert_eq!(
            patched["body"]["thinking"],
            json!({
                "type": "adaptive", "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"}
            })
        );
        assert_eq!(
            patched["headers"]["anthropic-beta"],
            "files-api-2025-04-14,thinking-binding-controls-2026-08-01"
        );
        assert_eq!(
            patched["body"]["output_config"]["effort"],
            if name.ends_with(":max") {
                "max"
            } else {
                "high"
            }
        );
        assert!(patched["body"].get("temperature").is_none());
        assert!(patched["body"].get("top_p").is_none());
    }
}

#[test]
fn gemini_3_8_drops_sampling_settings_without_losing_thinking_or_output_limit() {
    let gemini = ALL_PROVIDER_MODELS
        .iter()
        .find(|provider| provider.provider == "gemini")
        .expect("Gemini catalog");
    let model = Model::from_config("gemini", &gemini.models)
        .into_iter()
        .find(|model| model.name() == "gemini-3.8-flash")
        .expect("Gemini 3.8");
    let mut envelope = request_envelope(&model);
    envelope["body"]["generationConfig"] = json!({
        "temperature": 0.7, "topP": 0.9, "topK": 40, "candidateCount": 1,
        "maxOutputTokens": 8192, "thinkingConfig": {"thinkingLevel": "high"}
    });
    let patched = harnx_core::jaq::eval_filters_strict(
        model.patches().expect("Gemini request patch"),
        envelope,
    )
    .expect("valid Gemini patch");
    assert_eq!(
        patched["body"]["generationConfig"],
        json!({
            "maxOutputTokens": 8192, "thinkingConfig": {"thinkingLevel": "high"}
        })
    );
}

/// Returns the container key (`output_config` or `reasoning`) when the patch
/// sets an effort level under it.
fn effort_path(patch: &str) -> Option<&'static str> {
    ["output_config", "reasoning"]
        .into_iter()
        .find(|key| patch.contains(&format!(".body.{key}")) && patch.contains("effort"))
}
