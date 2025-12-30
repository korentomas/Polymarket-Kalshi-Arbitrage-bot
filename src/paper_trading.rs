#![allow(dead_code)]

//! Paper trading module for simulated order execution.
//!
//! This module provides a complete paper trading implementation that mirrors
//! live execution but uses simulated fills with realistic slippage. It enables
//! risk-free testing of trading strategies with real market data.
//!
//! ## Features
//!
//! - Trait-based abstraction for swappable execution backends
//! - Realistic slippage simulation (0-0.5% random)
//! - Paper capital tracking with P&L calculation
//! - Simulated position management
//! - Full integration with existing execution flow
//!
//! ## Usage
//!
//! Paper trading is the DEFAULT mode (DRY_RUN=1). Set DRY_RUN=0 for live execution.

use anyhow::Result;
use chrono::Utc;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::types::{PriceCents, SizeCents, cents_to_price};

// =============================================================================
// TRADING MODE CONFIGURATION
// =============================================================================

/// Trading mode configuration determining whether to execute live or paper trades
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TradingMode {
    /// Whether to execute live trades (false = paper trading)
    pub live: bool,
    /// Starting capital for paper trading (in dollars)
    pub paper_capital: f64,
}

impl Default for TradingMode {
    fn default() -> Self {
        Self {
            live: false,  // Paper trading by default
            paper_capital: 100.0,  // $100 starting capital as per spec
        }
    }
}

impl TradingMode {
    /// Create from environment variables
    pub fn from_env() -> Self {
        let live = std::env::var("DRY_RUN")
            .map(|v| v == "0" || v.to_lowercase() == "false")
            .unwrap_or(false);  // Default to paper trading (DRY_RUN=1)

        let paper_capital = std::env::var("PAPER_CAPITAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100.0);

        Self { live, paper_capital }
    }

    /// Check if paper trading is enabled
    #[inline]
    pub fn is_paper(&self) -> bool {
        !self.live
    }

    /// Check if live trading is enabled
    #[inline]
    pub fn is_live(&self) -> bool {
        self.live
    }
}

// =============================================================================
// ORDER EXECUTOR TYPES
// =============================================================================

/// Result of an order execution attempt
#[derive(Debug, Clone)]
pub struct ExecutionFill {
    /// Unique order identifier
    pub order_id: String,
    /// Platform where order was executed
    pub platform: String,
    /// Side of the order (YES/NO)
    pub side: String,
    /// Number of contracts filled
    pub filled_contracts: i64,
    /// Total cost in cents
    pub fill_cost_cents: i64,
    /// Fill price per contract in cents
    pub fill_price_cents: PriceCents,
    /// Timestamp of fill (Unix timestamp in seconds)
    pub timestamp_secs: i64,
    /// Whether this was a simulated (paper) fill
    pub is_simulated: bool,
}

// =============================================================================
// SIMULATED FILL LOGIC
// =============================================================================

/// Configuration for simulated fill behavior
#[derive(Debug, Clone)]
pub struct SimulationConfig {
    /// Minimum slippage percentage (0.0 = 0%)
    pub min_slippage_pct: f64,
    /// Maximum slippage percentage (0.005 = 0.5%)
    pub max_slippage_pct: f64,
    /// Fill rate for simulated orders (0.0-1.0, where 1.0 = 100% fill)
    pub fill_rate: f64,
    /// Whether to simulate partial fills
    pub allow_partial_fills: bool,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            min_slippage_pct: 0.0,
            max_slippage_pct: 0.005,  // 0.5% max slippage as per spec
            fill_rate: 1.0,  // Assume full fills by default
            allow_partial_fills: false,
        }
    }
}

/// Simulated orderbook for paper trading
#[derive(Debug, Clone, Default)]
pub struct SimulatedOrderbook {
    /// Best bid price in cents
    pub best_bid: PriceCents,
    /// Best ask price in cents
    pub best_ask: PriceCents,
    /// Available size at best bid
    pub bid_size: SizeCents,
    /// Available size at best ask
    pub ask_size: SizeCents,
}

