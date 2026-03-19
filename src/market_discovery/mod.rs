use crate::config::BotConfig;
use crate::types::{Market, MarketType};
use anyhow::{bail, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, info, warn};

// ── Slug Calculation ───────────────────────────────────────────────────────────

/// Returns the current window slug for a given asset + market type.
/// e.g. "btc-updown-5m-1710000000"
pub fn calculate_slug(asset: &str, market_type: MarketType, now_ts: i64) -> String {
    let period = market_type.duration_secs();
    let window_ts = now_ts - (now_ts % period);
    format!(
        "{}-updown-{}-{}",
        asset.to_lowercase(),
        market_type.slug_prefix(),
        window_ts
    )
}

/// Returns (slug, next_window_start_ts) for the next window.
pub fn calculate_next_slug(
    asset: &str,
    market_type: MarketType,
    now_ts: i64,
) -> (String, i64) {
    let period = market_type.duration_secs();
    let current_window = now_ts - (now_ts % period);
    let next_window = current_window + period;
    let slug = format!(
        "{}-updown-{}-{}",
        asset.to_lowercase(),
        market_type.slug_prefix(),
        next_window
    );
    (slug, next_window)
}

/// Seconds until the current window closes (based on fixed MarketType period).
pub fn time_remaining(market_type: MarketType, now_ts: i64) -> i64 {
    let period = market_type.duration_secs();
    let window_end = now_ts - (now_ts % period) + period;
    window_end - now_ts
}

// ── Market Discovery ──────────────────────────────────────────────────────────

pub struct MarketDiscovery {
    config: BotConfig,
    client: Client,
}

