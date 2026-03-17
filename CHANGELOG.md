# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [0.4.6] - 2026-03-17

### Fixed

- **Market parser fails on real btc-updown Gamma response format**
  (`src/market_discovery/mod.rs`)

  Gamma `/markets` returns token data as **JSON-encoded strings**, not arrays:
  ```json
  "clobTokenIds": "[\"21184...\", \"45121...\"]",
  "outcomes":     "[\"Up\", \"Down\"]"
  ```
  No `tokens` array is present. Both parsers required `tokens` and silently
  skipped every btc-updown market with `Err("Missing tokens")`.

  **Fix:** Added `extract_token_ids_from_stringified()`. Both `parse_gamma_market`
  and `parse_clob_market` now try `tokens` array first, then fall back to the
  stringified format. 3 new unit tests added.

- **Hardcoded 315 bps fee rate wrong for btc-updown markets**

  btc-updown markets use `takerBaseFee` = **1000 bps** (10%). Quoting at 315
  bps makes spreads too tight relative to actual cost.

  **Fix:** Both parsers read `takerBaseFee`/`makerBaseFee` from the response.
  Default falls back to 200 bps only when the field is absent.

- **Pre-open markets (acceptingOrders=false) not filtered in Gamma stage**

  Markets in a pre-open or closed state passed the Gamma filter and wasted
  CLOB lookups.

  **Fix:** `gamma_candidates()` skips markets where `acceptingOrders` is
  explicitly `false`. `parse_clob_market()` returns `Err` if CLOB confirms
  the market is not accepting orders.

- **`start_time` set to market creation date instead of trading-window open**

  `parse_gamma_market` used `startDate` (creation date, days earlier) instead
  of `eventStartTime`/`startTime` (trading window open). This inflated
  `actual_duration_secs()`.

  **Fix:** Parser now prefers `eventStartTime` → `startTime` → `startDate`.

---

## [0.4.5] - 2026-03-17

### Fixed

- **Bot idles forever when no short-duration BTC market exists**
  (`src/market_discovery/mod.rs`)

  After the v0.4.4 duration cap was added, Tier 3 correctly rejected novelty
  markets — but when *no* short-duration BTC market exists at all (current
  Polymarket situation), the worker spun indefinitely with "no market found"
  instead of trading anything.

  **Fix:** Tier 3 now runs in two passes:
  1. First pass applies the preferred duration cap (`5m` → 3 600 s, `15m` → 7 200 s).
  2. If no CLOB-active market is found in pass 1, a second pass with **no
     duration cap** allows any open BTC prediction market to be used as a
     stand-in. A `WARN` log is emitted:
     > `[5m] No short-duration BTC market found (btc-updown-5m/15m are not
     > active). Falling back to any BTC prediction market…`

  When Polymarket re-lists the dedicated windows, Tier 2 picks them up
  automatically and this fallback is never reached.

---

## [0.4.4] - 2026-03-17

### Fixed

- **Long-duration novelty markets accepted for 5m/15m workers**
  (`src/market_discovery/mod.rs`)

  When Tier 2 slug lookup fails (btc-updown markets inactive), Tier 3 keyword
  search was returning markets like `will-bitcoin-hit-1m-before-gta-vi-872` — a
  months-long speculative market whose slug happens to contain "bitcoin". The
  `_market_type` parameter in `gamma_candidates()` was never used, so there was
  no duration cap for short-term workers.

  **Fix:** `gamma_candidates()` now enforces per-type maximum duration:
  - `5m` workers: max 3 600 s (1 hour)
  - `15m` workers: max 7 200 s (2 hours)
  - `generic` workers: unlimited (no change)

  Additionally, 5m/15m candidates are sorted **ascending** by time-remaining
  so the most-active short-window market is tried first, rather than the
  longest-lived one.