impl SimulatedOrderbook {
    /// Create orderbook from bid/ask prices
    pub fn new(best_bid: PriceCents, best_ask: PriceCents, bid_size: SizeCents, ask_size: SizeCents) -> Self {
        Self { best_bid, best_ask, bid_size, ask_size }
    }

    /// Calculate mid price in cents
    #[inline]
    pub fn mid_price(&self) -> f64 {
        (self.best_bid as f64 + self.best_ask as f64) / 2.0
    }
}

/// Simulated fill result from paper trading
#[derive(Debug, Clone)]
pub struct SimulatedFill {
    /// Fill price per contract (includes slippage)
    pub price: f64,
    /// Number of contracts filled
    pub quantity: i64,
    /// Timestamp of simulated fill (Unix seconds)
    pub timestamp_secs: i64,
    /// Applied slippage percentage
    pub slippage_applied: f64,
}

/// Simulate a fill with realistic slippage
///
/// Uses the mid-price from the orderbook and applies random slippage
/// in the range [0, 0.5%] as specified in the SPEC.
pub fn simulate_fill(
    side: &str,
    _price_cents: PriceCents,
    quantity: i64,
    orderbook: &SimulatedOrderbook,
    config: &SimulationConfig,
) -> SimulatedFill {
    let mut rng = rand::thread_rng();

    // Calculate base price from orderbook mid-price
    let base_price = orderbook.mid_price();

    // Generate random slippage within configured range
    let slippage = rng.gen_range(config.min_slippage_pct..=config.max_slippage_pct);

    // Apply slippage based on order side
    // Buy orders pay slightly more, sell orders receive slightly less
    let fill_price = match side.to_lowercase().as_str() {
        "buy" | "yes" => base_price * (1.0 + slippage),
        "sell" | "no" => base_price * (1.0 - slippage),
        _ => base_price,  // Unknown side, no slippage adjustment
    };

    // Clamp fill price to reasonable bounds (1-99 cents)
    let clamped_price = fill_price.clamp(1.0, 99.0);

    // Calculate filled quantity (may be partial based on config)
    let filled_qty = if config.allow_partial_fills {
        let fill_rate = rng.gen_range(config.fill_rate..=1.0);
        ((quantity as f64) * fill_rate).floor() as i64
    } else {
        quantity
    };

    SimulatedFill {
        price: clamped_price,
        quantity: filled_qty,
        timestamp_secs: Utc::now().timestamp(),
        slippage_applied: slippage,
    }
}

// =============================================================================
// PAPER POSITION TRACKING
// =============================================================================

/// A single paper trading position leg
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaperPositionLeg {
    /// Number of contracts held
    pub contracts: f64,
    /// Total cost basis (in dollars)
    pub cost_basis: f64,
    /// Average entry price per contract
    pub avg_price: f64,
}

impl PaperPositionLeg {
    /// Add contracts to this position leg
    pub fn add(&mut self, contracts: f64, price: f64) {
        let cost = contracts * price;
        self.cost_basis += cost;
        self.contracts += contracts;
        if self.contracts > 0.0 {
            self.avg_price = self.cost_basis / self.contracts;
        }
    }

    /// Remove contracts from this position leg (for closing/selling)
    #[allow(dead_code)]
    pub fn remove(&mut self, contracts: f64) -> f64 {
        let contracts_to_remove = contracts.min(self.contracts);
        if contracts_to_remove <= 0.0 {
            return 0.0;
        }
        let cost_removed = contracts_to_remove * self.avg_price;
        self.contracts -= contracts_to_remove;
        self.cost_basis -= cost_removed;
        cost_removed
    }

    /// Calculate unrealized P&L at current price
    #[allow(dead_code)]
    pub fn unrealized_pnl(&self, current_price: f64) -> f64 {
        (self.contracts * current_price) - self.cost_basis
    }

    /// Calculate value if this position resolves as winner ($1 per contract)
    pub fn value_if_win(&self) -> f64 {
        self.contracts
    }

    /// Calculate profit if this position wins
    #[allow(dead_code)]
    pub fn profit_if_win(&self) -> f64 {
        self.value_if_win() - self.cost_basis
    }
}

