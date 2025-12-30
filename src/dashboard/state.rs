//! Shared application state for the dashboard.
//!
//! This module defines the state that is shared across all dashboard handlers
//! and provides thread-safe access to trading system data.

use crate::circuit_breaker::CircuitBreaker;
use crate::persistence::Database;
use crate::position_tracker::PositionTracker;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Trading mode (paper vs live)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradingMode {
    Paper,
    Live,
}

impl Default for TradingMode {
    fn default() -> Self {
        TradingMode::Paper
    }
}

impl std::fmt::Display for TradingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradingMode::Paper => write!(f, "paper"),
            TradingMode::Live => write!(f, "live"),
        }
    }
}

/// System status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemStatus {
    Running,
    Halted,
    Starting,
    Error,
}

impl Default for SystemStatus {
    fn default() -> Self {
        SystemStatus::Starting
    }
}

/// Trading opportunity detected by the scanner
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: String,
    pub market_title: String,
    pub strategy: String,
    pub expected_value: f64,
    pub kelly_size: f64,
    pub probability: f64,
    pub expiry_hours: f64,
    pub platform: String,
    pub timestamp: i64,
}

/// Position for display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayPosition {
    pub id: String,
    pub market: String,
    pub platform: String,
    pub side: String,
    pub entry_price: f64,
    pub current_price: f64,
    pub quantity: f64,
    pub pnl: f64,
    pub pnl_percent: f64,
}

/// Stats summary for the dashboard
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardStats {
    pub current_capital: f64,
    pub starting_capital: f64,
    pub today_pnl: f64,
    pub today_pnl_percent: f64,
    pub all_time_pnl: f64,
    pub all_time_pnl_percent: f64,
    pub open_positions_count: u32,
    pub open_positions_value: f64,
    pub win_rate: f64,
    pub total_trades: u32,
    pub drawdown_percent: f64,
    pub high_water_mark: f64,
    pub exposure_by_category: Vec<CategoryExposure>,
}

/// Exposure breakdown by category
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryExposure {
    pub category: String,
    pub exposure: f64,
    pub percent: f64,
}

/// Historical P&L data point for charts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlDataPoint {
    pub timestamp: i64,
    pub date: String,
    pub cumulative_pnl: f64,
    pub daily_pnl: f64,
}

/// Configuration update request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_drawdown_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_probability_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_hours_to_expiry: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kelly_multiplier: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_liquidity_usd: Option<f64>,
}

/// Current system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub max_drawdown_pct: f64,
    pub base_probability_threshold: f64,
    pub max_hours_to_expiry: f64,
    pub kelly_multiplier: f64,
    pub min_liquidity_usd: f64,
    pub circuit_breaker_enabled: bool,
    pub max_daily_loss: f64,
    pub max_position_per_market: i64,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            max_drawdown_pct: 0.05,
            base_probability_threshold: 0.90,
            max_hours_to_expiry: 24.0,
            kelly_multiplier: 0.5,
            min_liquidity_usd: 10.0,
            circuit_breaker_enabled: true,
            max_daily_loss: 500.0,
            max_position_per_market: 50000,
        }
    }
}

/// Event types for SSE broadcasts
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEvent {
    #[serde(rename = "stats")]
    Stats(DashboardStats),
    #[serde(rename = "positions")]
    Positions(Vec<DisplayPosition>),
    #[serde(rename = "opportunities")]
    Opportunities(Vec<Opportunity>),
    #[serde(rename = "status")]
    Status {
        status: SystemStatus,
        mode: TradingMode,
        kill_switch_active: bool,
    },
    #[serde(rename = "price_update")]
    PriceUpdate {
        market_id: String,
        platform: String,
        price: f64,
    },
    #[serde(rename = "alert")]
    Alert { level: String, message: String },
}

/// Shared dashboard state
pub struct DashboardState {
    /// Database connection for persistence
    pub db: Option<Arc<Database>>,

