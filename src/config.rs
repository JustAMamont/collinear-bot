use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::Path;

/// Chat ID that accepts both string and number in JSON
/// Telegram group chats have negative IDs like -1003319647629
#[derive(Debug, Clone, Default)]
pub struct ChatId(pub String);

impl ChatId {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for ChatId {
    fn from(s: String) -> Self {
        ChatId(s)
    }
}

impl Serialize for ChatId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Always serialize as string
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ChatId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, Visitor};
        struct ChatIdVisitor;

        impl<'de> Visitor<'de> for ChatIdVisitor {
            type Value = ChatId;

            fn expecting(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
                fmt.write_str("string or number for chat_id")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ChatId, E> {
                Ok(ChatId(v.to_string()))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ChatId, E> {
                Ok(ChatId(v.to_string()))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ChatId, E> {
                Ok(ChatId(v.to_string()))
            }
        }

        deserializer.deserialize_any(ChatIdVisitor)
    }
}

/// Application configuration loaded from config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Exchange settings
    pub exchange: ExchangeConfig,
    /// Scanner settings
    pub scanner: ScannerConfig,
    /// Trading settings
    pub trading: TradingConfig,
    /// Telegram alerts
    pub telegram: TelegramConfig,
    /// Monitor mode settings
    pub monitor: MonitorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    /// Exchange name: "bybit" (more coming)
    pub name: String,
    /// API base URL override (empty = default)
    pub base_url: String,
    /// Max concurrent API requests
    pub rate_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannerConfig {
    /// Minimum Pearson correlation to pass pre-filter
    pub min_correlation: f64,
    /// Minimum candles required for statistical tests
    pub min_candles: usize,
    /// Kline interval: "1", "3", "5", "15", "30", "60", "240", "D"
    pub interval: String,
    /// Number of klines to fetch per symbol
    pub kline_limit: u32,
    /// Top N symbols by 24h volume to scan
    pub top_n: usize,
    /// Max candles held per symbol in the cache
    pub candle_cache_len: usize,
    /// Number of periods for spread momentum (Δz) calculation
    #[serde(default = "default_momentum_window")]
    pub momentum_window: usize,
    /// Number of candles for Hurst exponent calculation
    #[serde(default = "default_hurst_window")]
    pub hurst_window: usize,
}

fn default_momentum_window() -> usize { 5 }
fn default_hurst_window() -> usize { 500 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    /// Initial paper balance in USDT
    pub balance: f64,
    /// Z-score threshold to open position
    pub zscore_entry: f64,
    /// Z-score threshold to close position
    pub zscore_exit: f64,
    /// Z-score threshold for emergency stop-loss
    pub stop_loss_zscore: f64,
    /// Max % of balance allocated per position (0.25 = 25%)
    pub max_position_pct: f64,
    /// Max simultaneous open positions
    pub max_open_positions: usize,
    /// Skip pairs with half-life longer than this (in periods)
    pub min_half_life: f64,
    /// Taker fee per leg in % (Bybit default: 0.055%)
    pub commission_pct: f64,
    /// Estimated slippage per leg in % (~2 bps for liquid futures)
    pub slippage_pct: f64,
    /// Max Hurst exponent for entry. H < threshold → mean-reverting regime.
    /// H > 0.5 = trending (danger). Default 0.5 = strict mean-reversion only.
    #[serde(default = "default_hurst_threshold")]
    pub hurst_threshold: f64,
    /// Emergency exit if Hurst exceeds this on an open position (regime break)
    #[serde(default = "default_hurst_exit")]
    pub hurst_exit: f64,
    /// Minimum number of EntryGuard factors (out of 5) that must pass for entry.
    /// Default 4 = strict (Hurst + velocity + at least 2 bonus factors).
    /// Set to 3 for more entries, 5 for very strict.
    #[serde(default = "default_entry_min_factors")]
    pub entry_min_factors: u8,
}

fn default_hurst_threshold() -> f64 { 0.5 }
fn default_hurst_exit() -> f64 { 0.6 }
fn default_entry_min_factors() -> u8 { 4 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather (leave empty to disable alerts)
    pub bot_token: String,
    /// Chat ID where alerts are sent (accepts number or string, leave empty to disable)
    pub chat_id: ChatId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Refresh interval in seconds for monitor mode
    pub refresh_sec: u64,
    /// Enable paper trading in monitor mode
    pub trade: bool,
    /// How often to re-scan for cointegrated pairs (minutes)
    pub rescan_min: u64,
    /// Number of symbols per WebSocket connection
    pub ws_batch_size: usize,
    /// WebSocket ping interval in seconds
    pub ws_ping_sec: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            exchange: ExchangeConfig {
                name: "bybit".into(),
                base_url: String::new(),
                rate_limit: 5,
            },
            scanner: ScannerConfig {
                min_correlation: 0.7,
                min_candles: 50,
                interval: "5".into(),
                kline_limit: 4000,
                top_n: 80,
                candle_cache_len: 4000,
                momentum_window: 5,
                hurst_window: 500,
            },
            trading: TradingConfig {
                balance: 100.0,
                zscore_entry: 2.0,
                zscore_exit: 0.5,
                stop_loss_zscore: 4.0,
                max_position_pct: 0.25,
                max_open_positions: 4,
                min_half_life: 200.0,
                commission_pct: 0.055,
                slippage_pct: 0.02,
                hurst_threshold: 0.5,
                hurst_exit: 0.6,
                entry_min_factors: 4,
            },
            telegram: TelegramConfig {
                bot_token: String::new(),
                chat_id: ChatId::default(),
            },
            monitor: MonitorConfig {
                refresh_sec: 60,
                trade: false,
                rescan_min: 30,
                ws_batch_size: 50,
                ws_ping_sec: 20,
            },
        }
    }
}

