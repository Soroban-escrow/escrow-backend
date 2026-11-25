//! AI review cost tracking and budget enforcement.

use chrono::{DateTime, Duration, Utc};

/// Cost of a single AI review in micro USD (1 USD = 1_000_000 micro USD)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostMicroUsd(pub i64);

impl CostMicroUsd {
    pub fn new(input_tokens: i32, output_tokens: i32, price_input_per_mtok: f64, price_output_per_mtok: f64) -> Self {
        let input_cost = (input_tokens as f64 / 1_000_000.0) * price_input_per_mtok * 1_000_000.0;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * price_output_per_mtok * 1_000_000.0;
        let total = (input_cost + output_cost).ceil() as i64;
        Self(total.max(0))
    }

    pub fn to_usd(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    pub fn from_usd(usd: f64) -> Self {
        Self((usd * 1_000_000.0).ceil() as i64)
    }
}

/// Spend within a time window
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spend {
    pub total_micro_usd: i64,
    pub review_count: i32,
}

/// Budget check result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetStatus {
    Ok,
    /// Nearing limit at 80%
    Warning { current_usd: f64, limit_usd: f64 },
    /// Budget exhausted
    Exhausted { current_usd: f64, limit_usd: f64 },
}

impl BudgetStatus {
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Ok => None,
            Self::Warning { current_usd, limit_usd } => Some(
                format!("AI review budget warning: ${:.2} of ${:.2} (80%) used today", current_usd, limit_usd)
            ),
            Self::Exhausted { current_usd, limit_usd } => Some(
                format!("AI review budget exhausted: ${:.2} of ${:.2} limit reached today; reviews resume tomorrow", current_usd, limit_usd)
            ),
        }
    }

    pub fn should_skip_review(&self) -> bool {
        matches!(self, Self::Exhausted { .. })
    }

    pub fn is_alert(&self) -> bool {
        !matches!(self, Self::Ok)
    }
}

pub fn check_budget(
    current_spend_micro_usd: i64,
    pending_cost_micro_usd: i64,
    limit_usd: Option<f64>,
) -> BudgetStatus {
    match limit_usd {
        None => BudgetStatus::Ok,
        Some(limit) => {
            let limit_micro = (limit * 1_000_000.0) as i64;
            let new_spend = current_spend_micro_usd + pending_cost_micro_usd;
            let current_usd = current_spend_micro_usd as f64 / 1_000_000.0;
            let new_usd = new_spend as f64 / 1_000_000.0;

            if new_spend > limit_micro {
                BudgetStatus::Exhausted { current_usd, limit_usd: limit }
            } else if new_spend > (limit_micro * 80 / 100) {
                BudgetStatus::Warning { current_usd, limit_usd: limit }
            } else {
                BudgetStatus::Ok
            }
        }
    }
}

/// Query start and end times for different windows
pub fn window_times(now: DateTime<Utc>) -> WindowTimes {
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_start = DateTime::<Utc>::from_naive_utc_and_offset(today_start, Utc);
    let today_end = today_start + Duration::days(1);

    let month_start = now.date_naive().with_day(1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let month_start = DateTime::<Utc>::from_naive_utc_and_offset(month_start, Utc);
    let month_end = (month_start + Duration::days(31)).with_day(1).unwrap();

    WindowTimes { today_start, today_end, month_start, month_end }
}

#[derive(Debug, Clone)]
pub struct WindowTimes {
    pub today_start: DateTime<Utc>,
    pub today_end: DateTime<Utc>,
    pub month_start: DateTime<Utc>,
    pub month_end: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_calculation_from_tokens() {
        // 1M tokens costs exactly 1 unit of price_per_mtok
        let cost = CostMicroUsd::new(1_000_000, 0, 3.0, 15.0);
        assert_eq!(cost.to_usd(), 3.0);

        let cost = CostMicroUsd::new(0, 1_000_000, 3.0, 15.0);
        assert_eq!(cost.to_usd(), 15.0);

        let cost = CostMicroUsd::new(500_000, 500_000, 3.0, 15.0);
        assert_eq!(cost.to_usd(), 1.5 + 7.5); // 9.0
    }

    #[test]
    fn budget_ok_when_no_limit() {
        let status = check_budget(0, 1_000_000, None);
        assert_eq!(status, BudgetStatus::Ok);
    }

    #[test]
    fn budget_exhausted_when_exceeded() {
        let status = check_budget(500_000_000, 1_000_000, Some(0.5)); // 500 USD + 1 USD > 0.5 USD
        assert_eq!(
            status,
            BudgetStatus::Exhausted { current_usd: 500.0, limit_usd: 0.5 }
        );
        assert!(status.should_skip_review());
    }

    #[test]
    fn budget_warning_at_80_percent() {
        // 80% of 100 USD = 80 USD
        let status = check_budget(80_500_000, 0, Some(100.0));
        assert_eq!(status, BudgetStatus::Warning { current_usd: 80.5, limit_usd: 100.0 });
        assert!(!status.should_skip_review());
    }

    #[test]
    fn budget_ok_before_80_percent() {
        let status = check_budget(79_000_000, 0, Some(100.0));
        assert_eq!(status, BudgetStatus::Ok);
    }

    #[test]
    fn window_times_calculations() {
        let now = Utc::now();
        let windows = window_times(now);

        // Today window should span 24 hours
        assert_eq!((windows.today_end - windows.today_start).num_hours(), 24);

        // Month window should start on day 1
        assert_eq!(windows.month_start.day(), 1);
    }
}
