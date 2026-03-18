use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Sub-configs ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub clob_api_url: String,
    pub ws_url: String,
    pub gamma_api_url: String,
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub funder_address: String,
    /// 0 = EOA, 1 = Proxy, 2 = GnosisSafe
    pub signature_type: u8,
    pub chain_id: u64,
}

impl Default for PolymarketConfig {
    fn default() -> Self {
        Self {
            clob_api_url: "https://clob.polymarket.com".into(),
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            gamma_api_url: "https://gamma-api.polymarket.com".into(),
            private_key: String::new(),
            funder_address: String::new(),
            signature_type: 1, // POLY_PROXY — default for Magic Link / email accounts
            chain_id: 137,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceConfig {
    pub ws_url: String,
    pub rest_url: String,
    pub symbol: String,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://stream.binance.com:9443/ws/btcusdt@ticker".into(),
            rest_url: "https://api.binance.com".into(),
            symbol: "BTCUSDT".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinbaseConfig {
    /// Coinbase Advanced Trade spot price endpoint (no auth required)
    pub rest_url: String,
    /// How often to poll when in fallback mode (milliseconds)
    pub poll_interval_ms: u64,
    /// Switch to Coinbase after this many consecutive Binance WS failures
    pub max_binance_failures: u32,
    /// While in Coinbase fallback, retry Binance WS every N seconds
    pub retry_binance_secs: u64,
    /// Set to false to disable the Coinbase fallback entirely
    pub enabled: bool,
}

impl Default for CoinbaseConfig {
    fn default() -> Self {
        Self {
            rest_url: "https://api.coinbase.com/v2/prices/BTC-USD/spot".into(),
            poll_interval_ms: 5000,
            max_binance_failures: 3,
            retry_binance_secs: 60,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Target bid price (e.g. 0.45 means we bid at 45 cents)
    pub target_bid_price: f64,
    /// Half-spread in price units (e.g. 0.03 = 3 cents)
    pub half_spread: f64,
    pub min_spread: f64,
    pub max_spread: f64,
    /// Default order size in shares
    pub order_size: f64,
    /// How often to refresh quotes in milliseconds
    pub quote_refresh_ms: u64,
    /// Inventory ratio deviation threshold before skewing
    pub inventory_skew_threshold: f64,
    /// Max skew adjustment amount
    pub inventory_skew_amount: f64,
    /// Market types to trade concurrently.
    /// Each entry spawns an independent market-making worker.
    ///   "5m"      → discovers btc-updown-5m-{ts} markets
    ///   "15m"     → discovers btc-updown-15m-{ts} markets
    ///   "generic" → keyword search only (no slug computation)
    /// Default: ["5m", "15m"] — trade both BTC up/down timeframes simultaneously.
    #[serde(default = "default_market_types")]
    pub market_types: Vec<String>,
    /// Deprecated: use `market_types` instead.
    /// If set and `market_types` is empty, this single value is used.
    #[serde(default, skip_serializing)]
    pub market_type: Option<String>,
    /// Assets to trade (e.g. ["BTC"])
    pub assets: Vec<String>,
    pub post_only: bool,
    /// If set, skip slug calculation and look up this exact market slug directly.
    /// Example: "btc-updown-5m-1773723900"
    #[serde(default)]
    pub market_slug: Option<String>,
    /// Primary Gamma API keyword for market discovery.
    /// Searched first. Use a specific phrase to avoid novelty/meme markets.
    /// Example: "Will Bitcoin" finds price prediction markets.
    /// Default: "Will Bitcoin"
    #[serde(default = "default_keyword_search")]
    pub keyword_search: String,
    /// Fallback keywords tried in order if `keyword_search` finds no CLOB-active market.
    /// Each is a separate Gamma API query. Default: ["Bitcoin price", "BTC price"]
    #[serde(default = "default_keyword_fallbacks")]
    pub keyword_fallbacks: Vec<String>,
    /// Only trade a market if it has at least this many seconds remaining.
    /// Must be > pre_settlement_cancel_secs. Default: 120s.
    #[serde(default = "default_min_market_secs")]
    pub min_market_secs_remaining: i64,
    /// Relevance guard: a Gamma candidate is only considered if its question or
    /// slug contains at least one of these terms (case-insensitive).
    /// Prevents Gamma's fuzzy search from matching unrelated markets.
    /// Default: ["bitcoin", "btc"]
    #[serde(default = "default_keyword_require_match")]
    pub keyword_require_match: Vec<String>,
}

fn default_market_types() -> Vec<String> {
    vec!["5m".into(), "15m".into()]
}

fn default_keyword_search() -> String {
    "Will Bitcoin".into()
}

fn default_keyword_fallbacks() -> Vec<String> {
    vec!["Bitcoin price".into(), "BTC price".into()]
}

fn default_keyword_require_match() -> Vec<String> {
    vec!["bitcoin".into(), "btc".into()]
}

fn default_min_market_secs() -> i64 {
    120
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            target_bid_price: 0.45,
            half_spread: 0.03,
            min_spread: 0.01,
            max_spread: 0.10,
            order_size: 10.0,
            quote_refresh_ms: 5000,
            inventory_skew_threshold: 0.1,
            inventory_skew_amount: 0.02,
            market_types: default_market_types(),
            market_type: None,
            assets: vec!["BTC".into()],
            post_only: true,
            market_slug: None,
            keyword_search: "Will Bitcoin".into(),
            keyword_fallbacks: default_keyword_fallbacks(),
            min_market_secs_remaining: 120,
            keyword_require_match: default_keyword_require_match(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Total bankroll in USDC
    pub bankroll: f64,
    /// Max fraction of bankroll at risk in any single market
    pub max_exposure_pct: f64,
    pub max_concurrent_markets: u32,
    /// Stop trading if daily loss exceeds this fraction of bankroll
    pub daily_loss_limit_pct: f64,
    /// Maximum inventory ratio before refusing new orders
    pub max_inventory_ratio: f64,
    /// Pause after this many consecutive losses
    pub circuit_breaker_losses: u64,
    /// Cancel all orders this many seconds before settlement
    pub pre_settlement_cancel_secs: i64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            bankroll: 1000.0,
            max_exposure_pct: 0.10,
            max_concurrent_markets: 2,
            daily_loss_limit_pct: 0.05,
            max_inventory_ratio: 0.75,
            circuit_breaker_losses: 5,
            pre_settlement_cancel_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotSettings {
    pub simulation: bool,
    pub log_level: String,
    pub metrics_enabled: bool,
    pub metrics_port: u16,
}

impl Default for BotSettings {
    fn default() -> Self {
        Self {
            simulation: true,
            log_level: "info".into(),
            metrics_enabled: false,
            metrics_port: 9090,
        }
    }
}

// ── Root config ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BotConfig {
    #[serde(default)]
    pub polymarket: PolymarketConfig,
    #[serde(default)]
    pub binance: BinanceConfig,
    #[serde(default)]
    pub coinbase: CoinbaseConfig,
    #[serde(default)]
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub risk: RiskConfig,
    #[serde(default)]
    pub bot: BotSettings,
}

impl BotConfig {
    /// Load config from TOML file, then override secrets from env vars.
    pub fn load(path: &str) -> Result<Self> {
        let mut cfg = if Path::new(path).exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str::<BotConfig>(&content)?
        } else {
            tracing::warn!("Config file '{}' not found, using defaults", path);
            BotConfig::default()
        };

        // Load .env if present
        let _ = dotenvy::dotenv();

        // Override secrets from environment
        if let Ok(pk) = std::env::var("POLY_PRIVATE_KEY") {
            cfg.polymarket.private_key = pk;
        }
        if let Ok(fa) = std::env::var("POLY_FUNDER_ADDRESS") {
            cfg.polymarket.funder_address = fa;
        }
        // 0 = EOA, 1 = POLY_PROXY (Magic Link / email), 2 = POLY_GNOSIS_SAFE
        if let Ok(st) = std::env::var("POLY_SIGNATURE_TYPE") {
            if let Ok(v) = st.parse::<u8>() {
                cfg.polymarket.signature_type = v;
            }
        }

        // Migrate deprecated market_type → market_types
        if cfg.strategy.market_types.is_empty() {
            if let Some(ref mt) = cfg.strategy.market_type.clone() {
                cfg.strategy.market_types = vec![mt.clone()];
            }
        }

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let s = &self.strategy;
        if s.target_bid_price <= 0.0 || s.target_bid_price >= 1.0 {
            bail!("target_bid_price must be between 0 and 1");
        }
        if self.risk.bankroll <= 0.0 {
            bail!("bankroll must be positive");
        }
        if self.risk.max_exposure_pct <= 0.0 || self.risk.max_exposure_pct > 1.0 {
            bail!("max_exposure_pct must be between 0 and 1");
        }
        if self.risk.pre_settlement_cancel_secs < 5 {
            bail!("pre_settlement_cancel_secs must be >= 5s");
        }
        if s.min_market_secs_remaining <= self.risk.pre_settlement_cancel_secs {
            bail!(
                "min_market_secs_remaining ({}) must be greater than pre_settlement_cancel_secs ({})",
                s.min_market_secs_remaining,
                self.risk.pre_settlement_cancel_secs
            );
        }
        if s.market_types.is_empty() {
            bail!("market_types must contain at least one entry (e.g. [\"5m\", \"15m\"])");
        }
        Ok(())
    }
}
