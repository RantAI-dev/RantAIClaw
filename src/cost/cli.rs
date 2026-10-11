use super::{ledger_for, types::ModelStats};
use crate::config::Config;
use anyhow::Result;
use std::fmt::Write;

/// `rantaiclaw usage`: today's token total against the daily ceiling, read
/// through the same tracker the ceiling is enforced by.
pub fn usage(config: &Config) -> Result<()> {
    print!("{}", render_usage(config)?);
    Ok(())
}

/// The report as text, so the tests can assert on it without capturing stdout.
///
/// With accounting off nothing is opened or enforced, so the report says so
/// and stops. With the ceiling at `0` the tokens are still counted, and the
/// report says there is no ceiling rather than inventing one.
fn render_usage(config: &Config) -> Result<String> {
    let mut out = String::new();
    let utc_day = chrono::Utc::now().date_naive();
    writeln!(out, "Token usage for {utc_day} (UTC)")?;

    let Some(ledger) = ledger_for(config) else {
        writeln!(
            out,
            "accounting off: [cost] enabled = false, nothing is counted or enforced"
        )?;
        return Ok(out);
    };

    let used = ledger.daily_tokens()?;
    let ceiling = config.cost.max_tokens_per_day;
    writeln!(out, "Tokens today: {used}")?;
    if ceiling == 0 {
        writeln!(
            out,
            "Ceiling: none ([cost] max_tokens_per_day = 0, counting only)"
        )?;
    } else {
        let percent = used.saturating_mul(100) / ceiling;
        writeln!(out, "Ceiling: {ceiling} ({percent}% used)")?;
    }

    let by_model = ledger.daily_by_model()?;
    if !by_model.is_empty() {
        writeln!(out, "By model:")?;
    }
    let mut models: Vec<&ModelStats> = by_model.values().collect();
    models.sort_by(|a, b| a.model.cmp(&b.model));
    for stats in models {
        let cost = match stats.cost_usd {
            Some(cost) => format!("${cost:.4}"),
            None => "not reported".to_string(),
        };
        writeln!(
            out,
            "  {}: {} tokens, {} requests, cost {cost}",
            stats.model, stats.total_tokens, stats.request_count
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{CostConfig, ModelPrice};
    use crate::cost::TokenUsage;
    use chrono::{Duration, Utc};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use tempfile::TempDir;

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_per_million: input,
            output_per_million: output,
        }
    }

    fn config_for(workspace: &TempDir, cost: CostConfig) -> Config {
        Config {
            workspace_dir: workspace.path().to_path_buf(),
            cost,
            ..Config::default()
        }
    }

    /// Append one record per usage to the workspace's `costs.jsonl`, as the
    /// tracker itself stores them.
    fn seed(workspace: &TempDir, usages: Vec<TokenUsage>, malformed_line: bool) {
        let path = workspace.path().join("state").join("costs.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for usage in usages {
            let record = crate::cost::types::CostRecord::new("seeded", usage);
            writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
        }
        if malformed_line {
            writeln!(file, "not-a-json-line").unwrap();
        }
        file.sync_all().unwrap();
    }

    fn enabled(ceiling: u64) -> CostConfig {
        CostConfig {
            enabled: true,
            max_tokens_per_day: ceiling,
            ..Default::default()
        }
    }

    /// Three records today across a priced and an unpriced model, plus one
    /// dated yesterday that must not count.
    #[test]
    fn usage_prints_todays_total_ceiling_and_per_model_lines() {
        let workspace = TempDir::new().unwrap();
        let mut cost = enabled(4_000);
        cost.prices
            .insert("priced/model".to_string(), price(3.0, 15.0));

        let priced_a = TokenUsage::new("priced/model", 1_000, 500, cost.prices.get("priced/model"));
        let priced_b = TokenUsage::new("priced/model", 200, 100, cost.prices.get("priced/model"));
        let unpriced = TokenUsage::new("unpriced/model", 400, 0, None);
        let mut yesterday = TokenUsage::new("unpriced/model", 9_000, 0, None);
        yesterday.timestamp = Utc::now() - Duration::days(1);

        seed(
            &workspace,
            vec![priced_a, priced_b, unpriced, yesterday],
            false,
        );
        let out = render_usage(&config_for(&workspace, cost)).unwrap();

        assert!(
            out.contains("2200"),
            "today's total, yesterday excluded: {out}"
        );
        assert!(out.contains("4000"), "the ceiling comes from config: {out}");
        assert!(out.contains("55%"), "the percentage used: {out}");
        assert!(
            out.contains("  priced/model: 1800 tokens, 2 requests, cost $0.0126"),
            "a priced model shows its cost for today, yesterday excluded: {out}"
        );
        assert!(
            out.contains("  unpriced/model: 400 tokens, 1 requests, cost not reported"),
            "an unpriced model says not reported, never 0.00: {out}"
        );
    }

    #[test]
    fn usage_says_accounting_is_off_when_cost_is_disabled() {
        let workspace = TempDir::new().unwrap();
        let cost = CostConfig {
            enabled: false,
            max_tokens_per_day: 4_000,
            ..Default::default()
        };

        let out = render_usage(&config_for(&workspace, cost)).unwrap();

        assert!(out.contains("accounting off"), "{out}");
        assert!(
            !out.contains("% used"),
            "no ceiling is claimed while nothing is enforced: {out}"
        );
        assert!(
            !workspace.path().join("state").join("costs.jsonl").exists(),
            "a disabled ledger is not opened or created"
        );
    }

    #[test]
    fn usage_says_there_is_no_ceiling_when_it_is_zero() {
        let workspace = TempDir::new().unwrap();
        seed(
            &workspace,
            vec![TokenUsage::new("test/model", 700, 300, None)],
            false,
        );

        let out = render_usage(&config_for(&workspace, enabled(0))).unwrap();

        assert!(out.contains("1000"), "tokens are still shown: {out}");
        assert!(
            out.contains("Ceiling: none"),
            "and the absence says so: {out}"
        );
        assert!(!out.contains("% used"), "{out}");
    }

    #[test]
    fn usage_on_an_empty_workspace_prints_zero_and_succeeds() {
        let workspace = TempDir::new().unwrap();

        let out = render_usage(&config_for(&workspace, enabled(2_000_000))).unwrap();

        assert!(
            out.contains("Tokens today: 0\n"),
            "zero tokens recorded: {out}"
        );
        assert!(out.contains("Ceiling: 2000000 (0% used)"), "{out}");
    }

    /// The malformed line is skipped with the tracker's own warning, and the
    /// total counts the valid records around it.
    #[test]
    fn usage_skips_a_malformed_ledger_line_and_counts_the_rest() {
        let workspace = TempDir::new().unwrap();
        seed(
            &workspace,
            vec![
                TokenUsage::new("test/model", 100, 0, None),
                TokenUsage::new("test/model", 250, 0, None),
            ],
            true,
        );

        let out = render_usage(&config_for(&workspace, enabled(2_000_000))).unwrap();

        assert!(
            out.contains("350"),
            "only the valid lines are summed: {out}"
        );
    }

    #[test]
    fn usage_is_ok_when_it_is_run() {
        let workspace = TempDir::new().unwrap();
        let config = config_for(&workspace, enabled(2_000_000));
        assert!(usage(&config).is_ok());
    }
}
