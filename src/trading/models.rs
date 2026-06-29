use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::stats::cointegration::EntryGuard;

/// A cointegrated pair being tracked for trading
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedPair {
    pub symbol_a: String,
    pub symbol_b: String,
    pub correlation: f64,
    pub hedge_ratio: f64,
    pub spread_mean: f64,
    pub spread_std: f64,
    pub adf_stat: f64,
    pub half_life: f64,
    pub discovered_at: DateTime<Utc>,
    pub last_zscore: f64,
    pub last_updated: DateTime<Utc>,
    /// Spread momentum: Δz over recent window.
    /// Negative = z declining (reverting from above). Positive = z rising (reverting from below).
    /// Zero or same-sign = still diverging.
    #[serde(default)]
    pub spread_momentum: f64,
    /// Hurst exponent on recent spread window.
    /// H < 0.5 = mean-reverting (safe). H > 0.5 = trending (danger).
    #[serde(default = "default_hurst")]
    pub hurst: f64,
    /// Multi-factor entry guard: 5-factor composite score for entry protection.
    #[serde(default)]
    pub entry_guard: EntryGuard,
}

fn default_hurst() -> f64 {
    0.5
}

/// Position in a pairs trade
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: u64,
    pub symbol_a: String,
    pub symbol_b: String,
    pub side_a: PositionSide,  // Long or Short on symbol A
    pub side_b: PositionSide,  // Opposite of side_a
    pub qty_a: f64,
    pub qty_b: f64,
    pub entry_price_a: f64,
    pub entry_price_b: f64,
    pub current_price_a: f64,
    pub current_price_b: f64,
    pub entry_zscore: f64,
    pub current_zscore: f64,
    pub entry_time: DateTime<Utc>,
    pub pnl: f64,
    pub status: PositionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PositionSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PositionStatus {
    Open,
    Closed,
}

/// Paper trading state persisted to disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingState {
    pub balance: f64,
    pub initial_balance: f64,
    pub positions: Vec<Position>,
    pub closed_trades: Vec<Position>,
    pub next_position_id: u64,
    pub total_pnl: f64,
    pub win_count: u32,
    pub loss_count: u32,
    pub tracked_pairs: Vec<TrackedPair>,
}

impl Default for TradingState {
    fn default() -> Self {
        Self {
            balance: 100.0,
            initial_balance: 100.0,
            positions: Vec::new(),
            closed_trades: Vec::new(),
            next_position_id: 1,
            total_pnl: 0.0,
            win_count: 0,
            loss_count: 0,
            tracked_pairs: Vec::new(),
        }
    }
}

impl TradingState {
    pub fn with_balance(balance: f64) -> Self {
        Self {
            balance,
            initial_balance: balance,
            ..Self::default()
        }
    }
}

/// Trading signal
#[derive(Debug, Clone)]
pub enum Signal {
    OpenLong { symbol_a: String, symbol_b: String, zscore: f64, hedge_ratio: f64 },
    OpenShort { symbol_a: String, symbol_b: String, zscore: f64, hedge_ratio: f64 },
    ClosePosition { position_id: u64, zscore: f64 },
}

/// Trading config
#[derive(Debug, Clone)]
pub struct TradingConfig {
    pub zscore_entry: f64,
    pub zscore_exit: f64,
    pub max_position_pct: f64, // max % of balance per position
    pub max_open_positions: usize,
    pub stop_loss_zscore: f64, // emergency exit if z-score exceeds this
    pub min_half_life: f64,    // skip pairs with half-life too long
    pub commission_pct: f64,   // taker fee per leg (0.055% = Bybit default)
    pub slippage_pct: f64,     // estimated slippage per leg
    #[allow(dead_code)]
    pub hurst_threshold: f64,  // max Hurst for entry (H < 0.5 = mean-reverting) — passed to EntryGuard
    pub hurst_exit: f64,       // emergency exit if H exceeds this on open position
    /// Minimum number of EntryGuard factors (out of 5) that must pass for entry.
    /// Default: 4 (requires Hurst + velocity + at least 2 of: accel, OU R², ADF stability).
    /// Set to 3 for more entries (less strict), 5 for very strict filtering.
    pub entry_min_factors: u8,
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            zscore_entry: 2.0,
            zscore_exit: 0.5,
            max_position_pct: 0.25,
            max_open_positions: 4,
            stop_loss_zscore: 4.0,
            min_half_life: 200.0,
            commission_pct: 0.055,
            slippage_pct: 0.02,
            hurst_threshold: 0.5,
            hurst_exit: 0.6,
            entry_min_factors: 4,
        }
    }
}
