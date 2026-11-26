//! Indexer poll completion status indicating whether poll was truncated at MAX_PAGES.
//! Resolves Issue #225.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollCompletionStatus {
    Exhausted,
    StoppedAtMaxPages { pages_fetched: u32, next_cursor: u64 },
}

#[derive(Debug, Clone)]
pub struct PollReport {
    pub status: PollCompletionStatus,
    pub total_records: usize,
    pub cursor_advanced_to: u64,
}

impl PollReport {
    pub fn reached_max_pages(&self) -> bool {
        matches!(self.status, PollCompletionStatus::StoppedAtMaxPages { .. })
    }
}
