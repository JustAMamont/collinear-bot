# cointegration-bot (experimental)

Cointegration scanner & paper trader for crypto futures (Bybit).

## Quick start

```bash
cargo build --release

# 1. Generate default config
./target/release/lead-lag init-config

# 2. Edit config.json — set Telegram credentials, adjust parameters
vim config.json

# 3. Scan for cointegrated pairs
./target/release/lead-lag scan

# 4. Monitor + paper trade
./target/release/lead-lag monitor --trade

# 5. CLI overrides (override any config field)
./target/release/lead-lag scan --min-correlation 0.6 --interval 1 --top-n 50
```

## Config

All settings live in `config.json` (created by `init-config`). CLI flags override config values.

```json
{
  "exchange": {
    "name": "bybit",
    "base_url": "",
    "rate_limit": 5
  },
  "scanner": {
    "min_correlation": 0.7,
    "min_candles": 50,
    "interval": "5",
    "kline_limit": 400,
    "top_n": 80
  },
  "trading": {
    "balance": 100.0,
    "zscore_entry": 2.0,
    "zscore_exit": 0.5,
    "stop_loss_zscore": 4.0,
    "max_position_pct": 0.25,
    "max_open_positions": 4,
    "min_half_life": 200.0,
    "commission_pct": 0.055,
    "slippage_pct": 0.02
  },
  "telegram": {
    "bot_token": "",
    "chat_id": ""
  },
  "monitor": {
    "refresh_sec": 60,
    "trade": false
  }
}
```

### Config fields

| Section | Field | Default | Description |
|---------|-------|---------|-------------|
| exchange | name | "bybit" | Exchange to use (only bybit for now) |
| exchange | base_url | "" | API base URL override (empty = default) |
| exchange | rate_limit | 5 | Max concurrent API requests |
| scanner | min_correlation | 0.7 | Minimum Pearson ρ for pre-filter |
| scanner | min_candles | 50 | Min candles for statistical tests |
| scanner | interval | "5" | Kline interval (1, 3, 5, 15, 30, 60, 240, D) |
| scanner | kline_limit | 400 | Candles per symbol (max 500) |
| scanner | top_n | 80 | Top symbols by 24h volume to scan |
| trading | balance | 100.0 | Paper trading balance (USDT) |
| trading | zscore_entry | 2.0 | Z-score to open position |
| trading | zscore_exit | 0.5 | Z-score to close position |
| trading | stop_loss_zscore | 4.0 | Emergency stop-loss threshold |
| trading | max_position_pct | 0.25 | Max % of balance per position |
| trading | max_open_positions | 4 | Max simultaneous open positions |
| trading | min_half_life | 200.0 | Skip pairs with longer half-life |
| trading | commission_pct | 0.055 | Taker fee per leg (Bybit: 0.055%) |
| trading | slippage_pct | 0.02 | Estimated slippage per leg |
| telegram | bot_token | "" | Bot token from @BotFather |
| telegram | chat_id | "" | Chat ID for alerts |
| monitor | refresh_sec | 60 | Monitor refresh interval (seconds) |
| monitor | trade | false | Enable paper trading in monitor |

## Commands

| Command | Description |
|---------|-------------|
| `scan` | Scan top futures for cointegrated pairs |
| `monitor` | Watch z-scores in real-time |
| `monitor --trade` | Monitor + paper trading (overrides config) |
| `status` | Show tracked pairs & trading state |
| `reset` | Clear all state |
| `test-tg` | Send test Telegram alert |
| `init-config` | Generate default config.json |

## Algorithm

**Phase 1 — Correlation pre-filter (cheap):**
Pearson ρ on log-returns for all N×(N-1)/2 pairs. Filter |ρ| < threshold → removes ~95% before expensive test.

**Phase 2 — Engle-Granger cointegration (only on correlated pairs):**
1. OLS: `price_B = α + β × price_A` → hedge ratio β
2. ADF test on spread: `Δspread = γ × spread_{t-1} + ε`
3. ADF stat < -2.86 → cointegrated (5% significance)
4. Half-life = `-ln(2) / γ` → mean-reversion speed

**Signals:**
- `|z| > 2.0` → entry (short overvalued, long undervalued)
- `|z| < 0.5` → exit (spread reverted)
- `|z| > 4.0` → stop-loss

## Telegram alerts

Deduplicated — won't spam the same signal twice. Alerts for:
- ⚡ Entry signals (with direction, z-score, hedge ratio, half-life)
- 🔚 Exit signals (with PnL)
- 🛑 Stop-loss (z-score exploded)
- 📊 Scan summary

Setup:
1. Create bot via @BotFather → get token
2. Get your chat ID via @userinfobot
3. Set `bot_token` and `chat_id` in config.json (or pass `--tg-bot-token` / `--tg-chat-id`)

## Paper trading

- Commission: 0.055% per leg (Bybit taker fee)
- Slippage: 0.02% per leg (estimated)
- Total round-trip cost: ~0.15%
- Position sizing: 25% of balance per trade
- Max 4 concurrent positions
- State persisted to `~/.local/share/lead-lag/state.json`

## CLI overrides

All config fields can be overridden via CLI flags:

```bash
# Override scanner settings
./target/release/lead-lag scan --min-correlation 0.6 --interval 1 --top-n 50

# Override trading settings
./target/release/lead-lag monitor --zscore-entry 2.5 --balance 500

# Override Telegram (one-time)
./target/release/lead-lag monitor --tg-bot-token "123:ABC" --tg-chat-id "999"

# Custom config path
./target/release/lead-lag --config /path/to/config.json scan
```

## Project structure

```
src/
├── main.rs              # CLI: scan/monitor/status/reset/test-tg/init-config
├── config.rs            # AppConfig: JSON config with serde, defaults, overrides
├── exchange/
│   ├── traits.rs        # Exchange trait — implement for new exchanges
│   └── bybit.rs         # Bybit USDT linear futures
├── stats/
│   ├── math.rs          # mean, std_dev, pearson, OLS, zscore
│   └── cointegration.rs # Engle-Granger, ADF, half-life
├── scanner/
│   └── finder.rs        # Two-phase scanner
├── trading/
│   ├── models.rs        # TrackedPair, Position, TradingState, Signal
│   └── engine.rs        # Paper trading + commission + slippage
└── notify/
    ├── telegram.rs      # Telegram Bot API sender
    └── dedup.rs         # Signal deduplication (no spam)
```

## Adding a new exchange

```rust
#[async_trait]
impl Exchange for MyExchange {
    fn name(&self) -> &str { "myexchange" }
    async fn get_futures_symbols(&self) -> Result<Vec<SymbolInfo>> { ... }
    async fn get_klines(&self, symbol: &str, interval: &str, limit: u32) -> Result<Vec<Kline>> { ... }
    async fn fetch_all_klines(&self, ...) -> Result<HashMap<String, Vec<f64>>> { ... }
    async fn get_mark_price(&self, symbol: &str) -> Result<f64> { ... }
}
```

## Limitations (for production)

- ADF without lag terms (simplified)
- No Kalman filter for dynamic hedge ratio
- No WebSocket (polling only)
- No real execution (paper only)
- No cross-margin / portfolio risk
