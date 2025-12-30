// Suppress warnings for future-use code
#![allow(dead_code)]

//! SQLite persistence layer for the prediction market trading system.
//!
//! This module provides:
//! - Database initialization and schema migration
//! - CRUD operations for all core entities (markets, positions, orders, etc.)
//! - Kalman filter state persistence
//! - Crash recovery with exchange reconciliation
//! - Price history collection for backtesting
//!
//! # Example
//! ```rust,ignore
//! use prediction_market_arbitrage::persistence::Database;
//!
//! let db = Database::open("./data/trading.db")?;
//! db.initialize()?;
//!
//! // Insert a market
//! let market = NewMarket { /* ... */ };
//! db.insert_market(&market)?;
//!
//! // Get open positions
//! let positions = db.get_open_positions()?;
//! ```

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

// ============================================================================
// ERROR TYPES
// ============================================================================

/// Errors that can occur during database operations
#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Database not initialized - call initialize() first")]
    NotInitialized,

    #[error("Record not found: {0}")]
    NotFound(String),

    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("Migration failed: {0}")]
    MigrationFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type DbResult<T> = Result<T, DbError>;

// ============================================================================
// PLATFORM ENUM
// ============================================================================

/// Trading platform identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Polymarket,
    Kalshi,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Polymarket => "polymarket",
            Platform::Kalshi => "kalshi",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "polymarket" => Some(Platform::Polymarket),
            "kalshi" => Some(Platform::Kalshi),
            _ => None,
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// DATA STRUCTURES
// ============================================================================

/// Market data for insertion
#[derive(Debug, Clone)]
pub struct NewMarket {
    pub id: String,
    pub platform: Platform,
    pub title: String,
    pub category: Option<String>,
    pub resolution_time: Option<DateTime<Utc>>,
    pub external_id: Option<String>,
}

/// Complete market record from database
#[derive(Debug, Clone)]
pub struct Market {
    pub id: String,
    pub platform: Platform,
    pub title: String,
    pub category: Option<String>,
    pub resolution_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub kalman_estimate: Option<f64>,
    pub kalman_variance: Option<f64>,
    pub kalman_updated_at: Option<DateTime<Utc>>,
    pub price_change_mean: f64,
    pub price_change_std: f64,
    pub volume_mean: f64,
    pub volatility_samples: i32,
    pub external_id: Option<String>,
    pub is_active: bool,
    pub last_bid: Option<f64>,
    pub last_ask: Option<f64>,
    pub volume_24h: Option<f64>,
}

/// Kalman filter state for a market
#[derive(Debug, Clone, Copy)]
pub struct KalmanState {
    pub estimate: f64,
    pub variance: f64,
    pub updated_at: DateTime<Utc>,
}

/// Volatility statistics for thesis invalidation
#[derive(Debug, Clone, Copy)]
pub struct VolatilityStats {
    pub price_change_mean: f64,
    pub price_change_std: f64,
    pub volume_mean: f64,
    pub samples: i32,
}

/// Position side
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Yes,
    No,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Yes => "yes",
            Side::No => "no",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "yes" => Some(Side::Yes),
            "no" => Some(Side::No),
            _ => None,
        }
    }
}

/// Position status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionStatus {
    Open,
    Closed,
    Resolved,
}

impl PositionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PositionStatus::Open => "open",
            PositionStatus::Closed => "closed",
            PositionStatus::Resolved => "resolved",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "open" => Some(PositionStatus::Open),
            "closed" => Some(PositionStatus::Closed),
            "resolved" => Some(PositionStatus::Resolved),
            _ => None,
        }
    }
}

/// Position data for insertion
#[derive(Debug, Clone)]
pub struct NewPosition {
    pub market_id: String,
    pub platform: Platform,
    pub side: Side,
    pub entry_price: f64,
    pub quantity: f64,
    pub strategy: Option<String>,
    pub notes: Option<String>,
}

/// Complete position record from database
#[derive(Debug, Clone)]
pub struct Position {
    pub id: i64,
    pub market_id: String,
    pub platform: Platform,
    pub side: Side,
    pub entry_price: f64,
    pub quantity: f64,
    pub entry_time: DateTime<Utc>,
    pub exit_price: Option<f64>,
    pub exit_time: Option<DateTime<Utc>>,
    pub pnl: Option<f64>,
    pub status: PositionStatus,
    pub strategy: Option<String>,
    pub notes: Option<String>,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Market,
    Limit,
    Twap,
}

impl OrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
            OrderType::Twap => "twap",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "market" => Some(OrderType::Market),
            "limit" => Some(OrderType::Limit),
            "twap" => Some(OrderType::Twap),
            _ => None,
        }
    }
}

/// Order side (buy/sell)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "buy" => Some(OrderSide::Buy),
            "sell" => Some(OrderSide::Sell),
            _ => None,
        }
    }
}

/// Order status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    Pending,
    Submitted,
    PartiallyFilled,
    Filled,
    Cancelled,
    Failed,
    Expired,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Submitted => "submitted",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Failed => "failed",
            OrderStatus::Expired => "expired",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "pending" => Some(OrderStatus::Pending),
            "submitted" => Some(OrderStatus::Submitted),
            "partially_filled" => Some(OrderStatus::PartiallyFilled),
            "filled" => Some(OrderStatus::Filled),
            "cancelled" => Some(OrderStatus::Cancelled),
            "failed" => Some(OrderStatus::Failed),
            "expired" => Some(OrderStatus::Expired),
            _ => None,
        }
    }
}

/// Order data for insertion
#[derive(Debug, Clone)]
pub struct NewOrder {
    pub position_id: Option<i64>,
    pub market_id: String,
    pub platform: Platform,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<f64>,
    pub quantity: f64,
}

/// Complete order record from database
#[derive(Debug, Clone)]
pub struct Order {
    pub id: i64,
    pub position_id: Option<i64>,
    pub market_id: String,
    pub platform: Platform,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<f64>,
    pub quantity: f64,
    pub filled_quantity: f64,
    pub fill_price: Option<f64>,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub filled_at: Option<DateTime<Utc>>,
    pub platform_order_id: Option<String>,
    pub error_message: Option<String>,
    pub platform_metadata: Option<String>,
}

