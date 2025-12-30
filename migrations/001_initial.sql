-- Prediction Market Trading System - Initial Schema
-- Version: 1.0.0
-- Created: 2024-12-30
--
-- This schema supports:
-- - Market tracking with Kalman filter state and volatility statistics
-- - Position management with entry/exit tracking
-- - Order execution audit trail
-- - Price history for backtesting
-- - Correlation tracking between markets
-- - Daily P&L snapshots
-- - System state for crash recovery

-- Enable foreign key enforcement (must be done per connection)
PRAGMA foreign_keys = ON;

-- ============================================================================
-- MARKETS TABLE
-- Tracks all markets across both platforms with Kalman filter state
-- ============================================================================
CREATE TABLE IF NOT EXISTS markets (
    -- Primary identifier (composite key from platform + external ID)
    id TEXT PRIMARY KEY NOT NULL,

    -- Platform: 'polymarket' or 'kalshi'
    platform TEXT NOT NULL CHECK (platform IN ('polymarket', 'kalshi')),

    -- Human-readable market title
    title TEXT NOT NULL,

    -- Market category (sports, politics, crypto, etc.)
    category TEXT,

    -- When the market resolves (UTC)
    resolution_time DATETIME,

    -- When we started tracking this market
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- Last time any field was updated
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- === Kalman Filter State ===
    -- Current probability estimate (0.0 to 1.0)
    kalman_estimate REAL,

    -- Estimation uncertainty (variance)
    kalman_variance REAL,

    -- When the Kalman state was last updated
    kalman_updated_at DATETIME,

    -- === Volatility Statistics (for thesis invalidation) ===
    -- Rolling mean of price changes
    price_change_mean REAL DEFAULT 0.0,

    -- Rolling standard deviation of price changes
    price_change_std REAL DEFAULT 0.0,

    -- Rolling mean of volume
    volume_mean REAL DEFAULT 0.0,

    -- Number of samples collected for volatility stats
    volatility_samples INTEGER DEFAULT 0,

    -- === Additional Metadata ===
    -- External market ID on the platform
    external_id TEXT,

    -- Whether we're actively trading this market
    is_active BOOLEAN DEFAULT 1,

    -- Last known best bid price
    last_bid REAL,

    -- Last known best ask price
    last_ask REAL,

    -- 24h trading volume
    volume_24h REAL
);

-- Index for querying active markets by platform
CREATE INDEX IF NOT EXISTS idx_markets_platform_active
    ON markets(platform, is_active);

-- Index for finding markets by resolution time
CREATE INDEX IF NOT EXISTS idx_markets_resolution
    ON markets(resolution_time)
    WHERE resolution_time IS NOT NULL;

-- Index for finding markets by category
CREATE INDEX IF NOT EXISTS idx_markets_category
    ON markets(category)
    WHERE category IS NOT NULL;

-- ============================================================================
-- POSITIONS TABLE
-- Tracks all trading positions with entry/exit details
-- ============================================================================
CREATE TABLE IF NOT EXISTS positions (
    -- Auto-incrementing position ID
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Reference to the market
    market_id TEXT NOT NULL,

    -- Platform where the position is held
    platform TEXT NOT NULL CHECK (platform IN ('polymarket', 'kalshi')),

    -- Position side: 'yes' or 'no'
    side TEXT NOT NULL CHECK (side IN ('yes', 'no')),

    -- Entry price (0.0 to 1.0)
    entry_price REAL NOT NULL CHECK (entry_price >= 0 AND entry_price <= 1),

    -- Position quantity (number of contracts or shares)
    quantity REAL NOT NULL CHECK (quantity > 0),

    -- When the position was entered
    entry_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- Exit price (NULL if still open)
    exit_price REAL CHECK (exit_price IS NULL OR (exit_price >= 0 AND exit_price <= 1)),

    -- When the position was closed (NULL if still open)
    exit_time DATETIME,

    -- Realized P&L (NULL if still open, calculated on close)
    pnl REAL,

    -- Position status: 'open', 'closed', 'resolved'
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'closed', 'resolved')),

    -- Strategy that opened this position
    strategy TEXT,

    -- Human-readable notes (for manual review)
    notes TEXT,

    -- Foreign key to markets
    FOREIGN KEY (market_id) REFERENCES markets(id) ON DELETE RESTRICT
);

-- Index for finding open positions
CREATE INDEX IF NOT EXISTS idx_positions_status
    ON positions(status)
    WHERE status = 'open';

-- Index for finding positions by market
CREATE INDEX IF NOT EXISTS idx_positions_market
    ON positions(market_id);

