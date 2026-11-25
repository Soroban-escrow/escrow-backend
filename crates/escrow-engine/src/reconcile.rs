//! Event indexer gap detection and state reconciliation from contract.

use chrono::{DateTime, Utc};
use escrow_chain::ChainReader;
use escrow_db::{Db, audit};
use serde::Serialize;

/// A gap in the event stream, usually due to RPC retention limits.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventGap {
    pub gap_start: u32,
    pub gap_end: u32,
    pub detected_at: DateTime<Utc>,
}

impl EventGap {
    pub fn size(&self) -> u32 {
        self.gap_end.saturating_sub(self.gap_start)
    }
}

/// Detect if there's a gap between our cursor and the RPC's oldest retained ledger.
pub async fn detect_event_gap(
    _db: &Db,
    _chain: &dyn ChainReader,
) -> Result<Option<EventGap>, Box<dyn std::error::Error>> {
    // TODO: Implement when Db.get_indexer_cursor and ChainReader.get_health are available
    Ok(None)
}

/// A change to a bounty made during reconciliation
#[derive(Debug, Clone, Serialize)]
pub struct BountyReconciliation {
    pub bounty_id: i64,
    pub before_status: String,
    pub after_status: String,
    pub changes: Vec<String>,
}

/// Reconcile bounty state from the contract.
/// This reads get_bounty(id) for each non-terminal bounty and updates the database.
pub async fn reconcile_bounties(
    _db: &Db,
    _chain: &dyn ChainReader,
    _dry_run: bool,
) -> Result<Vec<BountyReconciliation>, Box<dyn std::error::Error>> {
    // TODO: Implement when Db.get_non_terminal_bounties and Db.update_bounty_from_chain are available
    Ok(vec![])
}

/// Record a reconciliation audit entry
pub async fn record_reconciliation_audit(
    db: &Db,
    changes: &[BountyReconciliation],
    actor: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    for change in changes {
        let data = serde_json::json!({
            "bounty_id": change.bounty_id,
            "before_status": change.before_status,
            "after_status": change.after_status,
            "changes": change.changes,
        });
        audit::log(&db.pool, actor, "reconcile_bounty", &format!("bounty:{}", change.bounty_id), &data).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_gap_size_calculation() {
        let gap = EventGap {
            gap_start: 100,
            gap_end: 150,
            detected_at: Utc::now(),
        };
        assert_eq!(gap.size(), 50);
    }

    #[test]
    fn event_gap_zero_size_when_equal() {
        let gap = EventGap {
            gap_start: 100,
            gap_end: 100,
            detected_at: Utc::now(),
        };
        assert_eq!(gap.size(), 0);
    }
}
