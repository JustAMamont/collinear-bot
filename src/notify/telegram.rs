use anyhow::Result;
use log::{info, warn};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::Instant;

/// Telegram bot notifier with rate limiting
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    client: Client,
    rate: Arc<Mutex<RateLimit>>,
}

/// Simple rate limiter: respects Telegram's retry_after and enforces min interval
struct RateLimit {
    /// Earliest time we can send the next message
    next_allowed: Instant,
    /// Min seconds between messages (conservative default)
    min_interval_secs: u64,
}

impl TelegramNotifier {
    pub fn new(bot_token: String, chat_id: String) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            bot_token,
            chat_id,
            client,
            rate: Arc::new(Mutex::new(RateLimit {
                next_allowed: Instant::now(),
                min_interval_secs: 2,
            })),
        }
    }

    /// Send a message via Telegram Bot API (with rate limiting)
    pub async fn send(&self, text: &str) -> Result<()> {
        if self.bot_token.is_empty() || self.chat_id.is_empty() {
            return Ok(());
        }

        // Rate limit: wait until we're allowed
        {
            let rl = self.rate.lock().await;
            let now = Instant::now();
            if now < rl.next_allowed {
                let wait = rl.next_allowed - now;
                drop(rl);
                tokio::time::sleep(wait).await;
            }
        }

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );

        let mut params = HashMap::new();
        params.insert("chat_id", self.chat_id.as_str());
        params.insert("text", text);
        params.insert("parse_mode", "HTML");

        let resp = self.client.post(&url).form(&params).send().await;

        match resp {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    info!("TG alert sent OK");
                } else if status.as_u16() == 429 {
                    let body = r.text().await.unwrap_or_default();
                    // Parse retry_after from response
                    let retry_after = Self::parse_retry_after(&body).unwrap_or(30);
                    warn!("TG rate limited, retry after {}s", retry_after);
                    let mut rl = self.rate.lock().await;
                    rl.next_allowed = Instant::now() + std::time::Duration::from_secs(retry_after + 1);
                } else {
                    let body = r.text().await.unwrap_or_default();
                    warn!("TG alert failed: {} — {}", status, body);
                    // On other errors, just enforce min interval
                    let mut rl = self.rate.lock().await;
                    rl.next_allowed = Instant::now() + std::time::Duration::from_secs(2);
                }
            }
            Err(e) => {
                warn!("TG alert network error: {}", e);
            }
        }

        // After successful send, enforce min interval
        let mut rl = self.rate.lock().await;
        rl.next_allowed = Instant::now() + std::time::Duration::from_secs(rl.min_interval_secs);

        Ok(())
    }

    /// Parse retry_after from Telegram 429 response
    fn parse_retry_after(body: &str) -> Option<u64> {
        // {"ok":false,"error_code":429,"description":"Too Many Requests: retry after 38","parameters":{"retry_after":38}}
        if let Some(idx) = body.find("\"retry_after\":") {
            let rest = &body[idx + "\"retry_after\":".len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return num.parse().ok();
        }
        None
    }

    /// Alert: position opened (paper trade)
    pub async fn alert_open(
        &self,
        symbol_a: &str,
        symbol_b: &str,
        side_a: &str,
        side_b: &str,
        zscore: f64,
        hedge_ratio: f64,
        price_a: f64,
        price_b: f64,
        size: f64,
    ) -> Result<()> {
        let dir_emoji = if zscore > 0.0 { "🔴" } else { "🟢" };

        let msg = format!(
            "{} <b>OPEN #{}</b>\n\
             \n\
             {} ↔ {}\n\
             {} {} @ {} | {} {} @ {}\n\
             \n\
             Z-score: <code>{:+.3}</code>\n\
             Hedge β: <code>{:.4}</code>\n\
             Size: <code>${:.2}</code>",
            dir_emoji, "TRADE",
            symbol_a, symbol_b,
            side_a, symbol_a, format_price(price_a),
            side_b, symbol_b, format_price(price_b),
            zscore, hedge_ratio, size
        );

        self.send(&msg).await
    }

    /// Alert: position closed with PnL
    pub async fn alert_close(
        &self,
        symbol_a: &str,
        symbol_b: &str,
        entry_zscore: f64,
        exit_zscore: f64,
        pnl: f64,
        balance: f64,
    ) -> Result<()> {
        let pnl_emoji = if pnl >= 0.0 { "💰" } else { "📉" };
        let pnl_sign = if pnl >= 0.0 { "+" } else { "" };

        let msg = format!(
            "🔚 <b>CLOSE</b>\n\
             \n\
             {} ↔ {}\n\
             \n\
             Entry z: <code>{:+.3}</code> → Exit z: <code>{:+.3}</code>\n\
             {} PnL: <code>{}${:.4}</code>\n\
             Balance: <code>${:.2}</code>",
            symbol_a, symbol_b,
            entry_zscore, exit_zscore,
            pnl_emoji, pnl_sign, pnl.abs(), balance
        );

        self.send(&msg).await
    }

    /// Alert: stop loss hit
    pub async fn alert_stop_loss(
        &self,
        symbol_a: &str,
        symbol_b: &str,
        entry_zscore: f64,
        current_zscore: f64,
        pnl: f64,
        balance: f64,
    ) -> Result<()> {
        let msg = format!(
            "🛑 <b>STOP LOSS</b>\n\
             \n\
             {} ↔ {}\n\
             \n\
             Entry z: <code>{:+.3}</code> → Curr z: <code>{:+.3}</code>\n\
             PnL: <code>${:.4}</code>\n\
             Balance: <code>${:.2}</code>",
            symbol_a, symbol_b,
            entry_zscore, current_zscore, pnl, balance
        );

        self.send(&msg).await
    }

    /// Send scan results summary (one message, no spam)
    pub async fn alert_scan_summary(
        &self,
        total_pairs: usize,
        correlated: usize,
        cointegrated: usize,
        signals: usize,
    ) -> Result<()> {
        let msg = format!(
            "📊 <b>SCAN</b>\n\
             \n\
             Symbols: {} | Correlated: {} | Cointegrated: {} | Signals: {}",
            total_pairs, correlated, cointegrated, signals
        );

        self.send(&msg).await
    }
}

fn format_price(p: f64) -> String {
    if p >= 1000.0 {
        format!("{:.2}", p)
    } else if p >= 1.0 {
        format!("{:.4}", p)
    } else {
        format!("{:.8}", p)
    }
}
