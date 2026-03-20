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
    primitives::{Address, Signature as PrimSig, U256},
    signers::{local::PrivateKeySigner, Signer},
    sol,
    sol_types::{eip712_domain, SolStruct},
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
) -> Result<(String, String, u64)> {
    let local_signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|_| anyhow::anyhow!(
            "Invalid POLY_PRIVATE_KEY — must be a 64-hex-char Ethereum private key.\n  \
             Obtain your key at https://polymarket.com/profile?tab=api-keys"
        ))?;

    let signer_addr = local_signer.address();

    // For EOA (sig_type 0) the CTF Exchange contract requires maker == signer.
    // For POLY_PROXY / GnosisSafe (sig_type 1/2): maker = proxy/safe wallet.
    let maker: Address = if sig_type == 0 {
        signer_addr
    } else {
        maker_address.parse().map_err(|_| {
            anyhow::anyhow!("Invalid POLY_FUNDER_ADDRESS: '{}'", maker_address)
        })?
    };

    // For POLY_PROXY / GnosisSafe (sig_type 1/2): the `signer` field in the EIP-712
    // struct must be the proxy/safe address (same as maker), NOT the EOA.
    // The CTF Exchange contract verifies the signature via isValidSignature() on that
    // contract, which internally checks the EOA signature. The API key is also
    // registered under the proxy address, so signer must match it.
    // For EOA (sig_type 0): signer == maker == the EOA address.
    let order_signer = maker;

    let token_id_u256: U256 = token_id.parse().map_err(|_| {
        anyhow::anyhow!("Invalid token_id (expected decimal integer): '{}'", token_id)
    })?;

    // Salt: millisecond timestamp — unique per call and within JS MAX_SAFE_INTEGER
    // (nanoseconds ~1.7e18 exceed 2^53 ≈ 9e15 and lose precision in JSON numbers)
    let salt_u64 = chrono::Utc::now().timestamp_millis() as u64;
    let salt = U256::from(salt_u64);

    let order = Order {
        salt,
        maker,
        signer: order_signer,
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

    // Offline recovery check: verify that the EIP-712 hash + signature recovers
    // back to the expected signer.  This catches domain/struct mismatches before
    // the order reaches the server (server returns the same "Invalid order payload"
    // for both bad JSON format AND bad signature, making server errors hard to debug).
    let signing_hash = order.eip712_signing_hash(&domain);
    let prim_sig = PrimSig::try_from(&sig_bytes[..])
        .map_err(|e| anyhow::anyhow!("Signature parse failed: {}", e))?;
    let recovered = prim_sig
        .recover_address_from_prehash(&signing_hash)
        .map_err(|e| anyhow::anyhow!("Signature recovery failed: {}", e))?;
    if recovered != signer_addr {
        return Err(anyhow::anyhow!(
            "EIP-712 signature self-check FAILED: recovered={:?} expected={:?}\n  \
             contract={:?} neg_risk={} chain_id={}\n  \
             This means the Order struct fields or EIP-712 domain do not match \
             what Polymarket's contract expects.",
            recovered,
            signer_addr,
            verifying_contract,
            neg_risk,
            POLYGON_CHAIN_ID,
        ));
    }
    debug!(
        "EIP-712 sign OK: signer={:?} contract={:?} neg_risk={} hash=0x{}",
        signer_addr,
        verifying_contract,
        neg_risk,
        hex::encode(signing_hash)
    );

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
    // Return order_signer (proxy wallet for sig_type != 0, EOA for sig_type 0)
    // This is the address that goes into both the EIP-712 struct and the JSON "signer" field.
    let signer_hex = format!("{:?}", order_signer);

    Ok((sig_hex, signer_hex, salt_u64))
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

    Ok(URL_SAFE_NO_PAD.encode(result))
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
        ("POLY_API_KEY".into(), creds.api_key.clone()),
        ("POLY_SIGNATURE".into(), sig),
        ("POLY_TIMESTAMP".into(), timestamp.to_string()),
        ("POLY_PASSPHRASE".into(), creds.api_passphrase.clone()),
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
/// neg_risk SELL is special: you sell shares of one outcome and receive USDC
/// priced at the complement — taker_amount = size * (1 - price).
/// (Mirrors py-clob-client `build_limit_order_amounts` with is_neg_risk=True.)
pub fn calculate_amounts(
    price: Decimal,
    size: Decimal,
    side: &crate::types::Side,
    neg_risk: bool,
) -> (u128, u128) {
    let share_raw = (size * Decimal::new(1_000_000, 0))
        .round()
        .abs();

    use crate::types::Side;
    match side {
        Side::Buy => {
            let usdc_raw = (price * size * Decimal::new(1_000_000, 0))
                .round()
                .abs();
            let maker = usdc_raw.try_into().unwrap_or(0u128);
            let taker = share_raw.try_into().unwrap_or(0u128);
            (maker, taker)
        }
        Side::Sell => {
            // neg_risk SELL: taker receives (1-price)*size USDC (complement pricing)
            let sell_price = if neg_risk { dec!(1) - price } else { price };
            let usdc_raw = (sell_price * size * Decimal::new(1_000_000, 0))
                .round()
                .abs();
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
    fn test_calculate_amounts_sell_regular() {
        use crate::types::Side;
        // Regular SELL at price 0.45, size 10: taker = 0.45*10*1e6 = 4_500_000 USDC
        let (maker, taker) = calculate_amounts(dec!(0.45), dec!(10.0), &Side::Sell, false);
        assert_eq!(maker, 10_000_000); // shares
        assert_eq!(taker, 4_500_000);  // USDC = price * size
    }

    #[test]
    fn test_calculate_amounts_sell_neg_risk() {
        use crate::types::Side;
        // neg_risk SELL at price 0.45, size 10: taker = (1-0.45)*10*1e6 = 5_500_000 USDC
        let (maker, taker) = calculate_amounts(dec!(0.45), dec!(10.0), &Side::Sell, true);
        assert_eq!(maker, 10_000_000); // shares
        assert_eq!(taker, 5_500_000);  // USDC = (1 - price) * size
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
