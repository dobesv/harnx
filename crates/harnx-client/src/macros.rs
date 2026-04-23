#[macro_export]
macro_rules! register_client {
    (
        $(($module:ident, $name:literal, $config:ident, $client:ident),)+
    ) => {
        $(
            pub mod $module;
        )+
        $(
            use harnx_core::provider_config::$module::$config;
        )+

        #[derive(Debug, Clone, serde::Deserialize)]
        #[serde(tag = "type")]
        pub enum ClientConfig {
            $(
                #[serde(rename = $name)]
                $config($config),
            )+
            #[serde(other)]
            Unknown,
        }

        $(
            #[derive(Debug)]
            pub struct $client {
                config: $config,
                model: $crate::Model,
            }

            impl $client {
                pub const NAME: &'static str = $name;

                pub fn init(clients: &[ClientConfig], model: &$crate::Model) -> Option<Box<dyn Client>> {
                    let config = clients.iter().find_map(|client_config| {
                        if let ClientConfig::$config(c) = client_config {
                            if Self::name(c) == model.client_name() {
                                return Some(c.clone())
                            }
                        }
                        None
                    })?;

                    Some(Box::new(Self {
                        config,
                        model: model.clone(),
                    }))
                }

                pub fn list_models(local_config: &$config) -> Vec<Model> {
                    let client_name = Self::name(local_config);
                    let mut models = if local_config.models.is_empty() {
                        if let Some(v) = $crate::ALL_PROVIDER_MODELS.iter().find(|v| {
                            v.provider == $name ||
                                ($name == OpenAICompatibleClient::NAME
                                    && local_config.name.as_ref().map(|name| name.starts_with(&v.provider)).unwrap_or_default())
                        }) {
                            Model::from_config(client_name, &v.models)
                        } else {
                            vec![]
                        }
                    } else {
                        Model::from_config(client_name, &local_config.models)
                    };
                    // Propagate client-level system_prompt_prefix to models
                    if let Some(ref prefix) = local_config.system_prompt_prefix {
                        for model in &mut models {
                            if model.data().system_prompt_prefix.is_none() {
                                model.data_mut().system_prompt_prefix = Some(prefix.clone());
                            }
                        }
                    }
                    models
                }

                pub fn name(local_config: &$config) -> &str {
                    local_config.name.as_deref().unwrap_or(Self::NAME)
                }
            }

        )+

        /// Core client dispatch. The host (harnx) may wrap this with
        /// additional behaviors (e.g., a test-only override that returns
        /// a mock client) before calling it.
        pub fn init_client(clients: &[ClientConfig], model: &$crate::Model) -> anyhow::Result<Box<dyn Client>> {
            None
            $(.or_else(|| $client::init(clients, model)))+
            .ok_or_else(|| {
                anyhow::anyhow!("Invalid model '{}'", model.id())
            })
        }

        pub fn list_client_types() -> Vec<&'static str> {
            let mut client_types: Vec<_> = vec![$($client::NAME,)+];
            client_types.extend($crate::OPENAI_COMPATIBLE_PROVIDERS.iter().map(|(name, _)| *name));
            client_types
        }

        static ALL_CLIENT_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

        pub fn list_client_names(clients: &[ClientConfig]) -> Vec<&'static String> {
            let names = ALL_CLIENT_NAMES.get_or_init(|| {
                clients
                    .iter()
                    .flat_map(|v| match v {
                        $(ClientConfig::$config(c) => vec![$client::name(c).to_string()],)+
                        ClientConfig::Unknown => vec![],
                    })
                    .collect()
            });
            names.iter().collect()
        }

        static ALL_MODELS: std::sync::OnceLock<Vec<$crate::Model>> = std::sync::OnceLock::new();

        pub fn list_all_models(clients: &[ClientConfig]) -> Vec<&'static $crate::Model> {
            let models = ALL_MODELS.get_or_init(|| {
                clients
                    .iter()
                    .flat_map(|v| match v {
                        $(ClientConfig::$config(c) => $client::list_models(c),)+
                        ClientConfig::Unknown => vec![],
                    })
                    .collect()
            });
            models.iter().collect()
        }

        pub fn list_models(clients: &[ClientConfig], model_type: $crate::ModelType) -> Vec<&'static $crate::Model> {
            list_all_models(clients).into_iter().filter(|v| v.model_type() == model_type).collect()
        }

        /// All provider `PROMPTS` lists, paired with each client's public
        /// `NAME`. Used by harnx's `create_client_config` dispatcher to
        /// drive the interactive provider setup flow without requiring
        /// the client layer to depend on `inquire`/spinner UI crates.
        pub fn client_prompts() -> &'static [(&'static str, &'static [$crate::PromptAction<'static>])] {
            &[
                $(
                    ($client::NAME, &$client::PROMPTS),
                )+
            ]
        }
    };
}

