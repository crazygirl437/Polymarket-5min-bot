use anyhow::Result;
use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, U256};
use rust_decimal_macros::dec;
use tracing::{debug, info, trace};

use polypulse::positions::{get_positions, Position};

pub struct PositionTracker {
    positions: DashMap<U256, Decimal>, // token_id -> amount (pos=long, neg=short)
    exposure_costs: DashMap<U256, Decimal>, // token_id -> cost (USD) for risk exposure
    max_exposure: Decimal,
}

impl PositionTracker {
    pub fn new(max_exposure: Decimal) -> Self {
        Self {
            positions: DashMap::new(),
            exposure_costs: DashMap::new(),
            max_exposure,
        }
    }

    pub fn update_position(&self, token_id: U256, delta: Decimal) {
        trace!("update_position: start | token_id:{} | delta:{}", token_id, delta);
        
        trace!("update_position: acquiring positions write lock");
        let mut entry = self.positions.entry(token_id).or_insert(dec!(0));
        trace!("update_position: positions write lock acquired");
        *entry += delta;
        trace!("update_position: position updated, new value:{}", *entry);

        // Clean up if position goes to ~0
        // Key fix: release positions write lock before accessing exposure_costs to avoid deadlock
        let should_remove = entry.abs() < dec!(0.0001);
        trace!("update_position: should_remove:{}", should_remove);
        if should_remove {
            *entry = dec!(0);
            trace!("update_position: position zeroed");
        }
        drop(entry);
        trace!("update_position: positions write lock released");
        
        // Now safe to access exposure_costs
        if should_remove {
            trace!("update_position: removing exposure_costs");
            self.exposure_costs.remove(&token_id);
            trace!("update_position: exposure_costs removed");
        }
        
        trace!("update_position: done");
    }

    /// Update risk exposure cost (USD)
    /// price: buy price, delta: position change (pos=buy, neg=sell)
    pub fn update_exposure_cost(&self, token_id: U256, price: Decimal, delta: Decimal) {
        trace!("update_exposure_cost: start | token_id:{} | price:{} | delta:{}", token_id, price, delta);
        
        if delta == dec!(0) {
            trace!("update_exposure_cost: delta=0, return");
            return;
        }
        
        trace!("update_exposure_cost: acquiring positions read lock");
        // Key fix: get positions read lock first, then exposure_costs write lock to avoid deadlock
        let current_pos = if delta < dec!(0) {
            trace!("update_exposure_cost: sell, getting positions read lock");
            let pos = self.positions.get(&token_id);
            trace!("update_exposure_cost: positions read lock acquired");
            let result = pos.map(|v| *v.value()).unwrap_or(dec!(0));
            trace!("update_exposure_cost: positions read released, current_pos:{}", result);
            result
        } else {
            trace!("update_exposure_cost: buy, no positions read needed");
            dec!(0)
        };
        
        trace!("update_exposure_cost: acquiring exposure_costs write lock");
        let mut entry = self.exposure_costs.entry(token_id).or_insert(dec!(0));
        trace!("update_exposure_cost: exposure_costs write lock acquired");
        
        if delta > dec!(0) {
            trace!("update_exposure_cost: buy branch, compute cost_delta");
            let cost_delta = price * delta;
            *entry += cost_delta;
            trace!("update_exposure_cost: buy done, new cost:{}", *entry);
        } else {
            trace!("update_exposure_cost: sell branch, current_pos:{}", current_pos);
            if current_pos > dec!(0) {
                trace!("update_exposure_cost: compute sell ratio");
                let sell_amount = (-delta).min(current_pos);
                let reduction_ratio = sell_amount / current_pos;
                trace!("update_exposure_cost: sell_amount:{} | reduction_ratio:{} | current cost:{}", sell_amount, reduction_ratio, *entry);
                *entry = (*entry * (dec!(1) - reduction_ratio)).max(dec!(0));
                trace!("update_exposure_cost: sell done, new cost:{}", *entry);
            } else {
                trace!("update_exposure_cost: current_pos=0, zero out");
                *entry = dec!(0);
            }
        }
        
        trace!("update_exposure_cost: check cleanup, current cost:{}", *entry);
        if *entry < dec!(0.01) {
            trace!("update_exposure_cost: cost near 0, cleanup");
            *entry = dec!(0);
            drop(entry);
            trace!("update_exposure_cost: lock released, removing");
            self.exposure_costs.remove(&token_id);
            trace!("update_exposure_cost: remove done");
        } else {
            trace!("update_exposure_cost: cost nonzero, keep entry");
            drop(entry);
        }
        
        trace!("update_exposure_cost: done");
    }

