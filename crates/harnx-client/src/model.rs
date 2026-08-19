use crate::{list_all_models, list_client_names, ClientConfig, ALL_PROVIDER_MODELS};

use anyhow::{bail, Result};

pub use harnx_core::model::{Model, ModelType, ProviderModels, RequestPatches};

/// Build one client's effective model list from explicit entries followed by
/// every non-overridden provider-catalog entry.
#[doc(hidden)]
pub fn models_for_client_config(
    client_type: &str,
    client_name: &str,
    local_models: &[harnx_core::model::ModelData],
    system_prompt_prefix: Option<&[String]>,
) -> Vec<Model> {
    let catalog = provider_catalog(client_type, client_name);
    let model_data = merge_model_data(local_models, catalog);
    let mut models = Model::from_config(client_name, &model_data);
    apply_system_prompt_prefix(&mut models, system_prompt_prefix);
    models
}

fn provider_catalog(client_type: &str, client_name: &str) -> Option<&'static ProviderModels> {
    ALL_PROVIDER_MODELS.iter().find(|provider| {
        provider.provider == client_type
            || (client_type == "openai-compatible" && client_name.starts_with(&provider.provider))
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
    use crate::{list_all_models, ClientConfig, OpenAIConfig};
    use harnx_core::model::ModelData;

    fn openai_with_models(names: &[&str]) -> ClientConfig {
        ClientConfig::OpenAIConfig(OpenAIConfig {
            name: "openai".to_string(),
            models: names.iter().map(|name| ModelData::new(name)).collect(),
            ..OpenAIConfig::default()
        })
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
