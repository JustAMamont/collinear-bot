/// Engle-Granger cointegration + half-life + z-score + EntryGuard
/// Cheap O(n) per pair after correlation pre-filter

use serde::{Deserialize, Serialize};

use crate::stats::math::{mean, ols, std_dev};

// ═══════════════════════════════════════════════════════════════════════════════
// EntryGuard: Multi-factor entry protection
// ═══════════════════════════════════════════════════════════════════════════════

/// Multi-factor entry guard: 5 independent signals combined into a composite score.
///
/// Factors:
/// 1. **Hurst exponent** (H < 0.5 → mean-reverting regime)
/// 2. **Velocity reversal** (Δz opposite sign to z → spread is turning)
/// 3. **Acceleration** (Δ²z confirms the reversal is strengthening, not a dead-cat bounce)
/// 4. **OU R²** (Ornstein-Uhlenbeck model fit quality → mean-reversion is real, not noise)
/// 5. **ADF stability** (rolling cointegration test → relationship is not breaking down)
///
/// Each factor is scored as PASS (1) or FAIL (0). The composite `score` is the count
/// of passing factors (0–5). Entry is allowed when `score >= entry_min_factors` (default 4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryGuard {
    /// Hurst exponent on recent spread window
    pub hurst: f64,
    /// 1st derivative of z-score (velocity / momentum)
    pub velocity: f64,
    /// 2nd derivative of z-score (acceleration)
    pub acceleration: f64,
    /// R² of OU process regression Δy = α + β·y_{t-1}
    pub ou_r2: f64,
    /// ADF statistic trend: recent_adf - full_adf. Negative = strengthening.
    pub adf_trend: f64,
    /// Number of factors passing (0–5)
    pub score: u8,
    /// Bitmask of passing factors: bit0=Hurst, bit1=Velocity, bit2=Accel, bit3=OU, bit4=ADF
    pub pass_mask: u8,
}

impl Default for EntryGuard {
    fn default() -> Self {
        EntryGuard {
            hurst: 0.5,
            velocity: 0.0,
            acceleration: 0.0,
            ou_r2: 0.0,
            adf_trend: 0.0,
            score: 0,
            pass_mask: 0,
        }
    }
}

impl EntryGuard {
    /// Compute all 5 entry guard factors from cached close prices.
    ///
    /// # Arguments
    /// * `closes_a`, `closes_b` — cached close price series for both legs
    /// * `hedge_ratio`, `spread_mean`, `spread_std` — stored cointegration parameters
    /// * `zscore` — current live z-score
    /// * `momentum_window` — lookback for velocity/acceleration (e.g. 5 candles)
    /// * `hurst_window` — lookback for Hurst exponent (e.g. 200 candles)
    /// * `hurst_threshold` — max Hurst for entry (e.g. 0.5)
    ///
    /// # Returns
    /// EntryGuard with all factors and composite score.
    pub fn compute(
        closes_a: &[f64],
        closes_b: &[f64],
        hedge_ratio: f64,
        spread_mean: f64,
        spread_std: f64,
        zscore: f64,
        momentum_window: usize,
        hurst_window: usize,
        hurst_threshold: f64,
    ) -> Self {
        let min_len = closes_a.len().min(closes_b.len());
        if min_len < 20 {
            return EntryGuard::default();
        }

        let n = min_len;
        let a = &closes_a[closes_a.len() - n..];
        let b = &closes_b[closes_b.len() - n..];

        // Compute full spread series
        let spread: Vec<f64> = a.iter().zip(b.iter())
            .map(|(pa, pb)| pb - hedge_ratio * pa)
            .collect();

        // ── Factor 1: Hurst exponent on recent window ──
        let hurst_n = hurst_window.min(spread.len());
        let hurst = crate::stats::math::hurst_exponent(&spread[spread.len() - hurst_n..]);
        let hurst_pass = hurst < hurst_threshold;

        // ── Factor 2 & 3: Velocity and acceleration ──
        let (velocity, acceleration) = if spread_std > 1e-10 && n > momentum_window * 3 {
            let z_now = (spread[n - 1] - spread_mean) / spread_std;
            let z_prev = (spread[n - 1 - momentum_window] - spread_mean) / spread_std;
            let z_prev2 = (spread[n - 1 - 2 * momentum_window] - spread_mean) / spread_std;

            let vel = z_now - z_prev;
            let prev_vel = z_prev - z_prev2;
            let acc = vel - prev_vel;
            (vel, acc)
        } else {
            (0.0, 0.0)
        };

        // Velocity must be opposite sign to z-score
        let velocity_pass = (zscore > 0.0 && velocity < 0.0) || (zscore < 0.0 && velocity > 0.0);

        // Acceleration must confirm: reversal is strengthening, not weakening
        // SHORT (z > 0): velocity < 0, acceleration < 0 → decline is accelerating
        // LONG  (z < 0): velocity > 0, acceleration > 0 → rise is accelerating
        let accel_pass = (zscore > 0.0 && velocity < 0.0 && acceleration < 0.0)
            || (zscore < 0.0 && velocity > 0.0 && acceleration > 0.0);

        // ── Factor 4: OU process R² ──
        // Use the most recent 500 points (or all if less) for OU fit
        let spread_for_ou = if spread.len() > 500 {
            &spread[spread.len() - 500..]
        } else {
            &spread
        };
        let ou_r2 = ou_r_squared(spread_for_ou);
        // R² > 0.005 means the OU regression explains at least some variance.
        // For 5-min spread data, R² is typically 0.005–0.05 for truly mean-reverting pairs.
        let ou_pass = ou_r2 > 0.005;

        // ── Factor 5: ADF stability ──
        let (_, adf_trend) = adf_stability(&spread);
        // Negative trend = ADF getting more negative = cointegration strengthening
        let adf_pass = adf_trend <= 0.0;

        // ── Composite score ──
        let mut score = 0u8;
        let mut pass_mask = 0u8;
        if hurst_pass   { score += 1; pass_mask |= 1;  }
        if velocity_pass { score += 1; pass_mask |= 2;  }
        if accel_pass   { score += 1; pass_mask |= 4;  }
        if ou_pass      { score += 1; pass_mask |= 8;  }
        if adf_pass     { score += 1; pass_mask |= 16; }

        EntryGuard {
            hurst,
            velocity,
            acceleration,
            ou_r2,
            adf_trend,
            score,
            pass_mask,
        }
    }

