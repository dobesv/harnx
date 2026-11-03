use super::*;

impl FsServer {
    pub(crate) async fn write_file_impl(
        &self,
        params: WriteFileParams,
    ) -> Result<CallToolResult, ErrorData> {
        self.write_bytes_impl(&params.path, params.content.as_bytes())
            .await
    }

    pub(crate) async fn write_bytes_impl(
        &self,
        requested_path: &str,
        bytes: &[u8],
    ) -> Result<CallToolResult, ErrorData> {
        let path = validate_write_path(requested_path, &self.allowlist).map_err(invalid_params)?;
        let repo_lock = self.repo_lock_for_path(&path);
        let _repo_guard = repo_lock.read().await;
        let file_lock = self.lock_for_path(&path);
        let _file_guard = file_lock.lock().await;

        if bytes.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "Content too large ({}, max {})",
                format_size(bytes.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        let before_snap = self.snapshot_before(&path, "before write_file").await;
        write_file_bytes(&path, requested_path, bytes)?;

        let text = std::str::from_utf8(bytes).ok();
        let diff = if text.is_some() {
            self.snapshot_after_diff(&path, before_snap, "after write_file")
                .await
        } else {
            self.snapshot_after_binary_write(&path, before_snap).await
        };

        Ok(Self::mutation_success(
            write_summary(requested_path, bytes.len(), text),
            diff,
        ))
    }

    async fn snapshot_after_binary_write(
        &self,
        path: &Path,
        before: Option<gix::ObjectId>,
    ) -> Option<String> {
        before?;
        // Git's binary detection is NUL-based, not UTF-8-based. Skip rendering
        // entirely but keep the snapshot SHA available for rollback_file.
        self.history
            .snapshot_file(path, "after write_file")
            .await
            .map(|after| {
                format!(
                    "commit {after}\n    after write_file\n\nBinary file changed (diff omitted)."
                )
            })
            .map_err(|e| log::warn!("history after-snapshot failed: {e}"))
            .ok()
    }
}

fn write_file_bytes(path: &Path, requested_path: &str, bytes: &[u8]) -> Result<(), ErrorData> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            internal_error(format!(
                "failed to create directories for '{requested_path}': {err}"
            ))
        })?;
    }

    std::fs::write(path, bytes)
        .map_err(|err| internal_error(format!("failed to write '{requested_path}': {err}")))
}

fn write_summary(path: &str, byte_count: usize, text: Option<&str>) -> String {
    match text {
        Some(text) => format!(
            "Wrote {} ({} lines) to {}",
            format_size(byte_count),
            text.lines().count(),
            path
        ),
        None => format!("Wrote binary file, {byte_count} bytes written to {path}"),
    }
}