impl MarketDiscovery {
    pub fn new(config: BotConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { config, client })
    }

    /// Find an active market using a three-tier strategy:
    ///
    /// 1. If `strategy.market_slug` is set in config → direct CLOB + Gamma lookup
    /// 2. If `market_type` is "5m" or "15m" → try the computed window slug
    /// 3. Keyword search via Gamma API using `strategy.keyword_search`
    pub async fn find_active_market(
        &self,
        asset: &str,
        market_type: MarketType,
    ) -> Result<Market> {
        // ── Tier 1: operator-specified slug ─────────────────────────────────
        if let Some(ref slug) = self.config.strategy.market_slug {
            info!("Using configured market_slug: {}", slug);
            match self.fetch_from_clob(slug).await {
                Ok(m) => return Ok(m),
                Err(e) => debug!("CLOB lookup for configured slug '{}' failed: {}", slug, e),
            }
            match self.fetch_from_gamma_by_exact_slug(slug).await {
                Ok(m) => return Ok(m),
                Err(e) => debug!("Gamma lookup for configured slug '{}' failed: {}", slug, e),
            }
            warn!("Configured market_slug '{}' not found on CLOB or Gamma", slug);
        }

        // ── Tier 2: computed window slug (5m / 15m legacy markets) ──────────
        if market_type != MarketType::Generic {
            let now_ts = Utc::now().timestamp();
            let slug = calculate_slug(asset, market_type, now_ts);
            debug!("Trying computed slug: {}", slug);

            match self.fetch_from_clob(&slug).await {
                Ok(m) => {
                    info!("Found market via computed slug: {}", slug);
                    return Ok(m);
                }
                Err(e) => debug!("CLOB lookup for '{}' failed: {}", slug, e),
            }
            match self.fetch_from_gamma_by_exact_slug(&slug).await {
                Ok(m) => {
                    info!("Found market via Gamma slug search: {}", slug);
                    return Ok(m);
                }
                Err(e) => debug!("Gamma slug lookup for '{}' failed: {}", slug, e),
            }
        }

        // ── Tier 3: keyword search with CLOB verification ────────────────────
        let keywords: Vec<String> = {
            let primary = self.config.strategy.keyword_search.clone();
            let mut kws = vec![primary];
            kws.extend(self.config.strategy.keyword_fallbacks.iter().cloned());
            kws
        };

        // Preferred max duration for short-term market types.
        // If no market is found within this window, a second pass runs without
        // the cap so the worker can still trade longer-duration BTC markets
        // when 5m/15m are not listed on Polymarket.
        let preferred_max_secs: i64 = match market_type {
            MarketType::FiveMinute => 3_600,
            MarketType::FifteenMinute => 7_200,
            MarketType::Generic => i64::MAX,
        };

        // Run Tier 3 up to twice: first with preferred duration cap, then (if
        // nothing found) without cap so a long-duration BTC market can be used
        // as a stand-in when 5m/15m markets are inactive on Polymarket.
        let duration_caps: &[i64] = if preferred_max_secs == i64::MAX {
            &[i64::MAX]
        } else {
            &[preferred_max_secs, i64::MAX]
        };
        let market_type_label = match market_type {
            MarketType::FiveMinute => "5m",
            MarketType::FifteenMinute => "15m",
            MarketType::Generic => "generic",
        };

        for (pass, &max_dur) in duration_caps.iter().enumerate() {
            if pass == 1 {
                warn!(
                    "[{}] No short-duration BTC market found (btc-updown-5m/15m are not active). \
                     Falling back to any BTC prediction market — quotes will be placed on a \
                     longer-duration market until the dedicated windows return.",
                    market_type_label
                );
            }

        for keyword in &keywords {
            info!("Searching Gamma API with keyword: '{}'", keyword);
            match self.gamma_candidates(keyword, market_type, max_dur).await {
                Ok(candidates) if !candidates.is_empty() => {
                    let total = candidates.len();
                    info!("Keyword '{}': {} candidates, verifying against CLOB...", keyword, total);

                    // Try each candidate. The CLOB API accepts condition_id in its path
                    // (GET /markets/{condition_id}) for generic markets — slug-based routing
                    // only works for special Polymarket market types (e.g. btc-updown).
                    // We try condition_id first, then fall back to slug.
                    for (secs, market_json, slug) in candidates {
                        let condition_id = market_json["conditionId"]
                            .as_str()
                            .or_else(|| market_json["condition_id"].as_str())
                            .unwrap_or("")
                            .to_string();

                        // Try 1: condition_id lookup (works for all market types)
                        let clob_result = if !condition_id.is_empty() {
                            match self.fetch_from_clob_by_condition_id(&condition_id, &slug).await {
                                Ok(m) => Some(m),
                                Err(e) => {
                                    debug!(
                                        "CLOB lookup by condition_id '{}' failed: {}",
                                        condition_id, e
                                    );
                                    None
                                }
                            }
                        } else {
                            None
                        };

                        // Try 2: slug lookup (fallback; works for btc-updown style slugs)
                        let clob_result = match clob_result {
                            Some(m) => Some(m),
                            None => match self.fetch_from_clob(&slug).await {
                                Ok(m) => Some(m),
                                Err(e) => {
                                    info!(
                                        "  ✗ '{}' (cid={}) not in CLOB: {}",
                                        slug,
                                        if condition_id.is_empty() { "?" } else { &condition_id[..condition_id.len().min(10)] },
                                        e
                                    );
                                    None
                                }
                            },
                        };

                        if let Some(clob_market) = clob_result {
                            info!(
                                "Keyword '{}' → '{}' verified in CLOB ({}s / {:.1}h remaining)",
                                keyword,
                                slug,
                                secs,
                                secs as f64 / 3600.0,
                            );
                            return Ok(clob_market);
                        }
                    }
                    warn!(
                        "Keyword '{}': {} candidates found but none are active in CLOB \
                         (run with RUST_LOG=debug for per-slug details)",
                        keyword, total
                    );
                }
                Ok(_) => debug!("Keyword '{}' returned no eligible candidates", keyword),
                Err(e) => debug!("Keyword search '{}' failed: {}", keyword, e),
            }
        }
        } // end duration_caps loop

        bail!(
            "No active market found after trying keywords {:?}. \
             Tip: set [strategy] market_slug = \"<slug>\" in config.toml to pin a market directly.",
            keywords
        )
    }

    // ── CLOB lookup by slug ──────────────────────────────────────────────────
    // Works for btc-updown-style markets that have slug-based CLOB routing.

    async fn fetch_from_clob(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/markets/{}", self.config.polymarket.clob_api_url, slug);
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        parse_clob_market(&resp, slug)
    }

    // ── CLOB lookup by condition_id ──────────────────────────────────────────
    // The CLOB API accepts condition_id as the path segment for all market types.
    // This is the correct lookup method for generic markets found via Gamma search.

    async fn fetch_from_clob_by_condition_id(
        &self,
        condition_id: &str,
        slug: &str,
    ) -> Result<Market> {
        let url = format!(
            "{}/markets/{}",
            self.config.polymarket.clob_api_url, condition_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        // Use the Gamma slug as the canonical slug for our Market struct (the CLOB
        // response may not include a slug field for generic markets).
        let effective_slug = resp["market_slug"]
            .as_str()
            .unwrap_or(slug);
        parse_clob_market(&resp, effective_slug)
    }

    // ── Gamma lookup by exact slug ───────────────────────────────────────────

    async fn fetch_from_gamma_by_exact_slug(&self, slug: &str) -> Result<Market> {
        let url = format!(
            "{}/markets?slug={}&active=true&limit=1",
            self.config.polymarket.gamma_api_url, slug
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        let markets = resp.as_array().ok_or_else(|| anyhow::anyhow!("Not an array"))?;
        if markets.is_empty() {
            bail!("No markets from Gamma for slug: {}", slug);
        }

        // Verify the slug matches exactly (Gamma may do partial matching)
        let found_slug = markets[0]["slug"].as_str().unwrap_or("");
        if found_slug != slug {
            bail!(
                "Gamma returned slug '{}', expected '{}'",
                found_slug,
                slug
            );
        }

        // Derive MarketType from the actual slug
        let mt = market_type_from_slug(found_slug);
        let asset = extract_asset_from_slug(found_slug);
        parse_gamma_market(&markets[0], found_slug, mt, &asset)
    }

    // ── Gamma keyword search helpers ─────────────────────────────────────────

    /// Fetch and filter Gamma markets by keyword. Returns candidates sorted by
    /// most time remaining (highest first). CLOB availability is NOT checked here —
    /// callers iterate the list and verify each slug against the CLOB API.
    async fn gamma_candidates(
        &self,
        keyword: &str,
        market_type: MarketType,
        max_duration_secs: i64,
    ) -> Result<Vec<(i64, serde_json::Value, String)>> {
        let min_secs = self.config.strategy.min_market_secs_remaining;

        let max_secs = max_duration_secs;
        let base_url = format!("{}/markets", self.config.polymarket.gamma_api_url);

        // closed=false: only unresolved markets; limit=50: wider net
        let resp = self
            .client
            .get(&base_url)
            .query(&[("closed", "false"), ("q", keyword), ("limit", "50")])
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        // Gamma may return a plain array OR {"count": N, "data": [...]}
        let markets_val = if resp.is_array() {
            resp.clone()
        } else if let Some(arr) = resp.get("data").or_else(|| resp.get("results")) {
            arr.clone()
        } else {
            bail!(
                "Unrecognised Gamma response shape: {}",
                &resp.to_string()[..resp.to_string().len().min(200)]
            );
        };
        let markets = markets_val
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Gamma markets field is not an array"))?;

        if markets.is_empty() {
            bail!("No markets returned from Gamma for keyword '{}'", keyword);
        }

        // Relevance guard terms (case-insensitive): market question or slug must
        // contain at least one. Prevents Gamma's fuzzy search from returning
        // unrelated markets (e.g. "russia-ukraine-ceasefire-before-gta-vi" matched
        // "Will Bitcoin" because the Gamma search is not phrase-exact).
        let require_terms: Vec<String> = self
            .config
            .strategy
            .keyword_require_match
            .iter()
            .map(|t| t.to_lowercase())
            .collect();

        let now = Utc::now();
        let mut skipped_closed: usize = 0;
        let mut skipped_no_date: usize = 0;
        let mut skipped_expired: usize = 0;
        let mut skipped_irrelevant: usize = 0;

        let mut candidates: Vec<(i64, serde_json::Value, String)> = markets
            .iter()
            .filter_map(|m| {
                let slug = m["slug"].as_str()?.to_string();

                // Skip markets that are closed/resolved/archived or explicitly not
                // accepting orders. acceptingOrders=false can mean: closed, pre-open
                // window, or awaiting resolution. Only skip when the field is
                // explicitly false (absent means "unknown" — CLOB verify will catch it).
                let not_accepting = m["acceptingOrders"].as_bool() == Some(false);
                if m["closed"].as_bool().unwrap_or(false)
                    || m["resolved"].as_bool().unwrap_or(false)
                    || m["archived"].as_bool().unwrap_or(false)
                    || not_accepting
                {
                    skipped_closed += 1;
                    return None;
                }

                // Relevance check: SLUG must contain at least one required term.
                // We deliberately check slug only (not question text) because:
                // - Slugs are machine-generated from the market title and reliably
                //   identify what the market is about.
                // - Questions can mention BTC/Bitcoin tangentially in unrelated
                //   markets (e.g. "Will Russia sign ceasefire before Bitcoin hits $X?")
                //   which would incorrectly pass a question-based filter.
                if !require_terms.is_empty() {
                    let slug_lc = slug.to_lowercase();
                    let relevant = require_terms
                        .iter()
                        .any(|t| slug_lc.contains(t.as_str()));
                    if !relevant {
                        debug!(
                            "Skipping irrelevant market '{}' (slug has no match for {:?})",
                            slug, require_terms
                        );
                        skipped_irrelevant += 1;
                        return None;
                    }
                }

                let end_str = m["endDate"]
                    .as_str()
                    .or_else(|| m["end_date_iso"].as_str())
                    .or_else(|| m["end_time"].as_str())?;

                let end_time = match parse_datetime(end_str) {
                    Ok(t) => t,
                    Err(_) => {
                        skipped_no_date += 1;
                        return None;
                    }
                };

                let secs_left = (end_time - now).num_seconds();
                if secs_left <= min_secs {
                    skipped_expired += 1;
                    return None;
                }
                if secs_left > max_secs {
                    debug!(
                        "Skipping long-duration market '{}' ({}s > max {}s for {:?})",
                        slug, secs_left, max_secs, market_type
                    );
                    skipped_expired += 1;
                    return None;
                }

                Some((secs_left, m.clone(), slug))
            })
            .collect();

        // For short-term market types (5m/15m), sort ascending (soonest expiry first)
        // so we prefer the most-active short-term market over stale ones.
        // For generic markets, sort descending (most time remaining) to avoid
        // immediately-expiring markets.
        match market_type {
            MarketType::FiveMinute | MarketType::FifteenMinute => {
                candidates.sort_by(|a, b| a.0.cmp(&b.0)); // ascending
            }
            MarketType::Generic => {
                candidates.sort_by(|a, b| b.0.cmp(&a.0)); // descending
            }
        }

        debug!(
            "Gamma '{}': {} total, {} closed, {} irrelevant, {} bad-date, {} <{}s or >{max_s}s → {} candidates",
            keyword,
            markets.len(),
            skipped_closed,
            skipped_irrelevant,
            skipped_no_date,
            skipped_expired,
            min_secs,
            candidates.len(),
            max_s = if max_secs == i64::MAX { "∞".to_string() } else { max_secs.to_string() },
        );

        Ok(candidates)
    }

    /// Fetch fee rate for a token. Returns default 315 bps if unavailable.
    pub async fn get_fee_rate(&self, token_id: &str) -> u32 {
        let url = format!(
            "{}/fee-rate?tokenID={}",
            self.config.polymarket.clob_api_url, token_id
        );
        match self.client.get(&url).send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    if let Some(rate) = v["fee_rate"].as_u64() {
                        return rate as u32;
                    }
                    315
                }
                Err(_) => 315,
            },
            Err(_) => 315,
        }
    }
}

