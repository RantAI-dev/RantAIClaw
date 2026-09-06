use serde::{Deserialize, Serialize};

/// Token usage information from a single API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4-20250514")
    pub model: String,
    /// Input/prompt tokens
    pub input_tokens: u64,
    /// Output/completion tokens
    pub output_tokens: u64,
    /// Total tokens
    pub total_tokens: u64,
    /// Cost in USD, or `None` when the operator configured no price for this
    /// model.
    ///
    /// **Not a zeroed default.** There is no price source in the product — the
    /// bundled table was deleted as dead in schema v25 — so a `0.0` here would
    /// be a claim about spend that nothing measured, on every install that never
    /// filled in `[cost.prices]`. `None` is the answer the surfaces render as
    /// "not reported" (plan 306 step 6).
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Timestamp of the request
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl TokenUsage {
    fn sanitize_price(value: f64) -> Option<f64> {
        (value.is_finite() && value > 0.0).then_some(value)
    }

    /// Create a usage record, pricing it only if the operator supplied a price
    /// for this model.
    pub fn new(
        model: impl Into<String>,
        input_tokens: u64,
        output_tokens: u64,
        price: Option<&crate::config::schema::ModelPrice>,
    ) -> Self {
        let model = model.into();
        let total_tokens = input_tokens.saturating_add(output_tokens);

        // A price with a non-positive or non-finite side is not a price. Half a
        // price would produce a number that looks measured and is not, so the
        // whole record reports nothing rather than half of something.
        let cost_usd = price.and_then(|p| {
            let input_price = Self::sanitize_price(p.input_per_million)?;
            let output_price = Self::sanitize_price(p.output_per_million)?;
            let input_cost = (input_tokens as f64 / 1_000_000.0) * input_price;
            let output_cost = (output_tokens as f64 / 1_000_000.0) * output_price;
            Some(input_cost + output_cost)
        });

        Self {
            model,
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
            timestamp: chrono::Utc::now(),
        }
    }

    /// The cost, when one could be computed.
    pub fn cost(&self) -> Option<f64> {
        self.cost_usd
    }
}

/// Time period for cost aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsagePeriod {
    Session,
    Day,
    Month,
}

/// A single cost record for persistent storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    /// Unique identifier
    pub id: String,
    /// Token usage details
    pub usage: TokenUsage,
    /// Session identifier (for grouping)
    pub session_id: String,
}

impl CostRecord {
    /// Create a new cost record.
    pub fn new(session_id: impl Into<String>, usage: TokenUsage) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            usage,
            session_id: session_id.into(),
        }
    }
}

/// Whether a turn may start, measured in tokens.
///
/// Money is not a possible answer here: the ceiling is denominated in tokens
/// because tokens are what the product can count (plan 306's DECIDED note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetCheck {
    /// Within the ceiling; the turn can proceed.
    Allowed,
    /// Past the warning threshold, but the turn can still proceed.
    Warning {
        used_tokens: u64,
        limit_tokens: u64,
        period: UsagePeriod,
    },
    /// The ceiling is spent; the turn is refused.
    Exceeded {
        used_tokens: u64,
        limit_tokens: u64,
        period: UsagePeriod,
    },
}

/// Usage summary for reporting.
///
/// `Default` is derived now that every money field is `Option`: an empty summary
/// reports no cost rather than `0.00`, which is the same distinction the rest of
/// this module carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    /// Session cost, or `None` when no priced model was used.
    pub session_cost_usd: Option<f64>,
    /// Today's cost, or `None` when no priced model was used.
    pub daily_cost_usd: Option<f64>,
    /// Tokens used today — the number the ceiling is measured against.
    pub daily_tokens: u64,
    /// Total tokens used this session
    pub total_tokens: u64,
    /// Number of requests
    pub request_count: usize,
    /// Breakdown by model
    pub by_model: std::collections::HashMap<String, ModelStats>,
}

/// Statistics for a specific model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    /// Model name
    pub model: String,
    /// Total cost for this model, or `None` when it has no configured price.
    pub cost_usd: Option<f64>,
    /// Total tokens for this model
    pub total_tokens: u64,
    /// Number of requests for this model
    pub request_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ModelPrice;

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_per_million: input,
            output_per_million: output,
        }
    }

    #[test]
    fn a_configured_price_produces_a_cost() {
        let usage = TokenUsage::new("test/model", 1000, 500, Some(&price(3.0, 15.0)));

        // (1000/1M)*3 + (500/1M)*15 = 0.003 + 0.0075 = 0.0105
        let cost = usage.cost_usd.expect("a priced model reports a cost");
        assert!((cost - 0.0105).abs() < 0.0001);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
    }

    /// The half that matters: with no `[cost.prices]` entry the record reports
    /// **nothing**, not `0.00`. There is no price source in the product, so a
    /// zero would be a claim about spend on every default install.
    #[test]
    fn an_unpriced_model_reports_no_cost_rather_than_zero() {
        let usage = TokenUsage::new("test/model", 1000, 500, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.total_tokens, 1500, "tokens are still counted");
    }

    #[test]
    fn a_priced_model_that_used_nothing_costs_nothing() {
        let usage = TokenUsage::new("test/model", 0, 0, Some(&price(3.0, 15.0)));
        assert_eq!(usage.cost_usd, Some(0.0));
        assert_eq!(usage.total_tokens, 0);
    }

    /// Half a price is not a price. Reporting the priced side alone would look
    /// measured and be wrong by however much the other side cost.
    #[test]
    fn a_broken_price_reports_nothing_rather_than_half_a_number() {
        for p in [
            price(-3.0, 15.0),
            price(3.0, f64::NAN),
            price(0.0, 15.0),
            price(f64::INFINITY, 15.0),
        ] {
            let usage = TokenUsage::new("test/model", 1000, 1000, Some(&p));
            assert_eq!(usage.cost_usd, None, "{p:?} is not a usable price");
            assert_eq!(usage.total_tokens, 2000);
        }
    }

    #[test]
    fn cost_record_creation() {
        let usage = TokenUsage::new("test/model", 100, 50, Some(&price(1.0, 2.0)));
        let record = CostRecord::new("session-123", usage);

        assert_eq!(record.session_id, "session-123");
        assert!(!record.id.is_empty());
        assert_eq!(record.usage.model, "test/model");
    }
}