-- Index for finding positions by platform
CREATE INDEX IF NOT EXISTS idx_positions_platform
    ON positions(platform);

-- Index for P&L analysis by time
CREATE INDEX IF NOT EXISTS idx_positions_exit_time
    ON positions(exit_time)
    WHERE exit_time IS NOT NULL;

-- ============================================================================
-- ORDERS TABLE
-- Complete audit trail of all order submissions
-- ============================================================================
CREATE TABLE IF NOT EXISTS orders (
    -- Auto-incrementing order ID
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Reference to the position (NULL for orders that didn't create positions)
    position_id INTEGER,

    -- Reference to the market
    market_id TEXT NOT NULL,

    -- Platform where the order was submitted
    platform TEXT NOT NULL CHECK (platform IN ('polymarket', 'kalshi')),

    -- Order side: 'buy' or 'sell'
    side TEXT NOT NULL CHECK (side IN ('buy', 'sell')),

    -- Order type: 'market', 'limit', 'twap'
    order_type TEXT NOT NULL CHECK (order_type IN ('market', 'limit', 'twap')),

    -- Limit price (NULL for market orders)
    price REAL CHECK (price IS NULL OR (price >= 0 AND price <= 1)),

    -- Requested quantity
    quantity REAL NOT NULL CHECK (quantity > 0),

    -- Filled quantity (may differ from requested)
    filled_quantity REAL DEFAULT 0,

    -- Average fill price
    fill_price REAL,

    -- Order status: 'pending', 'submitted', 'partially_filled', 'filled', 'cancelled', 'failed', 'expired'
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'submitted', 'partially_filled', 'filled', 'cancelled', 'failed', 'expired')),

    -- When the order was created locally
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- When the order was submitted to the platform
    submitted_at DATETIME,

    -- When the order was fully filled
    filled_at DATETIME,

    -- Platform's order ID (for reconciliation)
    platform_order_id TEXT,

    -- Error message if order failed
    error_message TEXT,

    -- JSON blob of platform-specific order details
    platform_metadata TEXT,

    -- Foreign keys
    FOREIGN KEY (position_id) REFERENCES positions(id) ON DELETE SET NULL,
    FOREIGN KEY (market_id) REFERENCES markets(id) ON DELETE RESTRICT
);

-- Index for finding pending/active orders
CREATE INDEX IF NOT EXISTS idx_orders_status
    ON orders(status)
    WHERE status IN ('pending', 'submitted', 'partially_filled');

-- Index for finding orders by market
CREATE INDEX IF NOT EXISTS idx_orders_market
    ON orders(market_id);

-- Index for finding orders by position
CREATE INDEX IF NOT EXISTS idx_orders_position
    ON orders(position_id)
    WHERE position_id IS NOT NULL;

-- Index for order reconciliation by platform ID
CREATE INDEX IF NOT EXISTS idx_orders_platform_id
    ON orders(platform, platform_order_id)
    WHERE platform_order_id IS NOT NULL;

-- ============================================================================
-- PRICE_HISTORY TABLE
-- Historical price data for backtesting and analysis
-- ============================================================================
CREATE TABLE IF NOT EXISTS price_history (
    -- Auto-incrementing row ID
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Reference to the market
    market_id TEXT NOT NULL,

    -- Platform where the price was observed
    platform TEXT NOT NULL CHECK (platform IN ('polymarket', 'kalshi')),

    -- When the price was observed (UTC)
    timestamp DATETIME NOT NULL,

    -- Best bid price
    bid REAL CHECK (bid IS NULL OR (bid >= 0 AND bid <= 1)),

    -- Best ask price
    ask REAL CHECK (ask IS NULL OR (ask >= 0 AND ask <= 1)),

    -- Last traded price
    last_price REAL CHECK (last_price IS NULL OR (last_price >= 0 AND last_price <= 1)),

    -- 24-hour trading volume
    volume_24h REAL,

    -- Bid depth (available size at best bid)
    bid_depth REAL,

    -- Ask depth (available size at best ask)
    ask_depth REAL,

    -- Foreign key to markets
    FOREIGN KEY (market_id) REFERENCES markets(id) ON DELETE CASCADE
);

-- Composite index for time-series queries
CREATE INDEX IF NOT EXISTS idx_price_history_market_time
    ON price_history(market_id, timestamp DESC);

-- Index for backtesting date range queries
CREATE INDEX IF NOT EXISTS idx_price_history_timestamp
    ON price_history(timestamp);

