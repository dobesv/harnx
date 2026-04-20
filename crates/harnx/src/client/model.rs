use super::{list_all_models, list_client_names};

use crate::config::Config;

use anyhow::{bail, Result};

pub use harnx_core::model::{Model, ModelType, ProviderModels, RequestPatch};

pub fn retrieve_model(config: &Config, model_id: &str, model_type: ModelType) -> Result<Model> {
    let models = list_all_models(config);
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
                    return Ok((*model).clone());
                } else {
                    bail!("Model '{model_id}' is not a {model_type} model")
                }
            }
            if list_client_names(config)
                .into_iter()
                .any(|v| *v == client_name)
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
                return Ok((*found).clone());
            }
        }
    };
    bail!("Unknown {model_type} model '{model_id}'")
}