/// A complete paper trading position (both platforms, YES and NO)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaperPosition {
    /// Market identifier
    pub market_id: String,
    /// Human-readable description
    pub description: String,

    /// Kalshi YES position
    pub kalshi_yes: PaperPositionLeg,
    /// Kalshi NO position
    pub kalshi_no: PaperPositionLeg,
    /// Polymarket YES position
    pub poly_yes: PaperPositionLeg,
    /// Polymarket NO position
    pub poly_no: PaperPositionLeg,

    /// Total fees paid
    pub total_fees: f64,
    /// When position was opened (Unix timestamp)
    pub opened_at_secs: i64,
    /// Position status: "open", "closed", "resolved"
    pub status: String,
    /// Realized P&L (set when closed/resolved)
    pub realized_pnl: Option<f64>,
}

impl PaperPosition {
    /// Create a new paper position
    pub fn new(market_id: &str, description: &str) -> Self {
        Self {
            market_id: market_id.to_string(),
            description: description.to_string(),
            status: "open".to_string(),
            opened_at_secs: Utc::now().timestamp(),
            ..Default::default()
        }
    }

    /// Total contracts across all legs
    #[allow(dead_code)]
    pub fn total_contracts(&self) -> f64 {
        self.kalshi_yes.contracts + self.kalshi_no.contracts +
        self.poly_yes.contracts + self.poly_no.contracts
    }

    /// Total cost basis including fees
    pub fn total_cost(&self) -> f64 {
        self.kalshi_yes.cost_basis + self.kalshi_no.cost_basis +
        self.poly_yes.cost_basis + self.poly_no.cost_basis +
        self.total_fees
    }

    /// Number of matched contract pairs (guaranteed profit from arb)
    pub fn matched_contracts(&self) -> f64 {
        let yes_total = self.kalshi_yes.contracts + self.poly_yes.contracts;
        let no_total = self.kalshi_no.contracts + self.poly_no.contracts;
        yes_total.min(no_total)
    }

    /// Guaranteed profit from matched arb positions
    pub fn guaranteed_profit(&self) -> f64 {
        self.matched_contracts() - self.total_cost()
    }

    /// Unmatched exposure (contracts without offsetting position)
    pub fn unmatched_exposure(&self) -> f64 {
        let yes_total = self.kalshi_yes.contracts + self.poly_yes.contracts;
        let no_total = self.kalshi_no.contracts + self.poly_no.contracts;
        (yes_total - no_total).abs()
    }

    /// Mark position as resolved
    pub fn resolve(&mut self, yes_won: bool) {
        let payout = if yes_won {
            self.kalshi_yes.contracts + self.poly_yes.contracts
        } else {
            self.kalshi_no.contracts + self.poly_no.contracts
        };
        self.realized_pnl = Some(payout - self.total_cost());
        self.status = "resolved".to_string();
    }
}

// =============================================================================
// PAPER CAPITAL TRACKER
// =============================================================================

/// Paper trading statistics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaperStats {
    /// Total number of simulated trades
    pub total_trades: u64,
    /// Number of winning trades
    pub winning_trades: u64,
    /// Number of losing trades
    pub losing_trades: u64,
    /// Total realized profit (in dollars)
    pub realized_profit: f64,
    /// Total realized loss (in dollars)
    pub realized_loss: f64,
    /// Maximum drawdown encountered
    pub max_drawdown: f64,
    /// Peak capital achieved
    pub peak_capital: f64,
    /// Trading start time (Unix timestamp)
    pub started_at_secs: Option<i64>,
}

impl PaperStats {
    /// Calculate win rate as percentage
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }

    /// Calculate net P&L
    pub fn net_pnl(&self) -> f64 {
        self.realized_profit - self.realized_loss.abs()
    }

    /// Calculate profit factor (gross profit / gross loss)
    pub fn profit_factor(&self) -> f64 {
        if self.realized_loss.abs() < 0.0001 {
            return f64::INFINITY;
        }
        self.realized_profit / self.realized_loss.abs()
    }
}

/// Paper capital and position tracker
#[derive(Debug, Serialize, Deserialize)]
pub struct PaperCapitalTracker {
    /// Current available capital (in dollars)
    pub capital: f64,
    /// Initial starting capital
    pub starting_capital: f64,
    /// All paper positions keyed by market_id
    pub positions: HashMap<String, PaperPosition>,
    /// Accumulated trading statistics
    pub stats: PaperStats,
    /// Daily P&L tracking
    pub daily_pnl: f64,
    /// Current trading date
    pub trading_date: String,
}

