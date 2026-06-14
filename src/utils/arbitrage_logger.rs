use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use tracing::error;

use crate::monitor::ArbitrageOpportunity;

#[derive(Serialize)]
struct ArbitrageRecord {
    timestamp: String,
    market_id: String,
    market_name: String,
    yes_token_id: String,
    no_token_id: String,
    yes_ask_price: String,
    no_ask_price: String,
    total_cost: String,
    profit_percentage: String,
    yes_size: String,
    no_size: String,
}

/// Write arbitrage opportunity to file
pub fn log_arbitrage_opportunity(
    opp: &ArbitrageOpportunity,
    market_name: &str,
    file_path: &str,
) -> Result<()> {
    let record = ArbitrageRecord {
        timestamp: Utc::now().to_rfc3339(),
        market_id: format!("{:?}", opp.market_id),
        market_name: market_name.to_string(),
        yes_token_id: opp.yes_token_id.to_string(),
        no_token_id: opp.no_token_id.to_string(),
        yes_ask_price: opp.yes_ask_price.to_string(),
        no_ask_price: opp.no_ask_price.to_string(),
        total_cost: opp.total_cost.to_string(),
        profit_percentage: opp.profit_percentage.to_string(),
        yes_size: opp.yes_size.to_string(),
        no_size: opp.no_size.to_string(),
    };

    // Format record as JSON
    let json = serde_json::to_string_pretty(&record)?;
    
    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;
    
    writeln!(file, "{}", json)?;
    writeln!(file, "---")?; // Separator
    file.flush()?; // Flush to disk
    
    Ok(())
}

/// Async version of arbitrage log (avoids blocking)
pub async fn log_arbitrage_opportunity_async(
    opp: &ArbitrageOpportunity,
    market_name: &str,
    file_path: &str,
) {
    if let Err(e) = log_arbitrage_opportunity(opp, market_name, file_path) {
        error!(error = %e, "Failed to write arbitrage log file");
    }
}
