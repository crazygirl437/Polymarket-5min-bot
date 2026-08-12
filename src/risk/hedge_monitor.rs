use anyhow::Result;
use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use dashmap::DashMap;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::clob::types::{OrderType, Side};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, trace, warn};

use super::positions::PositionTracker;
use super::recovery::RecoveryAction;
use crate::trading::AuthenticatedClobClient;

#[derive(Debug, Clone)]
pub struct HedgePosition {
    pub token_id: U256,
    pub opposite_token_id: U256, // Opposite side token_id (for diff calc)
    pub amount: Decimal,
    pub entry_price: Decimal, // Buy price (best ask)
    pub take_profit_price: Decimal, // Take-profit price
    pub stop_loss_price: Decimal,   // Stop-loss price
    pub pair_id: String,
    pub market_display: String, // Market display name (e.g. "btc prediction")
    pub order_id: Option<String>, // GTC order ID if placed
    pub pending_sell_amount: Decimal, // Pending sell amount
}

pub struct HedgeMonitor {
    client: AuthenticatedClobClient,
    private_key: String,
    proxy_address: Option<Address>,
    positions: DashMap<String, HedgePosition>, // pair_id -> position
    position_tracker: Arc<PositionTracker>, // For exposure updates
}

impl HedgeMonitor {
    pub fn new(
        client: AuthenticatedClobClient,
        private_key: String,
        proxy_address: Option<Address>,
        position_tracker: Arc<PositionTracker>,
    ) -> Self {
        Self {
            client,
            private_key,
            proxy_address,
            positions: DashMap::new(),
            position_tracker,
        }
    }

    /// Add hedge position to monitor
    pub fn add_position(&self, action: &RecoveryAction) -> Result<()> {
        if let RecoveryAction::MonitorForExit {
            token_id,
            opposite_token_id,
            amount,
            entry_price,
            take_profit_pct,
            stop_loss_pct,
            pair_id,
            market_display,
        } = action
        {
            // Compute take-profit and stop-loss prices
            let take_profit_price = *entry_price * (dec!(1.0) + *take_profit_pct);
            let stop_loss_price = *entry_price * (dec!(1.0) - *stop_loss_pct);

            info!(
                "🛡️ Start hedge monitor | market:{} | pos:{} | entry:{:.4} | TP:{:.4} | SL:{:.4}",
                market_display,
                amount,
                entry_price,
                take_profit_price,
                stop_loss_price
            );

            let position = HedgePosition {
                token_id: *token_id,
                opposite_token_id: *opposite_token_id,
                amount: *amount,
                entry_price: *entry_price,
                take_profit_price,
                stop_loss_price,
                pair_id: pair_id.clone(),
                market_display: market_display.clone(),
                order_id: None,
                pending_sell_amount: dec!(0),
            };

            self.positions.insert(pair_id.clone(), position);
        }
        Ok(())
    }

    /// Update entry_price from orderbook best ask
    pub fn update_entry_price(&self, pair_id: &str, entry_price: Decimal) {
        if let Some(mut pos) = self.positions.get_mut(pair_id) {
            let old_entry = pos.entry_price;
            pos.entry_price = entry_price;
            // Recompute take-profit and stop-loss
            let take_profit_pct = (pos.take_profit_price - old_entry) / old_entry;
            let stop_loss_pct = (old_entry - pos.stop_loss_price) / old_entry;
            pos.take_profit_price = entry_price * (dec!(1.0) + take_profit_pct);
            pos.stop_loss_price = entry_price * (dec!(1.0) - stop_loss_pct);
            
            info!(
                pair_id = %pair_id,
                old_entry = %old_entry,
                new_entry = %entry_price,
                take_profit_price = %pos.take_profit_price,
                stop_loss_price = %pos.stop_loss_price,
                "Update buy price"
            );
        }
    }

