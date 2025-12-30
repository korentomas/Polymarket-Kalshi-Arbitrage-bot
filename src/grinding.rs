#![allow(dead_code)]

//! High-Probability Grinding Strategy
//!
//! This module implements the primary trading strategy: buying high-probability
//! outcomes (90%+) near expiry for small but consistent gains.
//!
//! # Strategy Overview
//!
//! The grinding strategy exploits the fact that near-certain outcomes often trade
//! slightly below their true probability due to:
//! - Time value of money (locked capital until resolution)
//! - Liquidity premium (market makers require edge)
//! - Behavioral biases (traders prefer higher potential returns)
//!
//! # Key Features
//!
//! - **Dynamic Probability Threshold**: As expiry approaches, require higher certainty
//!   (90% at 24h -> 99% at 1h)
//! - **Kelly Criterion Sizing**: Optimal bet sizing based on edge and bankroll
//! - **Expected Value Filtering**: Only take trades with positive EV after fees
//! - **Liquidity Requirements**: Ensure sufficient depth to execute
//!
//! # Example
//!
//! ```rust,ignore
//! use prediction_market_arbitrage::grinding::{GrindingStrategy, GrindingConfig};
//!
//! let config = GrindingConfig::default();
//! let strategy = GrindingStrategy::new(config, execution_tx);
//!
//! // Scan markets for opportunities
//! let opportunities = strategy.scan_markets(&global_state, capital).await;
//!
//! // Execute best opportunity
//! if let Some(best) = opportunities.first() {
//!     strategy.execute(best).await?;
//! }
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::kalman::{KalmanConfig, MarketFilterMap};
use crate::types::{
    cents_to_price, GlobalState, PriceCents, NO_PRICE,
};

/// Platform to trade on (re-exported from types for grinding-specific use)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Kalshi,
    Polymarket,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Kalshi => write!(f, "KALSHI"),
            Platform::Polymarket => write!(f, "POLYMARKET"),
        }
    }
}

// =============================================================================
// CONFIGURATION
// =============================================================================

/// Configuration for the high-probability grinding strategy.
///
/// All thresholds are dynamically adjusted based on time to expiry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GrindingConfig {
    /// Base probability threshold (at max_hours_to_expiry).
    /// Actual threshold increases as expiry approaches.
    /// Default: 0.90 (90%)
    pub base_probability_threshold: f64,

    /// Maximum hours until resolution to consider a market.
    /// Markets further from expiry are ignored.
    /// Default: 24.0 hours
    pub max_hours_to_expiry: f64,

    /// Base minimum expected value percentage.
    /// Adjusted upward for larger position sizes.
    /// Default: 0.005 (0.5%)
    pub base_min_ev_pct: f64,

    /// Minimum USD liquidity required at target price.
    /// Default: 10.0
    pub min_liquidity_usd: f64,

    /// Kelly criterion multiplier (fraction of full Kelly).
    /// Default: 0.5 (half Kelly - safer)
    pub kelly_multiplier: f64,

    /// Maximum position size as fraction of capital.
    /// Default: 0.25 (25%)
    pub max_position_pct: f64,

    /// Minimum position size in USD.
    /// Default: 1.0 (Polymarket minimum)
    pub min_position_usd: f64,

    /// Whether to trade on Kalshi.
    /// Default: true
    pub enable_kalshi: bool,

    /// Whether to trade on Polymarket.
    /// Default: true
    pub enable_polymarket: bool,

    /// Cooldown between trades on same market (seconds).
    /// Default: 60
    pub trade_cooldown_secs: u64,

    /// Maximum Kalshi fee percentage to tolerate.
    /// Default: 0.02 (2%)
    pub max_kalshi_fee_pct: f64,
}

impl Default for GrindingConfig {
    fn default() -> Self {
        Self {
            base_probability_threshold: 0.90,
            max_hours_to_expiry: 24.0,
            base_min_ev_pct: 0.005,
            min_liquidity_usd: 10.0,
            kelly_multiplier: 0.5,
            max_position_pct: 0.25,
            min_position_usd: 1.0,
            enable_kalshi: true,
            enable_polymarket: true,
            trade_cooldown_secs: 60,
            max_kalshi_fee_pct: 0.02,
        }
    }
}

impl GrindingConfig {
    /// Create a conservative config for cautious trading.
    pub fn conservative() -> Self {
        Self {
            base_probability_threshold: 0.93,
            max_hours_to_expiry: 12.0,
            base_min_ev_pct: 0.01,
            min_liquidity_usd: 25.0,
            kelly_multiplier: 0.25,
            max_position_pct: 0.15,
            min_position_usd: 5.0,
            enable_kalshi: true,
            enable_polymarket: true,
            trade_cooldown_secs: 120,
            max_kalshi_fee_pct: 0.015,
        }
    }

