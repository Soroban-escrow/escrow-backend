//! Domain constants and rules for dispute handling.

/// The promised turnaround SLA (in hours) within which a human will review a dispute.
/// Both the `/bounty dispute` reply and the admin queue `overdue` indicator use this SLA.
pub const DISPUTE_SLA_HOURS: i64 = 48;

/// Advance warning lead time (in hours) before the dispute SLA expires.
/// Internal alerts fire before the public deadline so operators have time to act
/// before the promised SLA is breached.
pub const DISPUTE_ALERT_LEAD_HOURS: i64 = 12;

/// Threshold (in hours) after which an unresolved dispute triggers an internal alert.
/// Deliberately set to fire 12 hours before the promised 48 h SLA (i.e. at 36 h).
pub const DISPUTE_ALERT_HOURS: i64 = DISPUTE_SLA_HOURS - DISPUTE_ALERT_LEAD_HOURS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispute_sla_and_alert_windows_are_consistent() {
        assert_eq!(DISPUTE_SLA_HOURS, 48);
        assert_eq!(DISPUTE_ALERT_LEAD_HOURS, 12);
        assert_eq!(DISPUTE_ALERT_HOURS, 36);
        assert!(DISPUTE_ALERT_HOURS < DISPUTE_SLA_HOURS);
        assert_eq!(DISPUTE_SLA_HOURS - DISPUTE_ALERT_HOURS, DISPUTE_ALERT_LEAD_HOURS);
    }
}
