//! `/v1/models` catalog construction.

use harnx_runtime::client::list_all_models;
use harnx_runtime::config::Config;
use serde_json::{json, Value};

/// Model id advertised for the frontend's active (default) model.
pub(crate) const DEFAULT_MODEL_NAME: &str = "default";

/// Build the `/v1/models` list: real client models, optionally preceded by the
/// active model advertised under the `"default"` alias.
///
/// The alias is only emitted when a model actually resolved. A remote-only
/// frontend has no local model, so listing a synthetic `"default"` with an empty
/// id/owner would be misleading. Each entry carries its advertised id explicitly
/// so dropping the alias never relabels the first real model as `"default"`.
pub(crate) fn advertised_models(config: &Config) -> Vec<Value> {
    let default_alias = (!config.model.id().is_empty())
        .then(|| (config.model.clone(), DEFAULT_MODEL_NAME.to_string()));
    default_alias
        .into_iter()
        .chain(list_all_models(&config.clients).into_iter().map(|model| {
            let id = model.id();
            (model, id)
        }))
        .map(|(model, id)| {
            let mut value = json!(model.data());
            if let Some(value_obj) = value.as_object_mut() {
                value_obj.insert("id".into(), id.into());
                value_obj.insert("object".into(), "model".into());
                value_obj.insert("owned_by".into(), model.client_name().into());
                value_obj.remove("name");
            }
            value
        })
        .collect()
}
