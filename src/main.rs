mod config;
mod data;
mod execution;
mod market_discovery;
mod monitoring;
mod risk;
mod strategy;
mod types;

use anyhow::Result;
use config::BotConfig;
use execution::ExecutionEngine;
use market_discovery::MarketDiscovery;
use monitoring::monitoring_loop;
use risk::RiskEngine;
use std::sync::Arc;
use strategy::MarketMakingStrategy;
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use types::{BotState, DataEvent};

const BANNER: &str = r#"
╔═══════════════════════════════════════════════════════╗
║       Polymarket Market Making Bot  v0.4.3            ║
║       Strategy: Maker Rebate Farming (BTC Up/Down)    ║
╚═══════════════════════════════════════════════════════╝
"#;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Config ────────────────────────────────────────────────────────────────
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let config = BotConfig::load(&config_path)?;

    // ── Logging ───────────────────────────────────────────────────────────────
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.bot.log_level));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false),
        )
        .with(filter)
        .init();

    println!("{}", BANNER);
    info!("Config loaded from: {}", config_path);
    info!(
        "Mode: {}",
        if config.bot.simulation {
            "SIMULATION"
        } else {
            "LIVE TRADING"
        }
    );
    info!(
        "Market workers: {:?} | Assets: {:?} | Spread: +/-{:.3}",
        config.strategy.market_types,
        config.strategy.assets,
        config.strategy.half_spread
    );
    info!(
        "Risk: bankroll=${:.0} | max_exp={:.0}% | loss_limit={:.0}%",
        config.risk.bankroll,
        config.risk.max_exposure_pct * 100.0,
        config.risk.daily_loss_limit_pct * 100.0
    );

    // ── Core Infrastructure ───────────────────────────────────────────────────
    let state = BotState::new();
    let (event_tx, _) = broadcast::channel::<DataEvent>(1024);

    let risk = Arc::new(RiskEngine::new(config.risk.clone(), state.clone()));
    let discovery = Arc::new(MarketDiscovery::new(config.clone())?);
    let execution = Arc::new(ExecutionEngine::new(config.clone(), state.clone())?);

    // ── BTC Price Feed (Binance WS + Coinbase REST fallback) ──────────────────
    {
        let ws_url = config.binance.ws_url.clone();
        let coinbase_cfg = config.coinbase.clone();
        let state_c = state.clone();
        let tx_c = event_tx.clone();
        info!(
            "Price feed: Binance WS primary | Coinbase REST fallback after {} failures ({})",
            coinbase_cfg.max_binance_failures,
            if coinbase_cfg.enabled { "enabled" } else { "disabled" },
        );
        tokio::spawn(async move {
            data::run_btc_price_feed(ws_url, coinbase_cfg, state_c, tx_c).await;
        });
    }

    // ── Monitoring Task ───────────────────────────────────────────────────────
    {
        let state_c = state.clone();
        let risk_c = risk.clone();
        tokio::spawn(async move {
            monitoring_loop(state_c, risk_c).await;
        });
    }

    // ── Ctrl+C Handler ────────────────────────────────────────────────────────
    {
        let execution_c = execution.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                warn!("Ctrl+C received -- cancelling all orders...");
                let n = execution_c.cancel_all_orders().await;
                info!("Cancelled {} orders, exiting.", n);
                std::process::exit(0);
            }
        });
    }

    // ── Market-Making Workers (one per market type) ───────────────────────────
    // Each worker independently discovers and trades its assigned market type.
    // They share the same price feed, risk engine, and execution engine.
    let market_types = config.strategy.market_types.clone();
    info!("Spawning {} market worker(s): {:?}", market_types.len(), market_types);

    let mut handles = Vec::new();

    for market_type_str in market_types {
        let config_c = config.clone();
        let state_c = state.clone();
        let execution_c = execution.clone();
        let risk_c = risk.clone();
        let discovery_c = discovery.clone();
        let event_rx = event_tx.subscribe();
        let mt = market_type_str.clone();

        let handle = tokio::spawn(async move {
            let mut strategy = MarketMakingStrategy::new(
                mt.clone(),
                config_c,
                state_c,
                execution_c,
                risk_c,
                discovery_c.clone(),
            );
            if let Err(e) = strategy.run(event_rx, discovery_c).await {
                error!("[{}] Worker error: {}", mt, e);
                Err(e)
            } else {
                Ok(())
            }
        });

        handles.push(handle);
    }

    // Wait for all workers — they loop indefinitely so only exit on error.
    // Ctrl+C handler above will call process::exit(0) before this.
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("Worker exited with error: {}", e);
            }
            Err(e) => {
                error!("Worker task panicked: {}", e);
            }
        }
    }

    let n = execution.cancel_all_orders().await;
    info!("Emergency cancelled {} orders, all workers exited", n);
    Ok(())
}
