use super::mutation_result::Report;
use super::*;
use crate::script_result::ResultStyle;

impl FsServer {
    pub(crate) async fn write_file_impl(
        &self,
        params: WriteFileParams,
        style: ResultStyle,
    ) -> Result<CallToolResult, ErrorData> {
        self.write_bytes_at(&params.path, params.content.as_bytes(), params.diff, style)
            .await
    }

    /// The write the sandbox transport uses: it always reports the whole diff
    /// as a model-style result, whoever calls it.
    pub(crate) async fn write_bytes_impl(
        &self,
        requested_path: &str,
        bytes: &[u8],
    ) -> Result<CallToolResult, ErrorData> {
        self.write_bytes_at(
            requested_path,
            bytes,
            Some(DiffLevel::Full),
            ResultStyle::Model,
        )
        .await
    }

    // qual:allow(iosp) — the size check has to run inside the file lock, so the function both decides and calls
    pub(crate) async fn write_bytes_at(
        &self,
        requested_path: &str,
        bytes: &[u8],
        diff: Option<DiffLevel>,
        style: ResultStyle,
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

        let report = Report::new(diff, style);
        let pending = self.begin_mutation(&path, "write_file", report).await;
        write_file_bytes(&path, requested_path, bytes)?;

        let text = std::str::from_utf8(bytes).ok();
        let message = write_summary(requested_path, bytes.len(), text);
        Ok(self.finish_mutation(pending, message, text.is_none()).await)
    }

    pub(super) async fn snapshot_after_binary_write(
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