    /// Create an aggressive config for more frequent trading.
    pub fn aggressive() -> Self {
        Self {
            base_probability_threshold: 0.88,
            max_hours_to_expiry: 48.0,
            base_min_ev_pct: 0.003,
            min_liquidity_usd: 5.0,
            kelly_multiplier: 0.75,
            max_position_pct: 0.35,
            min_position_usd: 1.0,
            enable_kalshi: true,
            enable_polymarket: true,
            trade_cooldown_secs: 30,
            max_kalshi_fee_pct: 0.025,
        }
    }

    /// Load config from environment variables.
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("GRINDING_BASE_PROB_THRESHOLD") {
            if let Ok(v) = val.parse() {
                config.base_probability_threshold = v;
            }
        }

        if let Ok(val) = std::env::var("GRINDING_MAX_HOURS") {
            if let Ok(v) = val.parse() {
                config.max_hours_to_expiry = v;
            }
        }

        if let Ok(val) = std::env::var("GRINDING_MIN_EV_PCT") {
            if let Ok(v) = val.parse() {
                config.base_min_ev_pct = v;
            }
        }

        if let Ok(val) = std::env::var("GRINDING_MIN_LIQUIDITY") {
            if let Ok(v) = val.parse() {
                config.min_liquidity_usd = v;
            }
        }

        if let Ok(val) = std::env::var("GRINDING_KELLY_MULT") {
            if let Ok(v) = val.parse() {
                config.kelly_multiplier = v;
            }
        }

        if let Ok(val) = std::env::var("GRINDING_MAX_POS_PCT") {
            if let Ok(v) = val.parse() {
                config.max_position_pct = v;
            }
        }

        config
    }

    /// Calculate dynamic probability threshold based on hours to expiry.
    ///
    /// Formula: threshold = base + (1.0 - base) * (1.0 - hours_remaining / max_hours)
    ///
    /// This creates a curve where:
    /// - At max_hours (24h): threshold = base (90%)
    /// - At 12h: threshold = 95%
    /// - At 6h: threshold = 97.5%
    /// - At 1h: threshold = ~99%
    /// - At 0h: threshold = 100%
    #[inline]
    pub fn dynamic_threshold(&self, hours_to_expiry: f64) -> f64 {
        let hours_clamped = hours_to_expiry.clamp(0.0, self.max_hours_to_expiry);
        let time_factor = 1.0 - (hours_clamped / self.max_hours_to_expiry);

        // threshold = base + (1 - base) * time_factor
        self.base_probability_threshold + (1.0 - self.base_probability_threshold) * time_factor
    }

    /// Calculate minimum EV threshold based on position size relative to capital.
    ///
    /// Larger positions require higher EV to justify the risk concentration.
    #[inline]
    pub fn dynamic_min_ev(&self, position_size: f64, capital: f64) -> f64 {
        let size_ratio = position_size / capital;
        self.base_min_ev_pct * (1.0 + size_ratio)
    }
}

// =============================================================================
// KELLY CRITERION
// =============================================================================

/// Kelly Criterion position sizing configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KellyConfig {
    /// Fraction of full Kelly to use (0.5 = half Kelly).
    pub kelly_multiplier: f64,

    /// Maximum single position as fraction of capital.
    pub max_position_pct: f64,

    /// Minimum position size in USD.
    pub min_position_usd: f64,
}

impl Default for KellyConfig {
    fn default() -> Self {
        Self {
            kelly_multiplier: 0.5,
            max_position_pct: 0.25,
            min_position_usd: 1.0,
        }
    }
}

/// Calculate optimal position size using Kelly Criterion.
///
/// Kelly formula: f* = (p * b - q) / b
/// where:
/// - p = probability of winning
/// - q = 1 - p (probability of losing)
/// - b = odds (payout ratio - 1, for 95c -> 5c profit: b = 0.0526)
///
/// # Arguments
/// * `true_probability` - Our estimated true probability of success
/// * `market_price` - Current market price (probability implied by market)
/// * `capital` - Available capital for betting
/// * `config` - Kelly configuration parameters
///
/// # Returns
/// Optimal position size in USD
pub fn kelly_size(
    true_probability: f64,
    market_price: f64,
    capital: f64,
    config: &KellyConfig,
) -> f64 {
    // Validate inputs
    if true_probability <= 0.0 || true_probability >= 1.0 {
        return 0.0;
    }
    if market_price <= 0.0 || market_price >= 1.0 {
        return 0.0;
    }
    if capital <= 0.0 {
        return 0.0;
    }

    // For prediction markets, when you buy YES at price P:
    // - You pay P per contract
    // - If YES wins, you get $1 (profit = 1 - P)
    // - If NO wins, you lose your stake P
    //
    // So odds b = (1 - P) / P

    let p = true_probability;
    let q = 1.0 - p;
    let price = market_price;

    // Odds: profit per dollar risked
    // At price 0.95, payout is 1.0, so profit = 0.05, odds = 0.05/0.95 = 0.0526
    let b = (1.0 - price) / price;

    if b <= 0.0 {
        return 0.0;
    }

    // Kelly fraction: f* = (p * b - q) / b
    let kelly_fraction = (p * b - q) / b;

    // If Kelly is negative or zero, no edge exists
    if kelly_fraction <= 0.0 {
        return 0.0;
    }

    // Apply fractional Kelly (risk management)
    let adjusted_fraction = kelly_fraction * config.kelly_multiplier;

    // Cap at maximum position percentage
    let capped_fraction = adjusted_fraction.min(config.max_position_pct);

    // Calculate position size
    let position_size = capital * capped_fraction;

    // Enforce minimum
    if position_size < config.min_position_usd {
        return 0.0;
    }

    position_size
}

