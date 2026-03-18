//! API key lifecycle management for Polymarket CLOB.
//!
//! On startup the bot calls [`ensure_valid_credentials`] which:
//! 1. Reads existing credentials from env vars / `.env`
//! 2. Validates them with a lightweight `GET /auth/api-keys` (L2 auth)
//! 3. If invalid **or** missing, derives fresh credentials via
//!    `GET /auth/derive-api-key` (deterministic — same key every time)
//! 4. Falls back to `POST /auth/api-key` (creates a new random key) if
//!    derivation also fails
//! 5. Writes the new credentials back to `.env` so they persist across restarts
//!
//! # L1 Authentication (used for derive / create)
//!
//! Polymarket uses EIP-712 typed-data signatures over a [`ClobAuth`] struct:
//! ```text
//! Domain: { name: "ClobAuthDomain", version: "1", chainId: 137 }
//! Struct: ClobAuth {
//!   string address    — wallet address (funder)
//!   string timestamp  — Unix timestamp as a string
//!   uint256 nonce     — 0 for derivation
//!   string message    — "This message attests that I control the given wallet"
//! }
//! ```
//!
//! Reference: <https://docs.polymarket.com/developers/CLOB/authentication>

use anyhow::{bail, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

use alloy::{
    primitives::{Address, U256},
    signers::{local::PrivateKeySigner, Signer},
    sol,
    sol_types::eip712_domain,
};

use crate::config::BotConfig;
use crate::execution::signing::{build_l2_headers, ClobCredentials};

// ── EIP-712 ClobAuth struct ───────────────────────────────────────────────────
//
// Field NAMES and TYPES must match the Polymarket contract exactly.
// Note: `address` here is Solidity `string`, not the `address` primitive —
// the wallet address is passed as a hex string ("0x..."), not as bytes.

sol! {
    #[derive(Debug)]
    struct ClobAuth {
        string address;
        string timestamp;
        uint256 nonce;
        string message;
    }
}

const CLOB_AUTH_MESSAGE: &str =
    "This message attests that I control the given wallet";
const AUTH_DOMAIN_NAME: &str = "ClobAuthDomain";
const AUTH_DOMAIN_VERSION: &str = "1";
const CHAIN_ID: u64 = 137;

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ApiKeyResponse {
    #[serde(alias = "apiKey", alias = "api_key")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

#[derive(Debug, Deserialize)]
struct ApiKeysListResponse {
    // GET /auth/api-keys returns an array at the root or in a "keys" field
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
}

// ── L1 header builder ────────────────────────────────────────────────────────

/// Build the four L1 auth headers required by Polymarket's key management
/// endpoints.  The EIP-712 signature proves wallet ownership without
/// requiring any pre-existing API credentials.
async fn build_l1_headers(
    private_key: &str,
    nonce: u64,
) -> Result<Vec<(String, String)>> {
    let local_signer: PrivateKeySigner = private_key.parse().map_err(|_| {
        anyhow::anyhow!(
            "Invalid POLY_PRIVATE_KEY — must be a 64-hex-char Ethereum private key"
        )
    })?;

    // L1 auth must use the signer's own EOA, not the proxy/funder address.
    let signer_address = format!("{:?}", local_signer.address());

    let timestamp = chrono::Utc::now().timestamp().to_string();

    let auth_struct = ClobAuth {
        address: signer_address.clone(),
        timestamp: timestamp.clone(),
        nonce: U256::from(nonce),
        message: CLOB_AUTH_MESSAGE.to_string(),
    };

    // Domain has no verifyingContract for auth (only name + version + chainId)
    let domain = eip712_domain! {
        name: AUTH_DOMAIN_NAME,
        version: AUTH_DOMAIN_VERSION,
        chain_id: CHAIN_ID,
    };

    let sig = local_signer
        .sign_typed_data(&auth_struct, &domain)
        .await
        .map_err(|e| anyhow::anyhow!("L1 auth signing failed: {}", e))?;

    let sig_bytes = sig.as_bytes();

    // alloy returns recovery id v = 0 or 1; ecrecover expects v = 27 or 28.
    let mut adjusted = sig_bytes;
    if adjusted[64] < 27 {
        adjusted[64] += 27;
    }
    let sig_hex = format!("0x{}", hex::encode(adjusted));

    Ok(vec![
        ("POLY_ADDRESS".into(), signer_address),
        ("POLY_SIGNATURE".into(), sig_hex),
        ("POLY_TIMESTAMP".into(), timestamp),
        ("POLY_NONCE".into(), nonce.to_string()),
    ])
}

// ── Core functions ────────────────────────────────────────────────────────────

/// Check whether the credentials in `creds` are currently valid by calling
/// `GET /auth/api-keys` with L2 auth headers.
///
/// Returns `true` if the API returns HTTP 200, `false` otherwise (401/403/network error).
async fn is_credential_valid(
    client: &Client,
    clob_url: &str,
    creds: &ClobCredentials,
) -> bool {
    if creds.api_key.is_empty() || creds.api_secret.is_empty() {
        debug!("API credentials are empty — skipping validation request");
        return false;
    }

    let path = "/auth/api-keys";
    let headers = match build_l2_headers(creds, "GET", path, "") {
        Ok(h) => h,
        Err(e) => {
            debug!("Could not build L2 headers for validation: {}", e);
            return false;
        }
    };

    let url = format!("{}{}", clob_url, path);
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            debug!("GET /auth/api-keys → HTTP {}", status);
            status.is_success()
        }
        Err(e) => {
            debug!("GET /auth/api-keys network error: {}", e);
            false
        }
    }
}