- **"Invalid symbol 46, offset 6" — `POLY_API_SECRET` base64 decode failure**
  (`src/execution/signing.rs`)

  `build_hmac_signature()` used only `STANDARD` base64 to decode the API
  secret. Polymarket can issue secrets in URL-safe base64 (uses `-`/`_`)
  or other variants; ASCII 46 (`.`) is not valid in any base64 alphabet, but
  some client tooling emits secrets that mix in non-standard characters.

  **Fix:** Decoding now tries three variants in order — `STANDARD` →
  `URL_SAFE_NO_PAD` → `URL_SAFE` — and falls back to raw bytes with a
  debug-level warning if all three fail, instead of hard-crashing.

---

## [0.4.3] - 2026-03-17

### Fixed

- **Relevance filter accepted `russia-ukraine-ceasefire-before-gta-vi-554` again**
  (`src/market_discovery/mod.rs`)

  The v0.4.2 relevance guard checked both question text AND slug. The Russia-Ukraine
  market question read something like "Will X happen before Bitcoin hits $Y?" — so
  it contained "bitcoin" and passed the filter, even though the market has nothing
  to do with BTC price prediction.

  **Fix:** Changed `gamma_candidates()` to check the **slug only** for required
  terms. Slugs are machine-generated from the market title and reliably identify
  what the market is about; question text can mention BTC tangentially in completely
  unrelated markets. Slug `russia-ukraine-ceasefire-before-gta-vi-554` has no
  "btc" or "bitcoin" → correctly rejected.

- **Both 5m and 15m workers claimed the same generic market simultaneously**
  (`src/strategy/market_making.rs`)

  When btc-updown window slugs are unavailable, both workers fall through to Tier 3
  keyword search and independently select the same best Gamma candidate. Both then
  register the same market in `current_markets` and place doubled orders on the
  same token.

  **Fix:** Added a duplicate-market guard in `wait_for_market()`. After a market
  is found, the worker checks `state.current_markets` for any other worker already
  trading the same `condition_id`. If one is found, the worker logs a message and
  sleeps 30 seconds before retrying — it will keep waiting until either a
  window-specific `btc-updown-{type}-{ts}` market appears, or the other worker's
  market expires and frees the slot.

---

## [0.4.2] - 2026-03-17

### Fixed

- **Bot selected irrelevant market `russia-ukraine-ceasefire-before-gta-vi`**
  (`src/market_discovery/mod.rs`, `src/config/mod.rs`, `config.toml`)

  After the v0.4.1 condition_id fix, Tier 3 CLOB verification started working for
  generic markets — but Gamma's `?q=` search is fuzzy/semantic and returned
  unrelated markets that contained no BTC terms. The "Will Bitcoin" query matched
  the Russia-Ukraine ceasefire market (likely due to shared event-prediction phrasing
  like "before GTA VI"). The bot picked the first CLOB-verified candidate, which
  was this unrelated market.

  **Fix:** Added a relevance guard in `gamma_candidates()`. A Gamma candidate is
  now kept only if its question text **or** slug contains at least one term from
  `keyword_require_match` (case-insensitive). Default: `["bitcoin", "btc"]`.
  Any market without "bitcoin" or "btc" in its question/slug is discarded before
  CLOB verification, so only genuinely BTC-related markets are ever traded.

  ```toml
  # config.toml — configurable, set to [] to disable
  keyword_require_match = ["bitcoin", "btc"]
  ```

- **403 Forbidden on order placement gives no actionable info**
  (`src/execution/mod.rs`)

  A missing API credential (empty `POLY_API_KEY` / `POLY_API_SECRET` / etc.)
  caused silent 403 failures logged only as "Quote refresh error: 403 Forbidden"
  with no explanation.

  **Fix 1:** `ExecutionEngine::new()` now checks for empty credentials in live
  mode at startup and logs a `WARN` listing the missing env vars + directions to
  the Polymarket API keys page.

  **Fix 2:** `place_order()` intercepts HTTP 403 specifically and logs:
  ```
  [WARN] Order placement returned 403 Forbidden — API credentials invalid or missing.
         Required env vars: POLY_API_KEY, POLY_API_SECRET, POLY_API_PASSPHRASE, POLY_FUNDER_ADDRESS
         Get credentials from: https://polymarket.com/profile?tab=api-keys
  ```

