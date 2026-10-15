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
            "stream": true,
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {}
        }
    })
}

fn catalog_models() -> impl Iterator<Item = Model> {
    ALL_PROVIDER_MODELS
        .iter()
        .flat_map(|provider| Model::from_config(&provider.provider, &provider.models))
}

fn provider_models(provider_name: &str) -> Vec<Model> {
    let provider = ALL_PROVIDER_MODELS
        .iter()
        .find(|provider| provider.provider == provider_name)
        .unwrap_or_else(|| panic!("missing provider catalog {provider_name}"));
    Model::from_config(provider_name, &provider.models)
}

fn patched_request(model: &Model, envelope: Value) -> Value {
    match model.patches() {
        Some(patches) => harnx_core::jaq::eval_filters_strict(patches, envelope)
            .unwrap_or_else(|err| panic!("patches for {} failed: {err:#}", model.id())),
        None => envelope,
    }
}

// Responses routing alone doesn't imply reasoning support.
fn openai_model_without_reasoning(name: &str) -> bool {
    ["gpt-4.1", "gpt-audio"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name.ends_with("-codex-spark")
}

/// Returns whether a checked reasoning model is an effort alias.
fn assert_responses_summary(model: &Model) -> Option<bool> {
    let patched = patched_request(model, request_envelope(model));
    let name = model.name();
    if model.client_name() == "openai" && openai_model_without_reasoning(name) {
        assert!(patched["body"].get("reasoning").is_none(), "{name}");
        return None;
    }
    if model.client_name() == "openai" {
        assert!(
            patched["body"].get("reasoning").is_some(),
            "Responses reasoning model {name} is missing its catalog patch"
        );
    }
    let reasoning = patched["body"].get("reasoning")?;
    assert_eq!(reasoning["summary"], "auto", "{}", model.id());
    let effort = name.rsplit_once(':').map(|(_, effort)| effort);
    if let Some(effort) = effort {
        assert_eq!(reasoning["effort"], effort, "{}", model.id());
    }
    Some(effort.is_some())
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
fn responses_reasoning_models_request_auto_summary() {
    let checked: Vec<bool> = catalog_models()
        .filter(|model| model.endpoint() == Some("responses"))
        .filter_map(|model| assert_responses_summary(&model))
        .collect();
    assert!(
        !checked.is_empty(),
        "no Responses reasoning patches checked"
    );
    assert!(
        checked.contains(&true),
        "no Responses effort aliases checked"
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
        "gpt-6-astra:high",
        "gpt-6-astra:max",
        "gpt-6-luna",
        "gpt-6-sol",
        "gpt-6-sol:high",
        "gpt-6-sol:max",
        "gpt-6.1-sol",
        "gpt-6.1-sol:high",
        "gpt-6.1-sol:max",
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
fn opus_5_5_uses_adaptive_thinking_and_required_output_limit() {
    for provider_name in ["claude", "vertexai"] {
        let provider = ALL_PROVIDER_MODELS
            .iter()
            .find(|provider| provider.provider == provider_name)
            .expect("Claude provider catalog");
        let names: &[&str] = if provider_name == "vertexai" {
            &[
                "claude-opus-5-5",
                "claude-opus-5-5:xhigh",
                "claude-opus-5-5:max",
                "claude-opus-5-5@default",
                "claude-opus-5-5@default:xhigh",
                "claude-opus-5-5@default:max",
            ]
        } else {
            &[
                "claude-opus-5-5",
                "claude-opus-5-5:xhigh",
                "claude-opus-5-5:max",
            ]
        };
        for &name in names {
            let model = Model::from_config(provider_name, &provider.models)
                .into_iter()
                .find(|model| model.name() == name)
                .unwrap_or_else(|| panic!("missing {provider_name} model {name}"));
            assert_eq!(model.real_name(), name.split(':').next().unwrap());
            assert_eq!(model.max_tokens_param(), Some(128000), "{name}");
            let patched = harnx_core::jaq::eval_filters_strict(
                model.patches().expect("Opus 5.5 request patch"),
                request_envelope(&model),
            )
            .expect("valid Opus 5.5 patch");
            assert_eq!(
                patched["body"]["thinking"],
                json!({"type": "adaptive", "display": "summarized"}),
                "{provider_name}:{name}"
            );
            let expected_effort = name.rsplit_once(':').map_or("medium", |(_, effort)| effort);
            assert_eq!(patched["body"]["output_config"]["effort"], expected_effort);
            assert!(patched["body"].get("temperature").is_none());
            assert!(patched["body"].get("top_p").is_none());
        }
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
            "maxOutputTokens": 8192,
            "thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}
        })
    );
}

fn gemini_version(name: &str) -> Option<(u32, u32)> {
    let suffix = name.strip_prefix("gemini-")?;
    let mut version = suffix.split('-').next()?.split('.');
    let major = version.next()?.parse().ok()?;
    let minor = version.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

fn gemini_thinking_capable(model: &Model) -> bool {
    let name = model.real_name();
    model.model_type() == ModelType::Chat
        && gemini_version(name).is_some_and(|version| version >= (2, 5))
        && !["-image", "-tts", "-native-audio"]
            .iter()
            .any(|suffix| name.contains(suffix))
}

fn assert_gemini_thinking_settings(model: &Model, thinking: Option<Value>) {
    let mut envelope = request_envelope(model);
    let output_limit = model.max_tokens_param().unwrap_or(4096);
    envelope["body"]["generationConfig"]["maxOutputTokens"] = json!(output_limit);
    let mut expected = thinking.clone().unwrap_or_else(|| json!({}));
    if let Some(thinking) = thinking {
        envelope["body"]["generationConfig"]["thinkingConfig"] = thinking;
    }
    expected["includeThoughts"] = json!(true);
    let patched = patched_request(model, envelope);
    let generation = &patched["body"]["generationConfig"];
    assert_eq!(generation["thinkingConfig"], expected, "{}", model.id());
    assert_eq!(
        generation["maxOutputTokens"],
        output_limit,
        "{}",
        model.id()
    );
    assert!(
        generation["thinkingConfig"].get("thinkingLevel").is_none()
            || generation["thinkingConfig"].get("thinkingBudget").is_none(),
        "{} must not send both level and budget",
        model.id()
    );
}

fn assert_gemini_thoughts(model: &Model) -> bool {
    if !gemini_thinking_capable(model) {
        let patched = patched_request(model, request_envelope(model));
        assert!(
            patched["body"]["generationConfig"]
                .get("thinkingConfig")
                .is_none(),
            "non-thinking model {} must not request thoughts",
            model.id()
        );
        return false;
    }
    model
        .patches()
        .unwrap_or_else(|| panic!("thinking model {} is missing its catalog patch", model.id()));
    // The builder omits thinkingConfig; existing request settings must also survive.
    for thinking in [
        None,
        Some(json!({"thinkingLevel": "high"})),
        Some(json!({"thinkingBudget": 8192})),
    ] {
        assert_gemini_thinking_settings(model, thinking);
    }
    true
}

#[test]
fn gemini_thinking_models_include_thoughts() {
    for provider_name in ["gemini", "vertexai"] {
        let checked = provider_models(provider_name)
            .iter()
            .filter(|model| assert_gemini_thoughts(model))
            .count();
        assert!(
            checked > 0,
            "no thinking models checked for {provider_name}"
        );
    }
}

#[test]
fn claude_thinking_entries_request_summarized_display() {
    for provider_name in ["claude", "vertexai"] {
        let provider = ALL_PROVIDER_MODELS
            .iter()
            .find(|provider| provider.provider == provider_name)
            .expect("Claude provider catalog");
        let mut checked = 0;
        for model in Model::from_config(provider_name, &provider.models) {
            if !model.real_name().starts_with("claude-") {
                continue;
            }
            let Some(patches) = model.patches() else {
                continue;
            };
            let patched = harnx_core::jaq::eval_filters_strict(patches, request_envelope(&model))
                .unwrap_or_else(|err| panic!("patches for {} failed: {err:#}", model.id()));
            let thinking = &patched["body"]["thinking"];
            if matches!(thinking["type"].as_str(), Some("adaptive" | "enabled")) {
                assert_eq!(
                    thinking["display"],
                    "summarized",
                    "model {} must request readable thinking: {patched}",
                    model.id()
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no thinking entries checked for {provider_name}"
        );
    }
}

/// Returns the container key (`output_config` or `reasoning`) when the patch
/// sets an effort level under it.
fn effort_path(patch: &str) -> Option<&'static str> {
    ["output_config", "reasoning"]
        .into_iter()
        .find(|key| patch.contains(&format!(".body.{key}")) && patch.contains("effort"))
}

fn opus_4_omits_thinking_by_default(family: &str) -> bool {
    family.strip_prefix("opus-4-").is_some_and(|version| {
        // Opus 4 uses a date here (4-20250514), not a minor version.
        let minor = version.split(['-', ':']).next().unwrap();
        minor.len() <= 2 && minor.parse::<u32>().is_ok_and(|minor| minor >= 7)
    })
}

fn bedrock_claude_omits_thinking_by_default(model: &Model) -> bool {
    let Some((_, family)) = model.real_name().split_once("anthropic.claude-") else {
        return false;
    };
    ["opus-5", "sonnet-5", "fable-5"]
        .iter()
        .any(|prefix| family.starts_with(prefix))
        || opus_4_omits_thinking_by_default(family)
}

fn assert_bedrock_summarized_thinking(model: &Model) -> bool {
    if model.patches().is_none() {
        return false;
    }
    let mut envelope = request_envelope(model);
    envelope["body"]["inferenceConfig"] =
        json!({"maxTokens": 24000, "temperature": 0.5, "topP": 0.9});
    let patched = patched_request(model, envelope);
    let thinking = &patched["body"]["additionalModelRequestFields"]["thinking"];
    if !matches!(thinking["type"].as_str(), Some("adaptive" | "enabled")) {
        return false;
    }
    // Converse acceptance remains provisional until the user's two-turn probe.
    assert_eq!(thinking["display"], "summarized", "{}", model.name());
    if thinking["type"] == "enabled" {
        assert_eq!(thinking["budget_tokens"], 16000, "{}", model.name());
    }
    true
}

#[test]
fn bedrock_omitted_default_claude_thinking_requests_summarized_display() {
    let checked = provider_models("bedrock")
        .iter()
        .filter(|model| bedrock_claude_omits_thinking_by_default(model))
        .filter(|model| assert_bedrock_summarized_thinking(model))
        .count();
    assert!(
        checked >= 5,
        "expected at least five Bedrock Claude thinking aliases, checked {checked}"
    );
}

#[test]
fn bedrock_sonnet_5_5_thinking_uses_adaptive_without_losing_output_limit() {
    let bedrock = ALL_PROVIDER_MODELS
        .iter()
        .find(|provider| provider.provider == "bedrock")
        .expect("Bedrock Converse catalog");
    let models = Model::from_config("bedrock", &bedrock.models);
    let model = models
        .iter()
        .find(|model| model.name() == "us.anthropic.claude-sonnet-5-5:thinking")
        .expect("Sonnet 5.5 thinking alias");
    assert_eq!(model.real_name(), "us.anthropic.claude-sonnet-5-5");
    assert_eq!(model.max_tokens_param(), Some(24000));

    for output_limit in [None, Some(24000), Some(4096)] {
        let mut envelope = request_envelope(model);
        envelope["body"]["inferenceConfig"] = json!({"temperature": 0.5, "topP": 0.9});
        if let Some(limit) = output_limit {
            envelope["body"]["inferenceConfig"]["maxTokens"] = json!(limit);
        }
        let messages = envelope["body"]["messages"].clone();
        let patched = harnx_core::jaq::eval_filters_strict(
            model.patches().expect("Sonnet 5.5 thinking patch"),
            envelope,
        )
        .expect("valid Sonnet 5.5 thinking patch");
        assert_eq!(
            patched["body"]["additionalModelRequestFields"],
            json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "high"}
            })
        );
        let inference = &patched["body"]["inferenceConfig"];
        assert_eq!(inference["maxTokens"], json!(output_limit));
        assert!(inference.get("temperature").is_none());
        assert!(inference.get("topP").is_none());
        assert_eq!(patched["body"]["messages"], messages);
    }

    // The correction must not opt the base model into thinking.
    let base = models
        .iter()
        .find(|model| model.name() == "us.anthropic.claude-sonnet-5-5")
        .expect("Sonnet 5.5 base");
    assert!(base.patches().is_none());
    assert_eq!(base.max_output_tokens(), Some(128000));
}