/// Call `GET /auth/derive-api-key` with L1 auth headers to obtain the
/// deterministic API key for the given wallet.  Returns the same key on
/// every call — safe to call any number of times.
async fn derive_api_key(
    client: &Client,
    clob_url: &str,
    private_key: &str,
) -> Result<ApiKeyResponse> {
    let headers = build_l1_headers(private_key, 0).await?;

    let url = format!("{}/auth/derive-api-key", clob_url);
    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "GET /auth/derive-api-key returned HTTP {}: {}",
            status,
            &body[..body.len().min(200)]
        );
    }

    let v: serde_json::Value = resp.json().await?;
    parse_api_key_response(&v)
}

/// Call `POST /auth/api-key` with L1 auth headers to create a **new** random
/// API key.  Use as fallback when derivation fails.
async fn create_api_key(
    client: &Client,
    clob_url: &str,
    private_key: &str,
    nonce: u64,
) -> Result<ApiKeyResponse> {
    let headers = build_l1_headers(private_key, nonce).await?;

    let url = format!("{}/auth/api-key", clob_url);
    let mut req = client.post(&url).header("Content-Type", "application/json");
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "POST /auth/api-key returned HTTP {}: {}",
            status,
            &body[..body.len().min(200)]
        );
    }

    let v: serde_json::Value = resp.json().await?;
    parse_api_key_response(&v)
}

