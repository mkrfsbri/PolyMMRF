# Polymarket Market Making Bot

A high-performance market making bot for [Polymarket](https://polymarket.com) BTC Up/Down binary prediction markets, written in Rust. Targets 5-minute and 15-minute window markets with a **maker-side rebate farming** strategy — placing post-only limit orders at both sides to collect the 25% maker rebate distributed daily.

```
╔═══════════════════════════════════════════════════════╗
║       Polymarket Market Making Bot  v0.2.0            ║
║       Strategy: Maker Rebate Farming (BTC 5m/15m)     ║
╚═══════════════════════════════════════════════════════╝
```

---

## Table of Contents

- [Strategy Overview](#strategy-overview)
- [Architecture](#architecture)
- [Prerequisites](#prerequisites)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [BTC Price Feed & Coinbase Fallback](#btc-price-feed--coinbase-fallback)
- [Environment Variables](#environment-variables)
- [Running](#running)
- [Docker](#docker)
- [Key Metrics](#key-metrics)
- [Risk Controls](#risk-controls)
- [Testing](#testing)
- [Project Structure](#project-structure)
- [Production Hardening](#production-hardening)
- [Changelog](#changelog)
- [Disclaimer](#disclaimer)

---

## Strategy Overview

Polymarket charges takers up to **3.15% fees** (`rate * min(price, 1-price) * size`) and redistributes **25% of collected taker fees back to makers daily** in USDC. This bot exploits that rebate by continuously posting limit orders on both sides of BTC Up/Down markets.

**Core mechanics:**

1. **Market window targeting** — auto-discovers the active 5m or 15m BTC Up/Down market using deterministic slug calculation (`btc-updown-5m-{window_ts}`)
2. **Symmetric quoting** — bids `target_bid_price ± half_spread` on both UP and DOWN tokens simultaneously
3. **Time-decay spread** — spread widens early in the window (more uncertainty) and tightens near close (time_factor = remaining/total_duration)
4. **Inventory skew** — adjusts bid prices when inventory becomes imbalanced to reduce directional risk
5. **Pre-settlement cancel** — cancels all orders 10 seconds before window closes to avoid adverse fills at settlement
6. **Auto-cycle** — immediately discovers and quotes the next window after settlement

**Fee math example** (10 shares @ $0.45, 3.15% rate):
```
Taker fee  = 0.0315 × min(0.45, 0.55) × 10 = $0.14175
Maker rebate = 0.14175 × 0.25             = ~$0.0354 per fill
```

---

## Architecture

```
main.rs
├── Binance WS Feed  ─────────────────────────────────► BotState (AtomicI64 BTC price)
├── Polymarket WS Feed ────────────────────────────────► BotState (DashMap orderbooks)
├── Monitoring Loop (30s) ─────────────────────────────► tracing logs
└── MarketMakingStrategy (main loop)
    ├── MarketDiscovery  ──── CLOB API / Gamma API ────► Market metadata
    ├── ExecutionEngine  ──── CLOB REST (or sim) ──────► Order placement/cancellation
    ├── RiskEngine       ──── Half-Kelly + guards ──────► SizingResult
    └── SimFillEngine    ──── Orderbook matching ───────► Simulated fills
```

**Concurrency model:** Tokio async runtime. Price updates and orderbook events flow over a `broadcast::channel<DataEvent>`. All shared state uses lock-free atomics (`AtomicI64`, `AtomicBool`, `AtomicU64`) with `DashMap` for order and orderbook tracking — no hot-path mutex contention.

---

## Prerequisites

- **Rust 1.75+** (uses async traits, `let-else`)
- A Polymarket account on **Polygon** with:
  - USDC balance in your CLOB wallet
  - API credentials (key, secret, passphrase) — derive via L1 auth flow
- Internet access to Binance and Polymarket APIs

Install Rust: https://rustup.rs

---

## Quick Start

```bash
# 1. Clone
git clone https://github.com/mkrfsbri/PolyMMRF
cd PolyMMRF

# 2. Copy and fill in secrets
cp .env.example .env
$EDITOR .env

# 3. Review config (simulation mode is ON by default)
$EDITOR config.toml

# 4. Build
./run.sh build

# 5. Run in simulation (no real orders)
./run.sh sim
```

---

## Configuration

All settings live in `config.toml`. Secrets are loaded from environment variables (see below) and override any values in the config file.

### `[polymarket]`

| Key | Default | Description |
|-----|---------|-------------|
| `clob_api_url` | `https://clob.polymarket.com` | CLOB REST API base URL |
| `ws_url` | `wss://ws-subscriptions-clob.polymarket.com/ws/market` | Orderbook WebSocket |
| `gamma_api_url` | `https://gamma-api.polymarket.com` | Market metadata API |
| `signature_type` | `0` | `0`=EOA, `1`=Proxy, `2`=GnosisSafe |
| `chain_id` | `137` | Polygon Mainnet |

### `[strategy]`

| Key | Default | Description |
|-----|---------|-------------|
| `target_bid_price` | `0.45` | Base bid price in USDC (45 cents) |
| `half_spread` | `0.03` | Half-spread width (3 cents each side) |
| `min_spread` | `0.01` | Minimum half-spread floor |
| `max_spread` | `0.10` | Maximum half-spread cap |
| `order_size` | `10.0` | Shares per order |
| `quote_refresh_ms` | `5000` | Quote refresh interval (ms) |
| `market_type` | `"5m"` | Window size: `"5m"` or `"15m"` |
| `assets` | `["BTC"]` | Assets to trade |
| `post_only` | `true` | Post-only flag (required for maker rebate) |

### `[risk]`

| Key | Default | Description |
|-----|---------|-------------|
| `bankroll` | `1000.0` | Total USDC bankroll |
| `max_exposure_pct` | `0.10` | Max 10% of bankroll per market |
| `daily_loss_limit_pct` | `0.05` | Pause if daily loss > 5% of bankroll |
| `max_inventory_ratio` | `0.75` | Refuse orders if ratio > 75:25 |
| `circuit_breaker_losses` | `5` | Pause after 5 consecutive losses |
| `pre_settlement_cancel_secs` | `10` | Cancel orders 10s before settlement |

### `[bot]`

| Key | Default | Description |
|-----|---------|-------------|
| `simulation` | `true` | **Must set to `false` for live trading** |
| `log_level` | `"info"` | `trace`/`debug`/`info`/`warn`/`error` |

---

## BTC Price Feed & Coinbase Fallback

The bot maintains an uninterrupted BTC spot price using a two-tier feed:

```
Primary  ──►  Binance WebSocket  wss://stream.binance.com:9443/ws/btcusdt@ticker
                    │  (geo-blocked or repeated errors?)
                    ▼  after max_binance_failures consecutive failures
Fallback ──►  Coinbase REST  GET https://api.coinbase.com/v2/prices/BTC-USD/spot
                    │  (polls every poll_interval_ms)
                    ▼  after retry_binance_secs
                    └─► silently retry Binance → switch back if reconnected
```

No Coinbase API key is required. The fallback is designed for geo-blocked
regions (e.g. some jurisdictions that block Binance but not Coinbase) and for
general Binance outages.

### `[coinbase]` config keys

| Key | Default | Description |
|-----|---------|-------------|
| `rest_url` | `https://api.coinbase.com/v2/prices/BTC-USD/spot` | Coinbase spot endpoint |
| `poll_interval_ms` | `5000` | How often to poll while in fallback mode |
| `max_binance_failures` | `3` | Failures before switching to Coinbase |
| `retry_binance_secs` | `60` | Seconds in fallback before retrying Binance |
| `enabled` | `true` | Set to `false` to disable the fallback entirely |

**Tuning for geo-blocked environments:** set `max_binance_failures = 1` and
`retry_binance_secs = 300` so the bot stops wasting time on refused connections
and only re-checks Binance every 5 minutes.

**Log indicators:**
```
WARN  Binance WS failure #3/3: connection refused
WARN  Binance WS unavailable after 3 failures — activating Coinbase REST fallback
INFO  BTC/Coinbase: $65432.10
...
INFO  Retrying Binance WebSocket after fallback period...
INFO  Binance WebSocket connected (source: primary)
```

---

## Environment Variables

Copy `.env.example` to `.env` and fill in your values. These override the config file.

```bash
POLY_PRIVATE_KEY=<hex private key, no 0x prefix>
POLY_API_KEY=<CLOB API key>
POLY_API_SECRET=<CLOB API secret, base64-encoded>
POLY_API_PASSPHRASE=<CLOB API passphrase>
POLY_FUNDER_ADDRESS=<0x wallet address>
RUST_LOG=info
```

> **Security:** Never commit `.env` to version control. `.gitignore` excludes it by default.

---

## Running

### Using `run.sh`

```bash
./run.sh build        # Compile release binary
./run.sh sim          # Run in simulation mode
./run.sh run          # Run with config.toml settings
./run.sh test         # Run all tests
./run.sh test-unit    # Run unit tests only
./run.sh check        # Fast compile check (no binary)
./run.sh clean        # Remove build artifacts
```

### Direct binary

```bash
# Default config
./target/release/mm-bot

# Custom config path
./target/release/mm-bot /path/to/my-config.toml
```

### Log levels

```bash
RUST_LOG=debug ./target/release/mm-bot   # Verbose
RUST_LOG=trace ./target/release/mm-bot   # Very verbose (includes WS frames)
```

---

## Docker

```bash
# Build image
./run.sh docker-build

# Run (requires .env file)
./run.sh docker-run

# Manual
docker run --rm -it \
  --env-file .env \
  -v $(pwd)/config.toml:/app/config.toml:ro \
  -v $(pwd)/logs:/app/logs \
  polymarket-mm-bot:latest
```

The container runs as a non-root user (`mmbot`, uid 1001).

---

## Key Metrics

The bot logs a status line every 30 seconds:

```
[STATUS] market=btc-updown-5m-1710000000 btc=$65432 delta=+0.12%
         pnl=$1.23 inv_up=10.0 inv_down=8.0 ratio=0.56
         orders=2 losses=0 paused=false
```

Target performance benchmarks:

| Metric | Target | Critical |
|--------|--------|----------|
| Fill rate | 30–50% | < 10% |
| Spread capture / fill | $0.02–0.06 | < $0.01 |
| Inventory imbalance | < 60:40 | > 75:25 |
| Daily maker rebate | $5–50+ | $0 |
| Quote-to-order latency | < 50ms | > 500ms |
| Win rate (settled) | 75%+ | < 50% |
| Daily PnL | Positive | > -5% bankroll |

Trade logs are written to `./logs/orders_YYYY-MM-DD.csv` and `./logs/settlements_YYYY-MM-DD.csv`.

---

## Risk Controls

Four independent layers protect capital:

1. **Daily loss limit** — bot pauses if cumulative daily PnL falls below `-bankroll × daily_loss_limit_pct`
2. **Circuit breaker** — pauses after `circuit_breaker_losses` consecutive losses; resumes at UTC midnight
3. **Inventory guard** — refuses new orders on an outcome when its share of total inventory exceeds `max_inventory_ratio`
4. **Pre-settlement cancel** — cancels all open orders `pre_settlement_cancel_secs` seconds before window close; prevents filling into an already-known settlement price

Position sizing uses **Half-Kelly criterion**: `size = bankroll × (edge / odds) / 2`, clamped to `[5 shares, max_exposure]`.

---

## Testing

```bash
cargo test
```

**21 unit tests** covering:

| Module | Tests |
|--------|-------|
| `execution::signing` | `normalize_price`, `calculate_taker_fee`, `calculate_amounts`, HMAC signature |
| `risk` | Half-Kelly sizing, minimum size, circuit breaker, daily loss limit, inventory skew, emergency cancel |
| `market_discovery` | Slug calculation (5m/15m), mid-window slug, next slug, time remaining |
| `strategy::market_making` | Quote calculation, tick rounding, update detection |

---

## Project Structure

```
PolyMMRF/
├── src/
│   ├── main.rs                    # Entry point, task spawning
│   ├── types.rs                   # BotState, Market, OrderBook, DataEvent, ...
│   ├── config/
│   │   └── mod.rs                 # BotConfig TOML loading + validation
│   ├── data/
│   │   └── mod.rs                 # Binance WS, Polymarket WS, REST fallback
│   ├── execution/
│   │   ├── mod.rs                 # ExecutionEngine: place/cancel orders
│   │   └── signing.rs             # HMAC auth, fee math, EIP-712 (placeholder)
│   ├── market_discovery/
│   │   └── mod.rs                 # Slug calculation, CLOB/Gamma API lookup
│   ├── risk/
│   │   └── mod.rs                 # Half-Kelly sizing, circuit breakers, skew
│   ├── strategy/
│   │   ├── mod.rs
│   │   ├── market_making.rs       # Core MM loop, quote calc, refresh
│   │   └── sim_fills.rs           # Simulation fill engine
│   └── monitoring/
│       ├── mod.rs                 # 30s status logger
│       └── trade_logger.rs        # CSV order + settlement logs
├── config.toml                    # All parameters documented
├── .env.example                   # Secret env vars template
├── Dockerfile                     # Multi-stage build, non-root runtime
├── run.sh                         # Helper: build/run/sim/test/docker
└── Cargo.toml
```

---

## Production Hardening

Items not yet implemented (Phase 11 roadmap):

- **Full EIP-712 order signing** — integrate [`clob-client-rust`](https://github.com/Polymarket/clob-client-rust) or `polymarket_client_sdk` for proper typed-data signing and API credential derivation from private key
- **Real-time fill notifications** — subscribe to Polymarket `user` WS channel
- **Volatility-adjusted spread** — ATR from Binance klines, widen during high vol
- **Partial fill tracking** — via WS user channel
- **SQLite persistence** — trade log, state recovery across restarts
- **Prometheus metrics** — `metrics_port` in config is reserved
- **Multi-asset trading** — BTC + ETH + SOL + XRP simultaneously
- **Telegram/Discord alerts** — critical event notifications
- **Retry logic** — exponential backoff for transient API failures

---

## Key Constants

```
CLOB API:        https://clob.polymarket.com
Polymarket WS:   wss://ws-subscriptions-clob.polymarket.com/ws/market
Gamma API:       https://gamma-api.polymarket.com
Binance WS:      wss://stream.binance.com:9443/ws/btcusdt@ticker
Polygon Chain:   137

5m slug format:  btc-updown-5m-{unix_ts}   (ts divisible by 300)
15m slug format: btc-updown-15m-{unix_ts}  (ts divisible by 900)

Taker fee:       up to 3.15% at 50/50 odds
Maker fee:       0%
Maker rebate:    25% of taker fees, paid daily in USDC
Tick size:       0.01
Min order:       5 shares
```

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a full history of changes by version.

---

## Disclaimer

This software is provided for educational and research purposes. Automated trading involves substantial financial risk. Always run in simulation mode first, understand the risks, and never trade with funds you cannot afford to lose. This is not financial advice.
