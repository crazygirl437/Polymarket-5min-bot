use anyhow::Result;
use chrono::{DateTime, Utc};
use polymarket_client_sdk::gamma::{Client, types::request::MarketsRequest};
use polymarket_client_sdk::types::{B256, U256};
use tracing::{info, warn};

/// 5-minute window duration in seconds (for main etc. to compute window_end)
pub const FIVE_MIN_SECS: i64 = 300;

#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub market_id: B256,
    pub slug: String,
    pub yes_token_id: U256,
    pub no_token_id: U256,
    pub title: String,
    pub end_date: DateTime<Utc>,
    pub crypto_symbol: String,
}

pub struct MarketDiscoverer {
    gamma_client: Client,
    crypto_symbols: Vec<String>,
}

impl MarketDiscoverer {
    pub fn new(crypto_symbols: Vec<String>) -> Self {
        Self {
            gamma_client: Client::default(),
            crypto_symbols,
        }
    }

    /// Current 5-minute window start timestamp (UTC)
    /// Window aligned to 0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55
    pub fn calculate_current_window_timestamp(now: DateTime<Utc>) -> i64 {
        let ts = now.timestamp();
        (ts / FIVE_MIN_SECS) * FIVE_MIN_SECS
    }

    /// Next 5-minute window start timestamp (UTC)
    pub fn calculate_next_window_timestamp(now: DateTime<Utc>) -> i64 {
        let ts = now.timestamp();
        ((ts / FIVE_MIN_SECS) + 1) * FIVE_MIN_SECS
    }

    /// Generate market slugs, e.g. btc-updown-5m-1770972300
    pub fn generate_market_slugs(&self, timestamp: i64) -> Vec<String> {
        self.crypto_symbols
            .iter()
            .map(|symbol| format!("{}-updown-5m-{}", symbol, timestamp))
            .collect()
    }

    /// Fetch 5-minute markets for given timestamp
    pub async fn get_markets_for_timestamp(&self, timestamp: i64) -> Result<Vec<MarketInfo>> {
        // Generate slugs for all crypto symbols
        let slugs = self.generate_market_slugs(timestamp);

        info!(timestamp, slug_count = slugs.len(), "Querying markets");

        // Batch query Gamma API
        let request = MarketsRequest::builder()
            .slug(slugs.clone())
            .build();

        match self.gamma_client.markets(&request).await {
            Ok(markets) => {
                // Filter and parse markets
                let valid_markets: Vec<MarketInfo> = markets
                    .into_iter()
                    .filter_map(|market| self.parse_market(market))
                    .collect();

                info!(count = valid_markets.len(), "Found valid markets");
                Ok(valid_markets)
            }
            Err(e) => {
                warn!(error = %e, timestamp = timestamp, "Market query failed, markets may not exist yet");
                Ok(Vec::new())
            }
        }
    }

    /// Parse market, extract YES and NO token_ids
    fn parse_market(&self, market: polymarket_client_sdk::gamma::types::response::Market) -> Option<MarketInfo> {
        // Check market is active, orderbook enabled and accepting orders
        if !market.active.unwrap_or(false) 
           || !market.enable_order_book.unwrap_or(false)
           || !market.accepting_orders.unwrap_or(false) {
            return None;
        }

        // Check outcomes are ["Up", "Down"]
        let outcomes = market.outcomes.as_ref()?;

        if outcomes.len() != 2 
           || !outcomes.contains(&"Up".to_string()) 
           || !outcomes.contains(&"Down".to_string()) {
            return None;
        }

        // Get clobTokenIds
        let token_ids = market.clob_token_ids.as_ref()?;

        if token_ids.len() != 2 {
            return None;
        }

        // First is Up token_id, second is Down
        let yes_token_id = token_ids[0];
        let no_token_id = token_ids[1];

        // Get conditionId
        let market_id = market.condition_id?;

        // Extract crypto symbol from slug
        let slug = market.slug.as_ref()?;
        let crypto_symbol = slug
            .split('-')
            .next()
            .unwrap_or("")
            .to_string();

        // Get endDate
        let end_date = market.end_date?;

        Some(MarketInfo {
            market_id,
            slug: slug.clone(),
            yes_token_id,
            no_token_id,
            title: market.question.unwrap_or_default(),
            end_date,
            crypto_symbol,
        })
    }
}