#[macro_export]
macro_rules! client_common_fns {
    () => {
        fn extra_config(&self) -> Option<&$crate::ExtraConfig> {
            self.config.extra.as_ref()
        }

        fn patch_config(&self) -> Option<&$crate::RequestPatch> {
            self.config.patch.as_ref()
        }

        fn name(&self) -> &str {
            Self::name(&self.config)
        }

        fn model(&self) -> &Model {
            &self.model
        }

        fn model_mut(&mut self) -> &mut Model {
            &mut self.model
        }
    };
}

#[macro_export]
macro_rules! impl_client_trait {
    (
        $client:ident,
        ($prepare_chat_completions:path, $chat_completions:path, $chat_completions_streaming:path),
        ($prepare_embeddings:path, $embeddings:path),
        ($prepare_rerank:path, $rerank:path),
    ) => {
        #[async_trait::async_trait]
        impl $crate::Client for $crate::$client {
            $crate::client_common_fns!();

            async fn chat_completions_inner(
                &self,
                client: &reqwest::Client,
                data: $crate::ChatCompletionsData,
            ) -> anyhow::Result<$crate::ChatCompletionsOutput> {
                let request_data = $prepare_chat_completions(self, data)?;
                let builder = self.request_builder(client, request_data)?;
                $chat_completions(builder, self.model()).await
            }

            async fn chat_completions_streaming_inner(
                &self,
                client: &reqwest::Client,
                handler: &mut $crate::SseHandler,
                data: $crate::ChatCompletionsData,
            ) -> Result<()> {
                let request_data = $prepare_chat_completions(self, data)?;
                let builder = self.request_builder(client, request_data)?;
                $chat_completions_streaming(builder, handler, self.model()).await
            }

            async fn embeddings_inner(
                &self,
                client: &reqwest::Client,
                data: &$crate::EmbeddingsData,
            ) -> Result<$crate::EmbeddingsOutput> {
                let request_data = $prepare_embeddings(self, data)?;
                let builder = self.request_builder(client, request_data)?;
                $embeddings(builder, self.model()).await
            }

            async fn rerank_inner(
                &self,
                client: &reqwest::Client,
                data: &$crate::RerankData,
            ) -> Result<$crate::RerankOutput> {
                let request_data = $prepare_rerank(self, data)?;
                let builder = self.request_builder(client, request_data)?;
                $rerank(builder, self.model()).await
            }
        }
    };
}

#[macro_export]
macro_rules! config_get_fn {
    ($field_name:ident, $fn_name:ident) => {
        fn $fn_name(&self) -> anyhow::Result<String> {
            let env_prefix = Self::name(&self.config);
            let env_name =
                format!("{}_{}", env_prefix, stringify!($field_name)).to_ascii_uppercase();
            std::env::var(&env_name)
                .ok()
                .or_else(|| self.config.$field_name.clone())
                .ok_or_else(|| anyhow::anyhow!("Miss '{}'", stringify!($field_name)))
        }
    };
}

#[macro_export]
macro_rules! unsupported_model {
    ($name:expr) => {
        anyhow::bail!("Unsupported model '{}'", $name)
    };
}
