//! RAG configuration methods extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub async fn use_rag(
        config: &GlobalConfig,
        rag: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if config.read().agent.is_some() {
            bail!("Cannot perform this operation because you are using a agent")
        }
        let rag = match rag {
            None => {
                let rag_path = config.read().rag_file(TEMP_RAG_NAME);
                if rag_path.exists() {
                    remove_file(&rag_path).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_RAG_NAME}' rag")
                    })?;
                }
                let (
                    clients_owned,
                    loaders_owned,
                    rag_embedding_model_owned,
                    rag_reranker_model,
                    rag_top_k,
                    rag_chunk_size,
                    rag_chunk_overlap,
                    user_agent_owned,
                    dry_run,
                ) = {
                    let cfg = config.read();
                    (
                        cfg.clients.clone(),
                        cfg.document_loaders.clone(),
                        cfg.rag_embedding_model.clone(),
                        cfg.rag_reranker_model.clone(),
                        cfg.rag_top_k,
                        cfg.rag_chunk_size,
                        cfg.rag_chunk_overlap,
                        cfg.user_agent.clone(),
                        cfg.dry_run,
                    )
                };
                let init_ctx = harnx_rag::RagInitContext {
                    clients: &clients_owned,
                    document_loaders: &loaders_owned,
                    rag_embedding_model: rag_embedding_model_owned.as_deref(),
                    rag_reranker_model,
                    rag_top_k,
                    rag_chunk_size,
                    rag_chunk_overlap,
                    user_agent: user_agent_owned.as_deref(),
                    dry_run,
                };
                Rag::init(&init_ctx, TEMP_RAG_NAME, &rag_path, &[], abort_signal).await?
            }
            Some(name) => {
                let rag_path = config.read().rag_file(name);
                if !rag_path.exists() {
                    if config.read().working_mode.is_cmd() {
                        bail!("Unknown RAG '{name}'")
                    }
                    let (
                        clients_owned,
                        loaders_owned,
                        rag_embedding_model_owned,
                        rag_reranker_model,
                        rag_top_k,
                        rag_chunk_size,
                        rag_chunk_overlap,
                        user_agent_owned,
                        dry_run,
                    ) = {
                        let cfg = config.read();
                        (
                            cfg.clients.clone(),
                            cfg.document_loaders.clone(),
                            cfg.rag_embedding_model.clone(),
                            cfg.rag_reranker_model.clone(),
                            cfg.rag_top_k,
                            cfg.rag_chunk_size,
                            cfg.rag_chunk_overlap,
                            cfg.user_agent.clone(),
                            cfg.dry_run,
                        )
                    };
                    let init_ctx = harnx_rag::RagInitContext {
                        clients: &clients_owned,
                        document_loaders: &loaders_owned,
                        rag_embedding_model: rag_embedding_model_owned.as_deref(),
                        rag_reranker_model,
                        rag_top_k,
                        rag_chunk_size,
                        rag_chunk_overlap,
                        user_agent: user_agent_owned.as_deref(),
                        dry_run,
                    };
                    Rag::init(&init_ctx, name, &rag_path, &[], abort_signal).await?
                } else {
                    Rag::load(&config.read().clients, name, &rag_path)?
                }
            }
        };
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn edit_rag_docs(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        let document_paths = rag.document_paths();
        let temp_file = temp_file(&format!("-rag-{}", rag.name()), ".txt");
        tokio::fs::write(&temp_file, &document_paths.join("\n"))
            .await
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut config_write = config.write();
            let editor = config_write.editor()?;
            let temp_file_path = temp_file.clone();
            config_write.edit_with_tui_hooks(|_| {
                let result = edit_file(&editor, &temp_file_path);
                let _ = tx.send(result);
                Ok(())
            })?;
        }
        rx.await
            .map_err(|_| anyhow!("Editor hook completion channel unexpectedly closed"))??;
        let new_document_paths = tokio::fs::read_to_string(&temp_file)
            .await
            .with_context(|| format!("Failed to read '{}'", temp_file.display()))?;
        let new_document_paths = new_document_paths
            .split('\n')
            .filter_map(|v| {
                let v = v.trim();
                if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                }
            })
            .collect::<Vec<_>>();
        if new_document_paths.is_empty() || new_document_paths == document_paths {
            bail!("No changes")
        }
        let (document_loaders, user_agent_owned, dry_run) = {
            let cfg = config.read();
            (
                cfg.document_loaders.clone(),
                cfg.user_agent.clone(),
                cfg.dry_run,
            )
        };
        let call_ctx = harnx_rag::RagCallContext {
            user_agent: user_agent_owned.as_deref(),
            dry_run,
        };
        rag.refresh_document_paths(
            &new_document_paths,
            false,
            &document_loaders,
            &call_ctx,
            abort_signal,
        )
        .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn rebuild_rag(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };
        let document_paths = rag.document_paths().to_vec();
        let (document_loaders, user_agent_owned, dry_run) = {
            let cfg = config.read();
            (
                cfg.document_loaders.clone(),
                cfg.user_agent.clone(),
                cfg.dry_run,
            )
        };
        let call_ctx = harnx_rag::RagCallContext {
            user_agent: user_agent_owned.as_deref(),
            dry_run,
        };
        rag.refresh_document_paths(
            &document_paths,
            true,
            &document_loaders,
            &call_ctx,
            abort_signal,
        )
        .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub fn rag_sources(config: &GlobalConfig) -> Result<String> {
        match config.read().rag.as_ref() {
            Some(rag) => match rag.get_last_sources() {
                Some(v) => Ok(v),
                None => bail!("No sources"),
            },
            None => bail!("No RAG"),
        }
    }

    pub fn rag_info(&self) -> Result<String> {
        if let Some(rag) = &self.rag {
            rag.export()
        } else {
            bail!("No RAG")
        }
    }

    pub fn exit_rag(&mut self) -> Result<()> {
        self.rag.take();
        Ok(())
    }

    pub async fn search_rag(
        config: &GlobalConfig,
        rag: &Rag,
        text: &str,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (reranker_model, top_k) = rag.get_config();
        let (embeddings, ids) = {
            let (user_agent_owned, dry_run) = {
                let cfg = config.read();
                (cfg.user_agent.clone(), cfg.dry_run)
            };
            let call_ctx = harnx_rag::RagCallContext {
                user_agent: user_agent_owned.as_deref(),
                dry_run,
            };
            rag.search(
                &call_ctx,
                text,
                top_k,
                reranker_model.as_deref(),
                abort_signal,
            )
            .await?
        };
        let text = config.read().rag_template(&embeddings, text);
        rag.set_last_sources(&ids);
        Ok(text)
    }

    pub fn list_rags() -> Vec<String> {
        match read_dir(Self::rags_dir()) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    if let Some(name) = name.to_string_lossy().strip_suffix(".yaml") {
                        names.push(name.to_string());
                    }
                }
                names.sort_unstable();
                names
            }
            Err(_) => vec![],
        }
    }

    pub fn rag_template(&self, embeddings: &str, text: &str) -> String {
        if embeddings.is_empty() {
            return text.to_string();
        }
        self.rag_template
            .as_deref()
            .unwrap_or(RAG_TEMPLATE)
            .replace("__CONTEXT__", embeddings)
            .replace("__INPUT__", text)
    }
}
