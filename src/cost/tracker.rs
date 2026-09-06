use super::types::{BudgetCheck, CostRecord, CostSummary, ModelStats, TokenUsage, UsagePeriod};
use crate::config::schema::CostConfig;
use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Cost tracker for API usage monitoring and budget enforcement.
pub struct CostTracker {
    config: CostConfig,
    storage: Arc<Mutex<CostStorage>>,
    session_id: String,
    session_costs: Arc<Mutex<Vec<CostRecord>>>,
}

impl CostTracker {
    /// Create a new cost tracker.
    pub fn new(config: CostConfig, workspace_dir: &Path) -> Result<Self> {
        let storage_path = resolve_storage_path(workspace_dir)?;

        let storage = CostStorage::new(&storage_path).with_context(|| {
            format!("Failed to open cost storage at {}", storage_path.display())
        })?;

        Ok(Self {
            config,
            storage: Arc::new(Mutex::new(storage)),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_costs: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lock_storage(&self) -> MutexGuard<'_, CostStorage> {
        self.storage.lock()
    }

    fn lock_session_costs(&self) -> MutexGuard<'_, Vec<CostRecord>> {
        self.session_costs.lock()
    }

    /// May a turn start?
    ///
    /// Measured in tokens already spent today, not in an estimate of what this
    /// turn will cost: the size of a turn is not knowable before it runs, and
    /// the guard this exists to be — a brake on unattended runaway — only needs
    /// to notice that the day's budget is gone.
    ///
    /// `max_tokens_per_day = 0` disables the ceiling while leaving accounting
    /// on.
    pub fn check_budget(&self) -> Result<BudgetCheck> {
        if !self.config.enabled || self.config.max_tokens_per_day == 0 {
            return Ok(BudgetCheck::Allowed);
        }

        let limit = self.config.max_tokens_per_day;
        let used = {
            let mut storage = self.lock_storage();
            storage.get_daily_tokens()?
        };

        if used >= limit {
            return Ok(BudgetCheck::Exceeded {
                used_tokens: used,
                limit_tokens: limit,
                period: UsagePeriod::Day,
            });
        }

        // Percentage of the ceiling, in integers: a f64 ratio here needed two
        // cast lints for no gain.
        let warn_at = limit.saturating_mul(u64::from(self.config.warn_at_percent.min(100))) / 100;
        if used >= warn_at {
            return Ok(BudgetCheck::Warning {
                used_tokens: used,
                limit_tokens: limit,
                period: UsagePeriod::Day,
            });
        }

        Ok(BudgetCheck::Allowed)
    }

    /// Tokens recorded today, across every surface sharing this workspace.
    pub fn daily_tokens(&self) -> Result<u64> {
        let mut storage = self.lock_storage();
        storage.get_daily_tokens()
    }

    /// The price the operator configured for `model`, if any.
    pub fn price_for(&self, model: &str) -> Option<&crate::config::schema::ModelPrice> {
        self.config.prices.get(model)
    }

    /// Build the usage record for a turn, pricing it only if the operator
    /// configured a price for this model.
    pub fn usage_for(&self, model: &str, input_tokens: u64, output_tokens: u64) -> TokenUsage {
        TokenUsage::new(model, input_tokens, output_tokens, self.price_for(model))
    }

    /// Refuse a turn when today's ceiling is spent.
    ///
    /// The error text names the numbers and the key, because the operator
    /// meeting this has to be able to act on it without reading the source.
    pub fn ensure_within_ceiling(&self) -> Result<()> {
        match self.check_budget()? {
            BudgetCheck::Exceeded {
                used_tokens,
                limit_tokens,
                ..
            } => Err(anyhow!(
                "Daily token ceiling reached: {used_tokens} of {limit_tokens} tokens used today. \
                 Raise [cost] max_tokens_per_day, or set it to 0 to disable the ceiling."
            )),
            BudgetCheck::Warning {
                used_tokens,
                limit_tokens,
                ..
            } => {
                tracing::warn!(
                    "Token usage at {used_tokens} of {limit_tokens} for today ([cost] max_tokens_per_day)"
                );
                Ok(())
            }
            BudgetCheck::Allowed => Ok(()),
        }
    }

    /// Record a usage event.
    pub fn record_usage(&self, usage: TokenUsage) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        if usage.cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0) {
            return Err(anyhow!(
                "Token usage cost must be a finite, non-negative value"
            ));
        }

