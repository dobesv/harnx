//! Keep the first append cause while a batch finishes updating session state.
use anyhow::Result;

pub(crate) struct AppendStatus {
    complete: bool,
    error: Option<anyhow::Error>,
}

impl AppendStatus {
    pub(crate) fn complete() -> Self {
        Self {
            complete: true,
            error: None,
        }
    }

    pub(crate) fn from_result(result: Result<Option<u64>>) -> Self {
        match result {
            Ok(Some(_)) => Self::complete(),
            Ok(None) => Self {
                complete: false,
                error: None,
            },
            Err(error) => Self {
                complete: false,
                error: Some(error),
            },
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(super) fn into_result(self) -> Result<()> {
        match self.error {
            Some(error) => return Err(error),
            None => anyhow::ensure!(self.complete, "session append was dropped"),
        }
        Ok(())
    }
}

impl std::ops::BitAndAssign for AppendStatus {
    fn bitand_assign(&mut self, other: Self) {
        self.complete &= other.complete;
        if self.error.is_none() {
            self.error = other.error;
        }
    }
}

impl std::ops::BitAnd for AppendStatus {
    type Output = Self;

    fn bitand(mut self, other: Self) -> Self {
        self &= other;
        self
    }
}