/// Price history record for insertion
#[derive(Debug, Clone)]
pub struct NewPriceHistory {
    pub market_id: String,
    pub platform: Platform,
    pub timestamp: DateTime<Utc>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub last_price: Option<f64>,
    pub volume_24h: Option<f64>,
    pub bid_depth: Option<f64>,
    pub ask_depth: Option<f64>,
}

/// Price history record from database
#[derive(Debug, Clone)]
pub struct PriceHistory {
    pub id: i64,
    pub market_id: String,
    pub platform: Platform,
    pub timestamp: DateTime<Utc>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub last_price: Option<f64>,
    pub volume_24h: Option<f64>,
    pub bid_depth: Option<f64>,
    pub ask_depth: Option<f64>,
}

/// Correlation type between markets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationType {
    SameEvent,
    SameResolution,
    CategoryTime,
}

impl CorrelationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorrelationType::SameEvent => "same_event",
            CorrelationType::SameResolution => "same_resolution",
            CorrelationType::CategoryTime => "category_time",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "same_event" => Some(CorrelationType::SameEvent),
            "same_resolution" => Some(CorrelationType::SameResolution),
            "category_time" => Some(CorrelationType::CategoryTime),
            _ => None,
        }
    }
}

/// Correlation record
#[derive(Debug, Clone)]
pub struct Correlation {
    pub market_id_1: String,
    pub market_id_2: String,
    pub correlation_type: CorrelationType,
    pub confidence: f64,
    pub created_at: DateTime<Utc>,
    pub validated_at: Option<DateTime<Utc>>,
    pub notes: Option<String>,
}

/// Daily snapshot for P&L tracking
#[derive(Debug, Clone)]
pub struct DailySnapshot {
    pub date: NaiveDate,
    pub starting_capital: f64,
    pub ending_capital: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub trades_count: i32,
    pub win_count: i32,
    pub max_drawdown: f64,
    pub high_water_mark: Option<f64>,
    pub open_positions_count: Option<i32>,
    pub total_exposure: Option<f64>,
    pub metrics_json: Option<String>,
}

/// System state value types
#[derive(Debug, Clone, PartialEq)]
pub enum StateValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    DateTime(DateTime<Utc>),
    Json(String),
}

impl StateValue {
    /// Convert to string representation for storage
    pub fn to_db_string(&self) -> String {
        match self {
            StateValue::String(s) => s.clone(),
            StateValue::Int(i) => i.to_string(),
            StateValue::Float(f) => f.to_string(),
            StateValue::Bool(b) => b.to_string(),
            StateValue::DateTime(dt) => dt.to_rfc3339(),
            StateValue::Json(j) => j.clone(),
        }
    }