        let record = CostRecord::new(&self.session_id, usage);

        // Persist first for durability guarantees.
        {
            let mut storage = self.lock_storage();
            storage.add_record(record.clone())?;
        }

        // Then update in-memory session snapshot.
        let mut session_costs = self.lock_session_costs();
        session_costs.push(record);

        Ok(())
    }

    /// Get the current usage summary.
    pub fn get_summary(&self) -> Result<CostSummary> {
        let (daily_cost, daily_tokens) = {
            let mut storage = self.lock_storage();
            (storage.get_daily_cost_usd()?, storage.get_daily_tokens()?)
        };

        let session_costs = self.lock_session_costs();
        let session_cost = sum_optional_costs(session_costs.iter().map(|r| r.usage.cost_usd));
        let total_tokens: u64 = session_costs
            .iter()
            .map(|record| record.usage.total_tokens)
            .sum();
        let request_count = session_costs.len();
        let by_model = build_session_model_stats(&session_costs);

        Ok(CostSummary {
            session_cost_usd: session_cost,
            daily_cost_usd: daily_cost,
            daily_tokens,
            total_tokens,
            request_count,
            by_model,
        })
    }

    /// Get the daily cost for a specific date, when anything priced was used.
    pub fn get_daily_cost(&self, date: NaiveDate) -> Result<Option<f64>> {
        let storage = self.lock_storage();
        storage.get_cost_for_date(date)
    }
}

fn resolve_storage_path(workspace_dir: &Path) -> Result<PathBuf> {
    let storage_path = workspace_dir.join("state").join("costs.jsonl");
    let legacy_path = workspace_dir.join(".rantaiclaw").join("costs.db");

    if !storage_path.exists() && legacy_path.exists() {
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        if let Err(error) = fs::rename(&legacy_path, &storage_path) {
            tracing::warn!(
                "Failed to move legacy cost storage from {} to {}: {error}; falling back to copy",
                legacy_path.display(),
                storage_path.display()
            );
            fs::copy(&legacy_path, &storage_path).with_context(|| {
                format!(
                    "Failed to copy legacy cost storage from {} to {}",
                    legacy_path.display(),
                    storage_path.display()
                )
            })?;
        }
    }

    Ok(storage_path)
}

/// Sum costs, where `None` means "no price configured" rather than zero.
///
/// A mix of priced and unpriced records sums the priced ones — the total is a
/// real floor, and reporting nothing because one model lacked a price would
/// discard what was measured. All-unpriced yields `None`, which is what the
/// surfaces render as "not reported".
fn sum_optional_costs(costs: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total: Option<f64> = None;
    for cost in costs.flatten() {
        total = Some(total.unwrap_or(0.0) + cost);
    }
    total
}

fn build_session_model_stats(session_costs: &[CostRecord]) -> HashMap<String, ModelStats> {
    let mut by_model: HashMap<String, ModelStats> = HashMap::new();

    for record in session_costs {
        let entry = by_model
            .entry(record.usage.model.clone())
            .or_insert_with(|| ModelStats {
                model: record.usage.model.clone(),
                cost_usd: None,
                total_tokens: 0,
                request_count: 0,
            });

        if let Some(cost) = record.usage.cost_usd {
            entry.cost_usd = Some(entry.cost_usd.unwrap_or(0.0) + cost);
        }
        entry.total_tokens += record.usage.total_tokens;
        entry.request_count += 1;
    }

    by_model
}

/// Persistent storage for usage records.
struct CostStorage {
    path: PathBuf,
    /// Today's tokens — the quantity the ceiling is enforced against.
    daily_tokens: u64,
    /// Today's cost, `None` until a priced model is recorded.
    daily_cost_usd: Option<f64>,
    cached_day: NaiveDate,
}

impl CostStorage {
    /// Create or open cost storage.
    fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let now = Utc::now();
        let mut storage = Self {
            path: path.to_path_buf(),
            daily_tokens: 0,
            daily_cost_usd: None,
            cached_day: now.date_naive(),
        };

        storage.rebuild_aggregates(storage.cached_day)?;

