pub mod tracker;
pub mod types;

#[allow(unused_imports)]
pub use tracker::CostTracker;
#[allow(unused_imports)]
pub use types::{BudgetCheck, CostRecord, CostSummary, ModelStats, TokenUsage, UsagePeriod};

/// The process's token ledger for this config, when accounting is on.
///
/// `None` when `[cost] enabled = false` — the operator turned the ceiling and
/// the usage record off together — or when the store cannot be opened. A ledger
/// that cannot be opened must not take the turn down with it: the failure is
/// logged and the turn proceeds unbraked, which is what the product did before
/// this existed at all.
pub fn ledger_for(config: &crate::config::Config) -> Option<std::sync::Arc<CostTracker>> {
    if !config.cost.enabled {
        return None;
    }
    match CostTracker::new(config.cost.clone(), &config.workspace_dir) {
        Ok(tracker) => Some(std::sync::Arc::new(tracker)),
        Err(e) => {
            tracing::warn!(
                "token accounting disabled — could not open the usage store: {e:#}. \
                 The daily ceiling is not enforced for this process."
            );
            None
        }
    }
}