    /// Position tracker
    pub positions: Arc<RwLock<PositionTracker>>,

    /// Circuit breaker reference
    pub circuit_breaker: Option<Arc<CircuitBreaker>>,

    /// Current trading mode
    pub trading_mode: RwLock<TradingMode>,

    /// System status
    pub system_status: RwLock<SystemStatus>,

    /// Kill switch state
    pub kill_switch_active: AtomicBool,

    /// Current capital (in cents to avoid floating point issues)
    pub current_capital_cents: AtomicU64,

    /// Starting capital (in cents)
    pub starting_capital_cents: AtomicU64,

    /// High water mark (in cents)
    pub high_water_mark_cents: AtomicU64,

    /// Current opportunities
    pub opportunities: RwLock<Vec<Opportunity>>,

    /// System configuration
    pub config: RwLock<SystemConfig>,

    /// Broadcast channel for SSE events
    pub event_tx: broadcast::Sender<DashboardEvent>,

    /// Historical P&L data
    pub pnl_history: RwLock<Vec<PnlDataPoint>>,
}

impl DashboardState {
    /// Create a new dashboard state with default values
    pub fn new() -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(1024);

        Arc::new(Self {
            db: None,
            positions: Arc::new(RwLock::new(PositionTracker::new())),
            circuit_breaker: None,
            trading_mode: RwLock::new(TradingMode::Paper),
            system_status: RwLock::new(SystemStatus::Starting),
            kill_switch_active: AtomicBool::new(false),
            current_capital_cents: AtomicU64::new(10000), // $100 default
            starting_capital_cents: AtomicU64::new(10000),
            high_water_mark_cents: AtomicU64::new(10000),
            opportunities: RwLock::new(Vec::new()),
            config: RwLock::new(SystemConfig::default()),
            event_tx,
            pnl_history: RwLock::new(Vec::new()),
        })
    }

    /// Create dashboard state with database and position tracker
    pub fn with_components(
        db: Arc<Database>,
        positions: Arc<RwLock<PositionTracker>>,
        circuit_breaker: Option<Arc<CircuitBreaker>>,
        starting_capital: f64,
    ) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(1024);
        let capital_cents = (starting_capital * 100.0) as u64;

        Arc::new(Self {
            db: Some(db),
            positions,
            circuit_breaker,
            trading_mode: RwLock::new(TradingMode::Paper),
            system_status: RwLock::new(SystemStatus::Starting),
            kill_switch_active: AtomicBool::new(false),
            current_capital_cents: AtomicU64::new(capital_cents),
            starting_capital_cents: AtomicU64::new(capital_cents),
            high_water_mark_cents: AtomicU64::new(capital_cents),
            opportunities: RwLock::new(Vec::new()),
            config: RwLock::new(SystemConfig::default()),
            event_tx,
            pnl_history: RwLock::new(Vec::new()),
        })
    }

    /// Subscribe to SSE events
    pub fn subscribe(&self) -> broadcast::Receiver<DashboardEvent> {
        self.event_tx.subscribe()
    }

    /// Broadcast an event to all SSE subscribers
    pub fn broadcast(&self, event: DashboardEvent) {
        // Ignore send errors (no subscribers)
        let _ = self.event_tx.send(event);
    }

    /// Get current capital as f64
    pub fn get_current_capital(&self) -> f64 {
        self.current_capital_cents.load(Ordering::Relaxed) as f64 / 100.0
    }

    /// Set current capital
    pub fn set_current_capital(&self, capital: f64) {
        let cents = (capital * 100.0) as u64;
        self.current_capital_cents.store(cents, Ordering::Relaxed);

        // Update high water mark if needed
        let hwm = self.high_water_mark_cents.load(Ordering::Relaxed);
        if cents > hwm {
            self.high_water_mark_cents.store(cents, Ordering::Relaxed);
        }
    }

    /// Get starting capital as f64
    pub fn get_starting_capital(&self) -> f64 {
        self.starting_capital_cents.load(Ordering::Relaxed) as f64 / 100.0
    }

    /// Get high water mark as f64
    pub fn get_high_water_mark(&self) -> f64 {
        self.high_water_mark_cents.load(Ordering::Relaxed) as f64 / 100.0
    }

    /// Calculate current drawdown percentage
    pub fn get_drawdown_percent(&self) -> f64 {
        let current = self.current_capital_cents.load(Ordering::Relaxed) as f64;
        let hwm = self.high_water_mark_cents.load(Ordering::Relaxed) as f64;
        if hwm > 0.0 {
            (hwm - current) / hwm * 100.0
        } else {
            0.0
        }
    }

    /// Activate kill switch
    pub async fn activate_kill_switch(&self) {
        self.kill_switch_active.store(true, Ordering::SeqCst);
        *self.system_status.write().await = SystemStatus::Halted;

        self.broadcast(DashboardEvent::Status {
            status: SystemStatus::Halted,
            mode: *self.trading_mode.read().await,
            kill_switch_active: true,
        });

        self.broadcast(DashboardEvent::Alert {
            level: "critical".to_string(),
            message: "Kill switch activated - all trading halted".to_string(),
        });

        tracing::error!("KILL SWITCH ACTIVATED");
    }

    /// Deactivate kill switch
    pub async fn deactivate_kill_switch(&self) {
        self.kill_switch_active.store(false, Ordering::SeqCst);
        *self.system_status.write().await = SystemStatus::Running;

        self.broadcast(DashboardEvent::Status {
            status: SystemStatus::Running,
            mode: *self.trading_mode.read().await,
            kill_switch_active: false,
        });

        self.broadcast(DashboardEvent::Alert {
            level: "info".to_string(),
            message: "Kill switch deactivated - trading resumed".to_string(),
        });

        tracing::info!("Kill switch deactivated");
    }

    /// Set trading mode
    pub async fn set_trading_mode(&self, mode: TradingMode) {
        *self.trading_mode.write().await = mode;

        self.broadcast(DashboardEvent::Status {
            status: *self.system_status.read().await,
            mode,
            kill_switch_active: self.kill_switch_active.load(Ordering::Relaxed),
        });

        tracing::info!("Trading mode changed to: {:?}", mode);
    }

    /// Update opportunities list
    pub async fn update_opportunities(&self, opportunities: Vec<Opportunity>) {
        *self.opportunities.write().await = opportunities.clone();
        self.broadcast(DashboardEvent::Opportunities(opportunities));
    }

    /// Get current stats
    pub async fn get_stats(&self) -> DashboardStats {
        let positions = self.positions.read().await;
        let summary = positions.summary();

        let current_capital = self.get_current_capital();
        let starting_capital = self.get_starting_capital();
        let hwm = self.get_high_water_mark();

        let today_pnl = positions.daily_pnl();
        let all_time_pnl = positions.all_time_pnl;

        let today_pnl_percent = if starting_capital > 0.0 {
            today_pnl / starting_capital * 100.0
        } else {
            0.0
        };

        let all_time_pnl_percent = if starting_capital > 0.0 {
            all_time_pnl / starting_capital * 100.0
        } else {
            0.0
        };

        let drawdown_percent = if hwm > 0.0 {
            (hwm - current_capital) / hwm * 100.0
        } else {
            0.0
        };

        let win_rate = if summary.resolved_positions > 0 {
            // This is a simplified calculation - actual win rate would need trade history
            50.0 // Placeholder
        } else {
            0.0
        };

        DashboardStats {
            current_capital,
            starting_capital,
            today_pnl,
            today_pnl_percent,
            all_time_pnl,
            all_time_pnl_percent,
            open_positions_count: summary.open_positions as u32,
            open_positions_value: summary.total_cost_basis,
            win_rate,
            total_trades: (summary.open_positions + summary.resolved_positions) as u32,
            drawdown_percent,
            high_water_mark: hwm,
            exposure_by_category: vec![], // Would be populated from actual position data
        }
    }

    /// Get display positions
    pub async fn get_display_positions(&self) -> Vec<DisplayPosition> {
        let positions = self.positions.read().await;
        let open = positions.open_positions();

        open.iter()
            .map(|pos| {
                let total_cost = pos.total_cost();
                let guaranteed_profit = pos.guaranteed_profit();

                DisplayPosition {
                    id: pos.market_id.clone(),
                    market: pos.description.clone(),
                    platform: "multi".to_string(), // Cross-platform positions
                    side: "arb".to_string(),
                    entry_price: if pos.matched_contracts() > 0.0 {
                        total_cost / pos.matched_contracts()
                    } else {
                        0.0
                    },
                    current_price: 1.0, // Arb positions pay $1 at resolution
                    quantity: pos.matched_contracts(),
                    pnl: guaranteed_profit,
                    pnl_percent: if total_cost > 0.0 {
                        guaranteed_profit / total_cost * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect()
    }

    /// Add P&L data point to history
    pub async fn add_pnl_datapoint(&self, datapoint: PnlDataPoint) {
        let mut history = self.pnl_history.write().await;
        history.push(datapoint);

        // Keep last 365 days of data
        if history.len() > 365 {
            history.remove(0);
        }
    }

    /// Get P&L history for charts
    pub async fn get_pnl_history(&self) -> Vec<PnlDataPoint> {
        self.pnl_history.read().await.clone()
    }
}

impl Default for DashboardState {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(1024);

        Self {
            db: None,
            positions: Arc::new(RwLock::new(PositionTracker::new())),
            circuit_breaker: None,
            trading_mode: RwLock::new(TradingMode::Paper),
            system_status: RwLock::new(SystemStatus::Starting),
            kill_switch_active: AtomicBool::new(false),
            current_capital_cents: AtomicU64::new(10000),
            starting_capital_cents: AtomicU64::new(10000),
            high_water_mark_cents: AtomicU64::new(10000),
            opportunities: RwLock::new(Vec::new()),
            config: RwLock::new(SystemConfig::default()),
            event_tx,
            pnl_history: RwLock::new(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dashboard_state_creation() {
        let state = DashboardState::new();
        assert_eq!(state.get_current_capital(), 100.0);
        assert_eq!(state.get_starting_capital(), 100.0);
    }

    #[tokio::test]
    async fn test_capital_updates() {
        let state = DashboardState::new();
        state.set_current_capital(150.0);
        assert_eq!(state.get_current_capital(), 150.0);
        assert_eq!(state.get_high_water_mark(), 150.0);
    }

    #[tokio::test]
    async fn test_drawdown_calculation() {
        let state = DashboardState::new();
        state.set_current_capital(150.0); // Set HWM to 150
        state.set_current_capital(120.0); // Drop to 120

        let drawdown = state.get_drawdown_percent();
        assert!((drawdown - 20.0).abs() < 0.01); // 20% drawdown
    }

    #[tokio::test]
    async fn test_kill_switch() {
        let state = DashboardState::new();
        assert!(!state.kill_switch_active.load(Ordering::Relaxed));

        state.activate_kill_switch().await;
        assert!(state.kill_switch_active.load(Ordering::Relaxed));
        assert_eq!(*state.system_status.read().await, SystemStatus::Halted);

        state.deactivate_kill_switch().await;
        assert!(!state.kill_switch_active.load(Ordering::Relaxed));
        assert_eq!(*state.system_status.read().await, SystemStatus::Running);
    }

    #[tokio::test]
    async fn test_trading_mode() {
        let state = DashboardState::new();
        assert_eq!(*state.trading_mode.read().await, TradingMode::Paper);

        state.set_trading_mode(TradingMode::Live).await;
        assert_eq!(*state.trading_mode.read().await, TradingMode::Live);
    }
}
