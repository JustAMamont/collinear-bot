use anyhow::{Result, bail};
use async_trait::async_trait;
use log::warn;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::traits::{Exchange, Kline, SymbolInfo};

pub struct BybitExchange {
    client: Client,
    base_url: String,
    semaphore: Arc<Semaphore>,
}

impl BybitExchange {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("lead-lag/0.4")
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            base_url: "https://api.bybit.com".to_string(),
            semaphore: Arc::new(Semaphore::new(5)),
        }
    }

    pub fn with_base_url(base_url: String, rate_limit: usize) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("lead-lag/0.4")
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            base_url,
            semaphore: Arc::new(Semaphore::new(rate_limit)),
        }
    }

    async fn get_tickers_map(&self) -> Result<HashMap<String, serde_json::Value>> {
        let url = format!("{}/v5/market/tickers?category=linear", self.base_url);
        let resp = self.client.get(&url).send().await?;
        let data: serde_json::Value = resp.json().await?;

        let list = data["result"]["list"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("No tickers list"))?;

        let mut map = HashMap::new();
        for t in list {
            if let Some(symbol) = t["symbol"].as_str() {
                map.insert(symbol.to_string(), t.clone());
            }
        }

        Ok(map)
    }

    /// Fetch candles for many symbols with proper rate limiting for the prefetch phase.
    /// Uses max 5 concurrent requests with 200ms delay between batches.
    /// After every 5 requests, sleeps 1 second to stay under API limits.
    pub async fn fetch_candles_batch(
        &self,
        symbols: &[String],
        interval: &str,
        limit: u32,
    ) -> Result<HashMap<String, Vec<f64>>> {
        let mut results = HashMap::new();
        let client = Arc::new(Self::new());
        let sem = self.semaphore.clone();

        // Process in chunks of 5 with rate limiting
        let chunk_size = 5;
        for (chunk_idx, chunk) in symbols.chunks(chunk_size).enumerate() {
            // After every 5 requests (i.e., after each chunk), sleep 1 second
            if chunk_idx > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            let mut handles = Vec::new();

            for symbol in chunk {
                let sym = symbol.clone();
                let interval = interval.to_string();
                let cl = client.clone();
                let s = sem.clone();

                let handle = tokio::spawn(async move {
                    let _permit = s.acquire().await.unwrap();
                    // 200ms delay between requests within a batch
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    match cl.get_klines(&sym, &interval, limit).await {
                        Ok(klines) => {
                            let closes: Vec<f64> = klines.iter().map(|k| k.close).collect();
                            Some((sym, closes))
                        }
                        Err(_) => None,
                    }
                });
                handles.push(handle);
            }

            for handle in handles {
                if let Ok(Some((sym, closes))) = handle.await {
                    results.insert(sym, closes);
                }
            }

            println!(
                "  {} Fetched {}/{} symbols ({} candles each)",
                "▸".to_string().bright_cyan(),
                results.len(),
                symbols.len(),
                limit
            );
        }

        Ok(results)
    }
}

#[async_trait]
impl Exchange for BybitExchange {
    fn name(&self) -> &str {
        "bybit"
    }

