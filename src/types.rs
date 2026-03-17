use chrono::{DateTime, Utc};
use dashmap::DashMap;
// parking_lot::RwLock no longer needed (current_markets uses DashMap)
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    Arc,
};


// ── Enums ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    Up,
    Down,
}

impl Outcome {
    pub fn opposite(self) -> Self {
        match self {
            Outcome::Up => Outcome::Down,
            Outcome::Down => Outcome::Up,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketType {
    FiveMinute,
    FifteenMinute,
    /// Any market that doesn't match a fixed window format.
    /// Duration is derived from `Market::start_time`..`end_time` at runtime.
    Generic,
}

impl MarketType {
    pub fn duration_secs(self) -> i64 {
        match self {
            MarketType::FiveMinute => 300,
            MarketType::FifteenMinute => 900,
            MarketType::Generic => 0, // caller must use Market::actual_duration_secs()
        }
    }

    pub fn slug_prefix(self) -> &'static str {
        match self {
            MarketType::FiveMinute => "5m",
            MarketType::FifteenMinute => "15m",
            MarketType::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriceSource {
    Binance,
    Coinbase,
    Chainlink,
    Polymarket,
}

// ── Market ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub token_id_up: String,
    pub token_id_down: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub market_type: MarketType,
    pub asset: String,
    pub fee_rate_bps: u32,
    pub neg_risk: bool,
}

impl Market {
    pub fn token_id(&self, outcome: Outcome) -> &str {
        match outcome {
            Outcome::Up => &self.token_id_up,
            Outcome::Down => &self.token_id_down,
        }
    }

    pub fn seconds_remaining(&self) -> i64 {
        let now = Utc::now();
        (self.end_time - now).num_seconds().max(0)
    }

    /// Total market duration from open to close.
    /// Prefers the known `MarketType` constants; falls back to
    /// `end_time – start_time` for `MarketType::Generic`.
    pub fn actual_duration_secs(&self) -> i64 {
        match self.market_type {
            MarketType::FiveMinute | MarketType::FifteenMinute => {
                self.market_type.duration_secs()
            }
            MarketType::Generic => {
                let d = (self.end_time - self.start_time).num_seconds();
                if d > 0 { d } else { 300 } // fallback 5m if times bogus
            }
        }
    }
}

// ── OrderBook ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PriceLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OrderBook {
    pub token_id: String,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: i64,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.iter().map(|l| l.price).reduce(Decimal::max)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.iter().map(|l| l.price).reduce(Decimal::min)
    }

    pub fn mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        Some((bid + ask) / Decimal::TWO)
    }

    pub fn spread(&self) -> Option<Decimal> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        Some(ask - bid)
    }
}

// ── Orders ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub outcome: Outcome,
    pub fee_rate_bps: u32,
    pub post_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: String,
    pub status: String,
    pub price: Decimal,
    pub size: Decimal,
    pub side: Side,
    pub token_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveOrder {
    pub order_id: String,
    pub token_id: String,
    pub outcome: Outcome,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub filled: Decimal,
    pub created_at: DateTime<Utc>,
}

// ── Price ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcPrice {
    pub price: Decimal,
    pub timestamp: i64,
    pub source: PriceSource,
}

// ── Data Events ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DataEvent {
    PriceUpdate(BtcPrice),
    OrderBookUpdate {
        token_id: String,
        book: OrderBook,
    },
    MarketResolved {
        condition_id: String,
        winning_outcome: Outcome,
    },
}

// ── Bot State ─────────────────────────────────────────────────────────────────