### Added

- `StrategyConfig.keyword_require_match: Vec<String>` (default `["bitcoin", "btc"]`)
  — relevance guard for Gamma keyword search results. Configurable in `config.toml`.

---

## [0.4.1] - 2026-03-17

### Fixed

- **Tier 3 keyword search never found any market — CLOB lookup used slug instead
  of `condition_id`** (`src/market_discovery/mod.rs`)

  The CLOB API path `GET /markets/{id}` accepts **`condition_id`** as the
  identifier for all market types. Slug-based routing (`/markets/{slug}`) only
  works for special Polymarket market types such as `btc-updown-*`.

  Generic BTC prediction markets returned by Gamma keyword search have human-readable
  slugs (e.g. `will-btc-close-above-90k-on-march-20`) but must be verified in CLOB
  via their `condition_id`. The previous code discarded the Gamma `condition_id`
  (`_market_json` was ignored) and only tried the slug path, which always returned
  HTTP 404 — causing the "43 candidates found but none active in CLOB" failure.

  **Fix:**
  1. Added `fetch_from_clob_by_condition_id(condition_id, slug)` — calls
     `GET /markets/{condition_id}`.
  2. Tier 3 now extracts `conditionId` / `condition_id` from the Gamma response
     and tries `condition_id` lookup **first**; falls back to slug lookup.
  3. CLOB rejection is now logged at `INFO` level (showing slug + error) rather
     than `DEBUG`, so failures are visible in default log output without
     `RUST_LOG=debug`.

---

## [0.4.0] - 2026-03-17

### Added

- **Concurrent dual-market trading — 5m and 15m simultaneously**
  (`src/main.rs`, `src/strategy/market_making.rs`, `src/config/mod.rs`)

  The bot now spawns one independent `MarketMakingStrategy` worker per entry in
  `market_types`. By default this is `["5m", "15m"]`, meaning:

  - A **5m worker** continuously discovers `btc-updown-5m-{ts}` markets and
    places maker quotes on every 5-minute window.
  - A **15m worker** concurrently discovers `btc-updown-15m-{ts}` markets and
    places maker quotes on every 15-minute window.
  - Both workers share the same BTC price feed, risk engine, and execution
    engine — but maintain fully **isolated per-worker inventory**.

- **Per-worker local inventory** (`src/strategy/market_making.rs`)

  Each `MarketMakingStrategy` now tracks `local_inv_up` and `local_inv_down`
  independently. This prevents cross-market inventory bleed: a 15m fill no
  longer corrupts the 5m worker's inventory skew or settlement PnL.

- **`market_types: Vec<String>` config field** (`src/config/mod.rs`, `config.toml`)

  Replaces the single `market_type: String`. Each element spawns one worker.
  Old configs with `market_type = "5m"` are transparently migrated.

  ```toml
  # Trade both timeframes concurrently (default)
  market_types = ["5m", "15m"]
  # Trade only 15m markets
  # market_types = ["15m"]
  ```

### Changed

- `config.toml`: `market_type = "5m"` → `market_types = ["5m", "15m"]`.
- `config.toml`: `max_concurrent_markets` updated from `1` to `2`.
- `src/types.rs`: `BotState.current_market: RwLock<Option<Market>>` replaced by
  `current_markets: DashMap<String, Market>` keyed by market-type string.
- `src/monitoring/mod.rs`: status log now shows all active markets, e.g.
  `markets=[5m:btc-updown-5m-…, 15m:btc-updown-15m-…]`.
- `src/strategy/market_making.rs`: `new()` takes `market_type_str: String` as
  first argument; `run()` takes `discovery: Arc<MarketDiscovery>`.
- `src/strategy/sim_fills.rs`: `simulate_settlement()` replaced by `record_pnl()`
  — PnL is now computed by the strategy from per-worker local inventory.
- Version banner updated to v0.4.0.

---

## [0.3.3] - 2026-03-17

### Fixed

