use std::collections::HashSet;

/// Simple dedup for trade alerts. Prevents sending duplicate
/// open/close alerts for the same position.
pub struct TradeDedup {
    /// Position IDs that have already had an open alert sent
    alerted_opens: HashSet<u64>,
    /// Position IDs that have already had a close alert sent
    alerted_closes: HashSet<u64>,
}

impl TradeDedup {
    pub fn new() -> Self {
        Self {
            alerted_opens: HashSet::new(),
            alerted_closes: HashSet::new(),
        }
    }

    /// Returns true if we should send an open alert for this position
    pub fn should_alert_open(&mut self, position_id: u64) -> bool {
        if self.alerted_opens.contains(&position_id) {
            false
        } else {
            self.alerted_opens.insert(position_id);
            true
        }
    }

    /// Returns true if we should send a close alert for this position
    pub fn should_alert_close(&mut self, position_id: u64) -> bool {
        if self.alerted_closes.contains(&position_id) {
            false
        } else {
            self.alerted_closes.insert(position_id);
            self.alerted_opens.remove(&position_id);
            true
        }
    }

    /// Clean up old entries to prevent unbounded growth
    pub fn cleanup(&mut self) {
        // Simple strategy: clear close alerts older than this call
        // Open alerts are cleared when close alerts fire
        self.alerted_closes.clear();
    }
}

impl Default for TradeDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_duplicate_open_alert() {
        let mut dedup = TradeDedup::new();
        assert!(dedup.should_alert_open(1));
        assert!(!dedup.should_alert_open(1)); // Duplicate suppressed
    }

    #[test]
    fn test_no_duplicate_close_alert() {
        let mut dedup = TradeDedup::new();
        assert!(dedup.should_alert_close(1));
        assert!(!dedup.should_alert_close(1)); // Duplicate suppressed
    }

    #[test]
    fn test_close_removes_from_opens() {
        let mut dedup = TradeDedup::new();
        dedup.should_alert_open(1);
        dedup.should_alert_close(1);
        // After close, the open record is cleaned up
        // (new position with same ID would be allowed)
    }

    #[test]
    fn test_different_positions() {
        let mut dedup = TradeDedup::new();
        assert!(dedup.should_alert_open(1));
        assert!(dedup.should_alert_open(2));
        assert!(!dedup.should_alert_open(1));
    }

    #[test]
    fn test_cleanup() {
        let mut dedup = TradeDedup::new();
        dedup.should_alert_close(1);
        dedup.should_alert_close(2);
        dedup.cleanup();
        // After cleanup, close alerts can re-fire
        assert!(dedup.should_alert_close(1));
    }
}
