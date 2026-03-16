# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [0.2.0] - 2026-03-16

### Added

- **Coinbase REST API fallback for BTC price feed** (`src/data/mod.rs`)
  - New top-level function `run_btc_price_feed` replaces `run_binance_feed` as
    the entry point for the BTC price task.  It orchestrates primary Binance WS
    and Coinbase REST seamlessly.
  - `fetch_coinbase_price` — polls `https://api.coinbase.com/v2/prices/BTC-USD/spot`
    (no API key required) and returns a `BtcPrice` with `source: PriceSource::Coinbase`.
  - `run_coinbase_fallback` (internal) — polls Coinbase at `poll_interval_ms`
    for `retry_binance_secs` then returns so Binance can be retried.
  - Automatic switchover: after `max_binance_failures` consecutive Binance WS
    connection errors the bot switches to Coinbase polling with a warning log.
    After `retry_binance_secs` it transparently retries Binance and switches
    back if the connection succeeds.
  - All active-source transitions are logged at `warn` / `info` level so
    operators can detect geo-blocking without digging through traces.

- **`CoinbaseConfig`** (`src/config/mod.rs`)
  - New `[coinbase]` section in `BotConfig` with five tunable fields:

    | Field | Default | Purpose |
    |-------|---------|---------|
    | `rest_url` | `https://api.coinbase.com/v2/prices/BTC-USD/spot` | Endpoint |
    | `poll_interval_ms` | `5000` | Coinbase poll cadence (ms) |
    | `max_binance_failures` | `3` | Failures before switching |
    | `retry_binance_secs` | `60` | Seconds in fallback before retry |
    | `enabled` | `true` | Master toggle for the fallback |

- **`PriceSource::Coinbase`** variant added to `types.rs` enum.

- **`[coinbase]` section in `config.toml`** with all fields documented.

- **3 new unit tests** in `src/data/mod.rs`:
  - `test_parse_coinbase_response` — validates JSON parsing of Coinbase response shape.
  - `test_parse_binance_ticker` — validates field extraction and timestamp handling.
  - `test_parse_binance_ticker_missing_price` — confirms `None` on malformed frames.

### Changed

- `main.rs`: replaced `run_binance_feed` spawn with `run_btc_price_feed`, which
  passes the new `CoinbaseConfig`; startup log now shows fallback enabled/disabled
  status and the failure threshold.

### Fixed

- `data/mod.rs`: Binance error frames (`!clean_exit`) now correctly increment the
  failure counter, whereas clean server-side closes (e.g. scheduled reconnects) do
  not penalise the counter — avoiding spurious fallback switches on routine
  Binance maintenance.

---

## [0.1.0] - 2026-03-16

### Added

- Initial implementation of the Polymarket market making bot (Phases 1–10).

- **`src/types.rs`** — `BotState` with lock-free atomics (`AtomicI64`,
  `AtomicBool`, `AtomicU64`), `Market`, `OrderBook`, `ActiveOrder`, `BtcPrice`,
  `DataEvent`, `Side`, `Outcome`, `MarketType`, `PriceSource`.

- **`src/config/mod.rs`** — `BotConfig` TOML loader with env-var overrides for
  secrets (`POLY_PRIVATE_KEY`, `POLY_FUNDER_ADDRESS`) and config validation.

- **`src/data/mod.rs`** — Binance WebSocket BTC/USDT ticker feed with
  auto-reconnect (3 s backoff); Polymarket orderbook WS feed; Polymarket CLOB
  REST orderbook fallback.

- **`src/market_discovery/mod.rs`** — deterministic slug calculation
  (`btc-updown-{5m|15m}-{window_ts}`), CLOB API primary lookup, Gamma API
  fallback, fee-rate endpoint.

- **`src/execution/signing.rs`** — HMAC-SHA256 L2 auth, `normalize_price`,
  `calculate_amounts`, `calculate_taker_fee`, `estimate_maker_rebate`, EIP-712
  order hash placeholder.

- **`src/execution/mod.rs`** — `ExecutionEngine` with `place_order`,
  `cancel_order`, `cancel_all_orders`, `cancel_and_replace`; transparent
  simulation mode (no HTTP, `sim-{uuid}` order IDs).

- **`src/risk/mod.rs`** — Half-Kelly position sizing, four-layer risk controls
  (daily loss limit, circuit breaker, inventory guard, pre-settlement cancel),
  `inventory_skew_adjustment`, `RiskMetrics` display.

- **`src/strategy/market_making.rs`** — `MarketMakingStrategy` main loop:
  market discovery → initial quote placement → time-decay adaptive spread refresh
  → pre-settlement cancel → settlement handler → market cycle.

- **`src/strategy/sim_fills.rs`** — `SimFillEngine`: orderbook-based simulated
  fill matching and settlement PnL estimation.

- **`src/monitoring/mod.rs`** — 30-second status logger.

- **`src/monitoring/trade_logger.rs`** — `TradeLogger` writing daily CSV files
  for orders and settlements.

- **`src/main.rs`** — Tokio async runtime bootstrap, task spawning (price feed,
  monitoring, Ctrl+C handler), strategy run loop with emergency order cancel on
  error.

- **`config.toml`** — fully documented configuration template.

- **`.env.example`** — environment variable template for secrets.

- **`Dockerfile`** — multi-stage build (rust:1.82-slim → debian:bookworm-slim),
  non-root `mmbot` user.

- **`run.sh`** — helper script: `build`, `run`, `sim`, `test`, `test-unit`,
  `check`, `docker-build`, `docker-run`, `clean`.

- **21 unit tests** covering signing math, risk sizing, market discovery slug
  logic, and strategy quote calculation.

[Unreleased]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mkrfsbri/PolyMMRF/releases/tag/v0.1.0