    async fn get_futures_symbols(&self) -> Result<Vec<SymbolInfo>> {
        let url = format!(
            "{}/v5/market/instruments-info?category=linear&limit=1000",
            self.base_url
        );
        let resp = self.client.get(&url).send().await?;
        let data: serde_json::Value = resp.json().await?;

        if data["retCode"].as_i64() != Some(0) {
            bail!("Bybit API error: {}", data["retMsg"]);
        }

        let tickers = self.get_tickers_map().await?;

        let instruments = data["result"]["list"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("No instruments in response"))?;

        let mut symbols = Vec::new();

        for inst in instruments {
            let quote = inst["quoteCoin"].as_str().unwrap_or("");
            let status = inst["status"].as_str().unwrap_or("");
            if quote != "USDT" || status != "Trading" {
                continue;
            }

            let symbol = inst["symbol"].as_str().unwrap_or("").to_string();

            let ticker = tickers.get(&symbol);
            let volume_24h_usd = ticker
                .and_then(|t| t["turnover24h"].as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let last_price = ticker
                .and_then(|t| t["lastPrice"].as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);

            if volume_24h_usd < 100_000.0 {
                continue; // Skip illiquid
            }

            symbols.push(SymbolInfo {
                symbol,
                volume_24h_usd,
                last_price,
            });
        }

        symbols.sort_by(|a, b| {
            b.volume_24h_usd
                .partial_cmp(&a.volume_24h_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(symbols)
    }

    async fn get_klines(&self, symbol: &str, interval: &str, limit: u32) -> Result<Vec<Kline>> {
        let mut all_klines = Vec::new();
        let per_page = 200u32;
        let mut end_time: Option<u64> = None;
        let mut remaining = limit;
        let mut request_count = 0u32;

        while remaining > 0 {
            let batch = remaining.min(per_page);
            let mut url = format!(
                "{}/v5/market/kline?category=linear&symbol={}&interval={}&limit={}",
                self.base_url, symbol, interval, batch
            );
            if let Some(et) = end_time {
                url.push_str(&format!("&end={}", et));
            }

            let _permit = self.semaphore.acquire().await.unwrap();

            // Rate limiting: after every 5 requests, sleep 1 second
            request_count += 1;
            if request_count > 0 && request_count % 5 == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            let resp = match self.client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Kline request failed for {}: {}", symbol, e);
                    break;
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    warn!("Kline parse failed for {}: {}", symbol, e);
                    break;
                }
            };

            let list = match data["result"]["list"].as_array() {
                Some(l) => l,
                None => break,
            };

            if list.is_empty() {
                break;
            }

            let mut batch_klines = Vec::new();
            for k in list {
                batch_klines.push(Kline {
                    timestamp: k[0].as_str().unwrap_or("0").parse::<u64>().unwrap_or(0),
                    close: k[4].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0),
                });
            }

            batch_klines.sort_by_key(|k| k.timestamp);
            if let Some(first) = batch_klines.first() {
                end_time = Some(first.timestamp - 1);
            }

            all_klines.extend(batch_klines);
            remaining = remaining.saturating_sub(batch);

            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }

        all_klines.sort_by_key(|k| k.timestamp);
        all_klines.dedup_by_key(|k| k.timestamp);

        Ok(all_klines)
    }

    async fn fetch_all_klines(
        &self,
        symbols: &[String],
        interval: &str,
        limit: u32,
    ) -> Result<HashMap<String, Vec<f64>>> {
        let mut results = HashMap::new();
        let client = Arc::new(Self::new());
        let sem = self.semaphore.clone();

        let mut handles = Vec::new();

        for symbol in symbols {
            let sym = symbol.clone();
            let interval = interval.to_string();
            let cl = client.clone();
            let s = sem.clone();

            let handle = tokio::spawn(async move {
                let _permit = s.acquire().await.unwrap();
                match cl.get_klines(&sym, &interval, limit).await {
                    Ok(klines) => {
                        let closes: Vec<f64> = klines.iter().map(|k| k.close).collect();
                        Some((sym, closes))
                    }
                    Err(_) => None,
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            if let Ok(Some((sym, closes))) = handle.await {
                results.insert(sym, closes);
            }
        }

        Ok(results)
    }

    async fn get_mark_price(&self, symbol: &str) -> Result<f64> {
        let url = format!(
            "{}/v5/market/tickers?category=linear&symbol={}",
            self.base_url, symbol
        );
        let resp = self.client.get(&url).send().await?;
        let data: serde_json::Value = resp.json().await?;

        let price = data["result"]["list"][0]["markPrice"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok(price)
    }
}

use colored::Colorize;
