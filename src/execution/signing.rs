use anyhow::Result;
use base64::{
    engine::general_purpose::{STANDARD as B64, URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sha2::Sha256;
use tracing::debug;

// alloy EIP-712 signing
use alloy::{
    primitives::{Address, U256},
    signers::{local::PrivateKeySigner, Signer},
    sol,
    sol_types::eip712_domain,
};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const POLYGON_CHAIN_ID: u64 = 137;
/// CTF Exchange (ERC1155 conditional token framework)
pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
/// Neg-Risk CTF Exchange
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
/// Polymarket Gnosis Safe factory
pub const SAFE_FACTORY: &str = "0xa6B71E26C5e0845f74c812102Ca7114b6a896AB2";

// ── EIP-712 Order struct ──────────────────────────────────────────────────────
//
// Field names and types must match exactly what Polymarket's CTF Exchange
// contract expects; any deviation produces an invalid signature.

sol! {
    #[derive(Debug)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint8 side;
        uint8 signatureType;
    }
}

// ── EIP-712 Order Signing ─────────────────────────────────────────────────────

/// Build and sign a Polymarket CLOB limit order using EIP-712.
///
/// Returns `(signature_hex, signer_address_hex, salt_decimal_string)`.
///
/// # Parameters
/// - `private_key`    — hex-encoded Ethereum private key (with or without `0x`)
/// - `maker_address`  — POLY_FUNDER_ADDRESS (the wallet that will hold shares)
/// - `token_id`       — decimal token ID from the market (e.g. clobTokenIds[0])
/// - `maker_amount`   — USDC 6-decimal units (for BUY: price * size * 1e6)
/// - `taker_amount`   — share 6-decimal units (for BUY: size * 1e6)
/// - `side`           — 0 = BUY, 1 = SELL
/// - `fee_rate_bps`   — e.g. 1000 for 10%
/// - `sig_type`       — 0 = EOA, 1 = POLY_PROXY, 2 = POLY_GNOSIS_SAFE
/// - `neg_risk`       — true → use NegRisk CTF Exchange contract
pub async fn sign_clob_order(
    private_key: &str,
    maker_address: &str,
    token_id: &str,
    maker_amount: u128,
    taker_amount: u128,
    side: u8,
    fee_rate_bps: u32,
    sig_type: u8,
    neg_risk: bool,
) -> Result<(String, String, String)> {
    let local_signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|_| anyhow::anyhow!(
            "Invalid POLY_PRIVATE_KEY — must be a 64-hex-char Ethereum private key.\n  \
             Obtain your key at https://polymarket.com/profile?tab=api-keys"
        ))?;

    let signer_addr = local_signer.address();

    // For EOA (sig_type 0) the CTF Exchange contract requires maker == signer.
    // The EIP-712 struct hash must use the same address that appears in the JSON
    // request body, otherwise the server's signature reconstruction fails.
    // For POLY_PROXY / GnosisSafe (sig_type 1/2): maker = proxy/safe wallet.
    let maker: Address = if sig_type == 0 {
        signer_addr
    } else {
        maker_address.parse().map_err(|_| {
            anyhow::anyhow!("Invalid POLY_FUNDER_ADDRESS: '{}'", maker_address)
        })?
    };

    let token_id_u256: U256 = token_id.parse().map_err(|_| {
        anyhow::anyhow!("Invalid token_id (expected decimal integer): '{}'", token_id)
    })?;

    // Salt: lower 64-bits of nanosecond timestamp — unique per call
    let salt_u64 = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis()) as u64;
    let salt = U256::from(salt_u64);

    let order = Order {
        salt,
        maker,
        signer: signer_addr,
        taker: Address::ZERO,
        tokenId: token_id_u256,
        makerAmount: U256::from(maker_amount),
        takerAmount: U256::from(taker_amount),
        expiration: U256::ZERO,
        nonce: U256::ZERO,
        feeRateBps: U256::from(fee_rate_bps as u64),
        side,
        signatureType: sig_type,
    };

    let verifying_contract: Address = if neg_risk {
        NEG_RISK_CTF_EXCHANGE.parse()?
    } else {
        CTF_EXCHANGE.parse()?
    };

    let domain = eip712_domain! {
        name: "CTF Exchange",
        version: "1",
        chain_id: POLYGON_CHAIN_ID,
        verifying_contract: verifying_contract,
    };

    let sig = local_signer
        .sign_typed_data(&order, &domain)
        .await
        .map_err(|e| anyhow::anyhow!("EIP-712 signing failed: {}", e))?;

    let sig_bytes = sig.as_bytes();

    // alloy returns recovery id v = 0 or 1 in the last byte of `as_bytes()`.
    // Polymarket's CTF Exchange contract calls ecrecover which expects
    // v = 27 or 28 (the legacy Ethereum convention used by web3.py / ethers.js).
    // Without this adjustment the on-chain signature verification always fails,
    // causing the CLOB to return 401 Unauthorized.
    let mut adjusted = sig_bytes;
    if adjusted[64] < 27 {
        adjusted[64] += 27;
    }

    let sig_hex = format!("0x{}", hex::encode(adjusted));
    let signer_hex = format!("{:?}", signer_addr);

    Ok((sig_hex, signer_hex, salt.to_string()))
}

// ── Credentials ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
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
    // Polymarket API secrets may be standard base64, URL-safe base64, or raw strings.
    // Try each variant; fall back to raw bytes so we never hard-fail on format.
    let key_bytes = B64
        .decode(secret)
        .or_else(|_| URL_SAFE_NO_PAD.decode(secret))
        .or_else(|_| URL_SAFE.decode(secret))
        .unwrap_or_else(|_| {
            debug!(
                "POLY_API_SECRET is not valid base64 — using raw bytes as HMAC key. \
                 Verify the secret at https://polymarket.com/profile?tab=api-keys"
            );
            secret.as_bytes().to_vec()
        });
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
    _neg_risk: bool,
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
