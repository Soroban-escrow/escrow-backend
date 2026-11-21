//! Queue and preserve funded events for untracked repositories
//! Resolves Issue #224.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct UntrackedFundedEvent {
    pub repo_identifier: String,
    pub amount_raw: u128,
    pub token_symbol: String,
    pub block_height: u64,
    pub tx_hash: String,
}

#[derive(Debug, Default)]
pub struct UntrackedEventQueue {
    events: VecDeque<UntrackedFundedEvent>,
}

impl UntrackedEventQueue {
    pub fn push_event(&mut self, event: UntrackedFundedEvent) {
        self.events.push_back(event);
    }

    pub fn drain_for_repo(&mut self, repo_identifier: &str) -> Vec<UntrackedFundedEvent> {
        let mut matched = Vec::new();
        let mut remaining = VecDeque::new();
        for ev in self.events.drain(..) {
            if ev.repo_identifier == repo_identifier {
                matched.push(ev);
            } else {
                remaining.push_back(ev);
            }
        }
        self.events = remaining;
        matched
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }
}