// ── Market type detection ─────────────────────────────────────────────────────

fn market_type_from_slug(slug: &str) -> MarketType {
    if slug.contains("-5m-") || slug.contains("-5m") {
        MarketType::FiveMinute
    } else if slug.contains("-15m-") || slug.contains("-15m") {
        MarketType::FifteenMinute
    } else {
        MarketType::Generic
    }
}

// ── Market parsers ────────────────────────────────────────────────────────────

fn parse_clob_market(v: &serde_json::Value, slug: &str) -> Result<Market> {
    let condition_id = v["condition_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing condition_id"))?
        .to_string();

    // Check whether the market is currently accepting orders. Markets that
    // exist in CLOB but have acceptingOrders=false are not tradeable right now
    // (e.g. closed, awaiting resolution, or pre-open window).
    if v["accepting_orders"].as_bool() == Some(false)
        || v["acceptingOrders"].as_bool() == Some(false)
    {
        bail!("Market '{}' exists in CLOB but is not accepting orders", slug);
    }

    // Try Format A (tokens array) then Format B (clobTokenIds + outcomes strings).
    let (token_id_up, token_id_down) = if let Some(tokens) = v["tokens"].as_array() {
        extract_token_ids(tokens)?
    } else {
        extract_token_ids_from_stringified(v)?
    };

    // end_date_iso is the correct end time; game_start_time is the *start*.
    let end_date_iso = v["end_date_iso"].as_str().unwrap_or("");
    let end_time = parse_datetime(end_date_iso)?;

    let start_time = v["game_start_time"]
        .as_str()
        .and_then(|s| parse_datetime(s).ok())
        .unwrap_or_else(Utc::now);

    let market_type = market_type_from_slug(slug);
    let asset = extract_asset_from_slug(slug);

    // Use actual fee rate from CLOB response; btc-updown uses 1000 bps (10%).
    let fee_rate_bps = v["maker_base_fee"]
        .as_u64()
        .or_else(|| v["taker_base_fee"].as_u64())
        .or_else(|| v["makerBaseFee"].as_u64())
        .or_else(|| v["takerBaseFee"].as_u64())
        .map(|f| f as u32)
        .unwrap_or(200);

    Ok(Market {
        condition_id,
        slug: slug.to_string(),
        question: v["question"].as_str().unwrap_or("").to_string(),
        token_id_up,
        token_id_down,
        start_time,
        end_time,
        market_type,
        asset,
        fee_rate_bps,
        // CLOB API uses snake_case; some responses use camelCase — try both.
        neg_risk: v["neg_risk"].as_bool()
            .or_else(|| v["negRisk"].as_bool())
            .unwrap_or(false),
    })
}