    /// Get the type name for the value_type column
    pub fn type_name(&self) -> &'static str {
        match self {
            StateValue::String(_) => "string",
            StateValue::Int(_) => "int",
            StateValue::Float(_) => "float",
            StateValue::Bool(_) => "bool",
            StateValue::DateTime(_) => "datetime",
            StateValue::Json(_) => "json",
        }
    }

    /// Parse from database string and type
    pub fn from_db(value: &str, type_name: &str) -> Option<Self> {
        match type_name {
            "string" => Some(StateValue::String(value.to_string())),
            "int" => value.parse().ok().map(StateValue::Int),
            "float" => value.parse().ok().map(StateValue::Float),
            "bool" => value.parse().ok().map(StateValue::Bool),
            "datetime" => DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|dt| StateValue::DateTime(dt.with_timezone(&Utc))),
            "json" => Some(StateValue::Json(value.to_string())),
            _ => None,
        }
    }

    /// Extract as bool if type matches
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            StateValue::Bool(b) => Some(*b),
            StateValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Extract as float if type matches
    pub fn as_float(&self) -> Option<f64> {
        match self {
            StateValue::Float(f) => Some(*f),
            StateValue::Int(i) => Some(*i as f64),
            StateValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Extract as int if type matches
    pub fn as_int(&self) -> Option<i64> {
        match self {
            StateValue::Int(i) => Some(*i),
            StateValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Extract as string
    pub fn as_string(&self) -> String {
        self.to_db_string()
    }
}

// ============================================================================
// DATABASE IMPLEMENTATION
// ============================================================================

/// Thread-safe database handle
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open or create a database at the given path
    pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let path = path.as_ref();

        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        // Enable foreign keys and WAL mode for better concurrency
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -64000;  -- 64MB cache
            PRAGMA temp_store = MEMORY;
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database (for testing)
    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Initialize the database schema from the migration file
    pub fn initialize(&self) -> DbResult<()> {
        let schema = include_str!("../migrations/001_initial.sql");
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute_batch(schema)?;
        Ok(())
    }

    /// Initialize with a custom schema (for testing)
    pub fn initialize_with_schema(&self, schema: &str) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute_batch(schema)?;
        Ok(())
    }

    // ========================================================================
    // MARKET OPERATIONS
    // ========================================================================

    /// Insert a new market
    pub fn insert_market(&self, market: &NewMarket) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO markets (id, platform, title, category, resolution_time, external_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                market.id,
                market.platform.as_str(),
                market.title,
                market.category,
                market.resolution_time.map(|dt| dt.to_rfc3339()),
                market.external_id,
            ],
        )?;

        Ok(())
    }

    /// Insert or update a market (upsert)
    pub fn upsert_market(&self, market: &NewMarket) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO markets (id, platform, title, category, resolution_time, external_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                category = excluded.category,
                resolution_time = excluded.resolution_time,
                external_id = excluded.external_id",
            params![
                market.id,
                market.platform.as_str(),
                market.title,
                market.category,
                market.resolution_time.map(|dt| dt.to_rfc3339()),
                market.external_id,
            ],
        )?;

        Ok(())
    }

    /// Get a market by ID
    pub fn get_market(&self, id: &str) -> DbResult<Option<Market>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let result = conn
            .query_row(
                "SELECT id, platform, title, category, resolution_time, created_at, updated_at,
                        kalman_estimate, kalman_variance, kalman_updated_at,
                        price_change_mean, price_change_std, volume_mean, volatility_samples,
                        external_id, is_active, last_bid, last_ask, volume_24h
                 FROM markets WHERE id = ?1",
                [id],
                |row| Self::row_to_market(row),
            )
            .optional()?;

        Ok(result)
    }

    /// Get all active markets
    pub fn get_active_markets(&self) -> DbResult<Vec<Market>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, platform, title, category, resolution_time, created_at, updated_at,
                    kalman_estimate, kalman_variance, kalman_updated_at,
                    price_change_mean, price_change_std, volume_mean, volatility_samples,
                    external_id, is_active, last_bid, last_ask, volume_24h
             FROM markets WHERE is_active = 1",
        )?;

        let markets = stmt
            .query_map([], |row| Self::row_to_market(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(markets)
    }

    /// Get markets by platform
    pub fn get_markets_by_platform(&self, platform: Platform) -> DbResult<Vec<Market>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, platform, title, category, resolution_time, created_at, updated_at,
                    kalman_estimate, kalman_variance, kalman_updated_at,
                    price_change_mean, price_change_std, volume_mean, volatility_samples,
                    external_id, is_active, last_bid, last_ask, volume_24h
             FROM markets WHERE platform = ?1 AND is_active = 1",
        )?;

        let markets = stmt
            .query_map([platform.as_str()], |row| Self::row_to_market(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(markets)
    }

    /// Update Kalman filter state for a market
    pub fn update_kalman_state(&self, market_id: &str, state: &KalmanState) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let rows_affected = conn.execute(
            "UPDATE markets
             SET kalman_estimate = ?1, kalman_variance = ?2, kalman_updated_at = ?3
             WHERE id = ?4",
            params![
                state.estimate,
                state.variance,
                state.updated_at.to_rfc3339(),
                market_id,
            ],
        )?;

        if rows_affected == 0 {
            return Err(DbError::NotFound(format!("Market not found: {}", market_id)));
        }

        Ok(())
    }

    /// Update volatility statistics for a market
    pub fn update_volatility_stats(&self, market_id: &str, stats: &VolatilityStats) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let rows_affected = conn.execute(
            "UPDATE markets
             SET price_change_mean = ?1, price_change_std = ?2, volume_mean = ?3, volatility_samples = ?4
             WHERE id = ?5",
            params![
                stats.price_change_mean,
                stats.price_change_std,
                stats.volume_mean,
                stats.samples,
                market_id,
            ],
        )?;

        if rows_affected == 0 {
            return Err(DbError::NotFound(format!("Market not found: {}", market_id)));
        }

        Ok(())
    }

    /// Update market prices (bid/ask)
    pub fn update_market_prices(
        &self,
        market_id: &str,
        bid: Option<f64>,
        ask: Option<f64>,
        volume_24h: Option<f64>,
    ) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "UPDATE markets SET last_bid = ?1, last_ask = ?2, volume_24h = ?3 WHERE id = ?4",
            params![bid, ask, volume_24h, market_id],
        )?;

        Ok(())
    }

    /// Set market active status
    pub fn set_market_active(&self, market_id: &str, is_active: bool) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "UPDATE markets SET is_active = ?1 WHERE id = ?2",
            params![is_active, market_id],
        )?;

        Ok(())
    }

    fn row_to_market(row: &Row) -> rusqlite::Result<Market> {
        let platform_str: String = row.get(1)?;
        let resolution_time_str: Option<String> = row.get(4)?;
        let created_at_str: String = row.get(5)?;
        let updated_at_str: String = row.get(6)?;
        let kalman_updated_str: Option<String> = row.get(9)?;

        Ok(Market {
            id: row.get(0)?,
            platform: Platform::from_str(&platform_str).unwrap_or(Platform::Polymarket),
            title: row.get(2)?,
            category: row.get(3)?,
            resolution_time: resolution_time_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            kalman_estimate: row.get(7)?,
            kalman_variance: row.get(8)?,
            kalman_updated_at: kalman_updated_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            price_change_mean: row.get(10)?,
            price_change_std: row.get(11)?,
            volume_mean: row.get(12)?,
            volatility_samples: row.get(13)?,
            external_id: row.get(14)?,
            is_active: row.get(15)?,
            last_bid: row.get(16)?,
            last_ask: row.get(17)?,
            volume_24h: row.get(18)?,
        })
    }

    // ========================================================================
    // POSITION OPERATIONS
    // ========================================================================

    /// Insert a new position and return its ID
    pub fn insert_position(&self, position: &NewPosition) -> DbResult<i64> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO positions (market_id, platform, side, entry_price, quantity, strategy, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                position.market_id,
                position.platform.as_str(),
                position.side.as_str(),
                position.entry_price,
                position.quantity,
                position.strategy,
                position.notes,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// Get a position by ID
    pub fn get_position(&self, id: i64) -> DbResult<Option<Position>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let result = conn
            .query_row(
                "SELECT id, market_id, platform, side, entry_price, quantity, entry_time,
                        exit_price, exit_time, pnl, status, strategy, notes
                 FROM positions WHERE id = ?1",
                [id],
                |row| Self::row_to_position(row),
            )
            .optional()?;

        Ok(result)
    }

    /// Get all open positions
    pub fn get_open_positions(&self) -> DbResult<Vec<Position>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, market_id, platform, side, entry_price, quantity, entry_time,
                    exit_price, exit_time, pnl, status, strategy, notes
             FROM positions WHERE status = 'open'",
        )?;

        let positions = stmt
            .query_map([], |row| Self::row_to_position(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(positions)
    }

    /// Get open positions by platform
    pub fn get_open_positions_by_platform(&self, platform: Platform) -> DbResult<Vec<Position>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, market_id, platform, side, entry_price, quantity, entry_time,
                    exit_price, exit_time, pnl, status, strategy, notes
             FROM positions WHERE status = 'open' AND platform = ?1",
        )?;

        let positions = stmt
            .query_map([platform.as_str()], |row| Self::row_to_position(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(positions)
    }

    /// Get positions by market
    pub fn get_positions_by_market(&self, market_id: &str) -> DbResult<Vec<Position>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, market_id, platform, side, entry_price, quantity, entry_time,
                    exit_price, exit_time, pnl, status, strategy, notes
             FROM positions WHERE market_id = ?1",
        )?;

        let positions = stmt
            .query_map([market_id], |row| Self::row_to_position(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(positions)
    }

    /// Update position quantity (for reconciliation)
    pub fn update_position_quantity(&self, position_id: i64, quantity: f64) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let rows_affected = conn.execute(
            "UPDATE positions SET quantity = ?1 WHERE id = ?2 AND status = 'open'",
            params![quantity, position_id],
        )?;

        if rows_affected == 0 {
            return Err(DbError::NotFound(format!(
                "Open position not found: {}",
                position_id
            )));
        }

        Ok(())
    }

    /// Close a position with optional exit price (None for resolution)
    pub fn close_position(&self, position_id: i64, exit_price: Option<f64>) -> DbResult<f64> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        // Get the position first
        let position: Position = conn
            .query_row(
                "SELECT id, market_id, platform, side, entry_price, quantity, entry_time,
                        exit_price, exit_time, pnl, status, strategy, notes
                 FROM positions WHERE id = ?1 AND status = 'open'",
                [position_id],
                |row| Self::row_to_position(row),
            )
            .map_err(|_| DbError::NotFound(format!("Open position not found: {}", position_id)))?;

        // Calculate P&L
        let actual_exit = exit_price.unwrap_or(1.0); // Resolution = 1.0 for winning side
        let pnl = match position.side {
            Side::Yes => (actual_exit - position.entry_price) * position.quantity,
            Side::No => (position.entry_price - actual_exit) * position.quantity,
        };

        let status = if exit_price.is_some() { "closed" } else { "resolved" };

        conn.execute(
            "UPDATE positions
             SET exit_price = ?1, exit_time = CURRENT_TIMESTAMP, pnl = ?2, status = ?3
             WHERE id = ?4",
            params![actual_exit, pnl, status, position_id],
        )?;

        Ok(pnl)
    }

    fn row_to_position(row: &Row) -> rusqlite::Result<Position> {
        let platform_str: String = row.get(2)?;
        let side_str: String = row.get(3)?;
        let entry_time_str: String = row.get(6)?;
        let exit_time_str: Option<String> = row.get(8)?;
        let status_str: String = row.get(10)?;

        Ok(Position {
            id: row.get(0)?,
            market_id: row.get(1)?,
            platform: Platform::from_str(&platform_str).unwrap_or(Platform::Polymarket),
            side: Side::from_str(&side_str).unwrap_or(Side::Yes),
            entry_price: row.get(4)?,
            quantity: row.get(5)?,
            entry_time: DateTime::parse_from_rfc3339(&entry_time_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            exit_price: row.get(7)?,
            exit_time: exit_time_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            pnl: row.get(9)?,
            status: PositionStatus::from_str(&status_str).unwrap_or(PositionStatus::Open),
            strategy: row.get(11)?,
            notes: row.get(12)?,
        })
    }

    // ========================================================================
    // ORDER OPERATIONS
    // ========================================================================

    /// Insert a new order and return its ID
    pub fn insert_order(&self, order: &NewOrder) -> DbResult<i64> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO orders (position_id, market_id, platform, side, order_type, price, quantity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                order.position_id,
                order.market_id,
                order.platform.as_str(),
                order.side.as_str(),
                order.order_type.as_str(),
                order.price,
                order.quantity,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// Get an order by ID
    pub fn get_order(&self, id: i64) -> DbResult<Option<Order>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let result = conn
            .query_row(
                "SELECT id, position_id, market_id, platform, side, order_type, price, quantity,
                        filled_quantity, fill_price, status, created_at, submitted_at, filled_at,
                        platform_order_id, error_message, platform_metadata
                 FROM orders WHERE id = ?1",
                [id],
                |row| Self::row_to_order(row),
            )
            .optional()?;

        Ok(result)
    }

    /// Get pending/active orders
    pub fn get_active_orders(&self) -> DbResult<Vec<Order>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, position_id, market_id, platform, side, order_type, price, quantity,
                    filled_quantity, fill_price, status, created_at, submitted_at, filled_at,
                    platform_order_id, error_message, platform_metadata
             FROM orders WHERE status IN ('pending', 'submitted', 'partially_filled')",
        )?;

        let orders = stmt
            .query_map([], |row| Self::row_to_order(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(orders)
    }

    /// Update order status
    pub fn update_order_status(
        &self,
        order_id: i64,
        status: OrderStatus,
        platform_order_id: Option<&str>,
        error_message: Option<&str>,
    ) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        // Handle submitted_at timestamp for submitted status
        if status == OrderStatus::Submitted {
            conn.execute(
                "UPDATE orders
                 SET status = ?1, platform_order_id = COALESCE(?2, platform_order_id),
                     error_message = ?3, submitted_at = CURRENT_TIMESTAMP
                 WHERE id = ?4",
                params![status.as_str(), platform_order_id, error_message, order_id],
            )?;
        } else {
            conn.execute(
                "UPDATE orders
                 SET status = ?1, platform_order_id = COALESCE(?2, platform_order_id), error_message = ?3
                 WHERE id = ?4",
                params![status.as_str(), platform_order_id, error_message, order_id],
            )?;
        }

        Ok(())
    }

    /// Update order fill
    pub fn update_order_fill(
        &self,
        order_id: i64,
        filled_quantity: f64,
        fill_price: f64,
    ) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        // Get current order to check if fully filled
        let order = self.get_order(order_id)?
            .ok_or_else(|| DbError::NotFound(format!("Order not found: {}", order_id)))?;

        let new_status = if filled_quantity >= order.quantity {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };

        if new_status == OrderStatus::Filled {
            conn.execute(
                "UPDATE orders
                 SET filled_quantity = ?1, fill_price = ?2, status = ?3, filled_at = CURRENT_TIMESTAMP
                 WHERE id = ?4",
                params![filled_quantity, fill_price, new_status.as_str(), order_id],
            )?;
        } else {
            conn.execute(
                "UPDATE orders SET filled_quantity = ?1, fill_price = ?2, status = ?3 WHERE id = ?4",
                params![filled_quantity, fill_price, new_status.as_str(), order_id],
            )?;
        }

        Ok(())
    }

    fn row_to_order(row: &Row) -> rusqlite::Result<Order> {
        let platform_str: String = row.get(3)?;
        let side_str: String = row.get(4)?;
        let order_type_str: String = row.get(5)?;
        let status_str: String = row.get(10)?;
        let created_at_str: String = row.get(11)?;
        let submitted_at_str: Option<String> = row.get(12)?;
        let filled_at_str: Option<String> = row.get(13)?;

        Ok(Order {
            id: row.get(0)?,
            position_id: row.get(1)?,
            market_id: row.get(2)?,
            platform: Platform::from_str(&platform_str).unwrap_or(Platform::Polymarket),
            side: OrderSide::from_str(&side_str).unwrap_or(OrderSide::Buy),
            order_type: OrderType::from_str(&order_type_str).unwrap_or(OrderType::Market),
            price: row.get(6)?,
            quantity: row.get(7)?,
            filled_quantity: row.get(8)?,
            fill_price: row.get(9)?,
            status: OrderStatus::from_str(&status_str).unwrap_or(OrderStatus::Pending),
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            submitted_at: submitted_at_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            filled_at: filled_at_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            platform_order_id: row.get(14)?,
            error_message: row.get(15)?,
            platform_metadata: row.get(16)?,
        })
    }

    // ========================================================================
    // PRICE HISTORY OPERATIONS
    // ========================================================================

    /// Insert price history record
    pub fn insert_price_history(&self, record: &NewPriceHistory) -> DbResult<i64> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO price_history (market_id, platform, timestamp, bid, ask, last_price, volume_24h, bid_depth, ask_depth)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.market_id,
                record.platform.as_str(),
                record.timestamp.to_rfc3339(),
                record.bid,
                record.ask,
                record.last_price,
                record.volume_24h,
                record.bid_depth,
                record.ask_depth,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// Insert multiple price history records efficiently
    pub fn insert_price_history_batch(&self, records: &[NewPriceHistory]) -> DbResult<()> {
        let mut conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let tx = conn.transaction()?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO price_history (market_id, platform, timestamp, bid, ask, last_price, volume_24h, bid_depth, ask_depth)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;

            for record in records {
                stmt.execute(params![
                    record.market_id,
                    record.platform.as_str(),
                    record.timestamp.to_rfc3339(),
                    record.bid,
                    record.ask,
                    record.last_price,
                    record.volume_24h,
                    record.bid_depth,
                    record.ask_depth,
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Get price history for a market within a time range
    pub fn get_price_history(
        &self,
        market_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> DbResult<Vec<PriceHistory>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, market_id, platform, timestamp, bid, ask, last_price, volume_24h, bid_depth, ask_depth
             FROM price_history
             WHERE market_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3
             ORDER BY timestamp ASC",
        )?;

        let records = stmt
            .query_map(
                params![market_id, start.to_rfc3339(), end.to_rfc3339()],
                |row| Self::row_to_price_history(row),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get all price history within a time range (for backtesting)
    pub fn get_all_price_history(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> DbResult<Vec<PriceHistory>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT id, market_id, platform, timestamp, bid, ask, last_price, volume_24h, bid_depth, ask_depth
             FROM price_history
             WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;

        let records = stmt
            .query_map(params![start.to_rfc3339(), end.to_rfc3339()], |row| {
                Self::row_to_price_history(row)
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Delete old price history (for data retention)
    pub fn delete_old_price_history(&self, before: DateTime<Utc>) -> DbResult<usize> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let deleted = conn.execute(
            "DELETE FROM price_history WHERE timestamp < ?1",
            params![before.to_rfc3339()],
        )?;

        Ok(deleted)
    }

    fn row_to_price_history(row: &Row) -> rusqlite::Result<PriceHistory> {
        let platform_str: String = row.get(2)?;
        let timestamp_str: String = row.get(3)?;

        Ok(PriceHistory {
            id: row.get(0)?,
            market_id: row.get(1)?,
            platform: Platform::from_str(&platform_str).unwrap_or(Platform::Polymarket),
            timestamp: DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            bid: row.get(4)?,
            ask: row.get(5)?,
            last_price: row.get(6)?,
            volume_24h: row.get(7)?,
            bid_depth: row.get(8)?,
            ask_depth: row.get(9)?,
        })
    }

    // ========================================================================
    // CORRELATION OPERATIONS
    // ========================================================================

    /// Insert or update a correlation
    pub fn upsert_correlation(&self, correlation: &Correlation) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        // Ensure market_id_1 < market_id_2 for consistency
        let (id1, id2) = if correlation.market_id_1 < correlation.market_id_2 {
            (&correlation.market_id_1, &correlation.market_id_2)
        } else {
            (&correlation.market_id_2, &correlation.market_id_1)
        };

        conn.execute(
            "INSERT INTO correlations (market_id_1, market_id_2, correlation_type, confidence, notes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(market_id_1, market_id_2) DO UPDATE SET
                correlation_type = excluded.correlation_type,
                confidence = excluded.confidence,
                validated_at = CURRENT_TIMESTAMP,
                notes = excluded.notes",
            params![
                id1,
                id2,
                correlation.correlation_type.as_str(),
                correlation.confidence,
                correlation.notes,
            ],
        )?;

        Ok(())
    }

    /// Get all correlations for a market
    pub fn get_correlations_for_market(&self, market_id: &str) -> DbResult<Vec<Correlation>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT market_id_1, market_id_2, correlation_type, confidence, created_at, validated_at, notes
             FROM correlations
             WHERE market_id_1 = ?1 OR market_id_2 = ?1",
        )?;

        let correlations = stmt
            .query_map([market_id], |row| Self::row_to_correlation(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(correlations)
    }

    /// Get correlated market IDs for a given market
    pub fn get_correlated_market_ids(&self, market_id: &str) -> DbResult<Vec<String>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT CASE WHEN market_id_1 = ?1 THEN market_id_2 ELSE market_id_1 END as other_market
             FROM correlations
             WHERE market_id_1 = ?1 OR market_id_2 = ?1",
        )?;

        let ids: Vec<String> = stmt
            .query_map([market_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ids)
    }

    /// Delete a correlation
    pub fn delete_correlation(&self, market_id_1: &str, market_id_2: &str) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        // Ensure correct ordering
        let (id1, id2) = if market_id_1 < market_id_2 {
            (market_id_1, market_id_2)
        } else {
            (market_id_2, market_id_1)
        };

        conn.execute(
            "DELETE FROM correlations WHERE market_id_1 = ?1 AND market_id_2 = ?2",
            params![id1, id2],
        )?;

        Ok(())
    }

    fn row_to_correlation(row: &Row) -> rusqlite::Result<Correlation> {
        let type_str: String = row.get(2)?;
        let created_at_str: String = row.get(4)?;
        let validated_at_str: Option<String> = row.get(5)?;

        Ok(Correlation {
            market_id_1: row.get(0)?,
            market_id_2: row.get(1)?,
            correlation_type: CorrelationType::from_str(&type_str).unwrap_or(CorrelationType::SameEvent),
            confidence: row.get(3)?,
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            validated_at: validated_at_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            notes: row.get(6)?,
        })
    }

    // ========================================================================
    // DAILY SNAPSHOT OPERATIONS
    // ========================================================================

    /// Insert or update a daily snapshot
    pub fn upsert_daily_snapshot(&self, snapshot: &DailySnapshot) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO daily_snapshots (date, starting_capital, ending_capital, realized_pnl,
                unrealized_pnl, trades_count, win_count, max_drawdown, high_water_mark,
                open_positions_count, total_exposure, metrics_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(date) DO UPDATE SET
                ending_capital = excluded.ending_capital,
                realized_pnl = excluded.realized_pnl,
                unrealized_pnl = excluded.unrealized_pnl,
                trades_count = excluded.trades_count,
                win_count = excluded.win_count,
                max_drawdown = MAX(max_drawdown, excluded.max_drawdown),
                high_water_mark = MAX(high_water_mark, excluded.high_water_mark),
                open_positions_count = excluded.open_positions_count,
                total_exposure = excluded.total_exposure,
                metrics_json = excluded.metrics_json",
            params![
                snapshot.date.format("%Y-%m-%d").to_string(),
                snapshot.starting_capital,
                snapshot.ending_capital,
                snapshot.realized_pnl,
                snapshot.unrealized_pnl,
                snapshot.trades_count,
                snapshot.win_count,
                snapshot.max_drawdown,
                snapshot.high_water_mark,
                snapshot.open_positions_count,
                snapshot.total_exposure,
                snapshot.metrics_json,
            ],
        )?;

        Ok(())
    }

    /// Get a daily snapshot by date
    pub fn get_daily_snapshot(&self, date: NaiveDate) -> DbResult<Option<DailySnapshot>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let result = conn
            .query_row(
                "SELECT date, starting_capital, ending_capital, realized_pnl, unrealized_pnl,
                        trades_count, win_count, max_drawdown, high_water_mark,
                        open_positions_count, total_exposure, metrics_json
                 FROM daily_snapshots WHERE date = ?1",
                params![date.format("%Y-%m-%d").to_string()],
                |row| Self::row_to_daily_snapshot(row),
            )
            .optional()?;

        Ok(result)
    }

    /// Get recent daily snapshots
    pub fn get_recent_snapshots(&self, days: i32) -> DbResult<Vec<DailySnapshot>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut stmt = conn.prepare(
            "SELECT date, starting_capital, ending_capital, realized_pnl, unrealized_pnl,
                    trades_count, win_count, max_drawdown, high_water_mark,
                    open_positions_count, total_exposure, metrics_json
             FROM daily_snapshots
             ORDER BY date DESC
             LIMIT ?1",
        )?;

        let snapshots = stmt
            .query_map([days], |row| Self::row_to_daily_snapshot(row))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(snapshots)
    }

    fn row_to_daily_snapshot(row: &Row) -> rusqlite::Result<DailySnapshot> {
        let date_str: String = row.get(0)?;

        Ok(DailySnapshot {
            date: NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").unwrap_or_else(|_| {
                chrono::Utc::now().date_naive()
            }),
            starting_capital: row.get(1)?,
            ending_capital: row.get(2)?,
            realized_pnl: row.get(3)?,
            unrealized_pnl: row.get(4)?,
            trades_count: row.get(5)?,
            win_count: row.get(6)?,
            max_drawdown: row.get(7)?,
            high_water_mark: row.get(8)?,
            open_positions_count: row.get(9)?,
            total_exposure: row.get(10)?,
            metrics_json: row.get(11)?,
        })
    }

    // ========================================================================
    // SYSTEM STATE OPERATIONS
    // ========================================================================

    /// Get a system state value
    pub fn get_state(&self, key: &str) -> DbResult<Option<StateValue>> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let result: Option<(String, String)> = conn
            .query_row(
                "SELECT value, value_type FROM system_state WHERE key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        Ok(result.and_then(|(value, type_name)| StateValue::from_db(&value, &type_name)))
    }

    /// Set a system state value
    pub fn set_state(&self, key: &str, value: StateValue, description: Option<&str>) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute(
            "INSERT INTO system_state (key, value, value_type, description)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                value_type = excluded.value_type,
                description = COALESCE(excluded.description, description)",
            params![key, value.to_db_string(), value.type_name(), description],
        )?;

        Ok(())
    }

    /// Get trading halted flag
    pub fn is_trading_halted(&self) -> DbResult<bool> {
        self.get_state("trading_halted")?
            .and_then(|v| v.as_bool())
            .ok_or_else(|| DbError::InvalidData("trading_halted state not found".into()))
            .or(Ok(false))
    }

    /// Set trading halted flag
    pub fn set_trading_halted(&self, halted: bool) -> DbResult<()> {
        self.set_state("trading_halted", StateValue::Bool(halted), None)
    }

    /// Get current capital
    pub fn get_current_capital(&self) -> DbResult<f64> {
        self.get_state("current_capital")?
            .and_then(|v| v.as_float())
            .ok_or_else(|| DbError::InvalidData("current_capital state not found".into()))
    }

    /// Set current capital
    pub fn set_current_capital(&self, capital: f64) -> DbResult<()> {
        self.set_state("current_capital", StateValue::Float(capital), None)
    }

    /// Get high water mark
    pub fn get_high_water_mark(&self) -> DbResult<f64> {
        self.get_state("high_water_mark")?
            .and_then(|v| v.as_float())
            .ok_or_else(|| DbError::InvalidData("high_water_mark state not found".into()))
    }

    /// Update high water mark (only if new value is higher)
    pub fn update_high_water_mark(&self, capital: f64) -> DbResult<()> {
        let current = self.get_high_water_mark().unwrap_or(0.0);
        if capital > current {
            self.set_state("high_water_mark", StateValue::Float(capital), None)?;
        }
        Ok(())
    }

    /// Get consecutive losses count
    pub fn get_consecutive_losses(&self) -> DbResult<i64> {
        self.get_state("consecutive_losses")?
            .and_then(|v| v.as_int())
            .ok_or_else(|| DbError::InvalidData("consecutive_losses state not found".into()))
            .or(Ok(0))
    }

    /// Set consecutive losses count
    pub fn set_consecutive_losses(&self, count: i64) -> DbResult<()> {
        self.set_state("consecutive_losses", StateValue::Int(count), None)
    }

    /// Record last heartbeat
    pub fn record_heartbeat(&self) -> DbResult<()> {
        self.set_state("last_heartbeat", StateValue::DateTime(Utc::now()), None)
    }

    /// Get last heartbeat time
    pub fn get_last_heartbeat(&self) -> DbResult<Option<DateTime<Utc>>> {
        let state = self.get_state("last_heartbeat")?;
        Ok(state.and_then(|v| {
            if let StateValue::DateTime(dt) = v {
                Some(dt)
            } else {
                None
            }
        }))
    }

    // ========================================================================
    // CRASH RECOVERY
    // ========================================================================

    /// Recover state by reconciling with exchange positions.
    ///
    /// This function should be called on startup to ensure database state
    /// matches actual exchange positions.
    ///
    /// # Arguments
    /// * `poly_positions` - Current positions from Polymarket API (market_id -> quantity)
    /// * `kalshi_positions` - Current positions from Kalshi API (market_id -> quantity)
    ///
    /// # Returns
    /// A summary of reconciliation actions taken
    pub fn recover_state(
        &self,
        poly_positions: &std::collections::HashMap<String, f64>,
        kalshi_positions: &std::collections::HashMap<String, f64>,
    ) -> DbResult<RecoveryReport> {
        let mut report = RecoveryReport::default();

        let saved_positions = self.get_open_positions()?;

        for saved in saved_positions {
            let actual_quantity = match saved.platform {
                Platform::Polymarket => poly_positions.get(&saved.market_id).copied(),
                Platform::Kalshi => kalshi_positions.get(&saved.market_id).copied(),
            };

            match actual_quantity {
                Some(qty) if (qty - saved.quantity).abs() > 0.0001 => {
                    // Quantity mismatch - update DB
                    tracing::warn!(
                        "Reconciling position {} quantity: {} -> {}",
                        saved.id,
                        saved.quantity,
                        qty
                    );

                    if qty > 0.0 {
                        self.update_position_quantity(saved.id, qty)?;
                        report.quantity_mismatches.push(QuantityMismatch {
                            position_id: saved.id,
                            market_id: saved.market_id.clone(),
                            saved_quantity: saved.quantity,
                            actual_quantity: qty,
                        });
                    } else {
                        // Position fully closed
                        self.close_position(saved.id, None)?;
                        report.closed_positions.push(saved.id);
                    }
                }
                Some(_) => {
                    // Quantities match - no action needed
                    report.verified_positions.push(saved.id);
                }
                None => {
                    // Position not found on exchange - mark as closed
                    tracing::warn!("Position {} closed externally", saved.id);
                    self.close_position(saved.id, None)?;
                    report.closed_positions.push(saved.id);
                }
            }
        }

        // Check for orphaned exchange positions (on exchange but not in DB)
        for (market_id, &qty) in poly_positions.iter() {
            let db_positions = self.get_positions_by_market(market_id)?;
            let has_open = db_positions.iter().any(|p| {
                p.platform == Platform::Polymarket && p.status == PositionStatus::Open
            });

            if !has_open && qty > 0.0 {
                report.orphaned_positions.push(OrphanedPosition {
                    platform: Platform::Polymarket,
                    market_id: market_id.clone(),
                    quantity: qty,
                });
            }
        }

        for (market_id, &qty) in kalshi_positions.iter() {
            let db_positions = self.get_positions_by_market(market_id)?;
            let has_open = db_positions.iter().any(|p| {
                p.platform == Platform::Kalshi && p.status == PositionStatus::Open
            });

            if !has_open && qty > 0.0 {
                report.orphaned_positions.push(OrphanedPosition {
                    platform: Platform::Kalshi,
                    market_id: market_id.clone(),
                    quantity: qty,
                });
            }
        }

        // Update heartbeat to indicate successful recovery
        self.record_heartbeat()?;

        Ok(report)
    }

    // ========================================================================
    // UTILITY OPERATIONS
    // ========================================================================

    /// Execute a raw SQL query (for migrations, etc.)
    pub fn execute_raw(&self, sql: &str) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        conn.execute_batch(sql)?;
        Ok(())
    }

    /// Create a backup of the database
    pub fn backup<P: AsRef<Path>>(&self, backup_path: P) -> DbResult<()> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let mut backup_conn = Connection::open(backup_path.as_ref())?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut backup_conn)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;

        Ok(())
    }

    /// Get database statistics
    pub fn get_stats(&self) -> DbResult<DatabaseStats> {
        let conn = self.conn.lock().map_err(|_| {
            DbError::Sqlite(rusqlite::Error::ExecuteReturnedResults)
        })?;

        let market_count: i64 = conn.query_row("SELECT COUNT(*) FROM markets", [], |r| r.get(0))?;
        let position_count: i64 = conn.query_row("SELECT COUNT(*) FROM positions", [], |r| r.get(0))?;
        let open_position_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM positions WHERE status = 'open'",
            [],
            |r| r.get(0),
        )?;
        let order_count: i64 = conn.query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))?;
        let price_history_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM price_history", [], |r| r.get(0))?;

        Ok(DatabaseStats {
            market_count: market_count as usize,
            position_count: position_count as usize,
            open_position_count: open_position_count as usize,
            order_count: order_count as usize,
            price_history_count: price_history_count as usize,
        })
    }
}