/// Calculate expected value of a grinding trade.
///
/// EV = (true_prob * profit_if_win) - ((1 - true_prob) * loss_if_lose)
///
/// For a YES contract at price P:
/// - Win: profit = 1 - P - fees
/// - Lose: loss = P
///
/// # Arguments
/// * `true_probability` - Estimated true probability
/// * `market_price` - Current market price
/// * `fee_pct` - Fee percentage (0.0 for Polymarket, varies for Kalshi)
///
/// # Returns
/// Expected value per dollar invested
pub fn expected_value(true_probability: f64, market_price: f64, fee_pct: f64) -> f64 {
    if market_price <= 0.0 || market_price >= 1.0 {
        return 0.0;
    }

    let p = true_probability;
    let q = 1.0 - p;

    // Profit if win (per contract, adjusted for fees)
    let profit_if_win = (1.0 - market_price) * (1.0 - fee_pct);

    // Loss if lose (entire stake)
    let loss_if_lose = market_price;

    // Expected value per contract
    let ev = p * profit_if_win - q * loss_if_lose;

    // Return EV as percentage of investment
    ev / market_price
}

// =============================================================================
// OPPORTUNITY DETECTION
// =============================================================================

/// A detected grinding opportunity.
#[derive(Debug, Clone)]
pub struct GrindingOpportunity {
    /// Market identifier (index into GlobalState)
    pub market_id: u16,

    /// Market description
    pub description: String,

    /// Platform to trade on
    pub platform: Platform,

    /// Side to buy (always "yes" for high-prob grinding)
    pub side: Side,

    /// Current market price (ask)
    pub market_price: f64,

    /// Available liquidity in USD at this price
    pub liquidity_usd: f64,

    /// Kalman filter estimate of true probability
    pub estimated_probability: f64,

    /// Hours until market resolution
    pub hours_to_expiry: f64,

    /// Dynamic probability threshold for this expiry
    pub threshold: f64,

    /// Expected value percentage
    pub ev_pct: f64,

    /// Kelly-optimal position size in USD
    pub kelly_size_usd: f64,

    /// Recommended position size (may be less than Kelly due to liquidity)
    pub recommended_size_usd: f64,

    /// Estimated fee percentage
    pub fee_pct: f64,

    /// Market expiry time
    pub expiry_time: Option<DateTime<Utc>>,

    /// Confidence in the probability estimate (1/variance)
    pub confidence: f64,
}

impl GrindingOpportunity {
    /// Calculate the edge over market price.
    #[inline]
    pub fn edge(&self) -> f64 {
        self.estimated_probability - self.market_price
    }

    /// Calculate expected profit in USD.
    #[inline]
    pub fn expected_profit_usd(&self) -> f64 {
        self.ev_pct * self.recommended_size_usd
    }

    /// Score for ranking opportunities (higher is better).
    /// Combines EV, confidence, and time to expiry.
    pub fn score(&self) -> f64 {
        // Weight factors (tunable)
        const EV_WEIGHT: f64 = 100.0;
        const CONFIDENCE_WEIGHT: f64 = 1.0;
        const TIME_PENALTY: f64 = 0.1;

        let ev_score = self.ev_pct * EV_WEIGHT;
        let confidence_score = self.confidence.min(100.0) * CONFIDENCE_WEIGHT;
        let time_score = -self.hours_to_expiry * TIME_PENALTY;

        ev_score + confidence_score + time_score
    }
}

/// Side of the trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Yes,
    No,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Yes => write!(f, "YES"),
            Side::No => write!(f, "NO"),
        }
    }
}

// =============================================================================
// EXECUTION REQUEST
// =============================================================================

/// Request to execute a grinding trade.
#[derive(Debug, Clone)]
pub struct GrindingExecutionRequest {
    /// Market identifier
    pub market_id: u16,

    /// Platform to execute on
    pub platform: Platform,

    /// Side to buy
    pub side: Side,

    /// Maximum price to pay (limit)
    pub max_price: f64,

    /// Number of contracts to buy
    pub contracts: f64,

    /// Expected value percentage
    pub ev_pct: f64,

