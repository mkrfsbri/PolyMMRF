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

        for keyword in &keywords {
            info!("Searching Gamma API with keyword: '{}'", keyword);
            match self.gamma_candidates(keyword, market_type).await {
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
        _market_type: MarketType,
    ) -> Result<Vec<(i64, serde_json::Value, String)>> {
        let min_secs = self.config.strategy.min_market_secs_remaining;
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

                if m["closed"].as_bool().unwrap_or(false)
                    || m["resolved"].as_bool().unwrap_or(false)
                    || m["archived"].as_bool().unwrap_or(false)
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

                Some((secs_left, m.clone(), slug))
            })
            .collect();

        // Sort descending: market with most time remaining first
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        debug!(
            "Gamma '{}': {} total, {} closed, {} irrelevant, {} bad-date, {} <{}s → {} candidates",
            keyword,
            markets.len(),
            skipped_closed,
            skipped_irrelevant,
            skipped_no_date,
            skipped_expired,
            min_secs,
            candidates.len(),
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

    let tokens = v["tokens"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing tokens array"))?;

    let (token_id_up, token_id_down) = extract_token_ids(tokens)?;

    // Bug fix: use end_date_iso for end time; do NOT fall back to game_start_time
    // (which is the *start* time — using it as end would give seconds_remaining ≤ 0).
    let end_date_iso = v["end_date_iso"].as_str().unwrap_or("");
    let end_time = parse_datetime(end_date_iso)?;

    let start_time = v["game_start_time"]
        .as_str()
        .and_then(|s| parse_datetime(s).ok())
        .unwrap_or_else(Utc::now);

    // Bug fix: derive MarketType from slug, defaulting to Generic (not FifteenMinute)
    let market_type = market_type_from_slug(slug);
    let asset = extract_asset_from_slug(slug);

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
        fee_rate_bps: 315,
        neg_risk: v["neg_risk"].as_bool().unwrap_or(false),
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

    let tokens = v["tokens"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing tokens"))?;

    let (token_id_up, token_id_down) = extract_token_ids(tokens)?;

    let end_date = v["endDate"]
        .as_str()
        .or_else(|| v["end_date_iso"].as_str())
        .unwrap_or("");
    let end_time = parse_datetime(end_date)?;

    let start_time = v["startDate"]
        .as_str()
        .or_else(|| v["start_date_iso"].as_str())
        .and_then(|s| parse_datetime(s).ok())
        .unwrap_or_else(Utc::now);

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
        fee_rate_bps: 315,
        neg_risk: v["negRisk"].as_bool().unwrap_or(false),
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
}
