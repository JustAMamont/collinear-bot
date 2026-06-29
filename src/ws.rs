use crate::candle_cache::CandleCache;
use futures_util::{SinkExt, StreamExt};
use log::{info, warn, error};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const BYBIT_WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";

/// WebSocket manager for Bybit kline streams.
/// Batches symbols into groups of `batch_size` per WS connection.
/// Auto-reconnects with exponential backoff on disconnect.
pub struct WsManager {
    cache: Arc<Mutex<CandleCache>>,
    symbols: Vec<String>,
    interval: String,
    batch_size: usize,
    ping_sec: u64,
}

impl WsManager {
    pub fn new(
        cache: Arc<Mutex<CandleCache>>,
        symbols: Vec<String>,
        interval: String,
        batch_size: usize,
        ping_sec: u64,
    ) -> Self {
        Self {
            cache,
            symbols,
            interval,
            batch_size,
            ping_sec,
        }
    }

    /// Start WS connections: spawns a tokio task for each batch of symbols.
    pub fn start(&self) {
        let batches = self.symbols.chunks(self.batch_size);
        for (i, batch) in batches.enumerate() {
            let batch: Vec<String> = batch.to_vec();
            let cache = self.cache.clone();
            let interval = self.interval.clone();
            let ping_sec = self.ping_sec;
            let batch_id = i;

            info!(
                "WS batch {}: starting connection for {} symbols",
                batch_id,
                batch.len()
            );

            tokio::spawn(async move {
                run_ws_batch(batch_id, batch, cache, interval, ping_sec).await;
            });
        }
    }
}

/// Run a single WS connection for a batch of symbols.
/// Auto-reconnects with exponential backoff on disconnect.
async fn run_ws_batch(
    batch_id: usize,
    symbols: Vec<String>,
    cache: Arc<Mutex<CandleCache>>,
    interval: String,
    ping_sec: u64,
) {
    let mut backoff_secs: u64 = 1;
    let max_backoff: u64 = 60;

    loop {
        match connect_async(BYBIT_WS_URL).await {
            Ok((ws_stream, _response)) => {
                info!("WS batch {}: connected", batch_id);
                backoff_secs = 1; // Reset backoff on successful connect

                let (mut write, mut read) = ws_stream.split();

                // Subscribe to kline topics
                let args: Vec<String> = symbols
                    .iter()
                    .map(|s| format!("kline.{}.{}", interval, s))
                    .collect();

                let sub_msg = serde_json::json!({
                    "op": "subscribe",
                    "args": args
                });

                if let Err(e) = write.send(Message::Text(sub_msg.to_string())).await {
                    error!("WS batch {}: subscribe failed: {}", batch_id, e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff);
                    continue;
                }

                info!(
                    "WS batch {}: subscribed to {} kline topics",
                    batch_id,
                    symbols.len()
                );

                // Spawn ping task
                let ping_write = Arc::new(tokio::sync::Mutex::new(write));
                let ping_write_clone = ping_write.clone();
                let ping_handle = tokio::spawn(async move {
                    let mut interval_timer =
                        tokio::time::interval(tokio::time::Duration::from_secs(ping_sec));
                    loop {
                        interval_timer.tick().await;
                        let mut w = ping_write_clone.lock().await;
                        let ping = serde_json::json!({"op": "ping"});
                        if w.send(Message::Text(ping.to_string())).await.is_err() {
                            break;
                        }
                    }
                });

                // Read loop
                let mut symbol_set = std::collections::HashSet::new();
                for s in &symbols {
                    symbol_set.insert(s.clone());
                }

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            // Parse kline message
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                                // Check if it's a pong response
                                if parsed["op"] == "pong" || parsed.get("ret_msg").map_or(false, |v| v == "pong") {
                                    continue;
                                }

                                // Check if it's a kline update
                                let topic = parsed["topic"].as_str().unwrap_or("");
                                if topic.starts_with("kline.") {
                                    if let Some(data_arr) = parsed["data"].as_array() {
                                        for kline in data_arr {
                                            let close_str = kline["close"].as_str().unwrap_or("0");
                                            let close: f64 = close_str.parse().unwrap_or(0.0);
                                            let confirm = kline["confirm"].as_bool().unwrap_or(false);

                                            // Extract symbol from topic: "kline.5.BTCUSDT"
                                            let parts: Vec<&str> = topic.split('.').collect();
                                            if parts.len() >= 3 {
                                                let symbol = parts[2];
                                                if symbol_set.contains(symbol) && close > 0.0 {
                                                    let mut cache = cache.lock().await;
                                                    if confirm {
                                                        // Confirmed candle complete: append new close
                                                        cache.update(symbol, close);
                                                    } else {
                                                        // Interim tick: replace last candle in place
                                                        // (do NOT append — would flood cache with ticks)
                                                        cache.update_last(symbol, close);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(_)) => {
                            // tungstenite handles pong automatically
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => {
                            warn!("WS batch {}: connection closed by server", batch_id);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            error!("WS batch {}: read error: {}", batch_id, e);
                            break;
                        }
                    }
                }

                // Clean up ping task
                ping_handle.abort();

                warn!(
                    "WS batch {}: disconnected, reconnecting in {}s",
                    batch_id, backoff_secs
                );
            }
            Err(e) => {
                error!("WS batch {}: connect failed: {}", batch_id, e);
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(max_backoff);
    }
}