- **`market_type = "generic"` in `config.toml` silently disabled BTC up/down slug discovery**
  (`config.toml`)

  The bot is designed to trade `btc-updown-5m` and `btc-updown-15m` window markets.
  These are targeted via a **three-tier discovery** path:

  | Tier | Description |
  |------|-------------|
  | 1 | Pinned `market_slug` in config (highest priority) |
  | 2 | Computed window slug — `btc-updown-5m-{ts}` / `btc-updown-15m-{ts}` |
  | 3 | Gamma keyword search, CLOB-verified (fallback only) |

  Tier 2 is **only active** when `market_type` is `"5m"` or `"15m"`. The previous
  default of `"generic"` skipped Tier 2 entirely, meaning the bot never attempted
  the canonical `btc-updown-{5m|15m}-{window_timestamp}` slugs and went straight
  to generic keyword search.

  **Fix:** Changed default `market_type` from `"generic"` to `"5m"`. The bot now
  tries `btc-updown-5m-{ts}` first on every cycle. Switch to `"15m"` for the
  15-minute window variant. Tier 3 keyword search remains as a fallback for when
  Polymarket's fixed-window markets are temporarily unavailable.

### Changed

- `config.toml`: `market_type` default changed from `"generic"` to `"5m"`.
- `config.toml`: Expanded `[strategy]` comments to document the three-tier discovery
  order and clarify when each tier is active.
- `config.toml`: `market_slug` example updated to show a BTC up/down slug
  (`btc-updown-5m-1773723900`) instead of a generic example.

---

## [0.3.2] - 2026-03-17

### Fixed

- **[Critical] Keyword search accepted Gamma markets not available in CLOB API**
  (`src/market_discovery/mod.rs`)

  The Gamma metadata API lists all markets — including novelty/meme markets
  (e.g. `will-bitcoin-hit-1m-before-gta-vi`) that have no active order book in the
  CLOB trading API. The previous code accepted the first Gamma hit as a tradeable
  market without verifying it exists in CLOB. The bot would then fail silently
  when trying to subscribe to orderbooks or place orders.

  **Fix:** After Gamma keyword search, candidates are now iterated in
  most-time-remaining order and each is verified against the CLOB API
  (`GET /markets/{slug}`). The first slug that returns a valid CLOB response is
  used. Slugs that 404 on CLOB are skipped with a `debug` log. If no candidate
  passes CLOB verification, the next fallback keyword is tried.

- **Keyword `"BTC"` too broad — matched novelty markets**
  Default `keyword_search` changed from `"BTC"` to `"Will Bitcoin"`.
  "Will Bitcoin" targets structured price-prediction markets (e.g.
  "Will Bitcoin exceed $80,000 by end of April?") and avoids meme/GTA VI/
  hypothetical markets that appear when searching bare "BTC".

- **Version banner showed `v0.1.0`** despite being at v0.3.2. Fixed.

### Added

- **`[strategy] keyword_fallbacks`** — ordered list of fallback keyword searches tried
  when the primary `keyword_search` finds no CLOB-active market.
  Default: `["Bitcoin price", "BTC price"]`.
  Each is an independent Gamma API query; the first keyword + CLOB combination
  that succeeds wins.

- **Config validation**: `min_market_secs_remaining` must be greater than
  `pre_settlement_cancel_secs`. A config where the bot would enter a market only
  to immediately trigger pre-settlement cancel now fails at startup with a clear
  error.

### Changed

- `fetch_from_gamma_by_keyword` refactored into `gamma_candidates` (returns all
  sorted candidates) + CLOB-verification loop in `find_active_market`.
  This means CLOB is always the authoritative data source — even when Gamma is
  used for discovery, the final market struct is populated from the CLOB response.

- `config.toml`: `keyword_search` updated to `"Will Bitcoin"`;
  `keyword_fallbacks` array added.

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

[Unreleased]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.4.3...HEAD
[0.4.3]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mkrfsbri/PolyMMRF/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mkrfsbri/PolyMMRF/releases/tag/v0.1.0
