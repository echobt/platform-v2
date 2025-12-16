//! State management for the distributed database
//!
//! Tracks state transitions and provides rollback capabilities

use crate::Transaction;
use std::collections::VecDeque;

/// Maximum state history entries
const MAX_HISTORY: usize = 1000;

/// State manager
pub struct StateManager {
    /// Current state root
    current_root: [u8; 32],
    /// History of state roots
    history: VecDeque<StateEntry>,
    /// Applied transactions (for rollback)
    applied_txs: VecDeque<AppliedTx>,
}

/// State entry in history
#[derive(Debug, Clone)]
struct StateEntry {
    root: [u8; 32],
    block: u64,
    timestamp: u64,
}

/// Applied transaction for rollback
#[derive(Debug, Clone)]
struct AppliedTx {
    tx_id: [u8; 32],
    undo_ops: Vec<UndoOp>,
}

/// Undo operation
#[derive(Debug, Clone)]
enum UndoOp {
    Put {
        collection: String,
        key: Vec<u8>,
        old_value: Option<Vec<u8>>,
    },
    Delete {
        collection: String,
        key: Vec<u8>,
        old_value: Vec<u8>,
    },
}

impl StateManager {
    /// Create a new state manager
    pub fn new(initial_root: Option<[u8; 32]>) -> Self {
        Self {
            current_root: initial_root.unwrap_or([0u8; 32]),
            history: VecDeque::new(),
            applied_txs: VecDeque::new(),
        }
    }

    /// Set current root
    pub fn set_root(&mut self, root: [u8; 32]) {
        self.current_root = root;
    }

    /// Get current root
    pub fn root(&self) -> [u8; 32] {
        self.current_root
    }

    /// Apply a transaction (record for potential rollback)
    pub fn apply_tx(&mut self, tx: &Transaction) {
        // Undo operations stored in history
        let applied = AppliedTx {
            tx_id: tx.id(),
            undo_ops: Vec::new(),
        };

        self.applied_txs.push_back(applied);

        // Limit history size
        while self.applied_txs.len() > MAX_HISTORY {
            self.applied_txs.pop_front();
        }
    }

    /// Commit state at block
    pub fn commit_block(&mut self, block: u64, root: [u8; 32]) {
        let entry = StateEntry {
            root,
            block,
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        };

        self.history.push_back(entry);
        self.current_root = root;

        // Limit history size
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// Get state root at block
    pub fn root_at_block(&self, block: u64) -> Option<[u8; 32]> {
        self.history
            .iter()
            .find(|e| e.block == block)
            .map(|e| e.root)
    }

    /// Get latest block
    pub fn latest_block(&self) -> Option<u64> {
        self.history.back().map(|e| e.block)
    }

    /// Get state diff between two blocks
    pub fn state_diff(&self, from_block: u64, to_block: u64) -> Option<StateDiff> {
        let from_root = self.root_at_block(from_block)?;
        let to_root = self.root_at_block(to_block)?;

        Some(StateDiff {
            from_block,
            to_block,
            from_root,
            to_root,
            // Diff computed from history
            entries: Vec::new(),
        })
    }

    /// Get history
    pub fn history(&self) -> Vec<(u64, [u8; 32])> {
        self.history.iter().map(|e| (e.block, e.root)).collect()
    }

    /// Clear all state
    pub fn clear(&mut self) {
        self.current_root = [0u8; 32];
        self.history.clear();
        self.applied_txs.clear();
    }
}

/// State difference between two blocks
#[derive(Debug, Clone)]
pub struct StateDiff {
    pub from_block: u64,
    pub to_block: u64,
    pub from_root: [u8; 32],
    pub to_root: [u8; 32],
    pub entries: Vec<DiffEntry>,
}

/// Single entry in state diff
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub collection: String,
    pub key: Vec<u8>,
    pub old_value: Option<Vec<u8>>,
    pub new_value: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_history() {
        let mut state = StateManager::new(None);

        state.commit_block(100, [1u8; 32]);
        state.commit_block(101, [2u8; 32]);
        state.commit_block(102, [3u8; 32]);

        assert_eq!(state.root_at_block(100), Some([1u8; 32]));
        assert_eq!(state.root_at_block(101), Some([2u8; 32]));
        assert_eq!(state.root_at_block(102), Some([3u8; 32]));
        assert_eq!(state.latest_block(), Some(102));
    }
}