    /// Detection timestamp
    pub detected_at: DateTime<Utc>,

    /// Token address (for Polymarket)
    pub token_address: Option<String>,

    /// Market ticker (for Kalshi)
    pub market_ticker: Option<String>,
}

// =============================================================================
// STRATEGY IMPLEMENTATION
// =============================================================================

/// High-probability grinding strategy scanner.
///
/// Scans markets for opportunities matching the grinding criteria
/// and generates execution requests.
pub struct GrindingStrategy {
    config: GrindingConfig,
    kelly_config: KellyConfig,
    kalman_filters: Arc<MarketFilterMap>,
    /// Last trade time per market (for cooldown)
    last_trades: std::sync::RwLock<std::collections::HashMap<u16, std::time::Instant>>,
}

impl GrindingStrategy {
    /// Create a new grinding strategy.
    pub fn new(config: GrindingConfig, kalman_config: Option<KalmanConfig>) -> Self {
        let kalman_filters = Arc::new(MarketFilterMap::new(
            kalman_config.unwrap_or_default(),
        ));

        let kelly_config = KellyConfig {
            kelly_multiplier: config.kelly_multiplier,
            max_position_pct: config.max_position_pct,
            min_position_usd: config.min_position_usd,
        };

        Self {
            config,
            kelly_config,
            kalman_filters,
            last_trades: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Get the Kalman filter map for external probability updates.
    pub fn kalman_filters(&self) -> Arc<MarketFilterMap> {
        self.kalman_filters.clone()
    }

    /// Update probability estimate from price observation.
    pub fn update_probability(
        &self,
        market_id: u16,
        price: f64,
        hours_to_expiry: f64,
    ) -> f64 {
        self.kalman_filters.update(market_id, price, hours_to_expiry)
    }

    /// Check if market is in cooldown period.
    fn is_in_cooldown(&self, market_id: u16) -> bool {
        let last_trades = self.last_trades.read().unwrap();
        if let Some(last_trade) = last_trades.get(&market_id) {
            let elapsed = last_trade.elapsed();
            elapsed.as_secs() < self.config.trade_cooldown_secs
        } else {
            false
        }
    }

    /// Record a trade for cooldown tracking.
    pub fn record_trade(&self, market_id: u16) {
        let mut last_trades = self.last_trades.write().unwrap();
        last_trades.insert(market_id, std::time::Instant::now());
    }

    /// Scan all markets for grinding opportunities.
    ///
    /// Returns opportunities sorted by score (best first).
    pub fn scan_markets(
        &self,
        state: &GlobalState,
        capital: f64,
        market_expiries: &std::collections::HashMap<u16, DateTime<Utc>>,
    ) -> Vec<GrindingOpportunity> {
        let mut opportunities = Vec::new();
        let now = Utc::now();

        for market_id in 0..state.market_count() {
            let market_id = market_id as u16;

            // Skip markets in cooldown
            if self.is_in_cooldown(market_id) {
                continue;
            }

            // Get market state
            let Some(market) = state.get_by_id(market_id) else {
                continue;
            };

            // Get market pair info
            let Some(pair) = &market.pair else {
                continue;
            };

            // Calculate hours to expiry
            let hours_to_expiry = if let Some(expiry) = market_expiries.get(&market_id) {
                let duration = expiry.signed_duration_since(now);
                let hours = duration.num_minutes() as f64 / 60.0;
                if hours <= 0.0 || hours > self.config.max_hours_to_expiry {
                    continue; // Expired or too far out
                }
                hours
            } else {
                // No expiry info, use max as default
                self.config.max_hours_to_expiry
            };

            // Calculate dynamic threshold
            let threshold = self.config.dynamic_threshold(hours_to_expiry);

            // Check Kalshi opportunities
            if self.config.enable_kalshi {
                let (k_yes_ask, k_no_ask, k_yes_size, k_no_size) = market.kalshi.load();

                if k_yes_ask != NO_PRICE && k_yes_size > 0 {
                    if let Some(opp) = self.evaluate_opportunity(
                        market_id,
                        &pair.description,
                        Platform::Kalshi,
                        Side::Yes,
                        k_yes_ask,
                        k_yes_size,
                        hours_to_expiry,
                        threshold,
                        capital,
                        Some(market_expiries.get(&market_id).copied()),
                    ) {
                        opportunities.push(opp);
                    }
                }

                // Note: For grinding, we typically only buy high-probability YES
                // but could also buy high-probability NO (low YES probability)
                if k_no_ask != NO_PRICE && k_no_size > 0 {
                    // For NO side, we're betting the event WON'T happen
                    // So we want low YES probability, which means high NO probability
                    if let Some(opp) = self.evaluate_opportunity(
                        market_id,
                        &pair.description,
                        Platform::Kalshi,
                        Side::No,
                        k_no_ask,
                        k_no_size,
                        hours_to_expiry,
                        threshold,
                        capital,
                        Some(market_expiries.get(&market_id).copied()),
                    ) {
                        opportunities.push(opp);
                    }
                }
            }

            // Check Polymarket opportunities
            if self.config.enable_polymarket {
                let (p_yes_ask, p_no_ask, p_yes_size, p_no_size) = market.poly.load();

                if p_yes_ask != NO_PRICE && p_yes_size > 0 {
                    if let Some(opp) = self.evaluate_opportunity(
                        market_id,
                        &pair.description,
                        Platform::Polymarket,
                        Side::Yes,
                        p_yes_ask,
                        p_yes_size,
                        hours_to_expiry,
                        threshold,
                        capital,
                        Some(market_expiries.get(&market_id).copied()),
                    ) {
                        opportunities.push(opp);
                    }
                }

                if p_no_ask != NO_PRICE && p_no_size > 0 {
                    if let Some(opp) = self.evaluate_opportunity(
                        market_id,
                        &pair.description,
                        Platform::Polymarket,
                        Side::No,
                        p_no_ask,
                        p_no_size,
                        hours_to_expiry,
                        threshold,
                        capital,
                        Some(market_expiries.get(&market_id).copied()),
                    ) {
                        opportunities.push(opp);
                    }
                }
            }
        }

        // Sort by score (descending)
        opportunities.sort_by(|a, b| {
            b.score().partial_cmp(&a.score()).unwrap_or(std::cmp::Ordering::Equal)
        });

        opportunities
    }

    /// Evaluate a single market/side for grinding opportunity.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_opportunity(
        &self,
        market_id: u16,
        description: &str,
        platform: Platform,
        side: Side,
        ask_cents: PriceCents,
        size_cents: u16,
        hours_to_expiry: f64,
        threshold: f64,
        capital: f64,
        expiry_time: Option<Option<DateTime<Utc>>>,
    ) -> Option<GrindingOpportunity> {
        let market_price = cents_to_price(ask_cents);
        let liquidity_usd = size_cents as f64 / 100.0;

        // Check minimum liquidity
        if liquidity_usd < self.config.min_liquidity_usd {
            return None;
        }

        // Get probability estimate from Kalman filter
        // If no prior estimate, use the market price as initial estimate
        let estimated_probability = self.kalman_filters
            .estimate(market_id)
            .unwrap_or_else(|| {
                // Initialize with market price and update
                self.kalman_filters.update(market_id, market_price, hours_to_expiry)
            });

        // For NO side, we need to invert the probability interpretation
        // estimated_probability is for YES
        // If we're buying NO, we care about (1 - estimated_probability)
        let effective_probability = match side {
            Side::Yes => estimated_probability,
            Side::No => 1.0 - estimated_probability,
        };

        // Check if probability meets dynamic threshold
        if effective_probability < threshold {
            debug!(
                "[GRINDING] {} {} prob={:.2}% < threshold={:.2}%",
                description, side, effective_probability * 100.0, threshold * 100.0
            );
            return None;
        }

        // Calculate fee (Kalshi has variable fees, Polymarket has none)
        let fee_pct = match platform {
            Platform::Kalshi => kalshi_fee_pct(ask_cents),
            Platform::Polymarket => 0.0,
        };

        // Check fee tolerance for Kalshi
        if fee_pct > self.config.max_kalshi_fee_pct {
            return None;
        }

        // Calculate expected value
        let ev_pct = expected_value(effective_probability, market_price, fee_pct);

        // Check minimum EV (we'll use a preliminary size estimate)
        let preliminary_kelly = kelly_size(
            effective_probability,
            market_price,
            capital,
            &self.kelly_config,
        );

        let min_ev = self.config.dynamic_min_ev(preliminary_kelly, capital);
        if ev_pct < min_ev {
            debug!(
                "[GRINDING] {} {} ev={:.3}% < min_ev={:.3}%",
                description, side, ev_pct * 100.0, min_ev * 100.0
            );
            return None;
        }

        // Calculate Kelly-optimal size
        let kelly_size_usd = preliminary_kelly;

        // Recommended size is min of Kelly and available liquidity
        let recommended_size_usd = kelly_size_usd.min(liquidity_usd);

        // Get confidence from Kalman filter
        let confidence = self.kalman_filters
            .state(market_id)
            .map(|s| s.confidence())
            .unwrap_or(1.0);

        Some(GrindingOpportunity {
            market_id,
            description: description.to_string(),
            platform,
            side,
            market_price,
            liquidity_usd,
            estimated_probability: effective_probability,
            hours_to_expiry,
            threshold,
            ev_pct,
            kelly_size_usd,
            recommended_size_usd,
            fee_pct,
            expiry_time: expiry_time.flatten(),
            confidence,
        })
    }

    /// Create an execution request from an opportunity.
    pub fn create_execution_request(
        &self,
        opportunity: &GrindingOpportunity,
        state: &GlobalState,
    ) -> Option<GrindingExecutionRequest> {
        let market = state.get_by_id(opportunity.market_id)?;
        let pair = market.pair.as_ref()?;

        // Get token address or market ticker
        let (token_address, market_ticker) = match opportunity.platform {
            Platform::Polymarket => {
                let token = match opportunity.side {
                    Side::Yes => pair.poly_yes_token.to_string(),
                    Side::No => pair.poly_no_token.to_string(),
                };
                (Some(token), None)
            }
            Platform::Kalshi => (None, Some(pair.kalshi_market_ticker.to_string())),
        };

        // Calculate contracts (position_size / price)
        let contracts = opportunity.recommended_size_usd / opportunity.market_price;

        // Add small slippage tolerance to max price
        let max_price = opportunity.market_price * 1.005; // 0.5% slippage

        Some(GrindingExecutionRequest {
            market_id: opportunity.market_id,
            platform: opportunity.platform,
            side: opportunity.side,
            max_price,
            contracts,
            ev_pct: opportunity.ev_pct,
            detected_at: Utc::now(),
            token_address,
            market_ticker,
        })
    }
}

/// Calculate Kalshi fee percentage for a given price.
///
/// Fee formula: 0.07 * P * (1 - P)
/// This is a parabolic fee that peaks at 50% and decreases toward 0 and 100.
#[inline]
fn kalshi_fee_pct(price_cents: PriceCents) -> f64 {
    if price_cents == 0 || price_cents >= 100 {
        return 0.0;
    }
    let p = price_cents as f64 / 100.0;
    0.07 * p * (1.0 - p)
}

// =============================================================================
// ASYNC RUNNER
// =============================================================================

/// Configuration for the grinding strategy runner.
#[derive(Debug, Clone)]
pub struct GrindingRunnerConfig {
    /// How often to scan for opportunities (milliseconds)
    pub scan_interval_ms: u64,