impl Default for PaperCapitalTracker {
    fn default() -> Self {
        Self::new(100.0)  // Default $100 starting capital
    }
}

impl PaperCapitalTracker {
    /// Create new tracker with starting capital
    pub fn new(starting_capital: f64) -> Self {
        Self {
            capital: starting_capital,
            starting_capital,
            positions: HashMap::new(),
            stats: PaperStats {
                peak_capital: starting_capital,
                started_at_secs: Some(Utc::now().timestamp()),
                ..Default::default()
            },
            daily_pnl: 0.0,
            trading_date: Utc::now().format("%Y-%m-%d").to_string(),
        }
    }

    /// Load from file or create new
    pub fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                match serde_json::from_str::<Self>(&contents) {
                    Ok(mut tracker) => {
                        // Check for new trading day
                        let today = Utc::now().format("%Y-%m-%d").to_string();
                        if tracker.trading_date != today {
                            info!("[PAPER] New trading day, resetting daily P&L");
                            tracker.daily_pnl = 0.0;
                            tracker.trading_date = today;
                        }
                        info!("[PAPER] Loaded paper trading state: ${:.2} capital, {} positions",
                              tracker.capital, tracker.positions.len());
                        tracker
                    }
                    Err(e) => {
                        warn!("[PAPER] Failed to parse state file: {}", e);
                        Self::default()
                    }
                }
            }
            Err(_) => {
                info!("[PAPER] No paper trading state file found, starting fresh");
                Self::default()
            }
        }
    }

    /// Save to file
    pub fn save(&self, path: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Record a simulated fill
    pub fn record_fill(
        &mut self,
        market_id: &str,
        description: &str,
        platform: &str,
        side: &str,
        contracts: f64,
        price: f64,
        fees: f64,
    ) {
        let position = self.positions
            .entry(market_id.to_string())
            .or_insert_with(|| PaperPosition::new(market_id, description));

        // Add to appropriate leg
        match (platform.to_lowercase().as_str(), side.to_lowercase().as_str()) {
            ("kalshi", "yes") => position.kalshi_yes.add(contracts, price),
            ("kalshi", "no") => position.kalshi_no.add(contracts, price),
            ("polymarket", "yes") => position.poly_yes.add(contracts, price),
            ("polymarket", "no") => position.poly_no.add(contracts, price),
            _ => warn!("[PAPER] Unknown platform/side: {}/{}", platform, side),
        }

        position.total_fees += fees;

        // Deduct cost from capital
        let total_cost = (contracts * price) + fees;
        self.capital -= total_cost;

        debug!("[PAPER] Recorded fill: {} {} {} @{:.4} x{:.0} (cost: ${:.4})",
               platform, side, market_id, price, contracts, total_cost);
    }

    /// Record realized P&L from a closed position
    pub fn record_pnl(&mut self, pnl: f64) {
        self.stats.total_trades += 1;

        if pnl >= 0.0 {
            self.stats.winning_trades += 1;
            self.stats.realized_profit += pnl;
        } else {
            self.stats.losing_trades += 1;
            self.stats.realized_loss += pnl.abs();
        }

        self.capital += pnl;
        self.daily_pnl += pnl;

        // Update peak and drawdown
        if self.capital > self.stats.peak_capital {
            self.stats.peak_capital = self.capital;
        }

        let drawdown = (self.stats.peak_capital - self.capital) / self.stats.peak_capital;
        if drawdown > self.stats.max_drawdown {
            self.stats.max_drawdown = drawdown;
        }
    }

    /// Resolve a position with outcome
    #[allow(dead_code)]
    pub fn resolve_position(&mut self, market_id: &str, yes_won: bool) -> Option<f64> {
        if let Some(position) = self.positions.get_mut(market_id) {
            position.resolve(yes_won);
            let pnl = position.realized_pnl?;
            self.record_pnl(pnl);
            info!("[PAPER] Resolved {}: {} won, P&L: ${:.2}",
                  market_id, if yes_won { "YES" } else { "NO" }, pnl);
            Some(pnl)
        } else {
            None
        }
    }

    /// Get current capital
    pub fn get_capital(&self) -> f64 {
        self.capital
    }

    /// Get total P&L since start
    #[allow(dead_code)]
    pub fn total_pnl(&self) -> f64 {
        self.capital - self.starting_capital
    }

    /// Get return percentage
    #[allow(dead_code)]
    pub fn return_pct(&self) -> f64 {
        if self.starting_capital < 0.0001 {
            return 0.0;
        }
        ((self.capital - self.starting_capital) / self.starting_capital) * 100.0
    }

    /// Get summary of open positions
    #[allow(dead_code)]
    pub fn open_positions_summary(&self) -> (usize, f64, f64) {
        let open: Vec<_> = self.positions.values()
            .filter(|p| p.status == "open")
            .collect();

        let count = open.len();
        let total_cost: f64 = open.iter().map(|p| p.total_cost()).sum();
        let guaranteed_profit: f64 = open.iter().map(|p| p.guaranteed_profit()).sum();

        (count, total_cost, guaranteed_profit)
    }

    /// Check if we have sufficient capital for a trade
    #[allow(dead_code)]
    pub fn can_afford(&self, cost: f64) -> bool {
        self.capital >= cost
    }

    /// Reset daily counters (call at midnight)
    #[allow(dead_code)]
    pub fn reset_daily(&mut self) {
        self.daily_pnl = 0.0;
        self.trading_date = Utc::now().format("%Y-%m-%d").to_string();
    }
}