-- ============================================================================
-- CORRELATIONS TABLE
-- Tracks correlations between markets for exposure management
-- ============================================================================
CREATE TABLE IF NOT EXISTS correlations (
    -- First market in the correlation pair
    market_id_1 TEXT NOT NULL,

    -- Second market in the correlation pair
    market_id_2 TEXT NOT NULL,

    -- Type of correlation:
    -- 'same_event': Same underlying event on different platforms
    -- 'same_resolution': Different markets resolving on same data point
    -- 'category_time': Same category with time overlap
    correlation_type TEXT NOT NULL
        CHECK (correlation_type IN ('same_event', 'same_resolution', 'category_time')),

    -- Confidence in the correlation (0.0 to 1.0)
    confidence REAL NOT NULL DEFAULT 1.0 CHECK (confidence >= 0 AND confidence <= 1),

    -- When this correlation was identified
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- When this correlation was last validated
    validated_at DATETIME,

    -- Notes about the correlation
    notes TEXT,

    -- Composite primary key (ordered to prevent duplicates)
    PRIMARY KEY (market_id_1, market_id_2),

    -- Ensure market_id_1 < market_id_2 to prevent duplicate pairs
    CHECK (market_id_1 < market_id_2),

    -- Foreign keys
    FOREIGN KEY (market_id_1) REFERENCES markets(id) ON DELETE CASCADE,
    FOREIGN KEY (market_id_2) REFERENCES markets(id) ON DELETE CASCADE
);

-- Index for finding all correlations for a market
CREATE INDEX IF NOT EXISTS idx_correlations_market1
    ON correlations(market_id_1);

CREATE INDEX IF NOT EXISTS idx_correlations_market2
    ON correlations(market_id_2);

-- Index for finding correlations by type
CREATE INDEX IF NOT EXISTS idx_correlations_type
    ON correlations(correlation_type);

-- ============================================================================
-- DAILY_SNAPSHOTS TABLE
-- Daily P&L and performance tracking
-- ============================================================================
CREATE TABLE IF NOT EXISTS daily_snapshots (
    -- Date of the snapshot (one per day)
    date DATE PRIMARY KEY NOT NULL,

    -- Capital at start of day
    starting_capital REAL NOT NULL CHECK (starting_capital >= 0),

    -- Capital at end of day
    ending_capital REAL NOT NULL CHECK (ending_capital >= 0),

    -- Total realized P&L for the day
    realized_pnl REAL NOT NULL,

    -- Unrealized P&L at end of day (mark-to-market)
    unrealized_pnl REAL NOT NULL,

    -- Number of trades executed
    trades_count INTEGER NOT NULL DEFAULT 0 CHECK (trades_count >= 0),

    -- Number of winning trades
    win_count INTEGER NOT NULL DEFAULT 0 CHECK (win_count >= 0),

    -- Maximum intraday drawdown (as decimal, e.g., 0.05 = 5%)
    max_drawdown REAL NOT NULL DEFAULT 0 CHECK (max_drawdown >= 0),

    -- High water mark reached during the day
    high_water_mark REAL,

    -- Number of open positions at end of day
    open_positions_count INTEGER DEFAULT 0,

    -- Total exposure at end of day
    total_exposure REAL DEFAULT 0,

    -- Additional metrics as JSON
    metrics_json TEXT,

    -- Constraints
    CHECK (win_count <= trades_count)
);

-- Index for time-range queries
CREATE INDEX IF NOT EXISTS idx_daily_snapshots_date
    ON daily_snapshots(date DESC);

-- ============================================================================
-- SYSTEM_STATE TABLE
-- Key-value store for system state and crash recovery
-- ============================================================================
CREATE TABLE IF NOT EXISTS system_state (
    -- State key (e.g., 'trading_halted', 'last_scan_time', 'current_capital')
    key TEXT PRIMARY KEY NOT NULL,

    -- State value (stored as text, parse as needed)
    value TEXT NOT NULL,

    -- Value type hint for parsing ('string', 'int', 'float', 'bool', 'json', 'datetime')
    value_type TEXT DEFAULT 'string'
        CHECK (value_type IN ('string', 'int', 'float', 'bool', 'json', 'datetime')),

    -- When this state was last updated
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- Description of what this state key represents
    description TEXT
);

-- ============================================================================
-- TRIGGERS
-- Automatic timestamp updates and data integrity
-- ============================================================================

-- Update markets.updated_at on any change
CREATE TRIGGER IF NOT EXISTS update_markets_timestamp
    AFTER UPDATE ON markets
    FOR EACH ROW
BEGIN
    UPDATE markets SET updated_at = CURRENT_TIMESTAMP WHERE id = NEW.id;
END;