    /// Maximum opportunities to process per scan
    pub max_opportunities_per_scan: usize,

    /// Whether to log opportunities even if not executed
    pub log_opportunities: bool,
}

impl Default for GrindingRunnerConfig {
    fn default() -> Self {
        Self {
            scan_interval_ms: 5000,
            max_opportunities_per_scan: 5,
            log_opportunities: true,
        }
    }
}

/// Run the grinding strategy in a loop.
///
/// This function continuously scans for opportunities and sends
/// execution requests through the provided channel.
pub async fn run_grinding_strategy(
    strategy: Arc<GrindingStrategy>,
    state: Arc<GlobalState>,
    execution_tx: mpsc::Sender<GrindingExecutionRequest>,
    runner_config: GrindingRunnerConfig,
    capital: Arc<std::sync::atomic::AtomicU64>, // Capital in cents
    market_expiries: Arc<std::sync::RwLock<std::collections::HashMap<u16, DateTime<Utc>>>>,
) {
    info!(
        "[GRINDING] Strategy runner started (scan_interval={}ms, max_per_scan={})",
        runner_config.scan_interval_ms, runner_config.max_opportunities_per_scan
    );

    let mut interval = tokio::time::interval(
        tokio::time::Duration::from_millis(runner_config.scan_interval_ms),
    );

    loop {
        interval.tick().await;

        // Get current capital
        let capital_cents = capital.load(std::sync::atomic::Ordering::Relaxed);
        let capital_usd = capital_cents as f64 / 100.0;

        if capital_usd < 1.0 {
            warn!("[GRINDING] Capital too low: ${:.2}", capital_usd);
            continue;
        }

        // Get market expiries
        let expiries = market_expiries.read().unwrap().clone();

        // Scan for opportunities
        let opportunities = strategy.scan_markets(&state, capital_usd, &expiries);

        if opportunities.is_empty() {
            debug!("[GRINDING] No opportunities found");
            continue;
        }

        if runner_config.log_opportunities {
            info!(
                "[GRINDING] Found {} opportunities (best: {} EV={:.2}% score={:.2})",
                opportunities.len(),
                opportunities[0].description,
                opportunities[0].ev_pct * 100.0,
                opportunities[0].score()
            );
        }

        // Process top opportunities
        for opportunity in opportunities.iter().take(runner_config.max_opportunities_per_scan) {
            // Create execution request
            let Some(request) = strategy.create_execution_request(opportunity, &state) else {
                warn!(
                    "[GRINDING] Failed to create execution request for {}",
                    opportunity.description
                );
                continue;
            };

            // Send to execution engine
            match execution_tx.try_send(request.clone()) {
                Ok(_) => {
                    info!(
                        "[GRINDING] Sent {} {} {} {:.0} contracts @ {:.2}c (EV={:.2}%)",
                        opportunity.platform,
                        opportunity.side,
                        opportunity.description,
                        request.contracts,
                        request.max_price * 100.0,
                        opportunity.ev_pct * 100.0
                    );

                    // Record trade for cooldown
                    strategy.record_trade(opportunity.market_id);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("[GRINDING] Execution channel full, skipping");
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("[GRINDING] Execution channel closed, stopping");
                    return;
                }
            }
        }
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // GrindingConfig Tests
    // =========================================================================

    #[test]
    fn test_config_default() {
        let config = GrindingConfig::default();
        assert!((config.base_probability_threshold - 0.90).abs() < 0.001);
        assert!((config.max_hours_to_expiry - 24.0).abs() < 0.001);
        assert!((config.base_min_ev_pct - 0.005).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_threshold_at_24h() {
        let config = GrindingConfig::default();
        let threshold = config.dynamic_threshold(24.0);
        // At 24h: threshold = base = 0.90
        assert!((threshold - 0.90).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_threshold_at_12h() {
        let config = GrindingConfig::default();
        let threshold = config.dynamic_threshold(12.0);
        // At 12h: threshold = 0.90 + 0.10 * 0.5 = 0.95
        assert!((threshold - 0.95).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_threshold_at_6h() {
        let config = GrindingConfig::default();
        let threshold = config.dynamic_threshold(6.0);
        // At 6h: threshold = 0.90 + 0.10 * 0.75 = 0.975
        assert!((threshold - 0.975).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_threshold_at_0h() {
        let config = GrindingConfig::default();
        let threshold = config.dynamic_threshold(0.0);
        // At 0h: threshold = 1.0
        assert!((threshold - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_threshold_beyond_max() {
        let config = GrindingConfig::default();
        let threshold = config.dynamic_threshold(48.0);
        // Beyond max hours, should clamp to base
        assert!((threshold - 0.90).abs() < 0.001);
    }

    #[test]
    fn test_dynamic_min_ev() {
        let config = GrindingConfig::default();

        // Small position: base EV
        let ev = config.dynamic_min_ev(10.0, 1000.0);
        assert!((ev - 0.005 * 1.01).abs() < 0.0001);

        // Larger position: higher EV required
        let ev = config.dynamic_min_ev(250.0, 1000.0);
        assert!((ev - 0.005 * 1.25).abs() < 0.0001);
    }

    // =========================================================================
    // Kelly Criterion Tests
    // =========================================================================

    #[test]
    fn test_kelly_size_with_edge() {
        let config = KellyConfig::default();

        // True probability 95%, market price 93%
        let size = kelly_size(0.95, 0.93, 1000.0, &config);

        // Should recommend a positive size
        assert!(size > 0.0);
        assert!(size <= 1000.0 * config.max_position_pct);
    }

    #[test]
    fn test_kelly_size_no_edge() {
        let config = KellyConfig::default();

        // True probability equals market price - no edge
        let size = kelly_size(0.90, 0.90, 1000.0, &config);

        // Should recommend zero (no edge)
        assert!((size - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_kelly_size_negative_edge() {
        let config = KellyConfig::default();

        // True probability below market price - negative edge
        let size = kelly_size(0.85, 0.90, 1000.0, &config);

        // Should recommend zero
        assert!((size - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_kelly_size_respects_max_position() {
        let config = KellyConfig {
            kelly_multiplier: 2.0, // Aggressive
            max_position_pct: 0.10, // But capped at 10%
            min_position_usd: 1.0,
        };

        // Very high edge should still be capped
        let size = kelly_size(0.99, 0.90, 1000.0, &config);
        assert!(size <= 1000.0 * 0.10 + 0.001);
    }

    #[test]
    fn test_kelly_size_respects_minimum() {
        let config = KellyConfig {
            kelly_multiplier: 0.001, // Very conservative
            max_position_pct: 0.25,
            min_position_usd: 10.0,
        };

        // Small edge with tiny multiplier would be below minimum
        let size = kelly_size(0.91, 0.90, 100.0, &config);

        // Should return zero (below minimum)
        assert!((size - 0.0).abs() < 0.001);
    }

    // =========================================================================
    // Expected Value Tests
    // =========================================================================

    #[test]
    fn test_expected_value_positive() {
        // True prob 95%, market price 90%, no fees
        let ev = expected_value(0.95, 0.90, 0.0);

        // EV should be positive
        assert!(ev > 0.0);
    }

    #[test]
    fn test_expected_value_zero_edge() {
        // True prob equals market - no edge
        let ev = expected_value(0.90, 0.90, 0.0);

        // EV should be approximately zero
        assert!(ev.abs() < 0.001);
    }

    #[test]
    fn test_expected_value_negative() {
        // True prob below market - negative edge
        let ev = expected_value(0.85, 0.90, 0.0);

        // EV should be negative
        assert!(ev < 0.0);
    }

    #[test]
    fn test_expected_value_with_fees() {
        // Same edge but with fees
        let ev_no_fees = expected_value(0.95, 0.90, 0.0);
        let ev_with_fees = expected_value(0.95, 0.90, 0.02);

        // EV with fees should be lower
        assert!(ev_with_fees < ev_no_fees);
        assert!(ev_with_fees > 0.0); // But still positive
    }

    // =========================================================================
    // Kalshi Fee Tests
    // =========================================================================

    #[test]
    fn test_kalshi_fee_at_50_cents() {
        // At 50 cents, fee = 0.07 * 0.5 * 0.5 = 0.0175
        let fee = kalshi_fee_pct(50);
        assert!((fee - 0.0175).abs() < 0.0001);
    }

    #[test]
    fn test_kalshi_fee_at_90_cents() {
        // At 90 cents, fee = 0.07 * 0.9 * 0.1 = 0.0063
        let fee = kalshi_fee_pct(90);
        assert!((fee - 0.0063).abs() < 0.0001);
    }

    #[test]
    fn test_kalshi_fee_at_extremes() {
        // At 0 and 100, fee should be 0
        assert!((kalshi_fee_pct(0) - 0.0).abs() < 0.0001);
        assert!((kalshi_fee_pct(100) - 0.0).abs() < 0.0001);
    }

    // =========================================================================
    // Opportunity Scoring Tests
    // =========================================================================

    #[test]
    fn test_opportunity_score() {
        let opp1 = GrindingOpportunity {
            market_id: 0,
            description: "Test 1".to_string(),
            platform: Platform::Polymarket,
            side: Side::Yes,
            market_price: 0.92,
            liquidity_usd: 100.0,
            estimated_probability: 0.95,
            hours_to_expiry: 6.0,
            threshold: 0.975,
            ev_pct: 0.02,
            kelly_size_usd: 50.0,
            recommended_size_usd: 50.0,
            fee_pct: 0.0,
            expiry_time: None,
            confidence: 10.0,
        };

        let opp2 = GrindingOpportunity {
            ev_pct: 0.01, // Lower EV
            hours_to_expiry: 12.0, // More time
            ..opp1.clone()
        };

        // Higher EV, less time should score higher
        assert!(opp1.score() > opp2.score());
    }

    #[test]
    fn test_opportunity_edge() {
        let opp = GrindingOpportunity {
            market_id: 0,
            description: "Test".to_string(),
            platform: Platform::Polymarket,
            side: Side::Yes,
            market_price: 0.90,
            liquidity_usd: 100.0,
            estimated_probability: 0.95,
            hours_to_expiry: 6.0,
            threshold: 0.975,
            ev_pct: 0.02,
            kelly_size_usd: 50.0,
            recommended_size_usd: 50.0,
            fee_pct: 0.0,
            expiry_time: None,
            confidence: 10.0,
        };

        assert!((opp.edge() - 0.05).abs() < 0.001);
    }

    // =========================================================================
    // Strategy Tests
    // =========================================================================

    #[test]
    fn test_strategy_cooldown() {
        let strategy = GrindingStrategy::new(GrindingConfig::default(), None);

        // Initially not in cooldown
        assert!(!strategy.is_in_cooldown(0));

        // Record trade
        strategy.record_trade(0);

        // Now in cooldown
        assert!(strategy.is_in_cooldown(0));

        // Different market not in cooldown
        assert!(!strategy.is_in_cooldown(1));
    }

    // =========================================================================
    // Integration Tests
    // =========================================================================

    #[test]
    fn test_full_kelly_calculation() {
        // Realistic scenario: 95% true prob, 92% market price, $1000 capital
        let config = KellyConfig::default();

        let size = kelly_size(0.95, 0.92, 1000.0, &config);
        let ev = expected_value(0.95, 0.92, 0.0);

        // Should have positive size and EV
        assert!(size > 0.0);
        assert!(ev > 0.0);

        // Size should be reasonable (not entire bankroll)
        assert!(size < 1000.0 * 0.30); // Less than 30%

        // Expected profit
        let expected_profit = ev * size;
        assert!(expected_profit > 0.0);

        println!(
            "Kelly size: ${:.2}, EV: {:.2}%, Expected profit: ${:.2}",
            size,
            ev * 100.0,
            expected_profit
        );
    }

    #[test]
    fn test_dynamic_threshold_table() {
        // Verify the threshold table from spec
        let config = GrindingConfig::default();

        let cases = [
            (24.0, 0.90),
            (12.0, 0.95),
            (6.0, 0.975),
            // At 1h: 0.90 + 0.10 * (23/24) = 0.9958...
        ];

        for (hours, expected) in cases {
            let actual = config.dynamic_threshold(hours);
            assert!(
                (actual - expected).abs() < 0.001,
                "At {}h: expected {:.2}%, got {:.2}%",
                hours,
                expected * 100.0,
                actual * 100.0
            );
        }

        // Special case: 1 hour
        let threshold_1h = config.dynamic_threshold(1.0);
        assert!(threshold_1h > 0.99, "At 1h threshold should be > 99%");
    }
}
