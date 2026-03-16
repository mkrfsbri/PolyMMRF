use crate::config::BotConfig;
use crate::types::{Market, MarketType, Outcome};
use anyhow::{bail, Result};
use chrono::{DateTime, TimeZone, Utc};
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

/// Seconds until the current window closes.
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

    /// Find an active market for the given asset and market type.
    /// Tries current window slug, falls back to Gamma API.
    pub async fn find_active_market(
        &self,
        asset: &str,
        market_type: MarketType,
    ) -> Result<Market> {
        let now_ts = Utc::now().timestamp();
        let slug = calculate_slug(asset, market_type, now_ts);
        info!("Looking for market: {}", slug);

        // Try CLOB API first
        match self.fetch_from_clob(&slug).await {
            Ok(m) => return Ok(m),
            Err(e) => {
                debug!("CLOB lookup failed for {}: {}", slug, e);
            }
        }

        // Fallback to Gamma API
        match self.fetch_from_gamma(&slug, market_type, asset).await {
            Ok(m) => return Ok(m),
            Err(e) => {
                debug!("Gamma lookup failed for {}: {}", slug, e);
            }
        }

        bail!("No active market found for slug: {}", slug)
    }

    async fn fetch_from_clob(
        &self,
        slug: &str,
    ) -> Result<Market> {
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

    async fn fetch_from_gamma(
        &self,
        slug: &str,
        market_type: MarketType,
        asset: &str,
    ) -> Result<Market> {
        let url = format!(
            "{}/markets?slug={}&active=true",
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
        parse_gamma_market(&markets[0], slug, market_type, asset)
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

fn parse_clob_market(v: &serde_json::Value, slug: &str) -> Result<Market> {
    let condition_id = v["condition_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing condition_id"))?
        .to_string();

    let tokens = v["tokens"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing tokens array"))?;

    let (token_id_up, token_id_down) = extract_token_ids(tokens)?;

    let end_date_iso = v["end_date_iso"]
        .as_str()
        .or_else(|| v["game_start_time"].as_str())
        .unwrap_or("");
    let end_time = parse_datetime(end_date_iso)?;

    let start_time = v["game_start_time"]
        .as_str()
        .and_then(|s| parse_datetime(s).ok())
        .unwrap_or_else(Utc::now);

    let market_type = if slug.contains("-5m-") {
        MarketType::FiveMinute
    } else {
        MarketType::FifteenMinute
    };

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

    Ok(Market {
        condition_id,
        slug: slug.to_string(),
        question: v["question"].as_str().unwrap_or("").to_string(),
        token_id_up,
        token_id_down,
        start_time: Utc::now(),
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

fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    if s.is_empty() {
        bail!("Empty datetime string");
    }
    // Try ISO 8601
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Try unix timestamp
    if let Ok(ts) = s.parse::<i64>() {
        return Ok(Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now));
    }
    bail!("Could not parse datetime: {}", s)
}

fn extract_asset_from_slug(slug: &str) -> String {
    slug.split('-').next().unwrap_or("btc").to_uppercase()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
