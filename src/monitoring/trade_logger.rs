use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use tracing::warn;

pub struct TradeLogger {
    orders_writer: BufWriter<File>,
    settlements_writer: BufWriter<File>,
    cumulative_pnl: Decimal,
}

impl TradeLogger {
    pub fn new(output_dir: &str) -> Result<Self> {
        fs::create_dir_all(output_dir)?;

        let date = Utc::now().format("%Y-%m-%d");
        let orders_path = PathBuf::from(output_dir).join(format!("orders_{}.csv", date));
        let settlements_path =
            PathBuf::from(output_dir).join(format!("settlements_{}.csv", date));

        let orders_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&orders_path)?;

        let settlements_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&settlements_path)?;

        let mut orders_writer = BufWriter::new(orders_file);
        let mut settlements_writer = BufWriter::new(settlements_file);

        // Write headers if new files
        if orders_path.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            writeln!(
                orders_writer,
                "timestamp,order_id,slug,token_id,outcome,side,price,size,status"
            )?;
        }

        if settlements_path
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
            == 0
        {
            writeln!(
                settlements_writer,
                "timestamp,slug,winning_outcome,inv_up,inv_down,pnl,cumulative_pnl"
            )?;
        }

        Ok(Self {
            orders_writer,
            settlements_writer,
            cumulative_pnl: Decimal::ZERO,
        })
    }

    pub fn log_order(
        &mut self,
        order_id: &str,
        slug: &str,
        token_id: &str,
        outcome: &str,
        side: &str,
        price: Decimal,
        size: Decimal,
        status: &str,
    ) {
        let ts = Utc::now().to_rfc3339();
        if let Err(e) = writeln!(
            self.orders_writer,
            "{},{},{},{},{},{},{:.4},{:.2},{}",
            ts, order_id, slug, token_id, outcome, side, price, size, status
        ) {
            warn!("Failed to write order log: {}", e);
        }
        let _ = self.orders_writer.flush();
    }

    pub fn log_settlement(
        &mut self,
        slug: &str,
        winning: &str,
        inv_up: Decimal,
        inv_down: Decimal,
        pnl: Decimal,
    ) {
        self.cumulative_pnl += pnl;
        let ts = Utc::now().to_rfc3339();
        if let Err(e) = writeln!(
            self.settlements_writer,
            "{},{},{},{:.2},{:.2},{:.4},{:.4}",
            ts, slug, winning, inv_up, inv_down, pnl, self.cumulative_pnl
        ) {
            warn!("Failed to write settlement log: {}", e);
        }
        let _ = self.settlements_writer.flush();
    }
}
