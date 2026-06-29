/// Core math primitives — no external dependencies

/// Arithmetic mean
#[inline]
pub fn mean(data: &[f64]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    data.iter().sum::<f64>() / data.len() as f64
}

/// Population standard deviation
#[inline]
pub fn std_dev(data: &[f64]) -> f64 {
    if data.len() < 2 {
        return 0.0;
    }
    let m = mean(data);
    let variance = data.iter().map(|x| (x - m).powi(2)).sum::<f64>() / data.len() as f64;
    variance.sqrt()
}

/// Pearson correlation coefficient
pub fn pearson(x: &[f64], y: &[f64]) -> f64 {
    if x.len() != y.len() || x.len() < 3 {
        return 0.0;
    }
    let mx = mean(x);
    let my = mean(y);
    let mut cov = 0.0_f64;
    let mut vx = 0.0_f64;
    let mut vy = 0.0_f64;
    for i in 0..x.len() {
        let dx = x[i] - mx;
        let dy = y[i] - my;
        cov += dx * dy;
        vx += dx * dx;
        vy += dy * dy;
    }
    let d = vx.sqrt() * vy.sqrt();
    if d < 1e-15 { 0.0 } else { cov / d }
}

/// OLS regression: y = alpha + beta * x. Returns (alpha, beta).
pub fn ols(x: &[f64], y: &[f64]) -> (f64, f64) {
    if x.len() != y.len() || x.len() < 3 {
        return (0.0, 1.0);
    }
    let mx = mean(x);
    let my = mean(y);
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for i in 0..x.len() {
        let dx = x[i] - mx;
        num += dx * (y[i] - my);
        den += dx * dx;
    }
    if den.abs() < 1e-15 {
        return (my, 0.0);
    }
    let beta = num / den;
    let alpha = my - beta * mx;
    (alpha, beta)
}

/// Hurst exponent via Rescaled Range (R/S) analysis.
///
/// H < 0.5  → mean-reverting (good for pairs entry)
/// H ≈ 0.5  → random walk (no edge)
/// H > 0.5  → trending (dangerous — cointegration likely broken)
///
/// Method: split series into sub-blocks of varying length,
/// compute R/S for each, regress log(R/S) on log(n), slope = H.
pub fn hurst_exponent(series: &[f64]) -> f64 {
    let n = series.len();
    if n < 100 {
        return 0.5; // insufficient data → assume random walk
    }

    let mut log_ns: Vec<f64> = Vec::new();
    let mut log_rs: Vec<f64> = Vec::new();

    // Test multiple subseries sizes for robust regression
    for &size in &[10, 20, 50, 100, 200, 500] {
        if size > n / 2 {
            break;
        }

        let num_blocks = n / size;
        let mut rs_values: Vec<f64> = Vec::new();

        for i in 0..num_blocks {
            let block = &series[i * size..(i + 1) * size];
            let m = mean(block);

            // Cumulative deviations from mean
            let mut cumdev = Vec::with_capacity(size);
            let mut running = 0.0_f64;
            for &val in block {
                running += val - m;
                cumdev.push(running);
            }

            let r = cumdev.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - cumdev.iter().cloned().fold(f64::INFINITY, f64::min);
            let s = std_dev(block);

            if s > 1e-10 && r > 0.0 {
                rs_values.push(r / s);
            }
        }

        if !rs_values.is_empty() {
            let avg_rs = mean(&rs_values);
            if avg_rs > 0.0 {
                log_ns.push((size as f64).ln());
                log_rs.push(avg_rs.ln());
            }
        }
    }

    if log_ns.len() < 2 {
        return 0.5;
    }

    let (_, h) = ols(&log_ns, &log_rs);

    // Clamp to [0, 1] — anything outside is a numerical artifact
    if h.is_nan() || h < 0.0 {
        0.5
    } else if h > 1.0 {
        1.0
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pearson_perfect() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![2.0, 4.0, 6.0, 8.0, 10.0];
        assert!((pearson(&x, &y) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_pearson_negative() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![10.0, 8.0, 6.0, 4.0, 2.0];
        assert!((pearson(&x, &y) + 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_ols() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![3.0, 5.0, 7.0, 9.0, 11.0]; // y = 1 + 2x
        let (alpha, beta) = ols(&x, &y);
        assert!((alpha - 1.0).abs() < 1e-10);
        assert!((beta - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_mean() {
        assert!((mean(&[2.0, 4.0, 6.0]) - 4.0).abs() < 1e-10);
    }

    #[test]
    fn test_std_dev() {
        let data = vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let s = std_dev(&data);
        assert!((s - 2.0).abs() < 0.1);
    }
}
