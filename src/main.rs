mod candle_cache;
mod config;
mod exchange;
mod notify;
mod scanner;
mod stats;
mod trading;
mod ws;

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::*;
use config::{AppConfig, ChatId};
use exchange::{BybitExchange, Exchange};
use log::{error, info};
use notify::TelegramNotifier;
use scanner::CointFinder;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use trading::engine::PaperEngine;
use trading::models::TradingConfig;

use crate::candle_cache::CandleCache;
use crate::notify::TradeDedup;

#[derive(Parser)]
#[command(name = "lead-lag", about = "Cointegration scanner & paper trader for crypto futures")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config.json
    #[arg(long, global = true, default_value = "config.json")]
    config: String,

    // ── Scanner overrides ─────────────────────────────────────
    /// Minimum correlation for pre-filter
    #[arg(long, global = true)]
    min_correlation: Option<f64>,

    /// Kline interval in minutes (1, 3, 5, 15, 30, 60, 240, D)
    #[arg(long, global = true)]
    interval: Option<String>,

    /// Number of klines per symbol
    #[arg(long, global = true)]
    kline_limit: Option<u32>,

    /// Top N symbols by 24h volume
    #[arg(long, global = true)]
    top_n: Option<usize>,

    // ── Trading overrides ─────────────────────────────────────
    /// Z-score entry threshold
    #[arg(long, global = true)]
    zscore_entry: Option<f64>,

    /// Z-score exit threshold
    #[arg(long, global = true)]
    zscore_exit: Option<f64>,

    /// Paper trading initial balance (USDT)
    #[arg(long, global = true)]
    balance: Option<f64>,

    // ── Telegram overrides ────────────────────────────────────
    /// Telegram bot token (for alerts)
    #[arg(long, global = true)]
    tg_bot_token: Option<String>,

    /// Telegram chat ID (for alerts)
    #[arg(long, global = true)]
    tg_chat_id: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan for cointegrated pairs once and exit (standalone)
    Scan,

    /// Monitor tracked pairs and generate signals (standalone, needs pairs)
    Monitor {
        /// Refresh interval in seconds (overrides config)
        #[arg(long)]
        refresh: Option<u64>,

        /// Enable paper trading (overrides config)
        #[arg(long, default_missing_value = "true", num_args = 0..=1)]
        trade: Option<bool>,
    },

    /// Show paper trading state
    Status,

    /// Reset paper trading state
    Reset,

    /// Send a test Telegram alert
    TestTg,

    /// Generate default config.json and exit
    InitConfig,
}

/// Load config from file, then apply CLI overrides
fn build_config(cli: &Cli) -> AppConfig {
    let config_path = PathBuf::from(&cli.config);
    let mut cfg = AppConfig::load_or_create(&config_path).unwrap_or_else(|e| {
        eprintln!(
            "  {} Failed to load config: {} — using defaults",
            "⚠".bright_yellow(),
            e
        );
        AppConfig::default()
    });

    // Scanner overrides
    if let Some(v) = cli.min_correlation {
        cfg.scanner.min_correlation = v;
    }
    if let Some(v) = cli.interval.clone() {
        cfg.scanner.interval = v;
    }
    if let Some(v) = cli.kline_limit {
        cfg.scanner.kline_limit = v;
    }
    if let Some(v) = cli.top_n {
        cfg.scanner.top_n = v;
    }

    // Trading overrides
    if let Some(v) = cli.zscore_entry {
        cfg.trading.zscore_entry = v;
    }
    if let Some(v) = cli.zscore_exit {
        cfg.trading.zscore_exit = v;
    }
    if let Some(v) = cli.balance {
        cfg.trading.balance = v;
    }

    // Telegram overrides
    if let Some(v) = cli.tg_bot_token.clone() {
        cfg.telegram.bot_token = v;
    }
    if let Some(v) = cli.tg_chat_id.clone() {
        cfg.telegram.chat_id = ChatId(v);
    }

    cfg
}

fn state_path() -> PathBuf {
    let dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = dir.join("lead-lag");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("state.json")
}