        Ok(storage)
    }

    fn for_each_record<F>(&self, mut on_record: F) -> Result<()>
    where
        F: FnMut(CostRecord),
    {
        if !self.path.exists() {
            return Ok(());
        }

        let file = File::open(&self.path)
            .with_context(|| format!("Failed to read cost storage from {}", self.path.display()))?;
        let reader = BufReader::new(file);

        for (line_number, line) in reader.lines().enumerate() {
            let raw_line = line.with_context(|| {
                format!(
                    "Failed to read line {} from cost storage {}",
                    line_number + 1,
                    self.path.display()
                )
            })?;

            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<CostRecord>(trimmed) {
                Ok(record) => on_record(record),
                Err(error) => {
                    tracing::warn!(
                        "Skipping malformed cost record at {}:{}: {error}",
                        self.path.display(),
                        line_number + 1
                    );
                }
            }
        }

        Ok(())
    }

    fn rebuild_aggregates(&mut self, day: NaiveDate) -> Result<()> {
        let mut daily_tokens: u64 = 0;
        let mut daily_cost: Option<f64> = None;

        self.for_each_record(|record| {
            if record.usage.timestamp.naive_utc().date() == day {
                daily_tokens = daily_tokens.saturating_add(record.usage.total_tokens);
                if let Some(cost) = record.usage.cost_usd {
                    daily_cost = Some(daily_cost.unwrap_or(0.0) + cost);
                }
            }
        })?;

        self.daily_tokens = daily_tokens;
        self.daily_cost_usd = daily_cost;
        self.cached_day = day;

        Ok(())
    }

    /// Roll the cache over at UTC midnight. The ceiling is a *daily* one, so a
    /// stale cache would carry yesterday's spend into today and refuse turns
    /// that are inside the budget.
    fn ensure_period_cache_current(&mut self) -> Result<()> {
        let day = Utc::now().date_naive();
        if day != self.cached_day {
            self.rebuild_aggregates(day)?;
        }
        Ok(())
    }

    /// Add a new record.
    fn add_record(&mut self, record: CostRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open cost storage at {}", self.path.display()))?;

        // One `write_all` of a line that already carries its newline.
        //
        // `writeln!` on a `File` issues **two** syscalls — body, then newline —
        // so two processes appending at once interleave into a single
        // unparseable line and both records are lost. That was unreachable while
        // nothing wrote here; with the ledger on by default and shared by the
        // gateway, the channels runtime and every CLI run against one workspace,
        // it is the ordinary case. Same fix, same reason, as the audit logger.
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .with_context(|| format!("Failed to write cost record to {}", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync cost storage at {}", self.path.display()))?;

        self.ensure_period_cache_current()?;

        if record.usage.timestamp.naive_utc().date() == self.cached_day {
            self.daily_tokens = self.daily_tokens.saturating_add(record.usage.total_tokens);
            if let Some(cost) = record.usage.cost_usd {
                self.daily_cost_usd = Some(self.daily_cost_usd.unwrap_or(0.0) + cost);
            }
        }

        Ok(())
    }

    /// Tokens recorded today.
    fn get_daily_tokens(&mut self) -> Result<u64> {
        self.ensure_period_cache_current()?;
        Ok(self.daily_tokens)
    }

    /// Today's cost, when anything priced was recorded.
    fn get_daily_cost_usd(&mut self) -> Result<Option<f64>> {
        self.ensure_period_cache_current()?;
        Ok(self.daily_cost_usd)
    }

    /// Get cost for a specific date, when anything priced was recorded.
    fn get_cost_for_date(&self, date: NaiveDate) -> Result<Option<f64>> {
        let mut cost: Option<f64> = None;

        self.for_each_record(|record| {
            if record.usage.timestamp.naive_utc().date() == date {
                if let Some(c) = record.usage.cost_usd {
                    cost = Some(cost.unwrap_or(0.0) + c);
                }
            }
        })?;

        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ModelPrice;
    use tempfile::TempDir;

    fn enabled_config() -> CostConfig {
        CostConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_per_million: input,
            output_per_million: output,
        }
    }

    #[test]
    fn cost_tracker_initialization() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert!(!tracker.session_id().is_empty());
    }

    #[test]
    fn the_ceiling_is_not_enforced_when_accounting_is_off() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: false,
            max_tokens_per_day: 1,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 1000, 500, None))
            .unwrap();
        assert_eq!(tracker.check_budget().unwrap(), BudgetCheck::Allowed);
    }

    /// `0` is the documented way to keep accounting and drop the ceiling.
    #[test]
    fn a_zero_ceiling_disables_enforcement_but_keeps_counting() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            max_tokens_per_day: 0,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 1000, 500, None))
            .unwrap();
        assert_eq!(tracker.check_budget().unwrap(), BudgetCheck::Allowed);
        assert_eq!(tracker.daily_tokens().unwrap(), 1500, "still counted");
    }

    /// The behaviour the whole plan exists for: spend the day's tokens and the
    /// next turn is refused.
    #[test]
    fn the_ceiling_refuses_a_turn_once_the_day_is_spent() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            max_tokens_per_day: 1_000,
            warn_at_percent: 80,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        assert_eq!(
            tracker.check_budget().unwrap(),
            BudgetCheck::Allowed,
            "a fresh day starts allowed"
        );
        assert!(tracker.ensure_within_ceiling().is_ok());

        tracker
            .record_usage(TokenUsage::new("test/model", 600, 500, None))
            .unwrap();

        assert_eq!(
            tracker.check_budget().unwrap(),
            BudgetCheck::Exceeded {
                used_tokens: 1_100,
                limit_tokens: 1_000,
                period: UsagePeriod::Day,
            }
        );
        let err = tracker.ensure_within_ceiling().unwrap_err().to_string();
        assert!(err.contains("1100"), "the error names what was used: {err}");
        assert!(
            err.contains("max_tokens_per_day"),
            "and the key to change: {err}"
        );
    }

    /// The load-bearing other half: a classifier that refused everything would
    /// pass the test above and stop a product that was inside its budget.
    #[test]
    fn usage_below_the_warning_line_is_simply_allowed() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            max_tokens_per_day: 10_000,
            warn_at_percent: 80,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 1_000, 1_000, None))
            .unwrap();

        assert_eq!(tracker.check_budget().unwrap(), BudgetCheck::Allowed);
        assert!(tracker.ensure_within_ceiling().is_ok());
    }

    #[test]
    fn the_warning_line_warns_without_refusing() {
        let tmp = TempDir::new().unwrap();
        let config = CostConfig {
            enabled: true,
            max_tokens_per_day: 1_000,
            warn_at_percent: 80,
            ..Default::default()
        };

        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new("test/model", 850, 0, None))
            .unwrap();

        assert_eq!(
            tracker.check_budget().unwrap(),
            BudgetCheck::Warning {
                used_tokens: 850,
                limit_tokens: 1_000,
                period: UsagePeriod::Day,
            }
        );
        assert!(
            tracker.ensure_within_ceiling().is_ok(),
            "a warning must not refuse the turn"
        );
    }

    #[test]
    fn record_usage_and_get_summary() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        tracker
            .record_usage(TokenUsage::new(
                "test/model",
                1000,
                500,
                Some(&price(1.0, 2.0)),
            ))
            .unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.request_count, 1);
        assert_eq!(summary.daily_tokens, 1500);
        assert!(summary.session_cost_usd.expect("priced") > 0.0);
        assert_eq!(summary.by_model.len(), 1);
    }

    /// Tokens are counted whether or not the operator configured a price — the
    /// ceiling must not depend on a table that is empty by default.
    #[test]
    fn an_unpriced_model_still_counts_toward_the_ceiling() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        tracker
            .record_usage(TokenUsage::new("test/model", 1000, 500, None))
            .unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.daily_tokens, 1500, "tokens count");
        assert_eq!(
            summary.session_cost_usd, None,
            "and money reports nothing rather than 0.00"
        );
    }

    #[test]
    fn the_ledger_prices_a_turn_only_from_the_operators_table() {
        let tmp = TempDir::new().unwrap();
        let mut config = enabled_config();
        config
            .prices
            .insert("priced/model".to_string(), price(3.0, 15.0));
        let tracker = CostTracker::new(config, tmp.path()).unwrap();

        let priced = tracker.usage_for("priced/model", 1_000_000, 0);
        assert_eq!(priced.cost_usd, Some(3.0));

        let unpriced = tracker.usage_for("other/model", 1_000_000, 0);
        assert_eq!(unpriced.cost_usd, None);
        assert_eq!(unpriced.total_tokens, 1_000_000);
    }

    #[test]
    fn summary_by_model_is_session_scoped() {
        let tmp = TempDir::new().unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let old_record = CostRecord::new(
            "old-session",
            TokenUsage::new("legacy/model", 500, 500, Some(&price(1.0, 1.0))),
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&old_record).unwrap()).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        tracker
            .record_usage(TokenUsage::new(
                "session/model",
                1000,
                1000,
                Some(&price(1.0, 1.0)),
            ))
            .unwrap();

        let summary = tracker.get_summary().unwrap();
        assert_eq!(summary.by_model.len(), 1);
        assert!(summary.by_model.contains_key("session/model"));
        assert!(!summary.by_model.contains_key("legacy/model"));
    }

    /// A record written by another process today counts against this process's
    /// ceiling — that is what makes the ceiling a *daily* one and not a
    /// per-process one.
    #[test]
    fn tokens_recorded_by_another_process_count_toward_today() {
        let tmp = TempDir::new().unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let earlier = CostRecord::new(
            "another-process",
            TokenUsage::new("test/model", 900, 100, None),
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&earlier).unwrap()).unwrap();
        file.sync_all().unwrap();

        let config = CostConfig {
            enabled: true,
            max_tokens_per_day: 1_000,
            ..Default::default()
        };
        let tracker = CostTracker::new(config, tmp.path()).unwrap();
        assert_eq!(tracker.daily_tokens().unwrap(), 1_000);
        assert!(matches!(
            tracker.check_budget().unwrap(),
            BudgetCheck::Exceeded { .. }
        ));
    }

    #[test]
    fn malformed_lines_are_ignored_while_loading() {
        let tmp = TempDir::new().unwrap();
        let storage_path = resolve_storage_path(tmp.path()).unwrap();
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let valid_usage = TokenUsage::new("test/model", 1000, 0, Some(&price(1.0, 1.0)));
        let valid_record = CostRecord::new("session-a", valid_usage.clone());

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&valid_record).unwrap()).unwrap();
        writeln!(file, "not-a-json-line").unwrap();
        writeln!(file).unwrap();
        file.sync_all().unwrap();

        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        let today_cost = tracker
            .get_daily_cost(Utc::now().date_naive())
            .unwrap()
            .expect("the valid record was priced");
        assert!((today_cost - valid_usage.cost_usd.unwrap()).abs() < f64::EPSILON);
    }

    /// Concurrent appends must each land as their own line.
    ///
    /// `writeln!` on a `File` is two syscalls, so two writers interleaved into
    /// one unparseable line and BOTH records were lost — silently, because the
    /// reader skips malformed lines. Unreachable while nothing wrote here; the
    /// ordinary case now that the ledger is on by default and shared.
    #[test]
    fn concurrent_appends_do_not_interleave_into_one_line() {
        let tmp = TempDir::new().unwrap();
        // Each writer gets its OWN tracker over the same workspace — the shape
        // of a daemon and a CLI run sharing one profile. Separate trackers hold
        // separate mutexes, so nothing serialises them and the append is the
        // only thing standing between the records and each other.
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let dir = tmp.path().to_path_buf();
                std::thread::spawn(move || {
                    let tracker = CostTracker::new(enabled_config(), &dir).unwrap();
                    for _ in 0..64 {
                        tracker
                            .record_usage(TokenUsage::new(format!("model/{i}"), 10, 0, None))
                            .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }

        // Every record parsed back: 8 threads × 16 records × 10 tokens.
        let reread = CostTracker::new(enabled_config(), tmp.path()).unwrap();
        assert_eq!(
            reread.daily_tokens().unwrap(),
            8 * 64 * 10,
            "a torn line loses both records it was made of"
        );
    }

    #[test]
    fn a_non_finite_cost_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let tracker = CostTracker::new(enabled_config(), tmp.path()).unwrap();

        let mut usage = TokenUsage::new("test/model", 10, 10, None);
        usage.cost_usd = Some(f64::NAN);
        let err = tracker.record_usage(usage).unwrap_err();
        assert!(err
            .to_string()
            .contains("Token usage cost must be a finite, non-negative value"));
    }
}