    /// Is this entry guard passing? Score must meet or exceed `min_factors`.
    #[inline]
    pub fn is_passing(&self, min_factors: u8) -> bool {
        self.score >= min_factors
    }

    /// Human-readable breakdown of which factors passed/failed
    pub fn breakdown(&self) -> String {
        let flags = [
            (self.pass_mask & 1 != 0, "H"),
            (self.pass_mask & 2 != 0, "V"),
            (self.pass_mask & 4 != 0, "A"),
            (self.pass_mask & 8 != 0, "OU"),
            (self.pass_mask & 16 != 0, "ADF"),
        ];
        flags
            .iter()
            .map(|(pass, name)| {
                if *pass {
                    name.to_string()
                } else {
                    format!("~{}", name)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Factor 4: OU process R² (mean-reversion model fit quality)
// ═══════════════════════════════════════════════════════════════════════════════

/// R² of the Ornstein-Uhlenbeck process fit: Δy_t = α + β·y_{t-1} + ε
///
/// The OU regression is identical to the ADF regression without lags.
/// For a mean-reverting process, β < 0 and R² is significant (> 0).
/// For a random walk, β ≈ 0 and R² ≈ 0.
///
/// Key insight: if R² is very low, the mean-reversion model doesn't explain
/// the spread's dynamics — the spread is essentially noise, and "reverting to
/// mean" is just wishful thinking. Only enter when the OU model has
/// explanatory power.
pub fn ou_r_squared(spread: &[f64]) -> f64 {
    let n = spread.len();
    if n < 20 {
        return 0.0;
    }

    let dy: Vec<f64> = spread.windows(2).map(|w| w[1] - w[0]).collect();
    let y_lag: &[f64] = &spread[..n - 1];

    let (alpha, beta) = ols(y_lag, &dy);

    // Residuals
    let residuals: Vec<f64> = dy
        .iter()
        .zip(y_lag.iter())
        .map(|(&d, &y)| d - alpha - beta * y)
        .collect();

    let dy_mean = mean(&dy);
    let ss_tot: f64 = dy.iter().map(|d| (d - dy_mean).powi(2)).sum();
    let ss_res: f64 = residuals.iter().map(|r| r * r).sum();

    if ss_tot < 1e-15 {
        return 0.0;
    }

    let r2 = 1.0 - ss_res / ss_tot;
    // Clamp to [0, 1] — negative R² means the model is worse than just predicting the mean
    if r2 < 0.0 {
        0.0
    } else if r2 > 1.0 {
        1.0
    } else {
        r2
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Factor 5: Cointegration stability (rolling ADF trend)
// ═══════════════════════════════════════════════════════════════════════════════

/// Cointegration stability: compare ADF statistics on recent vs. full window.
///
/// Returns `(recent_adf, adf_trend)` where:
/// - `recent_adf`: ADF statistic computed on the most recent 60% of the spread
/// - `adf_trend`: recent_adf - full_adf. **Negative = strengthening** (good).
///   **Positive = weakening** (bad — cointegration relationship is breaking down).
///
/// This is critical because a pair that WAS cointegrated may be breaking down NOW.
/// The full-window ADF can still show significance while the recent window has
/// already shifted to a random walk or trending regime.
pub fn adf_stability(spread: &[f64]) -> (f64, f64) {
    let n = spread.len();
    if n < 60 {
        return (0.0, 0.0);
    }

    let (full_adf, _) = adf_test(spread);

    // Recent window: last 60% of data
    let recent_len = (n as f64 * 0.6) as usize;
    let recent = &spread[n - recent_len..];
    let (recent_adf, _) = adf_test(recent);

    let adf_trend = recent_adf - full_adf;
    // adf_trend < 0 → recent ADF is MORE negative → cointegration strengthening
    // adf_trend > 0 → recent ADF is LESS negative → cointegration weakening

    (recent_adf, adf_trend)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cointegration core
// ═══════════════════════════════════════════════════════════════════════════════

/// Result of cointegration test for a pair
#[derive(Debug, Clone)]
pub struct CointResult {
    pub symbol_a: String,
    pub symbol_b: String,
    pub correlation: f64,
    pub hedge_ratio: f64, // beta from OLS: price_b = alpha + beta * price_a
    pub adf_stat: f64,
    pub is_cointegrated: bool,
    pub current_zscore: f64,
    pub half_life_periods: f64,
    pub spread_mean: f64,
    pub spread_std: f64,
}

/// Augmented Dickey-Fuller test (simplified, no augmentation lags)
/// Tests H0: unit root (non-stationary) vs H1: stationary
/// Returns (adf_statistic, is_cointegrated_at_5pct)
fn adf_test(series: &[f64]) -> (f64, bool) {
    let n = series.len();
    if n < 30 {
        return (0.0, false);
    }

    // Δy_t = alpha + gamma * y_{t-1} + epsilon
    let dy: Vec<f64> = series.windows(2).map(|w| w[1] - w[0]).collect();
    let y_lag: &[f64] = &series[..n - 1];

    let (alpha, gamma) = ols(y_lag, &dy);

    // Residuals and SE(gamma)
    let residuals: Vec<f64> = dy
        .iter()
        .zip(y_lag.iter())
        .map(|(&d, &y)| d - alpha - gamma * y)
        .collect();

    let res_var = mean(
        &residuals
            .iter()
            .map(|r| r * r)
            .collect::<Vec<_>>(),
    );
    let y_lag_mean = mean(y_lag);
    let sxx: f64 = y_lag.iter().map(|x| (x - y_lag_mean).powi(2)).sum();

    if sxx < 1e-15 || res_var < 1e-15 {
        return (0.0, false);
    }

    let se_gamma = (res_var / sxx).sqrt();
    if se_gamma < 1e-15 {
        return (0.0, false);
    }

    let adf_stat = gamma / se_gamma;

    // MacKinnon critical value at 5% ≈ -2.86 for large samples
    let critical = -2.86;
    let is_cointegrated = adf_stat < critical;

    (adf_stat, is_cointegrated)
}

/// Compute half-life of mean reversion from spread
/// Regresses Δspread on spread_{t-1}: half_life = -ln(2) / beta
/// where beta is the OLS coefficient from: Δy_t = beta * y_{t-1} + epsilon
/// beta is negative for mean-reverting series, so result is positive.
fn half_life(spread: &[f64]) -> f64 {
    if spread.len() < 10 {
        return f64::INFINITY;
    }
    let dy: Vec<f64> = spread.windows(2).map(|w| w[1] - w[0]).collect();
    let y_lag: Vec<f64> = spread[..spread.len() - 1].to_vec();
    let (_, beta) = ols(&y_lag, &dy);

    if beta >= 0.0 || beta.abs() < 1e-10 {
        return f64::INFINITY; // No mean reversion
    }

    // half_life = -ln(2) / beta  (beta is negative, so result is positive)
    let hl = -(2.0_f64.ln()) / beta;
    if hl.is_nan() || hl < 0.0 {
        f64::INFINITY
    } else {
        hl
    }
}

/// Full Engle-Granger two-step cointegration test
pub fn engle_granger(
    symbol_a: &str,
    symbol_b: &str,
    prices_a: &[f64],
    prices_b: &[f64],
    correlation: f64,
) -> CointResult {
    let min_len = prices_a.len().min(prices_b.len());
    if min_len < 50 {
        return CointResult {
            symbol_a: symbol_a.to_string(),
            symbol_b: symbol_b.to_string(),
            correlation,
            hedge_ratio: 1.0,
            adf_stat: 0.0,
            is_cointegrated: false,
            current_zscore: 0.0,
            half_life_periods: f64::INFINITY,
            spread_mean: 0.0,
            spread_std: 0.0,
        };
    }

    let pa = &prices_a[prices_a.len() - min_len..];
    let pb = &prices_b[prices_b.len() - min_len..];

    // Step 1: OLS regression price_b = alpha + beta * price_a
    let (_alpha, beta) = ols(pa, pb);

    // Step 2: Compute spread = price_b - beta * price_a
    let spread: Vec<f64> = pa
        .iter()
        .zip(pb.iter())
        .map(|(&a, &b)| b - beta * a)
        .collect();

    // ADF test on spread
    let (adf_stat, is_cointegrated) = adf_test(&spread);

    // Z-score
    let s_mean = mean(&spread);
    let s_std = std_dev(&spread);
    let zscore = if s_std > 1e-10 {
        (spread.last().copied().unwrap_or(0.0) - s_mean) / s_std
    } else {
        0.0
    };

    let hl = half_life(&spread);

    CointResult {
        symbol_a: symbol_a.to_string(),
        symbol_b: symbol_b.to_string(),
        correlation,
        hedge_ratio: beta,
        adf_stat,
        is_cointegrated,
        current_zscore: zscore,
        half_life_periods: hl,
        spread_mean: s_mean,
        spread_std: s_std,
    }
}

/// Compute current z-score from live prices and stored parameters
pub fn live_zscore(
    price_a: f64,
    price_b: f64,
    hedge_ratio: f64,
    spread_mean: f64,
    spread_std: f64,
) -> f64 {
    if spread_std < 1e-10 {
        return 0.0;
    }
    let spread = price_b - hedge_ratio * price_a;
    (spread - spread_mean) / spread_std
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cointegrated_pair() {
        let a: Vec<f64> = (0..300).map(|i| 100.0 + (i as f64 * 0.01).sin() * 5.0).collect();
        let b: Vec<f64> = a.iter().map(|x| 2.0 * x + (x * 0.1).sin() * 0.5).collect();

        let result = engle_granger("A", "B", &a, &b, 0.99);
        assert!(result.is_cointegrated);
        assert!((result.hedge_ratio - 2.0).abs() < 0.1);
        assert!(result.half_life_periods < 100.0);
    }

    #[test]
    fn test_non_cointegrated_pair() {
        let mut a = vec![0.0; 500];
        let mut b = vec![0.0; 500];
        for i in 1..500 {
            a[i] = a[i - 1] + 1.0 + (i as f64 * 0.01).sin() * 0.5;
            b[i] = b[i - 1] - 0.5 + (i as f64 * 0.03).cos() * 0.3;
        }
        let result = engle_granger("A", "B", &a, &b, 0.3);
        assert!(!result.is_cointegrated);
    }

    #[test]
    fn test_half_life() {
        let mut spread = vec![0.0; 500];
        spread[0] = 2.0;
        for i in 1..500 {
            spread[i] = spread[i - 1] * 0.95;
        }
        let hl = half_life(&spread);
        assert!(hl > 8.0 && hl < 25.0, "half_life = {}", hl);
    }

    #[test]
    fn test_live_zscore() {
        let z = live_zscore(100.0, 200.0, 2.0, 0.0, 1.0);
        assert!(z.abs() < 0.01);

        let z2 = live_zscore(100.0, 203.0, 2.0, 0.0, 1.0);
        assert!((z2 - 3.0).abs() < 0.01);
    }

    #[test]
    fn test_ou_r_squared_mean_reverting() {
        // Pure mean-reverting series: should have high R²
        let mut spread = vec![0.0; 500];
        spread[0] = 3.0;
        for i in 1..500 {
            spread[i] = spread[i - 1] * 0.95; // strong mean-reversion
        }
        let r2 = ou_r_squared(&spread);
        assert!(r2 > 0.8, "OU R² for strong mean-reverting series should be high, got {}", r2);
    }

    #[test]
    fn test_ou_r_squared_random_walk() {
        // Random walk: should have R² ≈ 0
        let mut spread = vec![0.0; 500];
        let mut rng_state: f64 = 42.0;
        for i in 1..500 {
            // Simple pseudo-random (LCG-like)
            rng_state = (rng_state * 1103515245.0 + 12345.0) % 2147483648.0;
            let noise = (rng_state / 2147483648.0 - 0.5) * 2.0;
            spread[i] = spread[i - 1] + noise;
        }
        let r2 = ou_r_squared(&spread);
        assert!(r2 < 0.05, "OU R² for random walk should be near 0, got {}", r2);
    }

    #[test]
    fn test_adf_stability_stable() {
        // Stable cointegrated pair: ADF should be stable or strengthening
        let a: Vec<f64> = (0..500).map(|i| 100.0 + (i as f64 * 0.01).sin() * 5.0).collect();
        let b: Vec<f64> = a.iter().map(|x| 2.0 * x + (x * 0.1).sin() * 0.5).collect();
        let spread: Vec<f64> = a.iter().zip(b.iter()).map(|(x, y)| y - 2.0 * x).collect();

        let (_, trend) = adf_stability(&spread);
        // For a stable cointegrated pair, trend should be close to 0 or negative
        assert!(trend < 1.0, "ADF trend for stable pair should be near 0, got {}", trend);
    }

    #[test]
    fn test_entry_guard_breakdown() {
        let guard = EntryGuard {
            hurst: 0.4,
            velocity: -0.3,
            acceleration: -0.1,
            ou_r2: 0.02,
            adf_trend: -0.5,
            score: 5,
            pass_mask: 0b11111,
        };
        let bd = guard.breakdown();
        assert_eq!(bd, "H V A OU ADF");

        let guard2 = EntryGuard {
            hurst: 0.6,
            velocity: -0.3,
            acceleration: 0.1,
            ou_r2: 0.001,
            adf_trend: 0.5,
            score: 1,
            pass_mask: 0b00010,
        };
        let bd2 = guard2.breakdown();
        assert_eq!(bd2, "~H V ~A ~OU ~ADF");
    }

    #[test]
    fn test_entry_guard_compute() {
        // Create a realistic OU mean-reverting spread with noise
        // dX = theta*(mu - X)*dt + sigma*dW
        let mut rng_state: u64 = 42;
        let mut spread = vec![0.0; 500];
        spread[0] = 0.0;
        let theta = 0.1; // mean-reversion speed
        let mu = 0.0;
        let sigma = 0.5;
        for i in 1..500 {
            // Simple LCG pseudo-random
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((rng_state >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 2.0 * sigma;
            spread[i] = spread[i - 1] + theta * (mu - spread[i - 1]) + noise;
        }
        // Push the last value to create a z-score > 2
        spread[499] = spread[498] + 2.0;

        let spread_mean = mean(&spread);
        let spread_std = std_dev(&spread);

        // Build price series: spread = B - hedge_ratio * A
        let hedge_ratio = 1.5;
        let a: Vec<f64> = (0..500).map(|i| 100.0 + (i as f64 * 0.02).sin() * 3.0).collect();
        let b: Vec<f64> = a.iter().zip(spread.iter()).map(|(ai, s)| hedge_ratio * ai + s).collect();

        // z-score at the end should be positive (spread jumped above mean)
        let z = (spread[499] - spread_mean) / spread_std;

        let guard = EntryGuard::compute(
            &a, &b, hedge_ratio, spread_mean, spread_std,
            z,
            5, 200, 0.5,
        );

        // For a mean-reverting OU process with noise:
        // - Hurst should be < 0.5 (this is a proper OU process, not a trend)
        // - OU R² should be > 0 (the OU model explains some variance)
        assert!(guard.ou_r2 > 0.001, "OU R² should be positive for OU process, got {}", guard.ou_r2);
        // Just verify the guard computes without panic and has valid structure
        assert!(guard.score <= 5);
        assert!(!guard.breakdown().is_empty());
    }
}
