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
║       Polymarket Market Making Bot  v0.1.0            ║
║       Strategy: Maker Rebate Farming (BTC 5m/15m)     ║
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
        "Strategy: {} | Assets: {:?} | Spread: +/-{:.3}",
        config.strategy.market_type,
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

    // ── Binance Feed Task ─────────────────────────────────────────────────────
    {
        let ws_url = config.binance.ws_url.clone();
        let state_c = state.clone();
        let tx_c = event_tx.clone();
        tokio::spawn(async move {
            data::run_binance_feed(ws_url, state_c, tx_c).await;
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

    // ── Strategy ──────────────────────────────────────────────────────────────
    let event_rx = event_tx.subscribe();
    let mut strategy = MarketMakingStrategy::new(
        config.clone(),
        state.clone(),
        execution.clone(),
        risk.clone(),
        discovery.clone(),
    );

    if let Err(e) = strategy.run(event_rx).await {
        error!("Strategy error: {}", e);
        let n = execution.cancel_all_orders().await;
        info!("Emergency cancelled {} orders", n);
        return Err(e);
    }

    Ok(())
}