fn make_tg_notifier(cfg: &AppConfig) -> TelegramNotifier {
    TelegramNotifier::new(
        cfg.telegram.bot_token.clone(),
        cfg.telegram.chat_id.0.clone(),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let cfg = build_config(&cli);
    let exchange = if cfg.exchange.base_url.is_empty() {
        BybitExchange::new()
    } else {
        BybitExchange::with_base_url(cfg.exchange.base_url.clone(), cfg.exchange.rate_limit)
    };
    let sp = state_path();
    let sp_str = sp.to_string_lossy().to_string();

    match cli.command {
        // Default: no subcommand → full automatic loop with WS
        None => cmd_run(&exchange, &cfg, &sp_str).await,

        Some(Commands::Scan) => cmd_scan(&exchange, &cfg, &sp_str).await,
        Some(Commands::Monitor { refresh, trade }) => {
            let mut mc = cfg.clone();
            if let Some(r) = refresh {
                mc.monitor.refresh_sec = r;
            }
            if let Some(t) = trade {
                mc.monitor.trade = t;
            }
            cmd_monitor(&exchange, &mc, &sp_str).await
        }
        Some(Commands::Status) => cmd_status(&cfg, &sp_str),
        Some(Commands::Reset) => cmd_reset(&sp_str),
        Some(Commands::TestTg) => cmd_test_tg(&cfg).await,
        Some(Commands::InitConfig) => cmd_init_config(&cli),
    }
}

// ─── RUN (default — fully automatic with WS) ─────────────────────────────────

/// Main loop: prefetch → scan → WS → monitor/trade → periodic rescan
async fn cmd_run(exchange: &BybitExchange, cfg: &AppConfig, state_path: &str) -> Result<()> {
    print_banner(cfg);

    let trading_cfg: TradingConfig = cfg.into();
    let mut engine = PaperEngine::new(trading_cfg, cfg.trading.balance, state_path);
    let tg = make_tg_notifier(cfg);
    let has_tg = cfg.has_telegram();
    let mut dedup = TradeDedup::new();

    let rescan_interval = cfg.monitor.rescan_min * 60; // convert minutes → seconds

    // ── Step 1: Prefetch phase — fetch top_n symbols' candles ──
    println!(
        "  {} Phase 1: Prefetching {} candles for top {} symbols...",
        "▸".bright_cyan(),
        cfg.scanner.kline_limit,
        cfg.scanner.top_n
    );

    let symbols = exchange.get_futures_symbols().await?;
    if symbols.is_empty() {
        error!("No futures symbols found. Check API access.");
        return Ok(());
    }

    let top_symbols: Vec<String> = symbols
        .iter()
        .take(cfg.scanner.top_n)
        .map(|s| s.symbol.clone())
        .collect();

    println!(
        "  {} Top {} symbols by volume:",
        "▸".bright_green(),
        top_symbols.len()
    );
    for (i, s) in symbols.iter().take(8).enumerate() {
        println!(
            "    {:>2}. {:<18} ${:>12.0}  price={}",
            i + 1,
            s.symbol,
            s.volume_24h_usd,
            s.last_price
        );
    }
    if cfg.scanner.top_n > 8 {
        println!("    ... and {} more", cfg.scanner.top_n - 8);
    }
    println!();

    // Fetch candles with rate-limited batch fetching
    let kline_data = exchange
        .fetch_candles_batch(&top_symbols, &cfg.scanner.interval, cfg.scanner.kline_limit)
        .await?;

    // Store in CandleCache
    let cache = Arc::new(Mutex::new(CandleCache::new(cfg.scanner.candle_cache_len)));
    {
        let mut cache_guard = cache.lock().await;
        for (sym, closes) in &kline_data {
            cache_guard.insert(sym, closes.clone());
        }
    }

    let cached_count = {
        let cache_guard = cache.lock().await;
        cache_guard.symbols().len()
    };
    println!(
        "  {} Cached candles for {} symbols",
        "▸".bright_green(),
        cached_count
    );

    // ── Step 2: Scan phase — cointegration scan on prefetched data ──
    println!(
        "\n  {} Phase 2: Running cointegration scan...",
        "▸".bright_cyan()
    );

    let finder = CointFinder::new(cfg.scanner.min_correlation);
    let correlated = finder.find_correlated(&kline_data);
    println!(
        "  {} {} pairs passed correlation filter (ρ ≥ {:.2})",
        "▸".bright_green(),
        correlated.len(),
        cfg.scanner.min_correlation
    );

    let cointegrated = finder.find_cointegrated(&kline_data, &correlated);
    println!(
        "  {} {} cointegrated pairs found",
        "▸".bright_green(),
        cointegrated.len()
    );

    if cointegrated.is_empty() {
        println!(
            "  {} No cointegrated pairs. Try --min-correlation 0.6",
            "⚠".bright_yellow()
        );
    } else {
        display_cointegrated(&cointegrated, cfg.trading.zscore_entry);
    }

    // Replace tracked pairs with fresh scan results
    engine.state.tracked_pairs.clear();
    for pair in &cointegrated {
        engine.track_pair(pair);
    }
    engine.save();

    let signal_count = cointegrated
        .iter()
        .filter(|p| p.current_zscore.abs() > cfg.trading.zscore_entry)
        .count();

    // Telegram scan summary
    tg.alert_scan_summary(top_symbols.len(), correlated.len(), cointegrated.len(), signal_count)
        .await
        .ok();

    if engine.state.tracked_pairs.is_empty() {
        println!(
            "  {} No cointegrated pairs found. Will retry in {} min.",
            "⚠".bright_yellow(),
            cfg.monitor.rescan_min
        );
    }

    // ── Step 3: WebSocket phase — start WS connections for tracked pair symbols ──
    let all_tracked_syms: Vec<String> = engine
        .state
        .tracked_pairs
        .iter()
        .flat_map(|p| vec![p.symbol_a.clone(), p.symbol_b.clone()])
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if !all_tracked_syms.is_empty() {
        println!(
            "\n  {} Phase 3: Starting WebSocket connections for {} symbols...",
            "▸".bright_cyan(),
            all_tracked_syms.len()
        );

        let ws_manager = ws::WsManager::new(
            cache.clone(),
            all_tracked_syms.clone(),
            cfg.scanner.interval.clone(),
            cfg.monitor.ws_batch_size,
            cfg.monitor.ws_ping_sec,
        );
        ws_manager.start();

        println!(
            "  {} WS connections started (batch size: {})",
            "▸".bright_green(),
            cfg.monitor.ws_batch_size
        );
    } else {
        println!(
            "\n  {} Phase 3: Skipped — no symbols to stream",
            "⚠".bright_yellow()
        );
    }

    println!(
        "\n  {} Auto-loop: {} pairs | refresh={}s | rescan={}min | trade={}",
        "▸".bright_cyan(),
        engine.state.tracked_pairs.len(),
        cfg.monitor.refresh_sec,
        cfg.monitor.rescan_min,
        cfg.monitor.trade
    );
    println!(
        "  {} Entry: |z|>{:.1}  Exit: |z|<{:.1}  Stop: |z|>{:.1}  Guard: {}/5 factors",
        "▸".bright_cyan(),
        cfg.trading.zscore_entry,
        cfg.trading.zscore_exit,
        cfg.trading.stop_loss_zscore,
        cfg.trading.entry_min_factors
    );
    if has_tg {
        println!("  {} Telegram alerts: ON (trades only)", "▸".bright_green());
    } else {
        println!(
            "  {} Telegram alerts: OFF (set bot_token + chat_id in config.json)",
            "▸".bright_yellow()
        );
    }
    println!();

    let mut last_scan = Instant::now();
    let mut cycle = 0u64;

    // ── Step 4: Monitor/Trade loop ──
    loop {
        cycle += 1;
        let elapsed_since_scan = last_scan.elapsed().as_secs();

        // ── Periodic rescan ───────────────────────────────────
        if elapsed_since_scan >= rescan_interval {
            println!(
                "\n  {} Rescanning ({} min elapsed)...",
                "⟳".bright_cyan(),
                cfg.monitor.rescan_min
            );

            // Rescan using WS-updated cache
            do_rescan(&cache, cfg, &mut engine, &tg).await?;
            last_scan = Instant::now();

            if engine.state.tracked_pairs.is_empty() {
                println!(
                    "  {} Still no pairs. Next rescan in {} min.",
                    "⚠".bright_yellow(),
                    cfg.monitor.rescan_min
                );
            }

            // Restart WS for new symbols if needed
            let new_syms: Vec<String> = engine
                .state
                .tracked_pairs
                .iter()
                .flat_map(|p| vec![p.symbol_a.clone(), p.symbol_b.clone()])
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            if !new_syms.is_empty() {
                let ws_manager = ws::WsManager::new(
                    cache.clone(),
                    new_syms,
                    cfg.scanner.interval.clone(),
                    cfg.monitor.ws_batch_size,
                    cfg.monitor.ws_ping_sec,
                );
                ws_manager.start();
            }
        }

        // ── Auto-scan if pairs empty (first run or after failed scan) ──
        if engine.state.tracked_pairs.is_empty() {
            println!(
                "  {} No tracked pairs — waiting {}s before rescan...",
                "▸".bright_yellow(),
                cfg.monitor.refresh_sec
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(cfg.monitor.refresh_sec)).await;
            continue;
        }

        // ── Read current prices from cache ────────────────────
        let mut prices = std::collections::HashMap::new();
        {
            let cache_guard = cache.lock().await;
            for pair in &engine.state.tracked_pairs {
                if let Some(pa) = cache_guard.last_price(&pair.symbol_a) {
                    prices.insert(pair.symbol_a.clone(), pa);
                }
                if let Some(pb) = cache_guard.last_price(&pair.symbol_b) {
                    prices.insert(pair.symbol_b.clone(), pb);
                }
            }
        }

        // Fallback: fetch missing prices via REST
        let all_syms: Vec<String> = engine
            .state
            .tracked_pairs
            .iter()
            .flat_map(|p| vec![p.symbol_a.clone(), p.symbol_b.clone()])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        for sym in &all_syms {
            if !prices.contains_key(sym) {
                if let Ok(p) = exchange.get_mark_price(sym).await {
                    prices.insert(sym.clone(), p);
                }
            }
        }

        // ── Update z-scores + entry filters ──────────────────
        for pair in &mut engine.state.tracked_pairs {
            if let (Some(pa), Some(pb)) = (prices.get(&pair.symbol_a), prices.get(&pair.symbol_b))
            {
                pair.last_zscore = stats::cointegration::live_zscore(
                    *pa,
                    *pb,
                    pair.hedge_ratio,
                    pair.spread_mean,
                    pair.spread_std,
                );
                pair.last_updated = chrono::Utc::now();

                // Compute EntryGuard (5-factor composite) from candle cache
                let cache_guard = cache.lock().await;
                if let (Some(a_deque), Some(b_deque)) = (
                    cache_guard.get(&pair.symbol_a),
                    cache_guard.get(&pair.symbol_b),
                ) {
                    let a_vec: Vec<f64> = a_deque.iter().copied().collect();
                    let b_vec: Vec<f64> = b_deque.iter().copied().collect();
                    let guard = stats::cointegration::EntryGuard::compute(
                        &a_vec,
                        &b_vec,
                        pair.hedge_ratio,
                        pair.spread_mean,
                        pair.spread_std,
                        pair.last_zscore,
                        cfg.scanner.momentum_window,
                        cfg.scanner.hurst_window,
                        cfg.trading.hurst_threshold,
                    );
                    pair.spread_momentum = guard.velocity;
                    pair.hurst = guard.hurst;
                    pair.entry_guard = guard;
                }
            }
        }

        // ── Update positions ──────────────────────────────────
        engine.update_positions(&prices);

        // ── Generate signals ──────────────────────────────────
        let signals = engine.generate_signals(&prices);

        // ── Print status ──────────────────────────────────────
        let now = chrono::Local::now().format("%H:%M:%S");
        let next_scan_in = if rescan_interval > elapsed_since_scan {
            rescan_interval - elapsed_since_scan
        } else {
            0
        };
        println!(
            "\n{} [cycle #{} | next scan in {}s]",
            format!("[{}]", now).dimmed(),
            cycle,
            next_scan_in
        );

        for pair in &engine.state.tracked_pairs {
            let z = pair.last_zscore;
            let z_str = format!("{:+.3}", z);
            let z_display = if z.abs() > cfg.trading.zscore_entry {
                z_str.red().bold().to_string()
            } else if z.abs() > 1.5 {
                z_str.yellow().to_string()
            } else {
                z_str.green().to_string()
            };

            let guard = &pair.entry_guard;
            let mom = guard.velocity;
            let mom_str = format!("{:+.3}", mom);
            let mom_display = if (z > 0.0 && mom < 0.0) || (z < 0.0 && mom > 0.0) {
                mom_str.green().to_string()
            } else if z.abs() > cfg.trading.zscore_entry {
                mom_str.red().to_string()
            } else {
                mom_str.dimmed().to_string()
            };

            let h = guard.hurst;
            let h_display = if h < cfg.trading.hurst_threshold {
                format!("{:.2}", h).green().to_string()
            } else if h < cfg.trading.hurst_exit {
                format!("{:.2}", h).yellow().to_string()
            } else {
                format!("{:.2}", h).red().bold().to_string()
            };

            // EntryGuard score and breakdown
            let score_str = format!("{}/5", guard.score);
            let score_display = if guard.is_passing(cfg.trading.entry_min_factors) {
                score_str.green().bold().to_string()
            } else {
                score_str.red().to_string()
            };
            let breakdown = guard.breakdown();

            let guard_passing = guard.is_passing(cfg.trading.entry_min_factors);

            let dir = if z.abs() > cfg.trading.zscore_entry && guard_passing {
                if z > 0.0 { format!(" ✓ SHORT A / LONG B [{}]", breakdown).bright_green().to_string() }
                else { format!(" ✓ LONG A / SHORT B [{}]", breakdown).bright_green().to_string() }
            } else if z.abs() > cfg.trading.zscore_entry {
                format!(" ✗ [{}] (need {}/5)", breakdown, cfg.trading.entry_min_factors).yellow().to_string()
            } else {
                "".to_string()
            };

            println!(
                "  {} ↔ {}  z={}  Δz={}  H={}  guard={}  hl={:.0}  β={:.4}{}",
                pair.symbol_a.bright_white(),
                pair.symbol_b.bright_white(),
                z_display,
                mom_display,
                h_display,
                score_display,
                pair.half_life,
                pair.hedge_ratio,
                dir
            );
        }

        // ── Execute paper trades + Telegram alerts ──────
        if cfg.monitor.trade && !signals.is_empty() {
            // Pre-fetch open positions for close alerts
            let closes: Vec<_> = signals
                .iter()
                .filter_map(|s| {
                    if let trading::models::Signal::ClosePosition { position_id, zscore } = s {
                        engine
                            .get_position(*position_id)
                            .map(|pos| (*position_id, *zscore, pos.symbol_a.clone(), pos.symbol_b.clone(), pos.entry_zscore, pos.pnl))
                    } else {
                        None
                    }
                })
                .collect();

            // Open position alerts — get info before execution
            let opens: Vec<_> = signals
                .iter()
                .filter_map(|s| match s {
                    trading::models::Signal::OpenLong {
                        symbol_a,
                        symbol_b,
                        zscore,
                        hedge_ratio,
                    } => Some((
                        symbol_a.clone(),
                        symbol_b.clone(),
                        "LONG".to_string(),
                        "SHORT".to_string(),
                        *zscore,
                        *hedge_ratio,
                    )),
                    trading::models::Signal::OpenShort {
                        symbol_a,
                        symbol_b,
                        zscore,
                        hedge_ratio,
                    } => Some((
                        symbol_a.clone(),
                        symbol_b.clone(),
                        "SHORT".to_string(),
                        "LONG".to_string(),
                        *zscore,
                        *hedge_ratio,
                    )),
                    _ => None,
                })
                .collect();

            // Execute all signals
            engine.execute_signals(signals, &prices);

            // Send TG alerts for opens
            for (sym_a, sym_b, side_a, side_b, z, beta) in &opens {
                // Find the position id that was just opened
                if let Some(pos) = engine
                    .state
                    .positions
                    .iter()
                    .find(|p| p.symbol_a == *sym_a && p.symbol_b == *sym_b && (p.current_zscore - z).abs() < 0.1)
                {
                    if dedup.should_alert_open(pos.id) {
                        let pa = prices.get(sym_a).copied().unwrap_or(0.0);
                        let pb = prices.get(sym_b).copied().unwrap_or(0.0);
                        let size = cfg.trading.balance * cfg.trading.max_position_pct;
                        tg.alert_open(sym_a, sym_b, side_a, side_b, *z, *beta, pa, pb, size)
                            .await
                            .ok();
                    }
                }
            }

            // Send TG alerts for closes
            for (pid, zscore, sym_a, sym_b, entry_z, pnl) in &closes {
                if dedup.should_alert_close(*pid) {
                    let is_stop = zscore.abs() > cfg.trading.stop_loss_zscore - 0.5;
                    if is_stop {
                        tg.alert_stop_loss(
                            &sym_a,
                            &sym_b,
                            *entry_z,
                            *zscore,
                            *pnl,
                            engine.state.balance,
                        )
                        .await
                        .ok();
                    } else {
                        tg.alert_close(
                            &sym_a,
                            &sym_b,
                            *entry_z,
                            *zscore,
                            *pnl,
                            engine.state.balance,
                        )
                        .await
                        .ok();
                    }
                }
            }

            dedup.cleanup();
        }

        engine.save();
        engine.print_summary();

        tokio::time::sleep(tokio::time::Duration::from_secs(cfg.monitor.refresh_sec)).await;
    }
}

/// Rescan using the WS-updated cache data
async fn do_rescan(
    cache: &Arc<Mutex<CandleCache>>,
    cfg: &AppConfig,
    engine: &mut PaperEngine,
    tg: &TelegramNotifier,
) -> Result<()> {
    let kline_data = {
        let cache_guard = cache.lock().await;
        cache_guard.to_hashmap()
    };

    let valid_count = kline_data
        .values()
        .filter(|p| p.len() >= cfg.scanner.min_candles)
        .count();
    info!(
        "Rescan: {} symbols with valid data in cache",
        valid_count
    );

    if valid_count < 2 {
        println!(
            "  {} Not enough symbols in cache for rescan ({})",
            "⚠".bright_yellow(),
            valid_count
        );
        return Ok(());
    }

    let finder = CointFinder::new(cfg.scanner.min_correlation);
    let correlated = finder.find_correlated(&kline_data);
    println!(
        "  {} {} pairs passed correlation filter",
        "▸".bright_green(),
        correlated.len()
    );

    let cointegrated = finder.find_cointegrated(&kline_data, &correlated);
    println!(
        "  {} {} cointegrated pairs found",
        "▸".bright_green(),
        cointegrated.len()
    );

    if !cointegrated.is_empty() {
        display_cointegrated(&cointegrated, cfg.trading.zscore_entry);
    }

    // Replace tracked pairs
    engine.state.tracked_pairs.clear();
    for pair in &cointegrated {
        engine.track_pair(pair);
    }
    engine.save();

    let signal_count = cointegrated
        .iter()
        .filter(|p| p.current_zscore.abs() > cfg.trading.zscore_entry)
        .count();

    tg.alert_scan_summary(valid_count, correlated.len(), cointegrated.len(), signal_count)
        .await
        .ok();

    Ok(())
}

// ─── SCAN (standalone) ──────────────────────────────────────────────────────

async fn cmd_scan(exchange: &BybitExchange, cfg: &AppConfig, state_path: &str) -> Result<()> {
    print_banner(cfg);

    let trading_cfg: TradingConfig = cfg.into();
    let mut engine = PaperEngine::new(trading_cfg, cfg.trading.balance, state_path);

    do_scan(exchange, cfg, &mut engine).await?;

    println!(
        "  {} {} pairs saved. Run <monitor> or just start the bot without arguments.",
        "▸".bright_green(),
        engine.state.tracked_pairs.len()
    );

    Ok(())
}

/// Core scan logic — reusable from both cmd_scan and cmd_run
async fn do_scan(
    exchange: &BybitExchange,
    cfg: &AppConfig,
    engine: &mut PaperEngine,
) -> Result<()> {
    info!("Fetching {} futures symbols...", exchange.name());
    let symbols = exchange.get_futures_symbols().await?;

    if symbols.is_empty() {
        error!("No futures symbols found. Check API access.");
        return Ok(());
    }

    let top_symbols: Vec<String> = symbols
        .iter()
        .take(cfg.scanner.top_n)
        .map(|s| s.symbol.clone())
        .collect();

    println!(
        "  {} Top {} symbols by volume:",
        "▸".bright_green(),
        top_symbols.len()
    );
    for (i, s) in symbols.iter().take(8).enumerate() {
        println!(
            "    {:>2}. {:<18} ${:>12.0}  price={}",
            i + 1,
            s.symbol,
            s.volume_24h_usd,
            s.last_price
        );
    }
    if cfg.scanner.top_n > 8 {
        println!("    ... and {} more", cfg.scanner.top_n - 8);
    }
    println!();

    // Scan
    let finder = CointFinder::new(cfg.scanner.min_correlation);
    let cointegrated = finder
        .scan(exchange, &top_symbols, &cfg.scanner.interval, cfg.scanner.kline_limit)
        .await?;

    if cointegrated.is_empty() {
        println!(
            "  {} No cointegrated pairs. Try --min-correlation 0.6",
            "⚠".bright_yellow()
        );
        return Ok(());
    }

    display_cointegrated(&cointegrated, cfg.trading.zscore_entry);

    // Get correlated count for TG summary
    let correlated_count = finder
        .find_correlated(
            &exchange
                .fetch_all_klines(&top_symbols, &cfg.scanner.interval, cfg.scanner.kline_limit)
                .await?,
        )
        .len();

    // Replace tracked pairs with fresh scan results
    engine.state.tracked_pairs.clear();
    for pair in &cointegrated {
        engine.track_pair(pair);
    }
    engine.save();

    let signal_count = cointegrated
        .iter()
        .filter(|p| p.current_zscore.abs() > cfg.trading.zscore_entry)
        .count();

    // Telegram alert
    let tg = make_tg_notifier(cfg);
    tg.alert_scan_summary(top_symbols.len(), correlated_count, cointegrated.len(), signal_count)
        .await
        .ok();

    Ok(())
}

// ─── MONITOR (standalone, manual — no WS) ───────────────────────────────────

async fn cmd_monitor(exchange: &BybitExchange, cfg: &AppConfig, state_path: &str) -> Result<()> {
    let trading_cfg: TradingConfig = cfg.into();
    let mut engine = PaperEngine::new(trading_cfg, cfg.trading.balance, state_path);
    let mut dedup = TradeDedup::new();
    let tg = make_tg_notifier(cfg);
    let has_tg = cfg.has_telegram();

    // Auto-scan if no tracked pairs
    if engine.state.tracked_pairs.is_empty() {
        println!(
            "  {} No tracked pairs — auto-scanning...",
            "▸".bright_cyan()
        );
        do_scan(exchange, cfg, &mut engine).await?;
        if engine.state.tracked_pairs.is_empty() {
            println!(
                "  {} No cointegrated pairs found. Try --min-correlation 0.6 or check API access.",
                "⚠".bright_yellow()
            );
            return Ok(());
        }
    }

    println!(
        "\n  {} Monitoring {} pairs | refresh={}s | trade={}",
        "▸".bright_cyan(),
        engine.state.tracked_pairs.len(),
        cfg.monitor.refresh_sec,
        cfg.monitor.trade
    );
    println!(
        "  {} Entry: |z|>{:.1}  Exit: |z|<{:.1}  Stop: |z|>{:.1}  Guard: {}/5 factors",
        "▸".bright_cyan(),
        cfg.trading.zscore_entry,
        cfg.trading.zscore_exit,
        cfg.trading.stop_loss_zscore,
        cfg.trading.entry_min_factors
    );
    if has_tg {
        println!("  {} Telegram alerts: ON (trades only)", "▸".bright_green());
    } else {
        println!(
            "  {} Telegram alerts: OFF (set bot_token + chat_id in config.json)",
            "▸".bright_yellow()
        );
    }
    println!();

    loop {
        let all_syms: Vec<String> = engine
            .state
            .tracked_pairs
            .iter()
            .flat_map(|p| vec![p.symbol_a.clone(), p.symbol_b.clone()])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let kline_data = exchange
            .fetch_all_klines(&all_syms, &cfg.scanner.interval, 500)
            .await
            .unwrap_or_default();

        let mut prices = std::collections::HashMap::new();
        for (sym, closes) in &kline_data {
            if let Some(last) = closes.last() {
                prices.insert(sym.clone(), *last);
            }
        }

        for sym in &all_syms {
            if !prices.contains_key(sym) {
                if let Ok(p) = exchange.get_mark_price(sym).await {
                    prices.insert(sym.clone(), p);
                }
            }
        }

        for pair in &mut engine.state.tracked_pairs {
            if let (Some(pa), Some(pb)) = (prices.get(&pair.symbol_a), prices.get(&pair.symbol_b))
            {
                pair.last_zscore = stats::cointegration::live_zscore(
                    *pa,
                    *pb,
                    pair.hedge_ratio,
                    pair.spread_mean,
                    pair.spread_std,
                );
                pair.last_updated = chrono::Utc::now();

                // Compute EntryGuard (5-factor composite) from kline data
                if let (Some(a_closes), Some(b_closes)) = (
                    kline_data.get(&pair.symbol_a),
                    kline_data.get(&pair.symbol_b),
                ) {
                    let guard = stats::cointegration::EntryGuard::compute(
                        a_closes,
                        b_closes,
                        pair.hedge_ratio,
                        pair.spread_mean,
                        pair.spread_std,
                        pair.last_zscore,
                        cfg.scanner.momentum_window,
                        cfg.scanner.hurst_window,
                        cfg.trading.hurst_threshold,
                    );
                    pair.spread_momentum = guard.velocity;
                    pair.hurst = guard.hurst;
                    pair.entry_guard = guard;
                }
            }
        }

        engine.update_positions(&prices);
        let signals = engine.generate_signals(&prices);

        let now = chrono::Local::now().format("%H:%M:%S");
        println!("\n{}", format!("[{} — monitor cycle]", now).dimmed());

        for pair in &engine.state.tracked_pairs {
            let z = pair.last_zscore;
            let z_str = format!("{:+.3}", z);
            let z_display = if z.abs() > cfg.trading.zscore_entry {
                z_str.red().bold().to_string()
            } else if z.abs() > 1.5 {
                z_str.yellow().to_string()
            } else {
                z_str.green().to_string()
            };

            let guard = &pair.entry_guard;
            let mom = guard.velocity;
            let h = guard.hurst;

            let score_str = format!("{}/5", guard.score);
            let score_display = if guard.is_passing(cfg.trading.entry_min_factors) {
                score_str.green().bold().to_string()
            } else {
                score_str.red().to_string()
            };
            let breakdown = guard.breakdown();

            let guard_passing = guard.is_passing(cfg.trading.entry_min_factors);

            let dir = if z.abs() > cfg.trading.zscore_entry && guard_passing {
                if z > 0.0 { format!(" ✓ SHORT A / LONG B [{}]", breakdown).bright_green().to_string() }
                else { format!(" ✓ LONG A / SHORT B [{}]", breakdown).bright_green().to_string() }
            } else if z.abs() > cfg.trading.zscore_entry {
                format!(" ✗ [{}] (need {}/5)", breakdown, cfg.trading.entry_min_factors).yellow().to_string()
            } else {
                "".to_string()
            };

            println!(
                "  {} ↔ {}  z={}  Δz={:+.3}  H={:.2}  guard={}  hl={:.0}  β={:.4}{}",
                pair.symbol_a.bright_white(),
                pair.symbol_b.bright_white(),
                z_display,
                mom,
                h,
                score_display,
                pair.half_life,
                pair.hedge_ratio,
                dir
            );
        }

        if cfg.monitor.trade && !signals.is_empty() {
            // Collect close info before execution
            let closes: Vec<_> = signals
                .iter()
                .filter_map(|s| {
                    if let trading::models::Signal::ClosePosition { position_id, zscore } = s {
                        engine.get_position(*position_id).map(|pos| {
                            (*position_id, *zscore, pos.symbol_a.clone(), pos.symbol_b.clone(), pos.entry_zscore, pos.pnl)
                        })
                    } else {
                        None
                    }
                })
                .collect();

            // Collect open info
            let opens: Vec<_> = signals
                .iter()
                .filter_map(|s| match s {
                    trading::models::Signal::OpenLong {
                        symbol_a,
                        symbol_b,
                        zscore,
                        hedge_ratio,
                    } => Some((
                        symbol_a.clone(),
                        symbol_b.clone(),
                        "LONG".to_string(),
                        "SHORT".to_string(),
                        *zscore,
                        *hedge_ratio,
                    )),
                    trading::models::Signal::OpenShort {
                        symbol_a,
                        symbol_b,
                        zscore,
                        hedge_ratio,
                    } => Some((
                        symbol_a.clone(),
                        symbol_b.clone(),
                        "SHORT".to_string(),
                        "LONG".to_string(),
                        *zscore,
                        *hedge_ratio,
                    )),
                    _ => None,
                })
                .collect();

            engine.execute_signals(signals, &prices);

            // Send TG alerts for opens
            for (sym_a, sym_b, side_a, side_b, z, beta) in &opens {
                if let Some(pos) = engine
                    .state
                    .positions
                    .iter()
                    .find(|p| p.symbol_a == *sym_a && p.symbol_b == *sym_b && (p.current_zscore - z).abs() < 0.1)
                {
                    if dedup.should_alert_open(pos.id) {
                        let pa = prices.get(sym_a).copied().unwrap_or(0.0);
                        let pb = prices.get(sym_b).copied().unwrap_or(0.0);
                        let size = cfg.trading.balance * cfg.trading.max_position_pct;
                        tg.alert_open(sym_a, sym_b, side_a, side_b, *z, *beta, pa, pb, size)
                            .await
                            .ok();
                    }
                }
            }

            // Send TG alerts for closes
            for (pid, zscore, sym_a, sym_b, entry_z, pnl) in &closes {
                if dedup.should_alert_close(*pid) {
                    let is_stop = zscore.abs() > cfg.trading.stop_loss_zscore - 0.5;
                    if is_stop {
                        tg.alert_stop_loss(&sym_a, &sym_b, *entry_z, *zscore, *pnl, engine.state.balance)
                            .await
                            .ok();
                    } else {
                        tg.alert_close(&sym_a, &sym_b, *entry_z, *zscore, *pnl, engine.state.balance)
                            .await
                            .ok();
                    }
                }
            }

            dedup.cleanup();
        }

        engine.save();
        engine.print_summary();

        tokio::time::sleep(tokio::time::Duration::from_secs(cfg.monitor.refresh_sec)).await;
    }
}

// ─── STATUS ──────────────────────────────────────────────────────────────────

fn cmd_status(cfg: &AppConfig, state_path: &str) -> Result<()> {
    let trading_cfg: TradingConfig = cfg.into();
    let engine = PaperEngine::new(trading_cfg, cfg.trading.balance, state_path);

    println!(
        "\n  {} Tracked pairs: {}",
        "▸".bright_cyan(),
        engine.state.tracked_pairs.len()
    );
    println!(
        "  {} Open positions: {}",
        "▸".bright_cyan(),
        engine.state.positions.len()
    );

    for pair in &engine.state.tracked_pairs {
        println!(
            "    {} ↔ {}  β={:.4}  hl={:.0}  z={:.3}  guard={}/5  {}",
            pair.symbol_a, pair.symbol_b, pair.hedge_ratio, pair.half_life,
            pair.last_zscore, pair.entry_guard.score, pair.entry_guard.breakdown()
        );
    }

    engine.print_summary();
    Ok(())
}

// ─── RESET ───────────────────────────────────────────────────────────────────

fn cmd_reset(state_path: &str) -> Result<()> {
    if std::path::Path::new(state_path).exists() {
        std::fs::remove_file(state_path)?;
        println!("  {} Trading state reset.", "✓".bright_green());
    } else {
        println!(
            "  {} No trading state to reset.",
            "▸".bright_yellow()
        );
    }
    Ok(())
}

// ─── TEST TG ─────────────────────────────────────────────────────────────────

async fn cmd_test_tg(cfg: &AppConfig) -> Result<()> {
    let tg = make_tg_notifier(cfg);
    println!("  {} Sending test message...", "▸".bright_cyan());
    match tg.send("✅ lead-lag Telegram alerts are working!").await {
        Ok(_) => println!("  {} Message sent!", "✓".bright_green()),
        Err(e) => println!("  {} Failed: {}", "✗".bright_red(), e),
    }
    Ok(())
}

// ─── INIT CONFIG ─────────────────────────────────────────────────────────────

fn cmd_init_config(cli: &Cli) -> Result<()> {
    let path = PathBuf::from(&cli.config);
    let config = AppConfig::default();
    config.save(&path)?;
    println!(
        "  {} Default config written to: {}",
        "✓".bright_green(),
        path.to_string_lossy()
    );
    println!(
        "  {} Edit it and run the bot (no args = auto loop)",
        "▸".bright_cyan()
    );
    Ok(())
}

// ─── UI HELPERS ──────────────────────────────────────────────────────────────

fn print_banner(cfg: &AppConfig) {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════════════════"
            .bright_cyan()
    );
    println!(
        "{}",
        "  lead-lag — Cointegration Scanner & Paper Trader"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "═══════════════════════════════════════════════════"
            .bright_cyan()
    );
    println!();
    println!(
        "  {}  Exchange:           {}",
        "▸".bright_green(),
        cfg.exchange.name
    );
    println!(
        "  {}  Correlation filter: ρ ≥ {:.2}",
        "▸".bright_green(),
        cfg.scanner.min_correlation
    );
    println!(
        "  {}  Z-score entry:      |z| > {:.1}",
        "▸".bright_green(),
        cfg.trading.zscore_entry
    );
    println!(
        "  {}  Z-score exit:       |z| < {:.1}",
        "▸".bright_green(),
        cfg.trading.zscore_exit
    );
    println!(
        "  {}  Stop-loss:          |z| > {:.1}",
        "▸".bright_green(),
        cfg.trading.stop_loss_zscore
    );
    println!(
        "  {}  Hurst entry:        H < {:.2}",
        "▸".bright_green(),
        cfg.trading.hurst_threshold
    );
    println!(
        "  {}  Hurst exit:         H > {:.2}",
        "▸".bright_green(),
        cfg.trading.hurst_exit
    );
    println!(
        "  {}  Momentum window:    {} periods",
        "▸".bright_green(),
        cfg.scanner.momentum_window
    );
    println!(
        "  {}  Interval:           {}m",
        "▸".bright_green(),
        cfg.scanner.interval
    );
    println!(
        "  {}  Klines per symbol:  {}",
        "▸".bright_green(),
        cfg.scanner.kline_limit
    );
    println!(
        "  {}  Candle cache len:   {}",
        "▸".bright_green(),
        cfg.scanner.candle_cache_len
    );
    println!(
        "  {}  Top N by volume:    {}",
        "▸".bright_green(),
        cfg.scanner.top_n
    );
    println!(
        "  {}  Balance:            ${:.0}",
        "▸".bright_green(),
        cfg.trading.balance
    );
    println!(
        "  {}  Commission:         {:.3}% + {:.3}% slippage",
        "▸".bright_green(),
        cfg.trading.commission_pct,
        cfg.trading.slippage_pct
    );
    println!(
        "  {}  WS batch size:      {}",
        "▸".bright_green(),
        cfg.monitor.ws_batch_size
    );
    println!(
        "  {}  WS ping interval:   {}s",
        "▸".bright_green(),
        cfg.monitor.ws_ping_sec
    );
    println!(
        "  {}  Rescan interval:    {} min",
        "▸".bright_green(),
        cfg.monitor.rescan_min
    );
    println!(
        "  {}  Telegram:           {}",
        "▸".bright_green(),
        if cfg.has_telegram() {
            "ON ✓".bright_green()
        } else {
            "OFF".bright_yellow()
        }
    );
    println!();
}

