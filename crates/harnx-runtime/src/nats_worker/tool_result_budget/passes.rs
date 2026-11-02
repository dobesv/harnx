//! Size accounting shared by the full, short, and minimal replacement passes.
use super::{
    degraded_len, replace_if_smaller, serialized_len, serialized_output_len, size_error_output,
    ToolResultsBudget,
};
use anyhow::Result;
use harnx_core::session::{SessionLogEntry, ToolOutput};

pub(super) struct BudgetPasses {
    results: Vec<ToolOutput>,
    sizes: Vec<usize>,
    order: Vec<usize>,
    retained: Vec<Option<String>>,
    total: usize,
}

impl BudgetPasses {
    pub(super) fn new(results: &[ToolOutput], total: usize) -> Result<Self> {
        let sizes: Vec<usize> = results
            .iter()
            .map(serialized_output_len)
            .collect::<Result<_>>()?;
        // Keep the original order for all passes, largest serialized output first.
        let mut order: Vec<usize> = (0..results.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(sizes[i]));
        Ok(Self {
            results: results.to_vec(),
            retained: vec![None; results.len()],
            sizes,
            order,
            total,
        })
    }

    pub(super) async fn retain_full_messages(
        &mut self,
        budget: &ToolResultsBudget<'_>,
    ) -> Result<()> {
        for position in 0..self.order.len() {
            if self.total <= budget.budget {
                break;
            }
            self.try_full_message(self.order[position], budget).await?;
        }
        Ok(())
    }

    async fn try_full_message(&mut self, i: usize, budget: &ToolResultsBudget<'_>) -> Result<()> {
        // Skip retention when even the full error's lower bound cannot shrink
        // this output. Shorter forms still get a chance in the fallback passes.
        if self.sizes[i] <= degraded_len(&self.results[i])? {
            return Ok(());
        }
        let replacement = budget
            .degrade(&self.results[i], &mut self.retained[i])
            .await?;
        // The real byte count and URL weren't known to the pre-retention bound.
        self.replace(i, replacement)
    }

    pub(super) fn apply_fallback(
        &mut self,
        budget: usize,
        message: impl Fn(Option<&str>) -> Option<String>,
    ) -> Result<()> {
        for position in 0..self.order.len() {
            if self.total <= budget {
                break;
            }
            self.try_fallback(self.order[position], &message)?;
        }
        Ok(())
    }

    fn try_fallback(
        &mut self,
        i: usize,
        message: &impl Fn(Option<&str>) -> Option<String>,
    ) -> Result<()> {
        let Some(message) = message(self.retained[i].as_deref()) else {
            return Ok(());
        };
        self.replace(i, size_error_output(&self.results[i], message))
    }

    fn replace(&mut self, i: usize, replacement: ToolOutput) -> Result<()> {
        replace_if_smaller(
            &mut self.results[i],
            replacement,
            &mut self.sizes[i],
            &mut self.total,
        )?;
        Ok(())
    }

    pub(super) fn finish(
        self,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<SessionLogEntry> {
        let bounded = SessionLogEntry::ToolResults {
            results: self.results,
            timestamp,
        };
        debug_assert_eq!(self.total, serialized_len(&bounded)?);
        Ok(bounded)
    }
}