    /// Get max risk exposure limit
    pub fn max_exposure(&self) -> Decimal {
        self.max_exposure
    }

    /// Reset exposure (called at round start; clears cost cache so round starts from 0)
    pub fn reset_exposure(&self) {
        self.exposure_costs.clear();
        info!("🔄 Risk exposure reset (new round)");
    }

    pub fn get_position(&self, token_id: U256) -> Decimal {
        self.positions
            .get(&token_id)
            .map(|v| *v.value())
            .unwrap_or(dec!(0))
    }

    /// Compute position imbalance (0.0 = balanced, 1.0 = fully imbalanced)
    pub fn calculate_imbalance(&self, yes_token: U256, no_token: U256) -> Decimal {
        let yes_pos = self.get_position(yes_token);
        let no_pos = self.get_position(no_token);

        let total = yes_pos + no_pos;
        if total == dec!(0) {
            return dec!(0); // fully balanced
        }

        // imbalance = abs(yes - no) / (yes + no)
        let imbalance = (yes_pos - no_pos).abs() / total;
        imbalance
    }

    /// Compute total risk exposure (USD), sum of all position costs
    pub fn calculate_exposure(&self) -> Decimal {
        // Sum all position costs; collect to Vec to avoid holding lock long
        let costs: Vec<Decimal> = self.exposure_costs
            .iter()
            .map(|entry| *entry.value())
            .collect();
        costs.iter().sum()
    }

    pub fn is_within_limits(&self) -> bool {
        self.calculate_exposure() <= self.max_exposure
    }

    /// Check if new order would exceed exposure limit
    /// yes_cost, no_cost: order costs (price * size)
    pub fn would_exceed_limit(&self, yes_cost: Decimal, no_cost: Decimal) -> bool {
        let current_exposure = self.calculate_exposure();
        let new_order_cost = yes_cost + no_cost;
        (current_exposure + new_order_cost) > self.max_exposure
    }

    /// Get YES and NO positions
    pub fn get_pair_positions(&self, yes_token: U256, no_token: U256) -> (Decimal, Decimal) {
        (self.get_position(yes_token), self.get_position(no_token))
    }

    /// Sync positions from Data API, fully overwrite local cache
    /// For scheduled sync; ensures local matches on-chain positions
    pub async fn sync_from_api(&self) -> Result<Vec<Position>> {
        use std::collections::HashMap;
        use polymarket_client_sdk::types::B256;
        
        let positions = get_positions().await?;
        
        // Clear positions (exposure only from arbitrage execution and Merge, not API)
        self.positions.clear();
        
        // Update local cache from API positions
        let mut updated_count = 0;
        let mut valid_positions = Vec::new();
        
        for pos in positions {
            if pos.size > dec!(0) {
                // Position.asset is token_id
                self.positions.insert(pos.asset, pos.size);
                valid_positions.push(pos);
                updated_count += 1;
            }
        }
        
        // Print positions grouped by market
        if !valid_positions.is_empty() {
            let mut by_market: HashMap<B256, Vec<&Position>> = HashMap::new();
            for pos in &valid_positions {
                by_market.entry(pos.condition_id).or_default().push(pos);
            }
            
            info!("📊 Position sync done | {} positions, {} markets", updated_count, by_market.len());
            
            // Print one line per market
            for (_condition_id, market_positions) in by_market.iter() {
                let mut yes_pos = dec!(0);
                let mut no_pos = dec!(0);
                let mut market_title = "";
                
                for pos in market_positions {
                    if pos.outcome_index == 0 {
                        yes_pos = pos.size;
                    } else if pos.outcome_index == 1 {
                        no_pos = pos.size;
                    }
                    if market_title.is_empty() {
                        market_title = &pos.title;
                    }
                }
                
                // Truncate long titles
                let title_display = if market_title.len() > 40 {
                    format!("{}...", &market_title[..37])
                } else {
                    market_title.to_string()
                };
                
                info!(
                    "  📈 {} | YES:{} NO:{}",
                    title_display,
                    yes_pos,
                    no_pos
                );
            }
        } else {
            info!("📊 Position sync done | no positions");
        }
        
        Ok(valid_positions)
    }
}
