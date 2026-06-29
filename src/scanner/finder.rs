use std::collections::HashMap;

use crate::exchange::traits::Exchange;
use crate::stats::cointegration::CointResult;
use crate::stats::math::pearson;

/// Finds correlated and cointegrated pairs using a cheap two-phase approach:
/// Phase 1: Pearson correlation filter (O(n²) but cheap — just arithmetic)
/// Phase 2: Engle-Granger test only on high-correlation pairs (expensive but few)
pub struct CointFinder {
    pub min_correlation: f64,
    pub min_candles: usize,
}

impl CointFinder {
    pub fn new(min_correlation: f64) -> Self {
        Self {
            min_correlation,
            min_candles: 50,
        }
    }

    /// Phase 1: Find all correlated pairs
    /// Returns (symbol_a, symbol_b, correlation) sorted by |correlation| descending
    pub fn find_correlated(
        &self,
        kline_data: &HashMap<String, Vec<f64>>,
    ) -> Vec<(String, String, f64)> {
        let symbols: Vec<&String> = kline_data
            .iter()
            .filter(|(_, prices)| prices.len() >= self.min_candles)
            .map(|(s, _)| s)
            .collect();

        let n = symbols.len();
        if n < 2 {
            return Vec::new();
        }

        let mut pairs = Vec::new();

        // Use log returns for correlation (more statistically sound than raw prices)
        let returns: HashMap<&String, Vec<f64>> = kline_data
            .iter()
            .filter(|(_, prices)| prices.len() >= self.min_candles)
            .map(|(sym, prices)| {
                let ret: Vec<f64> = prices
                    .windows(2)
                    .map(|w| (w[1] / w[0]) - 1.0)
                    .collect();
                (sym, ret)
            })
            .collect();

        for i in 0..n {
            for j in (i + 1)..n {
                let ret_a = match returns.get(symbols[i]) {
                    Some(r) => r,
                    None => continue,
                };
                let ret_b = match returns.get(symbols[j]) {
                    Some(r) => r,
                    None => continue,
                };

                let min_len = ret_a.len().min(ret_b.len());
                if min_len < self.min_candles {
                    continue;
                }

                let corr = pearson(
                    &ret_a[ret_a.len() - min_len..],
                    &ret_b[ret_b.len() - min_len..],
                );

                if corr.abs() >= self.min_correlation {
                    pairs.push((symbols[i].clone(), symbols[j].clone(), corr));
                }
            }
        }

        pairs.sort_by(|a, b| b.2.abs().partial_cmp(&a.2.abs()).unwrap_or(std::cmp::Ordering::Equal));

        pairs
    }

    /// Phase 2: Test cointegration on correlated pairs
    pub fn find_cointegrated(
        &self,
        kline_data: &HashMap<String, Vec<f64>>,
        correlated: &[(String, String, f64)],
    ) -> Vec<CointResult> {
        let mut results = Vec::new();

        for (sym_a, sym_b, corr) in correlated {
            let prices_a = match kline_data.get(sym_a) {
                Some(p) => p,
                None => continue,
            };
            let prices_b = match kline_data.get(sym_b) {
                Some(p) => p,
                None => continue,
            };

            if prices_a.len() < self.min_candles || prices_b.len() < self.min_candles {
                continue;
            }

            let result = crate::stats::cointegration::engle_granger(
                sym_a, sym_b, prices_a, prices_b, *corr,
            );

            if result.is_cointegrated && result.half_life_periods.is_finite() {
                results.push(result);
            }
        }

        results.sort_by(|a, b| {
            b.current_zscore
                .abs()
                .partial_cmp(&a.current_zscore.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results
    }

    /// Full scan: correlation pre-filter + cointegration test
    pub async fn scan<E: Exchange + Send + Sync>(
        &self,
        exchange: &E,
        symbols: &[String],
        interval: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<CointResult>> {
        let kline_data = exchange.fetch_all_klines(symbols, interval, limit).await?;

        let valid_count = kline_data.values().filter(|p| p.len() >= self.min_candles).count();
        println!(
            "  {} Fetched valid klines for {} symbols",
            "▸".to_string().bright_green(),
            valid_count
        );

        let correlated = self.find_correlated(&kline_data);
        println!(
            "  {} {} pairs passed correlation filter (ρ ≥ {:.2})",
            "▸".to_string().bright_green(),
            correlated.len(),
            self.min_correlation
        );

        let cointegrated = self.find_cointegrated(&kline_data, &correlated);
        println!(
            "  {} {} cointegrated pairs found",
            "▸".to_string().bright_green(),
            cointegrated.len()
        );

        Ok(cointegrated)
    }
}

use colored::Colorize;