// ============================================================================
// RECOVERY TYPES
// ============================================================================

/// Report from state recovery operation
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Positions that were verified as matching exchange state
    pub verified_positions: Vec<i64>,

    /// Positions that had quantity mismatches and were updated
    pub quantity_mismatches: Vec<QuantityMismatch>,

    /// Positions that were closed because they no longer exist on exchange
    pub closed_positions: Vec<i64>,

    /// Positions found on exchange but not in database
    pub orphaned_positions: Vec<OrphanedPosition>,
}

/// Details of a quantity mismatch during recovery
#[derive(Debug)]
pub struct QuantityMismatch {
    pub position_id: i64,
    pub market_id: String,
    pub saved_quantity: f64,
    pub actual_quantity: f64,
}

/// An exchange position not found in the database
#[derive(Debug)]
pub struct OrphanedPosition {
    pub platform: Platform,
    pub market_id: String,
    pub quantity: f64,
}

/// Database statistics
#[derive(Debug)]
pub struct DatabaseStats {
    pub market_count: usize,
    pub position_count: usize,
    pub open_position_count: usize,
    pub order_count: usize,
    pub price_history_count: usize,
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.initialize().unwrap();
        db
    }

    #[test]
    fn test_market_crud() {
        let db = setup_test_db();

        // Insert market
        let market = NewMarket {
            id: "test-market-1".to_string(),
            platform: Platform::Polymarket,
            title: "Test Market".to_string(),
            category: Some("sports".to_string()),
            resolution_time: None,
            external_id: Some("ext-123".to_string()),
        };

        db.insert_market(&market).unwrap();

        // Get market
        let fetched = db.get_market("test-market-1").unwrap().unwrap();
        assert_eq!(fetched.id, "test-market-1");
        assert_eq!(fetched.title, "Test Market");
        assert_eq!(fetched.platform, Platform::Polymarket);

        // Update Kalman state
        let kalman = KalmanState {
            estimate: 0.75,
            variance: 0.05,
            updated_at: Utc::now(),
        };
        db.update_kalman_state("test-market-1", &kalman).unwrap();

        let fetched = db.get_market("test-market-1").unwrap().unwrap();
        assert!((fetched.kalman_estimate.unwrap() - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_position_crud() {
        let db = setup_test_db();

        // First insert a market
        let market = NewMarket {
            id: "market-1".to_string(),
            platform: Platform::Kalshi,
            title: "Test Market".to_string(),
            category: None,
            resolution_time: None,
            external_id: None,
        };
        db.insert_market(&market).unwrap();

        // Insert position
        let position = NewPosition {
            market_id: "market-1".to_string(),
            platform: Platform::Kalshi,
            side: Side::Yes,
            entry_price: 0.65,
            quantity: 100.0,
            strategy: Some("grinding".to_string()),
            notes: None,
        };

        let id = db.insert_position(&position).unwrap();
        assert!(id > 0);

        // Get position
        let fetched = db.get_position(id).unwrap().unwrap();
        assert_eq!(fetched.market_id, "market-1");
        assert_eq!(fetched.side, Side::Yes);
        assert!((fetched.entry_price - 0.65).abs() < 0.001);

        // Get open positions
        let open = db.get_open_positions().unwrap();
        assert_eq!(open.len(), 1);

        // Close position
        let pnl = db.close_position(id, Some(0.85)).unwrap();
        assert!(pnl > 0.0); // Should be profitable

        // Verify closed
        let open = db.get_open_positions().unwrap();
        assert_eq!(open.len(), 0);
    }

    #[test]
    fn test_system_state() {
        let db = setup_test_db();

        // Get initial state
        let halted = db.is_trading_halted().unwrap();
        assert!(!halted);

        // Set state
        db.set_trading_halted(true).unwrap();
        let halted = db.is_trading_halted().unwrap();
        assert!(halted);

        // Test capital
        let capital = db.get_current_capital().unwrap();
        assert!((capital - 100.0).abs() < 0.001);

        db.set_current_capital(150.0).unwrap();
        let capital = db.get_current_capital().unwrap();
        assert!((capital - 150.0).abs() < 0.001);
    }

    #[test]
    fn test_price_history() {
        let db = setup_test_db();

        // Insert market first
        let market = NewMarket {
            id: "market-ph".to_string(),
            platform: Platform::Polymarket,
            title: "Price History Market".to_string(),
            category: None,
            resolution_time: None,
            external_id: None,
        };
        db.insert_market(&market).unwrap();

        // Insert price history
        let record = NewPriceHistory {
            market_id: "market-ph".to_string(),
            platform: Platform::Polymarket,
            timestamp: Utc::now(),
            bid: Some(0.45),
            ask: Some(0.48),
            last_price: Some(0.46),
            volume_24h: Some(10000.0),
            bid_depth: Some(500.0),
            ask_depth: Some(600.0),
        };

        let id = db.insert_price_history(&record).unwrap();
        assert!(id > 0);

        // Query history
        let start = Utc::now() - chrono::Duration::hours(1);
        let end = Utc::now() + chrono::Duration::hours(1);
        let history = db.get_price_history("market-ph", start, end).unwrap();
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn test_correlations() {
        let db = setup_test_db();

        // Insert markets
        for i in 1..=3 {
            let market = NewMarket {
                id: format!("corr-market-{}", i),
                platform: Platform::Polymarket,
                title: format!("Correlation Market {}", i),
                category: None,
                resolution_time: None,
                external_id: None,
            };
            db.insert_market(&market).unwrap();
        }

        // Insert correlation
        let correlation = Correlation {
            market_id_1: "corr-market-1".to_string(),
            market_id_2: "corr-market-2".to_string(),
            correlation_type: CorrelationType::SameEvent,
            confidence: 0.95,
            created_at: Utc::now(),
            validated_at: None,
            notes: Some("Test correlation".to_string()),
        };

        db.upsert_correlation(&correlation).unwrap();

        // Get correlations
        let correlations = db.get_correlations_for_market("corr-market-1").unwrap();
        assert_eq!(correlations.len(), 1);

        // Get correlated IDs
        let ids = db.get_correlated_market_ids("corr-market-1").unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "corr-market-2");
    }

    #[test]
    fn test_recovery() {
        let db = setup_test_db();

        // Setup: Insert market and position
        let market = NewMarket {
            id: "recovery-market".to_string(),
            platform: Platform::Polymarket,
            title: "Recovery Test".to_string(),
            category: None,
            resolution_time: None,
            external_id: None,
        };
        db.insert_market(&market).unwrap();

        let position = NewPosition {
            market_id: "recovery-market".to_string(),
            platform: Platform::Polymarket,
            side: Side::Yes,
            entry_price: 0.50,
            quantity: 100.0,
            strategy: None,
            notes: None,
        };
        let pos_id = db.insert_position(&position).unwrap();

        // Simulate recovery with mismatched quantity
        let mut poly_positions = std::collections::HashMap::new();
        poly_positions.insert("recovery-market".to_string(), 80.0); // Changed from 100

        let kalshi_positions = std::collections::HashMap::new();

        let report = db.recover_state(&poly_positions, &kalshi_positions).unwrap();

        // Verify mismatch was detected
        assert_eq!(report.quantity_mismatches.len(), 1);
        assert_eq!(report.quantity_mismatches[0].position_id, pos_id);

        // Verify quantity was updated
        let updated = db.get_position(pos_id).unwrap().unwrap();
        assert!((updated.quantity - 80.0).abs() < 0.001);
    }
}