    /// Check orderbook; sell if take-profit or stop-loss hit
    pub async fn check_and_execute(&self, book: &BookUpdate) -> Result<()> {
        // Best bid (last in bids, bids are price descending)
        let best_bid = book.bids.last();
        let best_bid_price = match best_bid {
            Some(bid) => bid.price,
            None => return Ok(()), // No bids, cannot sell
        };

        // Find positions to check
        let positions_to_check: Vec<(String, HedgePosition)> = self
            .positions
            .iter()
            .filter(|entry| entry.value().token_id == book.asset_id)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (pair_id, position) in positions_to_check {
            // Check if GTC order exists; if so, repost at latest price
            if let Some(ref order_id) = position.order_id {
                let pending_amount = position.pending_sell_amount;
                if pending_amount > dec!(0) {
                    // Unfilled order; repost at latest price
                    info!(
                        "🔄 Unfilled order | market:{} | order_id:{} | remain:{} | repost at {:.4}",
                        position.market_display,
                        &order_id[..16],
                        pending_amount,
                        best_bid_price
                    );
                    // Clear old order_id before repost
                    if let Some(mut pos) = self.positions.get_mut(&pair_id) {
                        pos.order_id = None;
                    }
                    // Proceed with sell logic using pending_amount
                } else {
                    // Order submitted but pending_amount=0, possibly in progress; skip
                    continue;
                }
            }

            // Check take-profit or stop-loss
            let (should_sell, reason) = if best_bid_price >= position.take_profit_price {
                let profit_pct = ((best_bid_price - position.entry_price) / position.entry_price * dec!(100.0)).to_f64().unwrap_or(0.0);
                (true, format!("Take-profit ({:.2}%)", profit_pct))
            } else if best_bid_price <= position.stop_loss_price {
                let loss_pct = ((position.entry_price - best_bid_price) / position.entry_price * dec!(100.0)).to_f64().unwrap_or(0.0);
                (true, format!("Stop-loss ({:.2}%)", loss_pct))
            } else {
                (false, String::new())
            };

            if should_sell {
                // Get current and opposite positions
                let current_position = self.position_tracker.get_position(position.token_id);
                let opposite_position = self.position_tracker.get_position(position.opposite_token_id);
                
                // Diff: current - opposite
                let difference = current_position - opposite_position;
                
                // If diff <= 0, opposite covers; no sell
                if difference <= dec!(0) {
                    info!(
                        "⏸️ No sell needed | market:{} | pos:{} | opposite:{} | diff:{} | opposite covers",
                        position.market_display,
                        current_position,
                        opposite_position,
                        difference
                    );
                    continue;
                }
                
                // Determine sell amount
                let sell_amount = if position.order_id.is_some() && position.pending_sell_amount > dec!(0) {
                    // Use pending_sell_amount if unfilled order exists
                    position.pending_sell_amount
                } else {
                    // Else use diff
                    difference
                };
                
                // Diff > 0; sell diff amount
                info!(
                    "✅ {} hit | market:{} | bid:{:.4} | entry:{:.4} | pos:{} | opposite:{} | diff:{} | sell:{}",
                    reason,
                    position.market_display,
                    best_bid_price,
                    position.entry_price,
                    current_position,
                    opposite_position,
                    difference,
                    sell_amount
                );
                
                // Sell via GTC order
                // Spawn async to avoid blocking main loop
                let position_clone = position.clone();
                let pair_id_clone = pair_id.clone();
                let position_tracker = self.position_tracker.clone();
                let positions = self.positions.clone();
                let client = self.client.clone();
                let private_key = self.private_key.clone();
                
                // Mark processing to avoid duplicate orders
                if let Some((_, mut pos)) = self.positions.remove(&pair_id) {
                    pos.order_id = Some("processing".to_string());
                    self.positions.insert(pair_id.clone(), pos);
                }
                
                tokio::spawn(async move {
                    // Recreate signer (cannot use self in spawn)
                    let signer = match LocalSigner::from_str(&private_key) {
                        Ok(s) => s.with_chain_id(Some(POLYGON)),
                        Err(e) => {
                            error!(
                                "❌ Create signer failed | market:{} | err:{}",
                                position_clone.market_display,
                                e
                            );
                            return;
                        }
                    };
                    
                    // Execute sell
                    match Self::execute_sell_order(
                        &client,
                        &signer,
                        &position_clone,
                        best_bid_price,
                        sell_amount,
                    ).await {
                        Ok((order_id, filled, remaining)) => {
                            // Update position, mark order placed
                            let order_id_short = order_id[..16].to_string();
                            if let Some((_, mut pos)) = positions.remove(&pair_id_clone) {
                                if remaining > dec!(0) {
                                    // Partial fill; save order_id
                                    pos.order_id = Some(order_id);
                                    pos.pending_sell_amount = remaining;
                                    info!("🔒 Position order_id updated | market:{} | id:{} | remain:{}", 
                                        position_clone.market_display, order_id_short, remaining);
                                } else {
                                    // Full fill; clear order_id
                                    pos.order_id = None;
                                    pos.pending_sell_amount = dec!(0);
                                    info!("✅ Sell order fully filled | market:{} | id:{} | filled:{}", 
                                        position_clone.market_display, order_id_short, filled);
                                }
                                positions.insert(pair_id_clone.clone(), pos);
                            } else {
                                warn!("⚠️ Position not found | pair_id:{}", pair_id_clone);
                            }
                            
                            // Only update position and exposure for actual fills
                            if filled > dec!(0) {
                                info!("📊 Updating position | market:{} | reduce:{}", 
                                    position_clone.market_display, filled);
                                position_tracker.update_position(position_clone.token_id, -filled);
                                info!("📊 Position update done | market:{}", position_clone.market_display);
                                
                                // Update exposure cost
                                info!("💰 Updating exposure | market:{} | entry:{} | sell:{}", 
                                    position_clone.market_display,
                                    position_clone.entry_price,
                                    filled);
                                position_tracker.update_exposure_cost(
                                    position_clone.token_id,
                                    position_clone.entry_price,
                                    -filled,
                                );
                                info!("💰 Exposure update done | market:{}", position_clone.market_display);
                                
                                // Compute exposure
                                let current_exposure = position_tracker.calculate_exposure();
                                info!(
                                    "📉 Exposure updated | market:{} | sold:{} | exposure:{:.2} USD",
                                    position_clone.market_display,
                                    filled,
                                    current_exposure
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                "❌ Sell order failed | market:{} | price:{:.4} | err:{}",
                                position_clone.market_display,
                                best_bid_price,
                                e
                            );
                            // On failure, clear processing
                            if let Some(mut pos) = positions.get_mut(&pair_id_clone) {
                                pos.order_id = None;
                            }
                        }
                    }
                });
            }
        }

        Ok(())
    }

    /// Compute actual sell amount (with fee)
    fn calculate_sell_amount(&self, position: &HedgePosition) -> Decimal {
        self.calculate_sell_amount_with_size(position, position.amount)
    }

    /// Compute actual sell amount for given size (with fee)
    fn calculate_sell_amount_with_size(&self, position: &HedgePosition, base_amount: Decimal) -> Decimal {
        // Compute fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Compute available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        // Floor to 2 decimals
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        
        if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        }
    }

