use crate::{list_all_models, list_client_names, ClientConfig, ALL_PROVIDER_MODELS};

use anyhow::{bail, Result};

pub use harnx_core::model::{Model, ModelType, ProviderModels, RequestPatches};

/// The parts of a client's config that decide which models it offers.
///
/// Assembled by `register_client!` from whichever config struct the client
/// uses, so every provider reaches the shared resolution the same way.
#[doc(hidden)]
pub struct ClientModelSources<'a> {
    pub client_type: &'a str,
    pub client_name: &'a str,
    pub model_catalog: Option<&'a str>,
    pub local_models: &'a [harnx_core::model::ModelData],
    pub system_prompt_prefix: Option<&'a [String]>,
}

/// Build one client's effective model list from explicit entries followed by
/// every non-overridden provider-catalog entry.
#[doc(hidden)]
pub fn models_for_client_config(sources: ClientModelSources<'_>) -> Vec<Model> {
    let ClientModelSources {
        client_type,
        client_name,
        model_catalog,
        local_models,
        system_prompt_prefix,
    } = sources;
    let catalog = provider_catalog(client_type, client_name, model_catalog);
    let model_data = merge_model_data(local_models, catalog);
    let mut models = Model::from_config(client_name, &model_data);
    apply_system_prompt_prefix(&mut models, system_prompt_prefix);
    models
}

/// Map a client type to the catalog block it should inherit models from.
///
/// Most clients use the block whose `provider` matches their type. Two
/// exceptions borrow another provider's catalog so users get the full model
/// list (context sizes, pricing, capabilities, the `responses` endpoint)
/// automatically as `models.yaml` is regenerated, with no per-user upkeep:
/// - `openai-compatible` clients match by provider-name prefix.
/// - `codex` (ChatGPT subscription) reuses the `openai` catalog, since it
///   speaks the same OpenAI Responses API and serves the same models.
fn catalog_provider_for(client_type: &str) -> &str {
    match client_type {
        "codex" => "openai",
        other => other,
    }
}

/// Resolve the catalog block, preferring the config's own `model_catalog`.
///
/// Naming a catalog explicitly is the supported way to inherit one. The
/// filename-derived fallback below predates that field and stays only so
/// existing configs keep working: it makes the client's name load-bearing,
/// which silently denies a catalog to a sensibly-named client such as
/// `aws-prod.yaml` and just as silently hands one to `deepseek-proxy.yaml`.
fn provider_catalog(
    client_type: &str,
    client_name: &str,
    model_catalog: Option<&str>,
) -> Option<&'static ProviderModels> {
    if let Some(requested) = model_catalog.map(str::trim).filter(|v| !v.is_empty()) {
        let found = ALL_PROVIDER_MODELS
            .iter()
            .find(|provider| provider.provider == requested);
        if found.is_none() {
            // Falling back to the filename would hide the typo behind a
            // catalog the user did not ask for, so report it and inherit
            // nothing; any explicit `models:` entries still apply.
            log::warn!(
                "client '{client_name}': unknown model_catalog '{requested}', inheriting no catalog models"
            );
        }
        return found;
    }

    let catalog_provider = catalog_provider_for(client_type);
    // Package clients keep the same provider prefix as their bare filename.
    let bare_name = client_name.rsplit('/').next().unwrap_or(client_name);
    ALL_PROVIDER_MODELS.iter().find(|provider| {
        provider.provider == catalog_provider
            || (client_type == "openai-compatible" && bare_name.starts_with(&provider.provider))
    })
}

fn merge_model_data(
    local_models: &[harnx_core::model::ModelData],
    catalog: Option<&ProviderModels>,
) -> Vec<harnx_core::model::ModelData> {
    let mut merged = local_models.to_vec();
    let Some(catalog) = catalog else {
        return merged;
    };
    for catalog_model in &catalog.models {
        if !merged.iter().any(|local| local.name == catalog_model.name) {
            merged.push(catalog_model.clone());
        }
    }
    merged
}