fn parse_gamma_market(
    v: &serde_json::Value,
    slug: &str,
    market_type: MarketType,
    asset: &str,
) -> Result<Market> {
    let condition_id = v["condition_id"]
        .as_str()
        .or_else(|| v["conditionId"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing condition_id in gamma response"))?
        .to_string();

    // Gamma returns token IDs in one of two formats:
    //   Format A (events API): tokens array [{outcome, token_id}, ...]
    //   Format B (markets API): clobTokenIds = "[\"id1\",\"id2\"]" (JSON string)
    //                           outcomes     = "[\"Up\",\"Down\"]"  (JSON string)
    let (token_id_up, token_id_down) = if let Some(tokens) = v["tokens"].as_array() {
        extract_token_ids(tokens)?
    } else {
        extract_token_ids_from_stringified(v)?
    };

    let end_date = v["endDate"]
        .as_str()
        .or_else(|| v["end_date_iso"].as_str())
        .unwrap_or("");
    let end_time = parse_datetime(end_date)?;

    // eventStartTime is the actual trading-window open time; startDate is market
    // creation time which is earlier and not the trading window.
    let start_time = v["eventStartTime"]
        .as_str()
        .or_else(|| v["startTime"].as_str())
        .or_else(|| v["startDate"].as_str())
        .or_else(|| v["start_date_iso"].as_str())
        .and_then(|s| parse_datetime(s).ok())
        .unwrap_or_else(Utc::now);

    // Prefer takerBaseFee from the market data; fall back to our default 200 bps
    // (btc-updown markets use 1000 bps = 10%).
    let fee_rate_bps = v["takerBaseFee"]
        .as_u64()
        .or_else(|| v["makerBaseFee"].as_u64())
        .map(|f| f as u32)
        .unwrap_or(200);

    Ok(Market {
        condition_id,
        slug: slug.to_string(),
        question: v["question"].as_str().unwrap_or("").to_string(),
        token_id_up,
        token_id_down,
        start_time,
        end_time,
        market_type,
        asset: asset.to_string(),
        fee_rate_bps,
        // Gamma API uses camelCase; fall back to snake_case just in case.
        neg_risk: v["negRisk"].as_bool()
            .or_else(|| v["neg_risk"].as_bool())
            .unwrap_or(false),
    })
}

fn extract_token_ids(tokens: &[serde_json::Value]) -> Result<(String, String)> {
    let mut up_id = String::new();
    let mut down_id = String::new();

    for token in tokens {
        let outcome = token["outcome"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        let token_id = token["token_id"]
            .as_str()
            .or_else(|| token["tokenId"].as_str())
            .unwrap_or("")
            .to_string();

        if outcome.contains("up") || outcome.contains("higher") || outcome.contains("yes") {
            up_id = token_id;
        } else if outcome.contains("down") || outcome.contains("lower") || outcome.contains("no") {
            down_id = token_id;
        }
    }

    if up_id.is_empty() || down_id.is_empty() {
        // Fallback: first = up, second = down
        if tokens.len() >= 2 {
            up_id = tokens[0]["token_id"]
                .as_str()
                .or_else(|| tokens[0]["tokenId"].as_str())
                .unwrap_or("")
                .to_string();
            down_id = tokens[1]["token_id"]
                .as_str()
                .or_else(|| tokens[1]["tokenId"].as_str())
                .unwrap_or("")
                .to_string();
        } else {
            bail!("Could not extract Up/Down token IDs");
        }
    }

    Ok((up_id, down_id))
}

/// Extract token IDs from Gamma's "Format B" where clobTokenIds and outcomes
/// are stored as JSON-encoded strings rather than proper arrays.
///
/// Example Gamma response:
///   "clobTokenIds": "[\"21184...\", \"45121...\"]"
///   "outcomes":     "[\"Up\", \"Down\"]"
fn extract_token_ids_from_stringified(v: &serde_json::Value) -> Result<(String, String)> {
    let ids_raw = v["clobTokenIds"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing clobTokenIds and tokens in Gamma response"))?;
    let ids: Vec<String> = serde_json::from_str(ids_raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse clobTokenIds JSON string: {}", e))?;

    let outcomes_raw = v["outcomes"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing outcomes field in Gamma response"))?;
    let outcomes: Vec<String> = serde_json::from_str(outcomes_raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse outcomes JSON string: {}", e))?;

    if ids.len() < 2 || outcomes.len() < 2 {
        bail!(
            "Expected ≥2 token IDs and outcomes, got ids={} outcomes={}",
            ids.len(),
            outcomes.len()
        );
    }

    let mut up_id = String::new();
    let mut down_id = String::new();

    for (outcome, id) in outcomes.iter().zip(ids.iter()) {
        let o = outcome.to_lowercase();
        if o.contains("up") || o.contains("higher") || o.contains("yes") {
            up_id = id.clone();
        } else if o.contains("down") || o.contains("lower") || o.contains("no") {
            down_id = id.clone();
        }
    }

    if up_id.is_empty() || down_id.is_empty() {
        // Fallback positional: index 0 = Up, index 1 = Down
        up_id = ids[0].clone();
        down_id = ids[1].clone();
    }

    Ok((up_id, down_id))
}

/// Parse a datetime string from Polymarket APIs.
///
/// Handles every format observed in the wild:
/// 1. RFC 3339 with timezone  — `"2026-03-20T12:00:00Z"` / `"…+00:00"`
/// 2. ISO 8601 without timezone — `"2026-03-20T12:00:00"` (assume UTC)
/// 3. Date-only              — `"2026-03-20"` (assume UTC midnight)
/// 4. Unix timestamp seconds — `"1742947200"` (< 1e12)
/// 5. Unix timestamp ms      — `"1742947200000"` (≥ 1e12, divide by 1000)
fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        bail!("Empty datetime string");
    }

    // 1. RFC 3339 with timezone
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // 2. ISO 8601 without timezone (assume UTC)
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(ndt.and_utc());
        }
    }

    // 3. Date-only (assume UTC midnight)
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(ndt) = d.and_hms_opt(0, 0, 0) {
            return Ok(ndt.and_utc());
        }
    }

    // 4 & 5. Unix timestamp (seconds or milliseconds)
    if let Ok(n) = s.parse::<i64>() {
        let ts_secs = if n > 1_000_000_000_000 { n / 1000 } else { n };
        return Ok(Utc.timestamp_opt(ts_secs, 0).single().unwrap_or_else(Utc::now));
    }

    bail!("Could not parse datetime: {:?}", s)
}

fn extract_asset_from_slug(slug: &str) -> String {
    slug.split('-').next().unwrap_or("btc").to_uppercase()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn test_slug_calculation() {
        // 1710000000 / 300 = 5700000, window = 1710000000
        let ts = 1710000000i64;
        let slug = calculate_slug("BTC", MarketType::FiveMinute, ts);
        assert_eq!(slug, "btc-updown-5m-1710000000");
    }

    #[test]
    fn test_slug_mid_window() {
        let ts = 1710000150i64; // 150s into 300s window
        let slug = calculate_slug("BTC", MarketType::FiveMinute, ts);
        assert_eq!(slug, "btc-updown-5m-1710000000");
    }

    #[test]
    fn test_15m_slug() {
        let ts = 1710000900i64;
        let slug = calculate_slug("BTC", MarketType::FifteenMinute, ts);
        assert_eq!(slug, "btc-updown-15m-1710000900");
    }

    #[test]
    fn test_time_remaining() {
        let ts = 1710000150i64; // 150s into 300s window
        let remaining = time_remaining(MarketType::FiveMinute, ts);
        assert_eq!(remaining, 150);
    }

    #[test]
    fn test_next_slug() {
        let ts = 1710000000i64;
        let (slug, next_ts) = calculate_next_slug("BTC", MarketType::FiveMinute, ts);
        assert_eq!(next_ts, 1710000300);
        assert_eq!(slug, "btc-updown-5m-1710000300");
    }

    #[test]
    fn test_market_type_from_slug() {
        assert_eq!(market_type_from_slug("btc-updown-5m-1710000000"), MarketType::FiveMinute);
        assert_eq!(market_type_from_slug("btc-updown-15m-1710000900"), MarketType::FifteenMinute);
        assert_eq!(market_type_from_slug("will-btc-hit-70k-by-march"), MarketType::Generic);
        assert_eq!(market_type_from_slug("eth-updown-5m-1710000000"), MarketType::FiveMinute);
    }

    #[test]
    fn test_parse_clob_end_date_iso_not_start_time() {
        // Bug fix regression: end_date_iso must NOT fall back to game_start_time
        let v = serde_json::json!({
            "condition_id": "0xabc",
            "tokens": [
                {"token_id": "tok1", "outcome": "Yes"},
                {"token_id": "tok2", "outcome": "No"}
            ],
            "end_date_iso": "2099-01-01T00:00:00Z",
            "game_start_time": "2020-01-01T00:00:00Z",
            "question": "Test"
        });
        let m = parse_clob_market(&v, "btc-updown-5m-1710000000").unwrap();
        // end_time must be 2099, not 2020
        assert!(m.end_time.year() == 2099, "end_time was incorrectly set to start time");
        assert!(m.seconds_remaining() > 0, "market appears already settled");
    }

    #[test]
    fn test_parse_clob_generic_market_type() {
        // Bug fix regression: non-5m/15m slugs must get MarketType::Generic
        let v = serde_json::json!({
            "condition_id": "0xabc",
            "tokens": [
                {"token_id": "tok1", "outcome": "Yes"},
                {"token_id": "tok2", "outcome": "No"}
            ],
            "end_date_iso": "2099-01-01T00:00:00Z",
            "question": "Will BTC hit $100k?"
        });
        let m = parse_clob_market(&v, "will-btc-hit-100k").unwrap();
        assert_eq!(m.market_type, MarketType::Generic);
    }

    // ── parse_datetime regression tests ──────────────────────────────────────

    #[test]
    fn test_parse_datetime_rfc3339() {
        let dt = parse_datetime("2026-04-01T12:00:00Z").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
    }

    #[test]
    fn test_parse_datetime_no_timezone() {
        // This is the format that was silently dropped — causes the "< 30s" bug
        let dt = parse_datetime("2026-04-01T12:00:00").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
    }

    #[test]
    fn test_parse_datetime_with_subseconds_no_tz() {
        let dt = parse_datetime("2026-04-01T12:00:00.000").unwrap();
        assert_eq!(dt.year(), 2026);
    }

    #[test]
    fn test_parse_datetime_date_only() {
        let dt = parse_datetime("2026-04-01").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn test_parse_datetime_unix_seconds() {
        // 1743465600 = 2025-04-01T00:00:00Z (approx)
        let dt = parse_datetime("1743465600").unwrap();
        assert_eq!(dt.year(), 2025);
    }

    #[test]
    fn test_parse_datetime_unix_milliseconds() {
        // 1743465600000 ms = same date as above
        let dt = parse_datetime("1743465600000").unwrap();
        assert_eq!(dt.year(), 2025);
    }

    #[test]
    fn test_parse_datetime_empty_fails() {
        assert!(parse_datetime("").is_err());
    }

    #[test]
    fn test_parse_datetime_garbage_fails() {
        assert!(parse_datetime("not-a-date").is_err());
    }

    #[test]
    fn test_extract_token_ids_from_stringified() {
        // Mirrors the real btc-updown-15m Gamma response format
        let v = serde_json::json!({
            "clobTokenIds": "[\"21184606377798540774527844593078494252416073470315245229442696895085648902685\", \"45121060453856693952230534427185255991290087022355829448310387271787735075443\"]",
            "outcomes": "[\"Up\", \"Down\"]"
        });
        let (up, down) = extract_token_ids_from_stringified(&v).unwrap();
        assert_eq!(up, "21184606377798540774527844593078494252416073470315245229442696895085648902685");
        assert_eq!(down, "45121060453856693952230534427185255991290087022355829448310387271787735075443");
    }

    #[test]
    fn test_extract_token_ids_from_stringified_yes_no() {
        let v = serde_json::json!({
            "clobTokenIds": "[\"aaa\", \"bbb\"]",
            "outcomes": "[\"Yes\", \"No\"]"
        });
        let (up, down) = extract_token_ids_from_stringified(&v).unwrap();
        assert_eq!(up, "aaa");
        assert_eq!(down, "bbb");
    }

    #[test]
    fn test_gamma_fee_rate_from_market_data() {
        // btc-updown markets report 1000 bps (10%)
        let v = serde_json::json!({
            "condition_id": "0xabc",
            "conditionId": "0xabc",
            "clobTokenIds": "[\"111\", \"222\"]",
            "outcomes": "[\"Up\", \"Down\"]",
            "endDate": "2026-04-01T12:00:00Z",
            "startDate": "2026-04-01T11:45:00Z",
            "takerBaseFee": 1000,
            "negRisk": false
        });
        let m = parse_gamma_market(&v, "btc-updown-15m-1234567890", MarketType::FifteenMinute, "BTC").unwrap();
        assert_eq!(m.fee_rate_bps, 1000);
        assert_eq!(m.token_id_up, "111");
        assert_eq!(m.token_id_down, "222");
    }
}