    /// Static: compute actual sell amount for given size (with fee)
    fn calculate_sell_amount_static(position: &HedgePosition, base_amount: Decimal) -> Decimal {
        // Compute fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Compute available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        // Floor to 2 decimals
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        
        if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        }
    }

    /// Static: execute sell order
    async fn execute_sell_order(
        client: &AuthenticatedClobClient,
        signer: &impl Signer<alloy::primitives::Signature>,
        position: &HedgePosition,
        price: Decimal,
        size: Decimal,
    ) -> Result<(String, Decimal, Decimal)> {
        // Compute fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Compute available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            size * multiplier
        };
        
        // Floor to 2 decimals
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        let order_size = if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        };

        info!(
            "💰 Sell amount | market:{} | base:{:.2} | entry:{:.4} | fee:{:.2}% | avail:{:.2} | order:{:.2}",
            position.market_display,
            size,
            position.entry_price,
            fee_decimal,
            available_amount,
            order_size
        );

        // Build GTC sell order
        let sell_order = client
            .limit_order()
            .token_id(position.token_id)
            .side(Side::Sell)
            .price(price)
            .size(order_size)
            .order_type(OrderType::GTC)
            .build()
            .await?;

        // Sign order
        let signed_order = client.sign(signer, sell_order).await?;

        // Submit order
        let result = client.post_order(signed_order).await?;

        if !result.success {
            let error_msg = result.error_msg.as_deref().unwrap_or("unknown error");
            return Err(anyhow::anyhow!("GTC sell order failed: {}", error_msg));
        }

        // Check immediate fill
        let filled = result.taking_amount;
        let remaining = order_size - filled;
        
        if filled > dec!(0) {
            info!(
                "💰 Sell order partial fill | market:{} | id:{} | filled:{} | remain:{}",
                position.market_display,
                &result.order_id[..16],
                filled,
                remaining
            );
        } else {
            info!(
                "📋 Sell order posted (no immediate fill) | market:{} | id:{} | size:{} | price:{:.4}",
                position.market_display,
                &result.order_id[..16],
                order_size,
                price
            );
        }
        
        Ok((result.order_id, filled, remaining))
    }

    /// Sell via GTC order; size: optional, else position.amount
    async fn sell_with_gtc(
        &self,
        position: &HedgePosition,
        price: Decimal,
        size: Option<Decimal>,
    ) -> Result<(String, Decimal, Decimal)> {
        let signer = LocalSigner::from_str(&self.private_key)?
            .with_chain_id(Some(POLYGON));

        // Compute fee
        // fee = c * fee_rate * (p * (1-p))^exponent; p=entry_price, c=100
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        // Fee ratio (0-1.56)
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        
        // To Decimal
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Use size or position.amount
        let base_amount = size.unwrap_or(position.amount);
        
        // Available = filled size * (100 - Fee) / 100
        // If Fee >= 100, use min tradeable unit
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01) // Edge case: min unit
        } else {
            // Normal: available = filled * (100 - Fee) / 100
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        let order_size = if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        };

        info!(
            "💰 Sell amount | market:{} | base:{:.2} | entry:{:.4} | fee:{:.2}% | avail:{:.2} | order:{:.2}",
            position.market_display,
            base_amount,
            position.entry_price,
            fee_decimal,
            available_amount,
            order_size
        );

        // Build GTC sell order
        let sell_order = self
            .client
            .limit_order()
            .token_id(position.token_id)
            .side(Side::Sell)
            .price(price)
            .size(order_size)
            .order_type(OrderType::GTC)
            .build()
            .await?;

        // Sign order
        let signed_order = self.client.sign(&signer, sell_order).await?;

        // Submit order
        let result = self.client.post_order(signed_order).await?;

        if !result.success {
            let error_msg = result.error_msg.as_deref().unwrap_or("unknown error");
            return Err(anyhow::anyhow!("GTC sell order failed: {}", error_msg));
        }

        // Check immediate fill
        let filled = result.taking_amount;
        let remaining = order_size - filled;
        
        if filled > dec!(0) {
            info!(
                "💰 Sell order partial fill | market:{} | id:{} | filled:{} | remain:{}",
                position.market_display,
                &result.order_id[..16],
                filled,
                remaining
            );
        } else {
            info!(
                "📋 Sell order posted (no immediate fill) | market:{} | id:{} | size:{} | price:{:.4}",
                position.market_display,
                &result.order_id[..16],
                order_size,
                price
            );
        }
        
        Ok((result.order_id, filled, remaining))
    }

    /// Remove completed position
    pub fn remove_position(&self, pair_id: &str) {
        self.positions.remove(pair_id);
        info!(pair_id = %pair_id, "Remove hedge position");
    }

    /// Get all monitored positions
    pub fn get_positions(&self) -> Vec<HedgePosition> {
        self.positions.iter().map(|e| e.value().clone()).collect()
    }
}