pub struct BotState {
    /// BTC spot price stored as price * 1_000_000 (micro-cents)
    btc_price: AtomicI64,
    /// Window opening BTC price
    window_open_price: AtomicI64,
    /// UP inventory stored as shares * 100
    inventory_up: AtomicI64,
    /// DOWN inventory stored as shares * 100
    inventory_down: AtomicI64,
    /// Daily PnL in micro-USDC
    daily_pnl: AtomicI64,
    /// Consecutive losses
    consecutive_losses: AtomicU64,
    /// Bot paused flag
    paused: AtomicBool,
    /// Active orders tracked by order_id
    pub active_orders: DashMap<String, ActiveOrder>,
    /// Live orderbooks tracked by token_id
    pub order_books: DashMap<String, OrderBook>,
    /// Current active markets keyed by market-type string ("5m", "15m", "generic").
    /// Each market worker inserts/removes its entry independently.
    pub current_markets: DashMap<String, Market>,
}

impl BotState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            btc_price: AtomicI64::new(0),
            window_open_price: AtomicI64::new(0),
            inventory_up: AtomicI64::new(0),
            inventory_down: AtomicI64::new(0),
            daily_pnl: AtomicI64::new(0),
            consecutive_losses: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            active_orders: DashMap::new(),
            order_books: DashMap::new(),
            current_markets: DashMap::new(),
        })
    }

    pub fn set_btc_price(&self, price: Decimal) {
        let micros = (price * Decimal::new(1_000_000, 0))
            .to_i64()
            .unwrap_or(0);
        self.btc_price.store(micros, Ordering::Relaxed);
    }

    pub fn get_btc_price(&self) -> Decimal {
        let micros = self.btc_price.load(Ordering::Relaxed);
        Decimal::new(micros, 6)
    }

    pub fn set_window_open_price(&self, price: Decimal) {
        let micros = (price * Decimal::new(1_000_000, 0))
            .to_i64()
            .unwrap_or(0);
        self.window_open_price.store(micros, Ordering::Relaxed);
    }

    pub fn get_window_open_price(&self) -> Decimal {
        let micros = self.window_open_price.load(Ordering::Relaxed);
        Decimal::new(micros, 6)
    }

    pub fn add_inventory(&self, outcome: Outcome, shares: Decimal) {
        let delta = (shares * Decimal::new(100, 0)).to_i64().unwrap_or(0);
        match outcome {
            Outcome::Up => {
                self.inventory_up.fetch_add(delta, Ordering::Relaxed);
            }
            Outcome::Down => {
                self.inventory_down.fetch_add(delta, Ordering::Relaxed);
            }
        }
    }

    pub fn get_inventory(&self, outcome: Outcome) -> Decimal {
        let raw = match outcome {
            Outcome::Up => self.inventory_up.load(Ordering::Relaxed),
            Outcome::Down => self.inventory_down.load(Ordering::Relaxed),
        };
        Decimal::new(raw, 2)
    }

    pub fn inventory_ratio(&self) -> Decimal {
        let up = self.get_inventory(Outcome::Up);
        let down = self.get_inventory(Outcome::Down);
        let total = up + down;
        if total.is_zero() {
            Decimal::new(5, 1) // 0.5
        } else {
            up / total
        }
    }

    pub fn add_daily_pnl(&self, pnl_usdc: Decimal) {
        let micro = (pnl_usdc * Decimal::new(1_000_000, 0))
            .to_i64()
            .unwrap_or(0);
        self.daily_pnl.fetch_add(micro, Ordering::Relaxed);
    }

    pub fn get_daily_pnl(&self) -> Decimal {
        let micro = self.daily_pnl.load(Ordering::Relaxed);
        Decimal::new(micro, 6)
    }

    pub fn record_win(&self) {
        self.consecutive_losses.store(0, Ordering::Relaxed);
    }

    pub fn record_loss(&self) -> u64 {
        self.consecutive_losses.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn consecutive_losses(&self) -> u64 {
        self.consecutive_losses.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn reset_daily(&self) {
        self.daily_pnl.store(0, Ordering::Relaxed);
        self.consecutive_losses.store(0, Ordering::Relaxed);
        self.resume();
    }

    pub fn reset_inventory(&self) {
        self.inventory_up.store(0, Ordering::Relaxed);
        self.inventory_down.store(0, Ordering::Relaxed);
    }
}