impl AppConfig {
    /// Load config from a JSON file, falling back to defaults for missing fields
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            log::info!("Config file not found at {:?}, using defaults", path);
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {:?}", path))?;

        let config: Self = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {:?}", path))?;

        Ok(config)
    }

    /// Load from file or create default + save template
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            let config = Self::default();
            if let Ok(json) = serde_json::to_string_pretty(&config) {
                if let Err(e) = std::fs::write(path, &json) {
                    log::warn!("Failed to write default config: {}", e);
                } else {
                    log::info!("Created default config at {:?}", path);
                }
            }
            Ok(config)
        }
    }

    /// Save current config to file
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;
        std::fs::write(path, json)
            .with_context(|| format!("Failed to write config to {:?}", path))?;
        Ok(())
    }

    /// Check if Telegram is configured
    pub fn has_telegram(&self) -> bool {
        !self.telegram.bot_token.is_empty() && !self.telegram.chat_id.is_empty()
    }
}

/// Convert our config TradingConfig to the engine's TradingConfig
impl From<&AppConfig> for crate::trading::models::TradingConfig {
    fn from(cfg: &AppConfig) -> Self {
        Self {
            zscore_entry: cfg.trading.zscore_entry,
            zscore_exit: cfg.trading.zscore_exit,
            stop_loss_zscore: cfg.trading.stop_loss_zscore,
            max_position_pct: cfg.trading.max_position_pct,
            max_open_positions: cfg.trading.max_open_positions,
            min_half_life: cfg.trading.min_half_life,
            commission_pct: cfg.trading.commission_pct,
            slippage_pct: cfg.trading.slippage_pct,
            hurst_threshold: cfg.trading.hurst_threshold,
            hurst_exit: cfg.trading.hurst_exit,
            entry_min_factors: cfg.trading.entry_min_factors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_roundtrip() {
        let config = AppConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.scanner.min_correlation, 0.7);
        assert_eq!(parsed.trading.zscore_entry, 2.0);
        assert_eq!(parsed.trading.commission_pct, 0.055);
        assert_eq!(parsed.exchange.name, "bybit");
        assert_eq!(parsed.scanner.kline_limit, 4000);
        assert_eq!(parsed.scanner.candle_cache_len, 4000);
        assert_eq!(parsed.monitor.ws_batch_size, 50);
        assert_eq!(parsed.monitor.ws_ping_sec, 20);
    }

    #[test]
    fn test_partial_config() {
        // Only override some fields
        let json = r#"{
            "exchange": { "name": "bybit", "base_url": "", "rate_limit": 10 },
            "scanner": { "min_correlation": 0.6, "min_candles": 50, "interval": "1", "kline_limit": 200, "top_n": 50, "candle_cache_len": 4000 },
            "trading": { "balance": 500.0, "zscore_entry": 2.5, "zscore_exit": 0.3, "stop_loss_zscore": 4.0, "max_position_pct": 0.2, "max_open_positions": 3, "min_half_life": 150.0, "commission_pct": 0.055, "slippage_pct": 0.02 },
            "telegram": { "bot_token": "123:ABC", "chat_id": "999" },
            "monitor": { "refresh_sec": 30, "trade": true, "rescan_min": 20, "ws_batch_size": 50, "ws_ping_sec": 20 }
        }"#;

        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.scanner.min_correlation, 0.6);
        assert_eq!(config.trading.balance, 500.0);
        assert_eq!(config.telegram.bot_token, "123:ABC");
        assert!(config.has_telegram());
    }

    #[test]
    fn test_telegram_disabled() {
        let config = AppConfig::default();
        assert!(!config.has_telegram());
    }

    #[test]
    fn test_save_and_load() {
        let dir = std::env::temp_dir().join("lead-lag-test-config");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");

        let config = AppConfig::default();
        config.save(&path).unwrap();

        let loaded = AppConfig::load(&path).unwrap();
        assert!((loaded.scanner.min_correlation - config.scanner.min_correlation).abs() < 1e-10);
        assert!((loaded.trading.balance - config.trading.balance).abs() < 1e-10);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
