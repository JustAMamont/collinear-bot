use std::collections::{HashMap, VecDeque};

/// Bounded cache for candle close prices, keyed by symbol.
/// Each symbol's data is kept in a VecDeque with a configurable max length.
pub struct CandleCache {
    data: HashMap<String, VecDeque<f64>>,
    max_len: usize,
}

#[allow(dead_code)]
impl CandleCache {
    pub fn new(max_len: usize) -> Self {
        Self {
            data: HashMap::new(),
            max_len,
        }
    }

    /// Replace all candles for a symbol. Trims to max_len from the end (keep newest).
    pub fn insert(&mut self, symbol: &str, closes: Vec<f64>) {
        let mut dq: VecDeque<f64> = closes.into_iter().collect();
        if dq.len() > self.max_len {
            let drain_count = dq.len() - self.max_len;
            dq.drain(0..drain_count);
        }
        self.data.insert(symbol.to_string(), dq);
    }

    /// Append a new confirmed candle close. If max_len exceeded, pop front.
    /// Use this ONLY when a candle is confirmed (complete) by the exchange.
    pub fn update(&mut self, symbol: &str, close: f64) {
        let dq = self.data.entry(symbol.to_string()).or_insert_with(VecDeque::new);
        dq.push_back(close);
        if dq.len() > self.max_len {
            dq.pop_front();
        }
    }

    /// Update the last candle's close price (interim / unconfirmed tick).
    /// Replaces the last element instead of appending. This is critical because
    /// Bybit sends many interim updates per candle — appending each one would
    /// flood the cache with thousands of near-identical values, destroying
    /// all statistical calculations (correlation, Hurst, etc).
    pub fn update_last(&mut self, symbol: &str, close: f64) {
        if let Some(dq) = self.data.get_mut(symbol) {
            if let Some(last) = dq.back_mut() {
                *last = close;
            } else {
                dq.push_back(close);
            }
        } else {
            // Symbol not in cache yet — first tick, create entry
            let mut dq = VecDeque::new();
            dq.push_back(close);
            self.data.insert(symbol.to_string(), dq);
        }
    }

    /// Get the close prices for a symbol.
    pub fn get(&self, symbol: &str) -> Option<&VecDeque<f64>> {
        self.data.get(symbol)
    }

    /// Get all symbols currently in the cache.
    pub fn symbols(&self) -> Vec<String> {
        self.data.keys().cloned().collect()
    }

    /// Get the number of candles cached for a symbol.
    pub fn len(&self, symbol: &str) -> usize {
        self.data.get(symbol).map(|dq| dq.len()).unwrap_or(0)
    }

    /// Clear all cached data.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Get the last close price for a symbol.
    pub fn last_price(&self, symbol: &str) -> Option<f64> {
        self.data.get(symbol).and_then(|dq| dq.back().copied())
    }

    /// Convert to a HashMap<String, Vec<f64>> for compatibility with scanner.
    pub fn to_hashmap(&self) -> HashMap<String, Vec<f64>> {
        self.data
            .iter()
            .map(|(k, dq)| (k.clone(), dq.iter().copied().collect()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut cache = CandleCache::new(100);
        cache.insert("BTCUSDT", vec![1.0, 2.0, 3.0]);
        assert_eq!(cache.len("BTCUSDT"), 3);
        assert_eq!(cache.last_price("BTCUSDT"), Some(3.0));
    }

    #[test]
    fn test_insert_trims_to_max_len() {
        let mut cache = CandleCache::new(5);
        cache.insert("BTCUSDT", vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        // Should keep newest 5: [3, 4, 5, 6, 7]
        assert_eq!(cache.len("BTCUSDT"), 5);
        let prices = cache.get("BTCUSDT").unwrap();
        assert_eq!(*prices.front().unwrap(), 3.0);
        assert_eq!(*prices.back().unwrap(), 7.0);
    }

    #[test]
    fn test_update_appends_and_trims() {
        let mut cache = CandleCache::new(3);
        cache.insert("BTCUSDT", vec![1.0, 2.0]);
        cache.update("BTCUSDT", 3.0);
        assert_eq!(cache.len("BTCUSDT"), 3);
        cache.update("BTCUSDT", 4.0);
        // Should be [2, 3, 4] now
        assert_eq!(cache.len("BTCUSDT"), 3);
        let prices = cache.get("BTCUSDT").unwrap();
        assert_eq!(*prices.front().unwrap(), 2.0);
        assert_eq!(*prices.back().unwrap(), 4.0);
    }

    #[test]
    fn test_update_new_symbol() {
        let mut cache = CandleCache::new(10);
        cache.update("ETHUSDT", 100.0);
        assert_eq!(cache.len("ETHUSDT"), 1);
        assert_eq!(cache.last_price("ETHUSDT"), Some(100.0));
    }

    #[test]
    fn test_symbols() {
        let mut cache = CandleCache::new(100);
        cache.insert("BTCUSDT", vec![1.0]);
        cache.insert("ETHUSDT", vec![2.0]);
        let mut syms = cache.symbols();
        syms.sort();
        assert_eq!(syms, vec!["BTCUSDT", "ETHUSDT"]);
    }

    #[test]
    fn test_clear() {
        let mut cache = CandleCache::new(100);
        cache.insert("BTCUSDT", vec![1.0]);
        cache.clear();
        assert!(cache.symbols().is_empty());
    }

    #[test]
    fn test_to_hashmap() {
        let mut cache = CandleCache::new(100);
        cache.insert("BTCUSDT", vec![1.0, 2.0, 3.0]);
        let map = cache.to_hashmap();
        assert_eq!(map["BTCUSDT"], vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_missing_symbol() {
        let cache = CandleCache::new(100);
        assert_eq!(cache.get("NOTFOUND"), None);
        assert_eq!(cache.len("NOTFOUND"), 0);
        assert_eq!(cache.last_price("NOTFOUND"), None);
    }
}
