//! Position balancer: periodically check positions and orders, cancel excess orders to maintain balance

use anyhow::Result;
use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
use polymarket_client_sdk_v2::clob::types::Side;
use polymarket_client_sdk::types::{B256, Decimal, U256};
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

use super::positions::PositionTracker;
use crate::config::Config as BotConfig;
use crate::trading::AuthenticatedClobClient;
use polypulse::positions::get_positions;

/// Position balancer
pub struct PositionBalancer {
    clob_client: AuthenticatedClobClient,
    position_tracker: std::sync::Arc<PositionTracker>,
    threshold: Decimal,
    min_total: Decimal,
    max_order_size: Decimal,
}

impl PositionBalancer {
    pub fn new(
        clob_client: AuthenticatedClobClient,
        position_tracker: std::sync::Arc<PositionTracker>,
        config: &BotConfig,
    ) -> Self {
        Self {
            clob_client,
            position_tracker,
            threshold: Decimal::try_from(config.position_balance_threshold).unwrap_or(dec!(2.0)),
            min_total: Decimal::try_from(config.position_balance_min_total).unwrap_or(dec!(5.0)),
            max_order_size: Decimal::try_from(config.max_order_size_usdc).unwrap_or(dec!(5.0)),
        }
    }

    /// Check and balance positions: fetch positions and orders, analyze YES/NO balance per market, cancel excess orders
    pub async fn check_and_balance_positions(
        &self,
        market_map: &HashMap<B256, (U256, U256)>, // condition_id -> (yes_token_id, no_token_id)
    ) -> Result<()> {
        // Fetch all active orders (handle pagination)
        let mut all_orders = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .clob_client
                .orders(&OrdersRequest::default(), cursor)
                .await?;
            
            all_orders.extend(page.data);
            
            if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
                break;
            }
            cursor = Some(page.next_cursor);
        }

        if all_orders.is_empty() {
            debug!("No active orders, skipping position balance check");
            return Ok(());
        }

        // Get positions (from PositionTracker, updated by scheduled sync)
        let positions = get_positions().await?;

        // Group orders and positions by market
        let mut market_data: HashMap<B256, MarketBalanceData> = HashMap::new();

        // Initialize market data
        for (condition_id, (yes_token, no_token)) in market_map {
            market_data.insert(*condition_id, MarketBalanceData {
                condition_id: *condition_id,
                yes_token_id: *yes_token,
                no_token_id: *no_token,
                yes_position: dec!(0),
                no_position: dec!(0),
                yes_orders: Vec::new(),
                no_orders: Vec::new(),
            });
        }

        // Fill position data
        for pos in positions {
            if let Some(data) = market_data.get_mut(&pos.condition_id) {
                // outcome_index: 0=YES, 1=NO
                if pos.outcome_index == 0 {
                    data.yes_position = pos.size;
                } else if pos.outcome_index == 1 {
                    data.no_position = pos.size;
                }
            }
        }

        // Fill order data
        for order in all_orders {
            // Only process buy orders (Side::Buy)
            if order.side != Side::Buy {
                continue;
            }

            // Find market for each order
            for data in market_data.values_mut() {
                if order.asset_id == data.yes_token_id {
                    let pending_size = order.original_size - order.size_matched;
                    if pending_size > dec!(0) {
                        data.yes_orders.push(OrderInfo {
                            order_id: order.id.clone(),
                            price: order.price,
                            pending_size,
                        });
                    }
                } else if order.asset_id == data.no_token_id {
                    let pending_size = order.original_size - order.size_matched;
                    if pending_size > dec!(0) {
                        data.no_orders.push(OrderInfo {
                            order_id: order.id.clone(),
                            price: order.price,
                            pending_size,
                        });
                    }
                }
            }
        }

        // Balance check per market
        for data in market_data.values() {
            if let Err(e) = self.balance_market(data).await {
                warn!(error = %e, "❌ Market position balance failed");
            }
        }

        Ok(())
    }

    /// Balance a single market
    async fn balance_market(&self, data: &MarketBalanceData) -> Result<()> {
        // Compute actual position diff
        let position_diff = (data.yes_position - data.no_position).abs();

        // Compute pending order amounts
        let yes_pending: Decimal = data.yes_orders.iter().map(|o| o.pending_size).sum();
        let no_pending: Decimal = data.no_orders.iter().map(|o| o.pending_size).sum();

        // Compute total positions
        let yes_total = data.yes_position + yes_pending;
        let no_total = data.no_position + no_pending;
        let total = yes_total + no_total;

        // Skip if total below minimum
        if total < self.min_total {
            debug!("Total position {} below min {}; skip balance", total, self.min_total);
            return Ok(());
        }

        // Case 1: actual positions imbalanced (without pending)
        if position_diff >= self.threshold {
            if data.yes_position > data.no_position {
                // YES excess: cancel all YES orders, cancel matching NO orders
                let cancel_yes_order_ids: Vec<String> = data.yes_orders.iter().map(|o| o.order_id.clone()).collect();
                let cancel_yes_count = cancel_yes_order_ids.len();
                
                // Cancel NO size: min(no_pending, yes_pending)
                let cancel_no_size = yes_pending.min(no_pending);

                if cancel_yes_count > 0 || cancel_no_size > dec!(0) {
                    info!(
                        "⚠️ YES excess detected | YES:{} NO:{} | cancel {} YES orders and ~{} NO pending",
                        data.yes_position,
                        data.no_position,
                        cancel_yes_count,
                        cancel_no_size
                    );

                    // Cancel YES orders
                    if cancel_yes_count > 0 {
                        let yes_order_ids: Vec<&str> = cancel_yes_order_ids.iter().map(|s| s.as_str()).collect();
                        if let Err(e) = self.clob_client.cancel_orders(&yes_order_ids).await {
                            error!(error = %e, "❌ Cancel YES orders failed");
                        } else {
                            info!("✅ Cancelled {} YES orders", cancel_yes_count);
                        }
                    }

                    // Cancel NO orders (by price, lowest first, until cancel_no_size)
                    if cancel_no_size > dec!(0) {
                        let mut no_orders_sorted = data.no_orders.clone();
                        no_orders_sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
                        
                        let mut cancel_no_order_ids = Vec::new();
                        let mut accumulated_size = dec!(0);
                        
                        for order in no_orders_sorted {
                            if accumulated_size >= cancel_no_size {
                                break;
                            }
                            cancel_no_order_ids.push(order.order_id.clone());
                            accumulated_size += order.pending_size;
                        }
                        
                        if !cancel_no_order_ids.is_empty() {
                            let cancel_no_order_ids_ref: Vec<&str> = cancel_no_order_ids.iter().map(|s| s.as_str()).collect();
                            if let Err(e) = self.clob_client.cancel_orders(&cancel_no_order_ids_ref).await {
                                error!(error = %e, "Cancel NO orders failed");
                            } else {
                                info!("Cancelled {} NO orders (acc {} shares)", cancel_no_order_ids.len(), accumulated_size);
                            }
                        }
                    }
                }
            } else {
                // NO excess: cancel all NO orders, cancel matching YES orders
                let cancel_no_order_ids: Vec<String> = data.no_orders.iter().map(|o| o.order_id.clone()).collect();
                let cancel_no_count = cancel_no_order_ids.len();
                
                // Cancel YES size: min(yes_pending, no_pending)
                let cancel_yes_size = no_pending.min(yes_pending);

                if cancel_no_count > 0 || cancel_yes_size > dec!(0) {
                    info!(
                        "⚠️ NO excess detected | YES:{} NO:{} | cancel {} NO orders and ~{} YES pending",
                        data.yes_position,
                        data.no_position,
                        cancel_no_count,
                        cancel_yes_size
                    );

                    // Cancel NO orders
                    if cancel_no_count > 0 {
                        let no_order_ids: Vec<&str> = cancel_no_order_ids.iter().map(|s| s.as_str()).collect();
                        if let Err(e) = self.clob_client.cancel_orders(&no_order_ids).await {
                            error!(error = %e, "Cancel NO orders failed");
                        } else {
                            info!("Cancelled {} NO orders", cancel_no_count);
                        }
                    }

                    // Cancel YES orders (by price, lowest first)
                    if cancel_yes_size > dec!(0) {
                        let mut yes_orders_sorted = data.yes_orders.clone();
                        yes_orders_sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
                        
                        let mut cancel_yes_order_ids = Vec::new();
                        let mut accumulated_size = dec!(0);
                        
                        for order in yes_orders_sorted {
                            if accumulated_size >= cancel_yes_size {
                                break;
                            }
                            cancel_yes_order_ids.push(order.order_id.clone());
                            accumulated_size += order.pending_size;
                        }
                        
                        if !cancel_yes_order_ids.is_empty() {
                            let cancel_yes_order_ids_ref: Vec<&str> = cancel_yes_order_ids.iter().map(|s| s.as_str()).collect();
                            if let Err(e) = self.clob_client.cancel_orders(&cancel_yes_order_ids_ref).await {
                                error!(error = %e, "❌ Cancel YES orders failed");
                            } else {
                                info!("✅ Cancelled {} YES orders (total {} shares)", cancel_yes_order_ids.len(), accumulated_size);
                            }
                        }
                    }
                }
            }
            return Ok(());
        }

        // Case 2: actual positions balanced but pending causes total imbalance
        let target = (yes_total + no_total) / dec!(2);
        let yes_imbalance = yes_total - target;
        let no_imbalance = no_total - target;

        // Cancel excess YES orders
        if yes_imbalance.abs() >= self.threshold && yes_imbalance > dec!(0) {
            let mut yes_orders_sorted = data.yes_orders.clone();
            yes_orders_sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

            let mut cancel_size = dec!(0);
            let mut cancel_order_ids = Vec::new();
            
            for order in yes_orders_sorted {
                if cancel_size >= yes_imbalance {
                    break;
                }
                cancel_order_ids.push(order.order_id.clone());
                cancel_size += order.pending_size;
            }

            if !cancel_order_ids.is_empty() {
                info!("⚠️ YES pending excess, cancelling {} YES orders", cancel_order_ids.len());

                let cancel_order_ids_ref: Vec<&str> = cancel_order_ids.iter().map(|s| s.as_str()).collect();
                if let Err(e) = self.clob_client.cancel_orders(&cancel_order_ids_ref).await {
                    error!(error = %e, "❌ Cancel YES orders failed");
                } else {
                    info!("✅ Cancelled {} YES orders", cancel_order_ids.len());
                }
            }
        }

        // Cancel excess NO orders
        if no_imbalance.abs() >= self.threshold && no_imbalance > dec!(0) {
            let mut no_orders_sorted = data.no_orders.clone();
            no_orders_sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

            let mut cancel_size = dec!(0);
            let mut cancel_order_ids = Vec::new();
            
            for order in no_orders_sorted {
                if cancel_size >= no_imbalance {
                    break;
                }
                cancel_order_ids.push(order.order_id.clone());
                cancel_size += order.pending_size;
            }

            if !cancel_order_ids.is_empty() {
                info!("NO pending excess, cancelling {} NO orders", cancel_order_ids.len());

                let cancel_order_ids_ref: Vec<&str> = cancel_order_ids.iter().map(|s| s.as_str()).collect();
                if let Err(e) = self.clob_client.cancel_orders(&cancel_order_ids_ref).await {
                    error!(error = %e, "Cancel NO orders failed");
                } else {
                    info!("Cancelled {} NO orders", cancel_order_ids.len());
                }
            }
        }

        Ok(())
    }

    /// Check if market should skip arbitrage (if severely imbalanced)
    /// Uses local cached positions, zero latency
    pub fn should_skip_arbitrage(&self, yes_token: U256, no_token: U256) -> bool {
        let (yes_pos, no_pos) = self.position_tracker.get_pair_positions(yes_token, no_token);
        let position_diff = (yes_pos - no_pos).abs();

        if position_diff >= self.threshold {
            warn!(
                yes_position = %yes_pos,
                no_position = %no_pos,
                position_diff = %position_diff,
                threshold = %self.threshold,
                "⛔ Positions severely imbalanced, skip arbitrage"
            );
            return true;
        }

        false
    }
}

/// Market balance data
struct MarketBalanceData {
    condition_id: B256,
    yes_token_id: U256,
    no_token_id: U256,
    yes_position: Decimal,
    no_position: Decimal,
    yes_orders: Vec<OrderInfo>,
    no_orders: Vec<OrderInfo>,
}

/// Order info
#[derive(Clone)]
struct OrderInfo {
    order_id: String,
    price: Decimal,
    pending_size: Decimal,
}