// =============================================================================
// PAPER ORDER EXECUTOR
// =============================================================================

/// Paper trading order executor
pub struct PaperOrderExecutor {
    /// Capital and position tracker
    tracker: Arc<RwLock<PaperCapitalTracker>>,
    /// Simulation configuration
    config: SimulationConfig,
    /// Order counter for generating unique IDs
    order_counter: std::sync::atomic::AtomicU64,
}

impl PaperOrderExecutor {
    /// Create new paper executor with given starting capital
    pub fn new(starting_capital: f64) -> Self {
        Self {
            tracker: Arc::new(RwLock::new(PaperCapitalTracker::new(starting_capital))),
            config: SimulationConfig::default(),
            order_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create from existing tracker
    #[allow(dead_code)]
    pub fn with_tracker(tracker: Arc<RwLock<PaperCapitalTracker>>) -> Self {
        Self {
            tracker,
            config: SimulationConfig::default(),
            order_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Load state from file
    #[allow(dead_code)]
    pub fn load(path: &str) -> Self {
        let tracker = PaperCapitalTracker::load(path);
        Self {
            tracker: Arc::new(RwLock::new(tracker)),
            config: SimulationConfig::default(),
            order_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get reference to tracker
    pub fn tracker(&self) -> Arc<RwLock<PaperCapitalTracker>> {
        self.tracker.clone()
    }

    /// Save current state
    #[allow(dead_code)]
    pub async fn save(&self, path: &str) -> Result<()> {
        let tracker = self.tracker.read().await;
        tracker.save(path)
    }

    /// Generate unique order ID
    fn next_order_id(&self) -> String {
        let counter = self.order_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("paper_{}", counter)
    }

    /// Execute a simulated order
    async fn execute_simulated(
        &self,
        platform: &str,
        ticker_or_token: &str,
        side: &str,
        is_buy: bool,
        price_cents: PriceCents,
        contracts: i64,
    ) -> Result<ExecutionFill> {
        // Create a simulated orderbook from the limit price
        // In real paper trading, we'd get actual orderbook data
        let orderbook = SimulatedOrderbook::new(
            price_cents.saturating_sub(1).max(1),  // Bid slightly below
            price_cents.saturating_add(1).min(99), // Ask slightly above
            (contracts * 100) as SizeCents,
            (contracts * 100) as SizeCents,
        );

        let fill_side = if is_buy { "buy" } else { "sell" };
        let fill = simulate_fill(fill_side, price_cents, contracts, &orderbook, &self.config);

        let order_id = self.next_order_id();
        let fill_price_cents = fill.price.round() as PriceCents;
        let fill_cost_cents = (fill.quantity as f64 * fill.price).round() as i64;

        // Record in tracker
        {
            let mut tracker = self.tracker.write().await;
            if is_buy {
                tracker.record_fill(
                    ticker_or_token,
                    ticker_or_token,  // Use token/ticker as description
                    platform,
                    side,
                    fill.quantity as f64,
                    cents_to_price(fill_price_cents),
                    0.0,  // Simulated fees handled separately
                );
            }
        }

        info!("[PAPER] {} {} {} {} @{}c x{} (slippage: {:.3}%)",
              if is_buy { "BUY" } else { "SELL" },
              platform, side, ticker_or_token,
              fill_price_cents, fill.quantity,
              fill.slippage_applied * 100.0);

        Ok(ExecutionFill {
            order_id,
            platform: platform.to_string(),
            side: side.to_string(),
            filled_contracts: fill.quantity,
            fill_cost_cents,
            fill_price_cents,
            timestamp_secs: fill.timestamp_secs,
            is_simulated: true,
        })
    }

    /// Execute a buy order (paper)
    pub async fn buy(
        &self,
        platform: &str,
        ticker_or_token: &str,
        side: &str,
        price_cents: PriceCents,
        contracts: i64,
    ) -> Result<ExecutionFill> {
        self.execute_simulated(platform, ticker_or_token, side, true, price_cents, contracts).await
    }

    /// Execute a sell order (paper)
    pub async fn sell(
        &self,
        platform: &str,
        ticker_or_token: &str,
        side: &str,
        price_cents: PriceCents,
        contracts: i64,
    ) -> Result<ExecutionFill> {
        self.execute_simulated(platform, ticker_or_token, side, false, price_cents, contracts).await
    }

    /// Check if this is paper mode
    #[allow(dead_code)]
    pub fn is_paper(&self) -> bool {
        true
    }

    /// Get current capital
    #[allow(dead_code)]
    pub async fn get_capital(&self) -> f64 {
        self.tracker.read().await.get_capital()
    }
}

// =============================================================================
// HYBRID EXECUTOR (LIVE OR PAPER)
// =============================================================================

/// Execution backend that can switch between live and paper modes
pub enum HybridExecutor {
    /// Live execution using real clients
    Live {
        kalshi: Arc<crate::kalshi::KalshiApiClient>,
        poly: Arc<crate::polymarket_clob::SharedAsyncClient>,
    },
    /// Paper execution with simulation
    Paper(PaperOrderExecutor),
}

impl HybridExecutor {
    /// Create from trading mode and clients
    pub fn new(
        mode: &TradingMode,
        kalshi: Arc<crate::kalshi::KalshiApiClient>,
        poly: Arc<crate::polymarket_clob::SharedAsyncClient>,
    ) -> Self {
        if mode.is_live() {
            Self::Live { kalshi, poly }
        } else {
            Self::Paper(PaperOrderExecutor::new(mode.paper_capital))
        }
    }

    /// Create paper executor only
    #[allow(dead_code)]
    pub fn paper(starting_capital: f64) -> Self {
        Self::Paper(PaperOrderExecutor::new(starting_capital))
    }

    /// Check if this is paper mode
    pub fn is_paper(&self) -> bool {
        matches!(self, Self::Paper(_))
    }

    /// Get paper tracker (if in paper mode)
    pub fn paper_tracker(&self) -> Option<Arc<RwLock<PaperCapitalTracker>>> {
        match self {
            Self::Paper(executor) => Some(executor.tracker()),
            Self::Live { .. } => None,
        }
    }

    /// Execute buy on Kalshi
    pub async fn kalshi_buy(
        &self,
        ticker: &str,
        side: &str,
        price_cents: i64,
        contracts: i64,
    ) -> Result<ExecutionFill> {
        match self {
            Self::Live { kalshi, .. } => {
                let resp = kalshi.buy_ioc(ticker, side, price_cents, contracts).await?;
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0)
                    + resp.order.maker_fill_cost.unwrap_or(0);

                Ok(ExecutionFill {
                    order_id: resp.order.order_id,
                    platform: "kalshi".to_string(),
                    side: side.to_string(),
                    filled_contracts: filled,
                    fill_cost_cents: cost,
                    fill_price_cents: if filled > 0 { (cost / filled) as PriceCents } else { 0 },
                    timestamp_secs: Utc::now().timestamp(),
                    is_simulated: false,
                })
            }
            Self::Paper(executor) => {
                executor.buy("kalshi", ticker, side, price_cents as PriceCents, contracts).await
            }
        }
    }

    /// Execute sell on Kalshi
    #[allow(dead_code)]
    pub async fn kalshi_sell(
        &self,
        ticker: &str,
        side: &str,
        price_cents: i64,
        contracts: i64,
    ) -> Result<ExecutionFill> {
        match self {
            Self::Live { kalshi, .. } => {
                let resp = kalshi.sell_ioc(ticker, side, price_cents, contracts).await?;
                let filled = resp.order.filled_count();
                let cost = resp.order.taker_fill_cost.unwrap_or(0)
                    + resp.order.maker_fill_cost.unwrap_or(0);

                Ok(ExecutionFill {
                    order_id: resp.order.order_id,
                    platform: "kalshi".to_string(),
                    side: side.to_string(),
                    filled_contracts: filled,
                    fill_cost_cents: cost,
                    fill_price_cents: if filled > 0 { (cost / filled) as PriceCents } else { 0 },
                    timestamp_secs: Utc::now().timestamp(),
                    is_simulated: false,
                })
            }
            Self::Paper(executor) => {
                executor.sell("kalshi", ticker, side, price_cents as PriceCents, contracts).await
            }
        }
    }

    /// Execute buy on Polymarket
    pub async fn poly_buy(
        &self,
        token_id: &str,
        price: f64,
        contracts: f64,
    ) -> Result<ExecutionFill> {
        match self {
            Self::Live { poly, .. } => {
                let fill = poly.buy_fak(token_id, price, contracts).await?;

                Ok(ExecutionFill {
                    order_id: fill.order_id,
                    platform: "polymarket".to_string(),
                    side: "yes".to_string(),  // Token determines actual side
                    filled_contracts: fill.filled_size as i64,
                    fill_cost_cents: (fill.fill_cost * 100.0) as i64,
                    fill_price_cents: if fill.filled_size > 0.0 {
                        ((fill.fill_cost / fill.filled_size) * 100.0) as PriceCents
                    } else {
                        0
                    },
                    timestamp_secs: Utc::now().timestamp(),
                    is_simulated: false,
                })
            }
            Self::Paper(executor) => {
                executor.buy(
                    "polymarket",
                    token_id,
                    "yes",
                    (price * 100.0).round() as PriceCents,
                    contracts as i64,
                ).await
            }
        }
    }

    /// Execute sell on Polymarket
    #[allow(dead_code)]
    pub async fn poly_sell(
        &self,
        token_id: &str,
        price: f64,
        contracts: f64,
    ) -> Result<ExecutionFill> {
        match self {
            Self::Live { poly, .. } => {
                let fill = poly.sell_fak(token_id, price, contracts).await?;

                Ok(ExecutionFill {
                    order_id: fill.order_id,
                    platform: "polymarket".to_string(),
                    side: "yes".to_string(),
                    filled_contracts: fill.filled_size as i64,
                    fill_cost_cents: (fill.fill_cost * 100.0) as i64,
                    fill_price_cents: if fill.filled_size > 0.0 {
                        ((fill.fill_cost / fill.filled_size) * 100.0) as PriceCents
                    } else {
                        0
                    },
                    timestamp_secs: Utc::now().timestamp(),
                    is_simulated: false,
                })
            }
            Self::Paper(executor) => {
                executor.sell(
                    "polymarket",
                    token_id,
                    "yes",
                    (price * 100.0).round() as PriceCents,
                    contracts as i64,
                ).await
            }
        }
    }
}

// =============================================================================
// PAPER TRADING STATE FILE
// =============================================================================

/// Default file path for paper trading state
pub const PAPER_STATE_FILE: &str = "paper_trading_state.json";

/// Save paper trading state to default file
#[allow(dead_code)]
pub async fn save_paper_state(tracker: &Arc<RwLock<PaperCapitalTracker>>) -> Result<()> {
    let guard = tracker.read().await;
    guard.save(PAPER_STATE_FILE)
}

/// Load paper trading state from default file
#[allow(dead_code)]
pub fn load_paper_state() -> PaperCapitalTracker {
    PaperCapitalTracker::load(PAPER_STATE_FILE)
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trading_mode_default() {
        let mode = TradingMode::default();
        assert!(!mode.live);
        assert!(mode.is_paper());
        assert_eq!(mode.paper_capital, 100.0);
    }

    #[test]
    fn test_simulate_fill_buy() {
        let orderbook = SimulatedOrderbook::new(49, 51, 1000, 1000);
        let config = SimulationConfig::default();

        let fill = simulate_fill("buy", 50, 10, &orderbook, &config);

        assert_eq!(fill.quantity, 10);
        assert!(fill.price >= 50.0);  // Buy should pay at or above mid
        assert!(fill.price <= 50.5);  // But not more than 0.5% above
        assert!(fill.slippage_applied >= 0.0);
        assert!(fill.slippage_applied <= 0.005);
    }

    #[test]
    fn test_simulate_fill_sell() {
        let orderbook = SimulatedOrderbook::new(49, 51, 1000, 1000);
        let config = SimulationConfig::default();

        let fill = simulate_fill("sell", 50, 10, &orderbook, &config);

        assert_eq!(fill.quantity, 10);
        assert!(fill.price <= 50.0);  // Sell should receive at or below mid
        assert!(fill.price >= 49.75); // But not less than 0.5% below
    }

    #[test]
    fn test_paper_position_leg() {
        let mut leg = PaperPositionLeg::default();

        leg.add(10.0, 0.45);
        assert_eq!(leg.contracts, 10.0);
        assert!((leg.cost_basis - 4.5).abs() < 0.001);
        assert!((leg.avg_price - 0.45).abs() < 0.001);

        // Add more at different price
        leg.add(10.0, 0.55);
        assert_eq!(leg.contracts, 20.0);
        assert!((leg.cost_basis - 10.0).abs() < 0.001);
        assert!((leg.avg_price - 0.50).abs() < 0.001);
    }

    #[test]
    fn test_paper_position_arb_profit() {
        let mut pos = PaperPosition::new("TEST", "Test Market");

        // Simulate arb: buy YES on Poly at 45c, buy NO on Kalshi at 50c
        pos.poly_yes.add(10.0, 0.45);
        pos.kalshi_no.add(10.0, 0.50);

        assert!((pos.total_cost() - 9.50).abs() < 0.001);
        assert!((pos.matched_contracts() - 10.0).abs() < 0.001);
        assert!((pos.guaranteed_profit() - 0.50).abs() < 0.001);
        assert!((pos.unmatched_exposure() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_paper_capital_tracker() {
        let mut tracker = PaperCapitalTracker::new(100.0);

        assert_eq!(tracker.capital, 100.0);
        assert_eq!(tracker.starting_capital, 100.0);

        // Record a fill
        tracker.record_fill("TEST", "Test", "polymarket", "yes", 10.0, 0.45, 0.0);

        // Should deduct cost
        assert!((tracker.capital - 95.50).abs() < 0.001);

        // Record P&L
        tracker.record_pnl(0.50);
        assert!((tracker.capital - 96.00).abs() < 0.001);
        assert_eq!(tracker.stats.winning_trades, 1);
    }

    #[test]
    fn test_paper_stats_calculations() {
        let mut stats = PaperStats::default();

        stats.total_trades = 10;
        stats.winning_trades = 7;
        stats.losing_trades = 3;
        stats.realized_profit = 7.0;
        stats.realized_loss = 3.0;

        assert!((stats.win_rate() - 70.0).abs() < 0.001);
        assert!((stats.net_pnl() - 4.0).abs() < 0.001);
        assert!((stats.profit_factor() - 7.0/3.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_paper_executor() {
        let executor = PaperOrderExecutor::new(100.0);

        let fill = executor.buy("kalshi", "TEST-TICKER", "yes", 50, 10).await.unwrap();

        assert!(fill.is_simulated);
        assert_eq!(fill.platform, "kalshi");
        assert_eq!(fill.side, "yes");
        assert!(fill.filled_contracts > 0);
    }
}
