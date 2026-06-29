use anyhow::Result;
use async_trait::async_trait;

/// Unified symbol info across exchanges
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub symbol: String,
    pub volume_24h_usd: f64,
    pub last_price: f64,
}

/// OHLCV candle — only fields we actually use
#[derive(Debug, Clone)]
pub struct Kline {
    pub timestamp: u64,
    pub close: f64,
}

/// Exchange trait — implement this to add a new exchange
#[async_trait]
pub trait Exchange: Send + Sync {
    /// Exchange name (e.g. "bybit", "binance")
    fn name(&self) -> &str;

    /// Fetch all available USDT perpetual futures
    async fn get_futures_symbols(&self) -> Result<Vec<SymbolInfo>>;

    /// Fetch klines for a symbol
    async fn get_klines(&self, symbol: &str, interval: &str, limit: u32) -> Result<Vec<Kline>>;

    /// Fetch klines for multiple symbols concurrently (with rate limiting)
    async fn fetch_all_klines(
        &self,
        symbols: &[String],
        interval: &str,
        limit: u32,
    ) -> Result<std::collections::HashMap<String, Vec<f64>>>;

    /// Get current mark price for a symbol
    async fn get_mark_price(&self, symbol: &str) -> Result<f64>;
}
