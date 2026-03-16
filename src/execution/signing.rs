use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sha2::Sha256;
use std::str::FromStr;
use tracing::debug;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const POLYGON_CHAIN_ID: u64 = 137;
/// CTF Exchange (ERC1155 conditional token framework)
pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
/// Neg-Risk CTF Exchange
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
/// Polymarket Gnosis Safe factory
pub const SAFE_FACTORY: &str = "0xa6B71E26C5e0845f74c812102Ca7114b6a896AB2";

// ── Credentials ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ClobCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
    pub address: String,
}

impl ClobCredentials {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            api_key: std::env::var("POLY_API_KEY").unwrap_or_default(),
            api_secret: std::env::var("POLY_API_SECRET").unwrap_or_default(),
            api_passphrase: std::env::var("POLY_API_PASSPHRASE").unwrap_or_default(),
            address: std::env::var("POLY_FUNDER_ADDRESS").unwrap_or_default(),
        })
    }
}

// ── HMAC Auth ─────────────────────────────────────────────────────────────────

/// Build HMAC-SHA256 signature for L2 Polymarket API auth.
/// secret is base64-encoded; decoded then used as HMAC key.
pub fn build_hmac_signature(
    secret: &str,
    timestamp: i64,
    method: &str,
    path: &str,
    body: &str,
) -> Result<String> {
    let key_bytes = B64.decode(secret)?;
    let message = format!("{}{}{}{}", timestamp, method, path, body);

    let mut mac = Hmac::<Sha256>::new_from_slice(&key_bytes)?;
    mac.update(message.as_bytes());
    let result = mac.finalize().into_bytes();

    Ok(B64.encode(result))
}