fn apply_system_prompt_prefix(models: &mut [Model], prefix: Option<&[String]>) {
    let Some(prefix) = prefix else {
        return;
    };
    for model in models {
        if model.data().system_prompt_prefix.is_none() {
            model.data_mut().system_prompt_prefix = Some(prefix.to_vec());
        }
    }
}

pub fn retrieve_model(
    clients: &[ClientConfig],
    model_id: &str,
    model_type: ModelType,
) -> Result<Model> {
    let models = list_all_models(clients);
    let (client_name, model_name) = match model_id.split_once(':') {
        Some((client_name, model_name)) => {
            if model_name.is_empty() {
                (client_name, None)
            } else {
                (client_name, Some(model_name))
            }
        }
        None => (model_id, None),
    };
    match model_name {
        Some(model_name) => {
            if let Some(model) = models.iter().find(|v| v.id() == model_id) {
                if model.model_type() == model_type {
                    return Ok(model.clone());
                } else {
                    bail!("Model '{model_id}' is not a {model_type} model")
                }
            }
            if list_client_names(clients)
                .into_iter()
                .any(|v| v == client_name)
                && model_type.can_create_from_name()
            {
                let mut new_model = Model::new(client_name, model_name);
                new_model.data_mut().model_type = model_type.to_string();
                return Ok(new_model);
            }
        }
        None => {
            if let Some(found) = models
                .iter()
                .find(|v| v.client_name() == client_name && v.model_type() == model_type)
            {
                return Ok(found.clone());
            }
        }
    };
    bail!("Unknown {model_type} model '{model_id}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{list_all_models, ClientConfig, CodexConfig, OpenAIConfig};
    use harnx_core::model::ModelData;

    /// Resolve a client's models the way `register_client!` does, minus the
    /// system-prompt prefix these cases do not exercise.
    fn catalog_models(
        client_type: &str,
        client_name: &str,
        model_catalog: Option<&str>,
        local_models: &[ModelData],
    ) -> Vec<Model> {
        models_for_client_config(ClientModelSources {
            client_type,
            client_name,
            model_catalog,
            local_models,
            system_prompt_prefix: None,
        })
    }

    fn openai_with_models(names: &[&str]) -> ClientConfig {
        ClientConfig::OpenAIConfig(OpenAIConfig {
            name: "openai".to_string(),
            models: names.iter().map(|name| ModelData::new(name)).collect(),
            ..OpenAIConfig::default()
        })
    }

    #[test]
    fn codex_client_inherits_openai_model_catalog() {
        // No models declared: the codex client should still resolve OpenAI
        // catalog models (with the responses endpoint + capabilities) so users
        // get new models automatically from harnx updates, not manual config.
        let clients = vec![ClientConfig::CodexConfig(CodexConfig {
            name: "codex".to_string(),
            ..CodexConfig::default()
        })];

        let model = retrieve_model(&clients, "codex:gpt-5", ModelType::Chat)
            .expect("codex client should resolve gpt-5 from the OpenAI catalog");

        assert_eq!(model.endpoint(), Some("responses"));
        assert!(model.supports_tool_use());
        assert!(model.max_input_tokens().is_some());
        assert!(
            list_all_models(&clients)
                .iter()
                .any(|model| model.id() == "codex:gpt-5"),
            "catalog models should be listed under the codex client name"
        );
    }

    #[test]
    fn package_compatible_clients_inherit_catalog_and_keep_local_overrides() {
        let mut override_model = ModelData::new("zai.glm-5");
        override_model.max_input_tokens = Some(12345);
        for name in ["bedrock", "pantheon/bedrock", "coding/bedrock-us"] {
            let models = catalog_models("openai-compatible", name, None, &[override_model.clone()]);
            let glm = models.iter().find(|m| m.name() == "zai.glm-5").unwrap();
            assert_eq!(glm.client_name(), name);
            assert_eq!(glm.max_input_tokens(), Some(12345));
            let minimax = models
                .iter()
                .find(|m| m.name() == "minimax.minimax-m2.5")
                .expect("inherited Bedrock model");
            assert_eq!(minimax.client_name(), name);
            assert!(minimax.supports_tool_use());
            assert_eq!(minimax.max_input_tokens(), Some(196000));
        }
    }

    /// The whole point of the field: a client whose filename says nothing
    /// about a provider can still inherit that provider's catalog.
    #[test]
    fn explicit_catalog_frees_the_client_from_its_filename() {
        let models = catalog_models("openai-compatible", "aws-prod", Some("bedrock"), &[]);
        let glm = models
            .iter()
            .find(|m| m.name() == "zai.glm-5")
            .expect("named catalog should be inherited");
        assert_eq!(glm.client_name(), "aws-prod");
        assert_eq!(glm.input_price(), Some(1.0));
    }

    /// Without it, that same client silently gets nothing — which is the
    /// behaviour the field exists to fix.
    #[test]
    fn filename_fallback_gives_an_unrecognised_name_no_catalog() {
        let models = catalog_models("openai-compatible", "aws-prod", None, &[]);
        assert!(models.is_empty());
    }

    #[test]
    fn explicit_catalog_overrides_what_the_filename_would_have_picked() {
        let models = catalog_models("openai-compatible", "deepseek-proxy", Some("bedrock"), &[]);
        assert!(models.iter().any(|m| m.name() == "zai.glm-5"));
        assert!(
            !models.iter().any(|m| m.name().starts_with("deepseek-")),
            "the filename's DeepSeek catalog must not leak in"
        );
    }

    /// A typo must not silently fall back to the filename's catalog: that
    /// would hand the user models they did not ask for and hide the mistake.
    #[test]
    fn unknown_catalog_inherits_nothing_but_keeps_local_models() {
        let local = ModelData::new("my-model");
        let models = catalog_models(
            "openai-compatible",
            "bedrock",
            Some("bedrok"),
            std::slice::from_ref(&local),
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name(), "my-model");
    }

    #[test]
    fn blank_catalog_is_treated_as_unset() {
        for value in ["", "   "] {
            let models = catalog_models("openai-compatible", "bedrock", Some(value), &[]);
            assert!(
                models.iter().any(|m| m.name() == "zai.glm-5"),
                "blank {value:?} should fall through to the filename"
            );
        }
    }

    /// Native clients pick their catalog from the client type, and naming
    /// one explicitly still works.
    #[test]
    fn explicit_catalog_applies_to_native_clients_too() {
        let models = catalog_models("claude", "my-claude", Some("gemini"), &[]);
        assert!(models.iter().any(|m| m.name().starts_with("gemini-")));
        assert!(!models.iter().any(|m| m.name().starts_with("claude-")));
    }

    #[test]
    fn custom_models_do_not_hide_embedded_provider_metadata() {
        let clients = vec![openai_with_models(&["test-model"])];

        let model = retrieve_model(&clients, "openai:gpt-5.6-sol", ModelType::Chat)
            .expect("embedded OpenAI model remains resolvable");

        assert_eq!(model.endpoint(), Some("responses"));
        assert!(model.supports_tool_use());
        assert!(
            list_all_models(&clients)
                .iter()
                .any(|model| model.id() == "openai:test-model"),
            "the custom model must remain available"
        );
    }

    #[test]
    fn model_lists_follow_the_current_client_configuration() {
        let first = vec![openai_with_models(&["first-custom-model"])];
        let second = vec![openai_with_models(&["second-custom-model"])];

        assert!(list_all_models(&first)
            .iter()
            .any(|model| model.name() == "first-custom-model"));
        let reloaded = list_all_models(&second);
        assert!(reloaded
            .iter()
            .any(|model| model.name() == "second-custom-model"));
        assert!(!reloaded
            .iter()
            .any(|model| model.name() == "first-custom-model"));
    }
}
