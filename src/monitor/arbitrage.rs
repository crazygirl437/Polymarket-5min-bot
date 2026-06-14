use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::{B256, Decimal, U256};
use rust_decimal_macros::dec;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub market_id: B256,
    pub yes_token_id: U256,
    pub no_token_id: U256,
    pub yes_ask_price: Decimal,
    pub no_ask_price: Decimal,
    pub total_cost: Decimal,
    pub profit_percentage: Decimal,
    pub yes_size: Decimal,
    pub no_size: Decimal,
}

pub struct ArbitrageDetector {
    min_profit_threshold: Decimal,
    max_depth: usize, // Max depth to probe
    min_order_value_usd: Decimal, // Min order value (USD)
}

impl ArbitrageDetector {
    pub fn new(min_profit_threshold: f64) -> Self {
        Self {
            min_profit_threshold: Decimal::try_from(min_profit_threshold)
                .unwrap_or(dec!(0.001)),
            max_depth: 10, // Max 10 levels
            min_order_value_usd: dec!(1.0), // Min $1 per order
        }
    }

    /// Select prices: best ask only. Returns (yes_ask, no_ask, size, profit_pct, total_price).
    /// Executor will: compare prices, add slippage, create orders.
    fn find_best_opportunity(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
    ) -> Option<(Decimal, Decimal, Decimal, Decimal, Decimal)> {
        // asks last element is best ask (lowest sell)
        let yes_best = yes_book.asks.last()?;
        let no_best = no_book.asks.last()?;

        let yes_price = yes_best.price.round_dp(2);
        let no_price = no_best.price.round_dp(2);
        let total_price = yes_price + no_price;

        if total_price > dec!(1.0) {
            return None; // Best ask total > 1, no arb
        }

        // Available size at best ask: min of both, floor to 2 decimals
        let raw_size = yes_best.size.min(no_best.size);
        let final_size = if raw_size.is_zero() {
            dec!(0.01)
        } else {
            (raw_size * dec!(100.0)).floor() / dec!(100.0)
        };

        let yes_order_value = yes_price * final_size;
        let no_order_value = no_price * final_size;
        if yes_order_value < self.min_order_value_usd || no_order_value < self.min_order_value_usd {
            return None;
        }

        let profit_pct = (dec!(1.0) - total_price) * dec!(100.0);
        Some((yes_price, no_price, final_size, profit_pct, total_price))
    }


    /// Print order depth (debug level to avoid log spam)
    fn print_orderbook_depth(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
        yes_final_price: Decimal,
        no_final_price: Decimal,
        yes_final_size: Decimal,
        no_final_size: Decimal,
    ) {
        let yes_asks = &yes_book.asks;
        let yes_depth_str: Vec<String> = yes_asks
            .iter()
            .rev()
            .take(5)
            .map(|level| {
                let m = if (level.price - yes_final_price).abs() < dec!(0.001) { "←" } else { "" };
                format!("{:.2}@{:.2}{}", level.price, level.size, m)
            })
            .collect();
        let no_asks = &no_book.asks;
        let no_depth_str: Vec<String> = no_asks
            .iter()
            .rev()
            .take(5)
            .map(|level| {
                let m = if (level.price - no_final_price).abs() < dec!(0.001) { "←" } else { "" };
                format!("{:.2}@{:.2}{}", level.price, level.size, m)
            })
            .collect();
        debug!(
            yes_depth = yes_depth_str.join(", "),
            no_depth = no_depth_str.join(", "),
            "Order depth"
        );
        // Level selection log moved to executor; prints slippage-adjusted prices
    }

    /// Check if orderbook has arbitrage opportunity
    pub fn check_arbitrage(
        &self,
        yes_book: &BookUpdate,
        no_book: &BookUpdate,
        market_id: &B256,
    ) -> Option<ArbitrageOpportunity> {
        // Select best ask first; executor: compare prices, add slippage, create orders
        let (yes_ask, no_ask, final_size, net_profit_pct, total_price) =
            self.find_best_opportunity(yes_book, no_book)?;

        self.print_orderbook_depth(yes_book, no_book, yes_ask, no_ask, final_size, final_size);

        debug!(
            market_id = %market_id,
            yes_price = %yes_ask,
            no_price = %no_ask,
            total_price = %total_price,
            net_profit_pct = %net_profit_pct,
            order_size = %final_size,
            "Arbitrage opportunity (best ask)"
        );

        Some(ArbitrageOpportunity {
            market_id: *market_id,
            yes_token_id: yes_book.asset_id,
            no_token_id: no_book.asset_id,
            yes_ask_price: yes_ask,
            no_ask_price: no_ask,
            total_cost: total_price * final_size,
            profit_percentage: net_profit_pct,
            yes_size: final_size,
            no_size: final_size,
        })
    }
}