/// Build L2 auth headers for a CLOB API request.
pub fn build_l2_headers(
    creds: &ClobCredentials,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Vec<(String, String)>> {
    let timestamp = chrono::Utc::now().timestamp();
    let sig = build_hmac_signature(&creds.api_secret, timestamp, method, path, body)?;

    Ok(vec![
        ("POLY-API-KEY".into(), creds.api_key.clone()),
        ("POLY-SIGNATURE".into(), sig),
        ("POLY-TIMESTAMP".into(), timestamp.to_string()),
        ("POLY-PASSPHRASE".into(), creds.api_passphrase.clone()),
    ])
}

// ── Price Normalization ────────────────────────────────────────────────────────

/// Round price to nearest tick_size and format as "0.XX".
pub fn normalize_price(price: Decimal, tick_size: Decimal) -> String {
    if tick_size.is_zero() {
        return format!("{:.4}", price);
    }
    let rounded = (price / tick_size).round() * tick_size;
    let clamped = rounded.max(dec!(0.01)).min(dec!(0.99));
    format!("{:.4}", clamped)
}

// ── Amount Calculation ────────────────────────────────────────────────────────

/// Calculate maker_amount and taker_amount in USDC 6-decimal raw units.
/// For a BUY:  maker_amount = size * price (USDC), taker_amount = size (shares)
/// For a SELL: maker_amount = size (shares), taker_amount = size * price (USDC)
///
/// neg_risk markets invert the token relationship.
pub fn calculate_amounts(
    price: Decimal,
    size: Decimal,
    side: &crate::types::Side,
    neg_risk: bool,
) -> (u128, u128) {
    let usdc_amount = price * size;
    // Convert to 6-decimal units
    let usdc_raw = (usdc_amount * Decimal::new(1_000_000, 0))
        .round()
        .abs();
    let share_raw = (size * Decimal::new(1_000_000, 0))
        .round()
        .abs();

    use crate::types::Side;
    match side {
        Side::Buy => {
            let maker = usdc_raw.try_into().unwrap_or(0u128);
            let taker = share_raw.try_into().unwrap_or(0u128);
            (maker, taker)
        }
        Side::Sell => {
            let maker = share_raw.try_into().unwrap_or(0u128);
            let taker = usdc_raw.try_into().unwrap_or(0u128);
            (maker, taker)
        }
    }
}

// ── Fee Calculation ───────────────────────────────────────────────────────────

/// Taker fee = base_rate_bps * min(price, 1-price) * size
/// base_rate_bps e.g. 315 means 3.15%
pub fn calculate_taker_fee(
    base_rate_bps: u32,
    price: Decimal,
    size: Decimal,
) -> Decimal {
    let rate = Decimal::new(base_rate_bps as i64, 4); // e.g. 315 → 0.0315
    let complement = dec!(1) - price;
    let min_price = price.min(complement);
    rate * min_price * size
}

/// Maker rebate = 25% of taker fee
pub fn estimate_maker_rebate(taker_fee: Decimal) -> Decimal {
    taker_fee * dec!(0.25)
}

// ── EIP-712 Order Hash (Placeholder) ─────────────────────────────────────────

/// Placeholder struct for a signable order.
/// In production, use the official `polymarket_client_sdk` or `clob-client-rust`.
pub struct SignableOrder {
    pub token_id: String,
    pub price: Decimal,
    pub size: Decimal,
    pub side: crate::types::Side,
    pub fee_rate_bps: u32,
    pub nonce: String,
    pub expiration: String,
    pub maker: String,
    pub taker: String,
    pub neg_risk: bool,
}

/// Compute EIP-712 struct hash.
/// NOTE: This is a placeholder. For production use, integrate the official
/// Polymarket Rust SDK which handles proper ABI encoding and domain separation.
pub fn compute_order_hash(order: &SignableOrder) -> String {
    // Placeholder: return a deterministic string for simulation
    format!(
        "0x{:064x}",
        u64::from_str_radix(&order.nonce[..8.min(order.nonce.len())], 16).unwrap_or(0)
    )
}

/// Derive Gnosis Safe address for a given EOA via CREATE2.
/// NOTE: Placeholder — use official SDK for production.
pub fn derive_safe_address(eoa: &str) -> String {
    format!("safe-for-{}", eoa)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_normalize_price() {
        assert_eq!(normalize_price(dec!(0.4567), dec!(0.01)), "0.4600");
        assert_eq!(normalize_price(dec!(0.001), dec!(0.01)), "0.0100");
        assert_eq!(normalize_price(dec!(0.999), dec!(0.01)), "0.9900");
    }

    #[test]
    fn test_calculate_taker_fee() {
        // At 0.5 odds, 3.15% rate, 10 shares
        let fee = calculate_taker_fee(315, dec!(0.50), dec!(10.0));
        // 0.0315 * 0.5 * 10 = 0.1575
        assert_eq!(fee, dec!(0.1575));
    }

    #[test]
    fn test_calculate_taker_fee_asymmetric() {
        // At 0.45 price, min(0.45, 0.55) = 0.45
        let fee = calculate_taker_fee(315, dec!(0.45), dec!(10.0));
        // 0.0315 * 0.45 * 10 = 0.14175
        assert_eq!(fee, dec!(0.14175));
    }

    #[test]
    fn test_maker_rebate() {
        let rebate = estimate_maker_rebate(dec!(0.1575));
        assert_eq!(rebate, dec!(0.039375));
    }

    #[test]
    fn test_calculate_amounts_buy() {
        use crate::types::Side;
        let (maker, taker) = calculate_amounts(dec!(0.45), dec!(10.0), &Side::Buy, false);
        // USDC: 0.45 * 10 * 1_000_000 = 4_500_000
        assert_eq!(maker, 4_500_000);
        // Shares: 10 * 1_000_000 = 10_000_000
        assert_eq!(taker, 10_000_000);
    }

    #[test]
    fn test_hmac_signature() {
        // base64-encode "secret" and use as key
        let secret = B64.encode(b"testsecret");
        let sig = build_hmac_signature(&secret, 1710000000, "GET", "/order", "");
        assert!(sig.is_ok());
        assert!(!sig.unwrap().is_empty());
    }
}
