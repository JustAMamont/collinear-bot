use chrono::Utc;
use log::{info, warn};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::stats::cointegration::{live_zscore, CointResult};
use crate::trading::models::*;

/// Paper trading engine
pub struct PaperEngine {
    pub state: TradingState,
    pub config: TradingConfig,
    pub state_path: String,
}

impl PaperEngine {
    pub fn new(config: TradingConfig, initial_balance: f64, state_path: &str) -> Self {
        let state = if Path::new(state_path).exists() {
            match fs::read_to_string(state_path) {
                Ok(json) => serde_json::from_str(&json).unwrap_or_else(|_| {
                    warn!("Corrupt state file, starting fresh");
                    TradingState::with_balance(initial_balance)
                }),
                Err(_) => TradingState::with_balance(initial_balance),
            }
        } else {
            TradingState::with_balance(initial_balance)
        };

        Self {
            state,
            config,
            state_path: state_path.to_string(),
        }
    }

    /// Save state to disk
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            if let Err(e) = fs::write(&self.state_path, json) {
                warn!("Failed to save state: {}", e);
            }
        }
    }

    /// Add a cointegrated pair to tracking list
    pub fn track_pair(&mut self, result: &CointResult) {
        if self.state.tracked_pairs.iter().any(|p| {
            (p.symbol_a == result.symbol_a && p.symbol_b == result.symbol_b)
                || (p.symbol_a == result.symbol_b && p.symbol_b == result.symbol_a)
        }) {
            return;
        }

        if result.half_life_periods > self.config.min_half_life {
            return;
        }

        self.state.tracked_pairs.push(TrackedPair {
            symbol_a: result.symbol_a.clone(),
            symbol_b: result.symbol_b.clone(),
            correlation: result.correlation,
            hedge_ratio: result.hedge_ratio,
            spread_mean: result.spread_mean,
            spread_std: result.spread_std,
            adf_stat: result.adf_stat,
            half_life: result.half_life_periods,
            discovered_at: Utc::now(),
            last_zscore: result.current_zscore,
            last_updated: Utc::now(),
            spread_momentum: 0.0,
            hurst: 0.5,
            entry_guard: Default::default(),
        });
    }

    /// Generate signals for all tracked pairs given current prices.
    ///
    /// Entry filter: **EntryGuard** — 5-factor composite score.
    /// Each factor is PASS (1) or FAIL (0); entry allowed when score ≥ `entry_min_factors`.
    ///
    /// Factors:
    /// 1. **Hurst** (H < threshold → mean-reverting regime)
    /// 2. **Velocity** (Δz opposite sign to z → spread is turning)
    /// 3. **Acceleration** (Δ²z confirms reversal is strengthening)
    /// 4. **OU R²** (Ornstein-Uhlenbeck model explains spread dynamics)
    /// 5. **ADF stability** (cointegration is not weakening)
    ///
    /// Exit conditions:
    /// - Z-score reverted to exit threshold
    /// - Stop-loss hit (z-score exploded)
    /// - Emergency exit: Hurst > hurst_exit (regime break)
    pub fn generate_signals(&self, prices: &HashMap<String, f64>) -> Vec<Signal> {
        let mut signals = Vec::new();

        for pair in &self.state.tracked_pairs {
            let price_a = match prices.get(&pair.symbol_a) {
                Some(p) => *p,
                None => continue,
            };
            let price_b = match prices.get(&pair.symbol_b) {
                Some(p) => *p,
                None => continue,
            };

            let z = live_zscore(price_a, price_b, pair.hedge_ratio, pair.spread_mean, pair.spread_std);

            // Check if this exact pair already has an open position
            let has_open = self.state.positions.iter().any(|p| {
                (p.symbol_a == pair.symbol_a && p.symbol_b == pair.symbol_b)
                    || (p.symbol_a == pair.symbol_b && p.symbol_b == pair.symbol_a)
            });

            // Check if ANY symbol from this pair is already used in another open position
            let symbol_busy = self.state.positions.iter().any(|p| {
                p.symbol_a == pair.symbol_a
                    || p.symbol_b == pair.symbol_a
                    || p.symbol_a == pair.symbol_b
                    || p.symbol_b == pair.symbol_b
            });

            if has_open {
                for pos in &self.state.positions {
                    if (pos.symbol_a == pair.symbol_a && pos.symbol_b == pair.symbol_b)
                        || (pos.symbol_a == pair.symbol_b && pos.symbol_b == pair.symbol_a)
                    {
                        // Exit: z-score reverted
                        if z.abs() < self.config.zscore_exit {
                            signals.push(Signal::ClosePosition { position_id: pos.id, zscore: z });
                        }
                        // Stop-loss: z-score exploded
                        else if z.abs() > self.config.stop_loss_zscore {
                            signals.push(Signal::ClosePosition { position_id: pos.id, zscore: z });
                        }
                        // Emergency exit: regime break — Hurst says trending, not mean-reverting
                        else if pair.hurst > self.config.hurst_exit {
                            signals.push(Signal::ClosePosition { position_id: pos.id, zscore: z });
                        }
                    }
                }
            } else if !symbol_busy && self.state.positions.len() < self.config.max_open_positions {
                // ── Entry: EntryGuard 5-factor composite ──────────
                let guard = &pair.entry_guard;

                if z > self.config.zscore_entry
                    && guard.is_passing(self.config.entry_min_factors)
                {
                    signals.push(Signal::OpenShort {
                        symbol_a: pair.symbol_a.clone(),
                        symbol_b: pair.symbol_b.clone(),
                        zscore: z,
                        hedge_ratio: pair.hedge_ratio,
                    });
                } else if z < -self.config.zscore_entry
                    && guard.is_passing(self.config.entry_min_factors)
                {
                    signals.push(Signal::OpenLong {
                        symbol_a: pair.symbol_a.clone(),
                        symbol_b: pair.symbol_b.clone(),
                        zscore: z,
                        hedge_ratio: pair.hedge_ratio,
                    });
                }
            }
        }

        signals
    }

    /// Execute signals (paper trade)
    pub fn execute_signals(&mut self, signals: Vec<Signal>, prices: &HashMap<String, f64>) {
        for signal in signals {
            match signal {
                Signal::OpenLong { symbol_a, symbol_b, zscore, hedge_ratio } => {
                    self.open_position(&symbol_a, &symbol_b, PositionSide::Long, PositionSide::Short, zscore, hedge_ratio, prices);
                }
                Signal::OpenShort { symbol_a, symbol_b, zscore, hedge_ratio } => {
                    self.open_position(&symbol_a, &symbol_b, PositionSide::Short, PositionSide::Long, zscore, hedge_ratio, prices);
                }
                Signal::ClosePosition { position_id, zscore } => {
                    self.close_position(position_id, zscore, prices);
                }
            }
        }
        self.save();
    }

    fn open_position(
        &mut self,
        symbol_a: &str,
        symbol_b: &str,
        side_a: PositionSide,
        side_b: PositionSide,
        zscore: f64,
        hedge_ratio: f64,
        prices: &HashMap<String, f64>,
    ) {
        let price_a = match prices.get(symbol_a) {
            Some(p) => *p,
            None => return,
        };
        let price_b = match prices.get(symbol_b) {
            Some(p) => *p,
            None => return,
        };

        if price_a <= 0.0 || price_b <= 0.0 {
            return;
        }

        let position_size = self.state.balance * self.config.max_position_pct;
        let half_per_leg = position_size / 2.0;

        // Apply slippage to entry prices
        let (fill_a, fill_b) = match (side_a, side_b) {
            (PositionSide::Long, PositionSide::Short) => {
                // Buy A at higher price, sell B at lower price
                let fa = price_a * (1.0 + self.config.slippage_pct / 100.0);
                let fb = price_b * (1.0 - self.config.slippage_pct / 100.0);
                (fa, fb)
            }
            (PositionSide::Short, PositionSide::Long) => {
                // Sell A at lower price, buy B at higher price
                let fa = price_a * (1.0 - self.config.slippage_pct / 100.0);
                let fb = price_b * (1.0 + self.config.slippage_pct / 100.0);
                (fa, fb)
            }
            _ => (price_a, price_b),
        };

        // Size leg B relative to leg A using hedge ratio
        // leg_a_size = half budget, leg_b_size = half budget * hedge_ratio (capped)
        let qty_a = half_per_leg / fill_a;
        let qty_b = (half_per_leg * hedge_ratio.min(3.0)) / fill_b;

        // Opening commission: 2 legs × (size × fee%)
        let commission = position_size * self.config.commission_pct / 100.0 * 2.0;

        self.state.balance -= position_size + commission;

        let pos = Position {
            id: self.state.next_position_id,
            symbol_a: symbol_a.to_string(),
            symbol_b: symbol_b.to_string(),
            side_a,
            side_b,
            qty_a,
            qty_b,
            entry_price_a: fill_a,
            entry_price_b: fill_b,
            current_price_a: fill_a,
            current_price_b: fill_b,
            entry_zscore: zscore,
            current_zscore: zscore,
            entry_time: Utc::now(),
            pnl: -commission,
            status: PositionStatus::Open,
        };

        info!(
            "OPEN #{}: {} {:?} {} @ {} | {} {:?} {} @ {} | z={:.2} | fee=${:.4}",
            pos.id, symbol_a, side_a, format_qty(qty_a), format_price(fill_a),
            symbol_b, side_b, format_qty(qty_b), format_price(fill_b),
            zscore, commission
        );

        self.state.positions.push(pos);
        self.state.next_position_id += 1;
    }

    fn close_position(&mut self, position_id: u64, zscore: f64, prices: &HashMap<String, f64>) {
        let idx = match self.state.positions.iter().position(|p| p.id == position_id) {
            Some(i) => i,
            None => return,
        };

        let mut pos = self.state.positions.remove(idx);

        let price_a = prices.get(&pos.symbol_a).copied().unwrap_or(pos.current_price_a);
        let price_b = prices.get(&pos.symbol_b).copied().unwrap_or(pos.current_price_b);

        // Apply slippage to exit prices
        let (fill_a, fill_b) = match (pos.side_a, pos.side_b) {
            (PositionSide::Long, PositionSide::Short) => {
                // Sell A at lower, buy B at higher
                let fa = price_a * (1.0 - self.config.slippage_pct / 100.0);
                let fb = price_b * (1.0 + self.config.slippage_pct / 100.0);
                (fa, fb)
            }
            (PositionSide::Short, PositionSide::Long) => {
                // Buy A at higher, sell B at lower
                let fa = price_a * (1.0 + self.config.slippage_pct / 100.0);
                let fb = price_b * (1.0 - self.config.slippage_pct / 100.0);
                (fa, fb)
            }
            _ => (price_a, price_b),
        };

        // PnL from price movement
        let pnl_a = match pos.side_a {
            PositionSide::Long => (fill_a - pos.entry_price_a) * pos.qty_a,
            PositionSide::Short => (pos.entry_price_a - fill_a) * pos.qty_a,
        };
        let pnl_b = match pos.side_b {
            PositionSide::Long => (fill_b - pos.entry_price_b) * pos.qty_b,
            PositionSide::Short => (pos.entry_price_b - fill_b) * pos.qty_b,
        };

        // Closing commission
        let position_size_now = fill_a * pos.qty_a + fill_b * pos.qty_b;
        let close_commission = position_size_now / 2.0 * self.config.commission_pct / 100.0 * 2.0;

        let total_pnl = pnl_a + pnl_b + pos.pnl - close_commission;

        // Return position size + PnL
        let original_size = (pos.entry_price_a * pos.qty_a + pos.entry_price_b * pos.qty_b) / 2.0;
        self.state.balance += original_size + total_pnl;

        pos.current_price_a = fill_a;
        pos.current_price_b = fill_b;
        pos.current_zscore = zscore;
        pos.pnl = total_pnl;
        pos.status = PositionStatus::Closed;

        if total_pnl >= 0.0 {
            self.state.win_count += 1;
        } else {
            self.state.loss_count += 1;
        }
        self.state.total_pnl += total_pnl;

        info!(
            "CLOSE #{}: {} {} | PnL=${:.4} (gross ${:.4} - fees ${:.4}) | z={:.2}→{:.2}",
            pos.id, pos.symbol_a, pos.symbol_b, total_pnl,
            pnl_a + pnl_b, pos.pnl - (pnl_a + pnl_b) + total_pnl - (pnl_a + pnl_b),
            pos.entry_zscore, zscore
        );

        self.state.closed_trades.push(pos);
    }

    /// Update z-scores for all open positions
    pub fn update_positions(&mut self, prices: &HashMap<String, f64>) {
        for pos in &mut self.state.positions {
            let price_a = prices.get(&pos.symbol_a).copied().unwrap_or(pos.current_price_a);
            let price_b = prices.get(&pos.symbol_b).copied().unwrap_or(pos.current_price_b);

            pos.current_price_a = price_a;
            pos.current_price_b = price_b;

            if let Some(pair) = self.state.tracked_pairs.iter().find(|p| {
                (p.symbol_a == pos.symbol_a && p.symbol_b == pos.symbol_b)
                    || (p.symbol_a == pos.symbol_b && p.symbol_b == pos.symbol_a)
            }) {
                pos.current_zscore = live_zscore(price_a, price_b, pair.hedge_ratio, pair.spread_mean, pair.spread_std);
            }

            // Recalc unrealized PnL (without exit commission)
            let pnl_a = match pos.side_a {
                PositionSide::Long => (price_a - pos.entry_price_a) * pos.qty_a,
                PositionSide::Short => (pos.entry_price_a - price_a) * pos.qty_a,
            };
            let pnl_b = match pos.side_b {
                PositionSide::Long => (price_b - pos.entry_price_b) * pos.qty_b,
                PositionSide::Short => (pos.entry_price_b - price_b) * pos.qty_b,
            };
            pos.pnl = pnl_a + pnl_b + pos.pnl; // pnl field includes entry commission
        }
    }

    /// Get position by id
    pub fn get_position(&self, id: u64) -> Option<&Position> {
        self.state.positions.iter().find(|p| p.id == id)
    }

    /// Print trading summary
    pub fn print_summary(&self) {
        let total_trades = self.state.win_count + self.state.loss_count;
        let win_rate = if total_trades > 0 {
            self.state.win_count as f64 / total_trades as f64 * 100.0
        } else {
            0.0
        };

        let roi = if self.state.initial_balance > 0.0 {
            (self.state.balance - self.state.initial_balance) / self.state.initial_balance * 100.0
        } else {
            0.0
        };

        println!();
        println!("{}", "═══════════════════════════════════════════════".bright_cyan());
        println!("{}", "  PAPER TRADING".bright_cyan().bold());
        println!("{}", "═══════════════════════════════════════════════".bright_cyan());
        println!(
            "  Balance:     ${:.2}  (ROI: {}{:.2}%)",
            self.state.balance,
            if roi >= 0.0 { "+" } else { "" },
            roi
        );
        println!("  Initial:     ${:.2}", self.state.initial_balance);
        println!("  Total PnL:   {}", format_pnl(self.state.total_pnl));
        println!(
            "  Win/Loss:    {}/{} ({:.1}% win rate)",
            self.state.win_count, self.state.loss_count, win_rate
        );
        println!("  Open pos:    {}", self.state.positions.len());
        println!(
            "  Entry guard: {}/5 factors required",
            self.config.entry_min_factors
        );
        println!(
            "  Commission:  {:.3}% per leg + {:.3}% slippage",
            self.config.commission_pct, self.config.slippage_pct
        );

        if !self.state.positions.is_empty() {
            println!();
            println!(
                "  {:<4} {:<12} {:<12} {:>8} {:>8} {:>10}",
                "ID", "Symbol A", "Symbol B", "Entry z", "Curr z", "Unr. PnL"
            );
            println!("  {}", "─".repeat(60));
            for pos in &self.state.positions {
                let pnl_str = format_pnl(pos.pnl);
                println!(
                    "  {:<4} {:<12} {:<12} {:>8.2} {:>8.2} {}",
                    pos.id, pos.symbol_a, pos.symbol_b,
                    pos.entry_zscore, pos.current_zscore, pnl_str
                );
            }
        }

        if !self.state.closed_trades.is_empty() {
            println!();
            println!("  Last 5 closed:");
            for trade in self.state.closed_trades.iter().rev().take(5) {
                println!(
                    "    #{} {} ↔ {} {}",
                    trade.id, trade.symbol_a, trade.symbol_b, format_pnl(trade.pnl)
                );
            }
        }
        println!();
    }
}

fn format_pnl(pnl: f64) -> String {
    if pnl >= 0.0 {
        format!("+${:.4}", pnl).green().to_string()
    } else {
        format!("-${:.4}", pnl.abs()).red().to_string()
    }
}

fn format_price(p: f64) -> String {
    if p >= 1000.0 { format!("{:.2}", p) }
    else if p >= 1.0 { format!("{:.4}", p) }
    else { format!("{:.8}", p) }
}

fn format_qty(q: f64) -> String {
    if q >= 1.0 { format!("{:.4}", q) }
    else { format!("{:.8}", q) }
}

use colored::Colorize;
