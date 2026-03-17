# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [0.3.1] - 2026-03-17

### Fixed

- **[Critical] `parse_datetime` silently dropped all Gamma API markets, causing "All markets
  matching 'BTC' have < 30s remaining"** (`src/market_discovery/mod.rs`)

  The root cause: Polymarket Gamma API returns dates in ISO 8601 *without* a timezone suffix
  (e.g. `"2026-04-01T00:00:00"` instead of `"2026-04-01T00:00:00Z"`). The old parser only
  handled RFC 3339 and Unix integer strings. Non-RFC-3339 strings hit `bail!`, `filter_map`
  returned `None` for every valid future market, and only markets with unix-integer `endDate`
  remained — all of which happened to be expired.

  `parse_datetime` now handles 5 formats in order:
  1. RFC 3339 with timezone (`"…Z"` / `"…+00:00"`)
  2. ISO 8601 without timezone (`"2026-04-01T12:00:00"`) — **the previously broken case**
  3. ISO 8601 with subseconds (`"2026-04-01T12:00:00.000"`)
  4. Date-only (`"2026-04-01"`) — assume UTC midnight
  5. Unix seconds (`"1742947200"`) and milliseconds (`"1743465600000"`, auto-detected by > 1e12)

- **`fetch_from_gamma_by_keyword` improvements:**
  - Changed `?active=true` → `?closed=false` so markets that Polymarket has marked active
    but are in a transient state (e.g. just-created, paused) are also considered.
  - Increased result limit from `20` → `50` to reduce the chance that near-expiry markets
    crowd out long-running future markets.
  - Added handling for wrapped Gamma response `{"data": [...]}` / `{"results": [...]}` in
    addition to the raw array format — prevents a silent `Err("Not an array")` crash if
    Polymarket changes the response envelope.
  - Now filters on `closed`, `resolved`, and `archived` fields before date parsing.
  - Debug log now shows breakdown: `N returned, X closed, Y bad-date, Z <Xs, W eligible`.
  - Error message now includes total market count and the configured minimum threshold so
    operators know exactly what to change.

- **`wait_for_market` hardcoded `> 30s` threshold** replaced by
  `config.strategy.min_market_secs_remaining` (default `120`).

### Added

- **`[strategy] min_market_secs_remaining`** (`config.toml`, default `120`) — only enter
  a market if it has at least this many seconds remaining. Prevents the bot from entering
  markets that would immediately trigger pre-settlement cancel. Set higher (e.g. `3600`)
  for hourly+ duration markets.

- **8 new `parse_datetime` unit tests** covering all supported formats and error cases
  (35 tests total, all passing).

---

## [0.3.0] - 2026-03-17

### Fixed

- **[Critical] Bot loops forever — `btc-updown-5m` / `btc-updown-15m` markets no longer exist on Polymarket**
  (`src/market_discovery/mod.rs`, `src/strategy/market_making.rs`, `src/config/mod.rs`)
  — All 7 bugs documented below prevented the bot from ever trading once Polymarket retired
  the fixed-window BTC markets.

- **Bug 1 (Critical)** — `config/mod.rs:validate()` hard-rejected any `market_type` other
  than `"5m"` or `"15m"`, so the bot would not even start if an operator changed the hint.
  Removed the restriction; the validator now accepts any string.

- **Bug 2 (Critical)** — `find_active_market` only generated the defunct
  `{asset}-updown-{5m|15m}-{timestamp}` slug, failed to find it on both APIs, and
  retried every 5 s forever without ever trading. Replaced with a **three-tier discovery**:
  1. **Tier 1**: if `[strategy] market_slug` is set in config, look that up directly.
  2. **Tier 2**: for `market_type = "5m"` or `"15m"`, try the computed window slug
     (preserves compatibility if Polymarket ever re-launches those markets).
  3. **Tier 3**: keyword search via Gamma API `?q=<keyword_search>&active=true&limit=20`;
     picks the active market with the most time remaining.

- **Bug 3 (Logic)** — `parse_clob_market` fell through to `MarketType::FifteenMinute`
  for any slug that didn't contain `"-5m-"`. Generic markets (e.g. `will-btc-hit-100k`)
  were misclassified, corrupting the time-decay spread calculation.
  Fixed by extracting `market_type_from_slug()` which defaults to `MarketType::Generic`.

- **Bug 4 (Logic)** — `parse_clob_market` used `game_start_time` as a fallback for
  `end_date_iso`. This set the market's `end_time` to its *start* time, causing
  `seconds_remaining()` to return ≤ 0 and immediately triggering pre-settlement cancel
  (cancelling all orders at market open). Fixed by removing the wrong fallback; the
  parser now bails with a clear error if `end_date_iso` is missing.

- **Bug 5 (Logic)** — `calculate_quotes` read `market.market_type.duration_secs()` for
  the time-decay spread window. For `MarketType::Generic` this returned 0, giving a
  `time_factor` of 0 (maximum tightening) for the entire market life.
  Fixed by using `market.actual_duration_secs()` which derives duration from
  `end_time – start_time` for generic markets.

- **Bug 6 (Logic)** — `fetch_from_gamma` passed the computed (defunct) slug as
  `?slug=` query param and did an exact match against current Gamma slugs which always
  returned empty. Replaced with `fetch_from_gamma_by_exact_slug` that also verifies
  the returned slug matches exactly (Gamma does partial matching).

- **Bug 7 (UX)** — No operator escape hatch: if all discovery failed, the bot printed
  a `debug`-level message and silently looped. Fixed: discovery errors are now logged at
  `warn` with a clear message telling the operator to set `market_slug` in config.

### Added

- **`MarketType::Generic`** variant in `types.rs` for markets that don't follow the
  fixed 5m/15m window format.

- **`Market::actual_duration_secs()`** — derives total window length from
  `end_time – start_time` for `Generic` markets; returns the constant for known types.

- **`[strategy] market_slug`** (optional, `config.toml`) — pin the bot to a specific
  Polymarket market by its exact URL slug. Highest-priority discovery path.

- **`[strategy] keyword_search`** (default `"BTC"`, `config.toml`) — Gamma API full-text
  search term used when slug-based discovery fails. The matching market with the most
  time remaining is selected.

- **`market_type_from_slug()`** helper that detects `FiveMinute` / `FifteenMinute` /
  `Generic` from a slug string.

- **`fetch_from_gamma_by_exact_slug()`** and **`fetch_from_gamma_by_keyword()`** — two
  new private methods on `MarketDiscovery` replacing the old single `fetch_from_gamma`.

- **3 new regression tests** in `market_discovery::tests`:
  - `test_market_type_from_slug` — verifies slug → MarketType mapping for all variants.
  - `test_parse_clob_end_date_iso_not_start_time` — regression for Bug 4.
  - `test_parse_clob_generic_market_type` — regression for Bug 3.

### Changed

- `config.toml`: `market_type` default changed from `"5m"` to `"generic"`;
  new fields `keyword_search = "BTC"` and commented-out `market_slug` example added.
- `strategy/market_making.rs`: `wait_for_market` maps unknown `market_type` strings to
  `MarketType::Generic` (previously fell through to `FiveMinute`).
- Discovery failure in `wait_for_market` now logs at `warn` instead of `debug` so
  operators see it in default `info`-level logging.
- `parse_gamma_market` now reads `startDate` / `start_date_iso` for `start_time`
  (previously always defaulted to `Utc::now()`).

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

[Unreleased]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mkrfsbri/PolyMMRF/releases/tag/v0.1.0