fn display_cointegrated(
    cointegrated: &[stats::cointegration::CointResult],
    zscore_entry: f64,
) {
    println!(
        "\n{}",
        "─── Cointegrated Pairs ───"
            .bright_green()
            .bold()
    );
    println!(
        "  {:<4} {:<14} {:<14} {:>7} {:>9} {:>9} {:>8} {:>6}",
        "#", "Asset A", "Asset B", "ρ", "ADF", "Hedge β", "Z-score", "HL"
    );
    println!("  {}", "─".repeat(75));

    for (i, pair) in cointegrated.iter().enumerate() {
        let z_str = format!("{:+.3}", pair.current_zscore);
        let z_colored = if pair.current_zscore.abs() > zscore_entry {
            z_str.red().bold().to_string()
        } else if pair.current_zscore.abs() > 1.5 {
            z_str.yellow().to_string()
        } else {
            z_str.normal().to_string()
        };

        let hl_str = if pair.half_life_periods.is_finite() {
            format!("{:.0}", pair.half_life_periods)
        } else {
            "∞".to_string()
        };

        println!(
            "  {:<4} {:<14} {:<14} {:>7.3} {:>9.3} {:>9.4} {} {:>6}",
            i + 1,
            pair.symbol_a,
            pair.symbol_b,
            pair.correlation,
            pair.adf_stat,
            pair.hedge_ratio,
            z_colored,
            hl_str
        );
    }

    let signals: Vec<_> = cointegrated
        .iter()
        .filter(|p| p.current_zscore.abs() > zscore_entry)
        .collect();

    if !signals.is_empty() {
        println!(
            "\n{}",
            "─── ⚡ DIVERGENCE SIGNALS ───"
                .bright_red()
                .bold()
        );
        for (i, sig) in signals.iter().enumerate() {
            let dir = if sig.current_zscore > 0.0 {
                "SHORT A / LONG B".bright_red()
            } else {
                "LONG A / SHORT B".bright_green()
            };
            println!(
                "  {}. {} ↔ {}  z={:+.3}  {}",
                i + 1,
                sig.symbol_a.bright_white(),
                sig.symbol_b.bright_white(),
                sig.current_zscore,
                dir
            );
        }
    }
    println!();
}