fn parse_api_key_response(v: &serde_json::Value) -> Result<ApiKeyResponse> {
    let api_key = v["apiKey"]
        .as_str()
        .or_else(|| v["api_key"].as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing apiKey in auth response: {}", v))?
        .to_string();
    let secret = v["secret"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing secret in auth response"))?
        .to_string();
    let passphrase = v["passphrase"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing passphrase in auth response"))?
        .to_string();
    Ok(ApiKeyResponse {
        api_key,
        secret,
        passphrase,
    })
}

/// Persist fresh credentials to `.env` so they survive process restarts.
///
/// If `.env` already exists the relevant lines are replaced in-place.
/// If it does not exist it is created with only the three key lines.
fn write_credentials_to_env(creds: &ApiKeyResponse, env_path: &str) -> Result<()> {
    let path = Path::new(env_path);

    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let keys = [
        ("POLY_API_KEY", creds.api_key.as_str()),
        ("POLY_API_SECRET", creds.secret.as_str()),
        ("POLY_API_PASSPHRASE", creds.passphrase.as_str()),
    ];

    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            let trimmed = l.trim_start();
            !keys
                .iter()
                .any(|(k, _)| trimmed.starts_with(&format!("{}=", k)))
        })
        .map(String::from)
        .collect();

    for (k, v) in &keys {
        lines.push(format!("{}={}", k, v));
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    info!("Credentials written to '{}'", env_path);
    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Validate existing API credentials and regenerate them if invalid.
///
/// Call once at startup before the market-making loop begins.
///
/// # Behaviour
/// 1. Read `POLY_API_KEY / _SECRET / _PASSPHRASE` from the environment.
/// 2. Call `GET /auth/api-keys` with L2 HMAC headers.
///    - If HTTP 200 → credentials are valid, return early.
/// 3. Attempt `GET /auth/derive-api-key` with L1 EIP-712 signature.
///    - If successful → update env vars in process + write to `.env`.
/// 4. If derivation fails, attempt `POST /auth/api-key` (new random key).
///    - If successful → update env vars in process + write to `.env`.
/// 5. If all attempts fail, log a warning and continue (bot will get 401s
///    on order placement, but market discovery and price feeds still work).
pub async fn ensure_valid_credentials(config: &BotConfig, env_path: &str) {
    // Skip entirely in simulation mode — no credentials are needed
    if config.bot.simulation {
        debug!("Simulation mode — skipping API key validation");
        return;
    }

    // Escape hatch: set POLY_SKIP_L1_AUTH=true to skip auto-regen entirely.
    // Use this when you have set POLY_API_KEY/SECRET/PASSPHRASE manually from
    // polymarket.com/profile → API Keys, and L1 auth is not expected to work
    // (e.g. Magic Link account where the exported key doesn't match the proxy).
    if std::env::var("POLY_SKIP_L1_AUTH")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
    {
        info!("POLY_SKIP_L1_AUTH=true — skipping automatic key derivation, using .env credentials as-is");
        return;
    }

    let private_key = &config.polymarket.private_key;
    let funder_address = &config.polymarket.funder_address;

    if private_key.is_empty() || funder_address.is_empty() {
        warn!(
            "POLY_PRIVATE_KEY or POLY_FUNDER_ADDRESS not set — \
             cannot validate or regenerate API credentials.\n  \
             Set them in your .env file to enable live trading.\n  \
             See: https://docs.polymarket.com/developers/CLOB/authentication"
        );
        return;
    }

    // Derive and log the signer address so mismatches are immediately visible.
    let signer_addr_display = match private_key.parse::<PrivateKeySigner>() {
        Ok(s) => format!("{:?}", s.address()),
        Err(_) => "<invalid key>".into(),
    };
    info!(
        "L1 auth — signer EOA: {}  proxy (funder): {}  sig_type: {}",
        signer_addr_display, funder_address, config.polymarket.signature_type
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP client build");

    let clob_url = &config.polymarket.clob_api_url;
    let creds = ClobCredentials::from_env().unwrap_or_default();

    // ── Step 1: validate existing credentials ────────────────────────────────
    if is_credential_valid(&client, clob_url, &creds).await {
        info!("API credentials are valid (key={}…)", &creds.api_key[..creds.api_key.len().min(8)]);
        return;
    }

    warn!(
        "API credentials are missing or invalid — attempting to {} credentials …",
        if creds.api_key.is_empty() { "create" } else { "regenerate" }
    );

    // ── Step 2: derive (deterministic) ───────────────────────────────────────
    match derive_api_key(&client, clob_url, private_key).await {
        Ok(new_creds) => {
            info!(
                "Derived API key successfully (key={}…)",
                &new_creds.api_key[..new_creds.api_key.len().min(8)]
            );
            apply_credentials(&new_creds);
            if let Err(e) = write_credentials_to_env(&new_creds, env_path) {
                warn!("Could not write credentials to '{}': {}", env_path, e);
            }
            return;
        }
        Err(e) => {
            let hint = if e.to_string().contains("401") {
                // 401 on L1 auth almost always means the private key is not
                // registered with Polymarket as controlling the given proxy.
                // For Magic Link (POLY_PROXY) accounts this happens when the
                // exported key is not the original Magic Link session key.
                format!(
                    "\n  Signer EOA derived from POLY_PRIVATE_KEY : {}\n  \
                     Proxy wallet (POLY_FUNDER_ADDRESS)         : {}\n  \
                     \n  \
                     Polymarket rejected the signature — the private key is not\n  \
                     registered as a signer for the proxy wallet.\n  \
                     \n  \
                     FIX — choose one:\n  \
                     A) Re-export your key from polymarket.com → Profile → Export Key.\n  \
                        The correct key derives to the proxy address shown there.\n  \
                     B) Copy your existing API key from polymarket.com/profile → API Keys\n  \
                        and paste POLY_API_KEY, POLY_API_SECRET, POLY_API_PASSPHRASE into .env,\n  \
                        then add POLY_SKIP_L1_AUTH=true to skip this auto-regen step.",
                    signer_addr_display, funder_address
                )
            } else {
                String::new()
            };
            warn!("Credential derivation failed: {}{} — trying creation …", e, hint);
        }
    }

    // ── Step 3: create new random key ────────────────────────────────────────
    let nonce = chrono::Utc::now().timestamp() as u64;
    match create_api_key(&client, clob_url, private_key, nonce).await {
        Ok(new_creds) => {
            info!(
                "Created new API key successfully (key={}…)",
                &new_creds.api_key[..new_creds.api_key.len().min(8)]
            );
            apply_credentials(&new_creds);
            if let Err(e) = write_credentials_to_env(&new_creds, env_path) {
                warn!("Could not write credentials to '{}': {}", env_path, e);
            }
        }
        Err(e) => {
            warn!(
                "Failed to create API key: {}\n  \
                 Bot will continue but orders will fail with 401 Unauthorized.\n  \
                 \n  \
                 To fix: manually set the three lines below in your .env file,\n  \
                 then add POLY_SKIP_L1_AUTH=true to skip this auto-regen step:\n  \
                 \n  \
                 POLY_API_KEY=<key>\n  \
                 POLY_API_SECRET=<secret>\n  \
                 POLY_API_PASSPHRASE=<passphrase>\n  \
                 POLY_SKIP_L1_AUTH=true\n  \
                 \n  \
                 Obtain the values at: https://polymarket.com/profile?tab=api-keys",
                e
            );
        }
    }
}

/// Set the three credential env vars in the current process so the execution
/// engine (which calls `ClobCredentials::from_env()`) picks them up without
/// a restart.
fn apply_credentials(creds: &ApiKeyResponse) {
    std::env::set_var("POLY_API_KEY", &creds.api_key);
    std::env::set_var("POLY_API_SECRET", &creds.secret);
    std::env::set_var("POLY_API_PASSPHRASE", &creds.passphrase);
}