-- Update system_state.updated_at on any change
CREATE TRIGGER IF NOT EXISTS update_system_state_timestamp
    AFTER UPDATE ON system_state
    FOR EACH ROW
BEGIN
    UPDATE system_state SET updated_at = CURRENT_TIMESTAMP WHERE key = NEW.key;
END;

-- ============================================================================
-- INITIAL SYSTEM STATE
-- Default values for required system state keys
-- ============================================================================

INSERT OR IGNORE INTO system_state (key, value, value_type, description) VALUES
    ('trading_halted', 'false', 'bool', 'Emergency stop flag - if true, no new trades'),
    ('trading_mode', 'paper', 'string', 'Trading mode: paper or live'),
    ('current_capital', '100.0', 'float', 'Current available capital in USD'),
    ('starting_capital', '100.0', 'float', 'Initial capital when system started'),
    ('high_water_mark', '100.0', 'float', 'All-time high capital (for drawdown calc)'),
    ('consecutive_losses', '0', 'int', 'Number of consecutive losing trades'),
    ('last_trade_time', '', 'datetime', 'Timestamp of last executed trade'),
    ('last_heartbeat', '', 'datetime', 'Last system heartbeat timestamp'),
    ('circuit_breaker_triggered_at', '', 'datetime', 'When circuit breaker was last triggered'),
    ('schema_version', '1', 'int', 'Database schema version for migrations');

-- ============================================================================
-- VIEWS
-- Convenient views for common queries
-- ============================================================================

-- View: Open positions with market details
CREATE VIEW IF NOT EXISTS v_open_positions AS
SELECT
    p.id AS position_id,
    p.market_id,
    m.title AS market_title,
    m.platform,
    p.side,
    p.entry_price,
    p.quantity,
    p.entry_time,
    p.strategy,
    m.last_bid,
    m.last_ask,
    -- Estimate current value (mid price)
    CASE
        WHEN m.last_bid IS NOT NULL AND m.last_ask IS NOT NULL
        THEN (m.last_bid + m.last_ask) / 2.0
        ELSE p.entry_price
    END AS current_price,
    -- Estimate unrealized P&L
    CASE
        WHEN p.side = 'yes' THEN
            (COALESCE((m.last_bid + m.last_ask) / 2.0, p.entry_price) - p.entry_price) * p.quantity
        ELSE
            (p.entry_price - COALESCE((m.last_bid + m.last_ask) / 2.0, p.entry_price)) * p.quantity
    END AS unrealized_pnl
FROM positions p
JOIN markets m ON p.market_id = m.id
WHERE p.status = 'open';

-- View: Recent trades (last 100)
CREATE VIEW IF NOT EXISTS v_recent_trades AS
SELECT
    p.id AS position_id,
    p.market_id,
    m.title AS market_title,
    p.platform,
    p.side,
    p.entry_price,
    p.exit_price,
    p.quantity,
    p.pnl,
    p.entry_time,
    p.exit_time,
    p.strategy
FROM positions p
LEFT JOIN markets m ON p.market_id = m.id
WHERE p.status IN ('closed', 'resolved')
ORDER BY p.exit_time DESC
LIMIT 100;

-- View: Market correlations (bidirectional lookup)
CREATE VIEW IF NOT EXISTS v_market_correlations AS
SELECT
    c.market_id_1,
    c.market_id_2,
    c.correlation_type,
    c.confidence,
    m1.title AS market_1_title,
    m2.title AS market_2_title
FROM correlations c
JOIN markets m1 ON c.market_id_1 = m1.id
JOIN markets m2 ON c.market_id_2 = m2.id
UNION ALL
SELECT
    c.market_id_2 AS market_id_1,
    c.market_id_1 AS market_id_2,
    c.correlation_type,
    c.confidence,
    m2.title AS market_1_title,
    m1.title AS market_2_title
FROM correlations c
JOIN markets m1 ON c.market_id_1 = m1.id
JOIN markets m2 ON c.market_id_2 = m2.id;

-- View: Daily performance summary
CREATE VIEW IF NOT EXISTS v_performance_summary AS
SELECT
    date,
    starting_capital,
    ending_capital,
    realized_pnl,
    unrealized_pnl,
    trades_count,
    win_count,
    CASE
        WHEN trades_count > 0 THEN CAST(win_count AS REAL) / trades_count * 100
        ELSE 0
    END AS win_rate_pct,
    max_drawdown * 100 AS max_drawdown_pct,
    (ending_capital - starting_capital) / starting_capital * 100 AS daily_return_pct
FROM daily_snapshots
ORDER BY date DESC;
