//! What both `read_exec_log` paths share: loading a log's matching lines and
//! selecting the window a call asked for.

use super::{BashServer, ErrorData, Path, PathBuf, ReadExecLogParams};

/// Lines a read returns when the call names no `limit`.
const DEFAULT_LOG_LIMIT: usize = 200;

/// The numbered lines a read selected and the notices that came with them.
pub(crate) type SelectedLines = (Vec<(usize, String)>, Vec<String>);

/// A log's matching lines, with notes from reading it.
pub(crate) struct LoadedLog {
    pub path: PathBuf,
    pub lines: Vec<(usize, String)>,
    pub notices: Vec<String>,
}

/// The error for an `offset` past the last matching line of a log that has
/// matching lines.
pub(crate) fn offset_error(path: &Path, offset: Option<usize>, total: usize) -> Option<String> {
    offset.filter(|offset| *offset > total).map(|offset| {
        format!(
            "offset {offset} is beyond the {total} matching lines in {}",
            path.display()
        )
    })
}

impl BashServer {
    /// The path of the log a call names, after checking the stream and that
    /// the file can be read.
    pub(crate) async fn open_log(&self, params: &ReadExecLogParams) -> Result<PathBuf, ErrorData> {
        Self::validate_read_exec_log_params(params)?;
        let path = self.resolve_log_path(&params.execution_id, &params.stream)?;
        self.ensure_regular_log_file(&path).await?;
        Ok(path)
    }

    /// The log's lines that match the call's `grep`, with notes about it.
    pub(crate) fn filter_log_lines(
        path: PathBuf,
        content: &str,
        params: &ReadExecLogParams,
    ) -> Result<LoadedLog, ErrorData> {
        let grep_regex = Self::build_grep_regex(params.grep.as_deref())?;
        let (lines, regex_error) = Self::collect_matching_log_lines(content, grep_regex.as_ref());
        Ok(LoadedLog {
            path,
            lines,
            notices: Self::build_regex_notices(regex_error),
        })
    }

    /// The lines a read selects, after checking `offset`, with the notice a
    /// `tail` window adds.
    pub(crate) fn select_checked_log_lines(
        numbered_lines: Vec<(usize, String)>,
        params: &ReadExecLogParams,
    ) -> Result<SelectedLines, ErrorData> {
        Self::reject_zero_offset(params)?;
        let limit = params.limit.unwrap_or(DEFAULT_LOG_LIMIT).max(1);
        Ok(Self::select_log_lines(
            numbered_lines,
            params.offset,
            limit,
            params.tail,
        ))
    }

    fn reject_zero_offset(params: &ReadExecLogParams) -> Result<(), ErrorData> {
        match params.offset {
            Some(0) => Err(ErrorData::invalid_params("offset must be >= 1", None)),
            _ => Ok(()),
        }
    }
}
