# Prediction Market Trading System — Technical Specification v2.0

> A fully automated, multi-strategy prediction market trading system with Bayesian probability estimation, real-time risk management, and a web-based dashboard.

---

## Table of Contents

1. [Overview](#overview)
2. [Capital & Risk Parameters](#capital--risk-parameters)
3. [Platform Integration](#platform-integration)
4. [Trading Strategies](#trading-strategies)
5. [Probability Estimation (Kalman Filter)](#probability-estimation-kalman-filter)
6. [Position Management](#position-management)
7. [Order Execution](#order-execution)
8. [Thesis Invalidation & Exit Logic](#thesis-invalidation--exit-logic)
9. [Data Persistence](#data-persistence)
10. [Web Dashboard](#web-dashboard)
11. [Alerting & Notifications](#alerting--notifications)
12. [Operational Controls](#operational-controls)
13. [Development Phases](#development-phases)
14. [Database Schema](#database-schema)
15. [API Specifications](#api-specifications)
16. [Configuration](#configuration)

---

## Overview

### System Goals

- **Primary Strategy**: High-probability grinding (buy 90%+ outcomes, compound gains)
- **Secondary**: Cross-platform arbitrage (Polymarket ↔ Kalshi)
- **Tertiary**: Multi-outcome bundle arbitrage
- **Automation Level**: Fully automated 24/7 execution
- **Deployment**: Local machine (macOS)

### Key Metrics

| Parameter | Value |
|-----------|-------|
| Starting Capital | $100 |
| Target Capital | $50,000+ |
| Max Drawdown | 5% of capital |
| Max Expiry Window | 24 hours |
| Position Sizing | Kelly Criterion |
| Compounding | Full (reinvest all profits) |

---

## Capital & Risk Parameters

### Risk Limits

```rust
pub struct RiskConfig {
    /// Maximum drawdown before circuit breaker triggers (0.05 = 5%)
    pub max_drawdown_pct: f64,  // 0.05

    /// Maximum exposure to correlated markets (treated as single position)
    pub max_correlated_exposure_pct: f64,  // 1.0 (no limit, but tracked)

    /// Maximum concurrent positions (0 = unlimited)
    pub max_positions: u32,  // 0 (unlimited)

    /// Minimum time between trades on same market (seconds)
    pub trade_cooldown_secs: u64,  // 60
}
```

### Correlation Tracking

Markets are considered **correlated** if they share:
- Same underlying event (e.g., "Trump wins 2028" variants)
- Same resolution dependency (e.g., multiple markets resolving on same data point)
- Category + time overlap (e.g., two NBA games on same night)

**Treatment**: Correlated markets are aggregated for exposure calculation. If market A and B are correlated, `exposure = position_A + position_B` for risk checks.

---

## Platform Integration

### Supported Platforms

| Platform | API Type | Auth Method | Features |
|----------|----------|-------------|----------|
| Polymarket | REST + WebSocket | Wallet signature (ECDSA) | CLOB, real-time orderbook |
| Kalshi | REST + WebSocket | API Key + RSA signature | Traditional exchange |

### Rate Limit Handling

```rust
pub struct RateLimiter {
    /// Exponential backoff configuration
    pub initial_delay_ms: u64,    // 1000
    pub max_delay_ms: u64,        // 60000
    pub backoff_multiplier: f64,  // 2.0

    /// Per-platform limits
    pub polymarket_rpm: u32,      // 100
    pub kalshi_rpm: u32,          // 60
}
```

**Strategy**: On 429 response, apply exponential backoff. Queue non-critical requests, prioritize order execution.

---

## Trading Strategies

### Strategy 1: High-Probability Grinding (PRIMARY)

**Concept**: Buy outcomes trading at high probability (90%+) that are near-certain to resolve favorably. Compound small gains rapidly.

#### Entry Criteria

```rust
pub struct GrindingConfig {
    /// Dynamic probability threshold based on time to expiry
    /// threshold = base + (1.0 - base) * (1.0 - hours_remaining / 24.0)
    pub base_probability_threshold: f64,  // 0.90

    /// Maximum hours until resolution
    pub max_hours_to_expiry: f64,  // 24.0

    /// Minimum expected value (dynamic based on position size)
    /// min_ev = base_ev * (1.0 + position_size / capital)
    pub base_min_ev_pct: f64,  // 0.005 (0.5%)

    /// Minimum liquidity (USD) at target price
    pub min_liquidity_usd: f64,  // 10.0
}
```

#### Dynamic Probability Threshold

| Hours to Expiry | Min Probability |
|-----------------|-----------------|
| 24 | 90.0% |
| 12 | 95.0% |
| 6 | 97.5% |
| 1 | 99.0% |

**Rationale**: As expiry approaches, require higher certainty because there's less time for edge cases to be priced in.

### Strategy 2: Cross-Platform Arbitrage

**Concept**: Exploit price discrepancies between Polymarket and Kalshi on equivalent markets.

#### Entry Criteria

```rust
pub struct ArbitrageConfig {
    /// Minimum profit after fees (cents per dollar)
    pub min_profit_cents: f64,  // 1.0 (1%)

    /// Maximum time to hold arb position (seconds)
    pub max_hold_time_secs: u64,  // 3600

    /// Require both legs executable simultaneously
    pub require_simultaneous_fill: bool,  // true
}
```

### Strategy 3: Multi-Outcome Bundle Arbitrage

**Concept**: In markets with N mutually exclusive outcomes, if `sum(best_asks) < 1.0`, buy all outcomes for guaranteed profit.

#### Entry Criteria

```rust
pub struct BundleArbConfig {
    /// Minimum profit margin
    pub min_profit_pct: f64,  // 0.01 (1%)

    /// All outcomes must have sufficient liquidity
    pub min_liquidity_per_outcome_usd: f64,  // 5.0
}
```

---

## Probability Estimation (Kalman Filter)

### Model Overview

We use a Kalman filter to estimate the "true" underlying probability of each market, smoothing noise from orderbook fluctuations.

```rust
pub struct KalmanState {
    /// Current probability estimate (0.0 to 1.0)
    pub estimate: f64,

    /// Estimation uncertainty (variance)
    pub variance: f64,

    /// Last update timestamp
    pub last_update: DateTime<Utc>,
}

pub struct KalmanConfig {
    /// Initial variance for new markets
    pub initial_variance: f64,  // 0.1

    /// Measurement noise (how much we trust each price observation)
    pub measurement_noise: f64,  // 0.01

    /// Base process noise (how much true probability drifts)
    pub base_process_noise: f64,  // 0.001

    /// Process noise multiplier as expiry approaches
    /// actual_noise = base * (1.0 + multiplier * (1.0 - hours_remaining / 24.0))
    pub expiry_noise_multiplier: f64,  // 10.0
}
```

### Adaptive Process Noise

Process noise increases as expiry approaches:

| Hours to Expiry | Process Noise |
|-----------------|---------------|
| 24 | 0.001 |
| 12 | 0.006 |
| 6 | 0.0085 |
| 1 | 0.0106 |

**Rationale**: Near expiry, new information is more likely to cause large probability shifts.

### Update Equations

```rust
fn kalman_update(state: &mut KalmanState, measurement: f64, config: &KalmanConfig, hours_to_expiry: f64) {
    // Adaptive process noise
    let process_noise = config.base_process_noise
        * (1.0 + config.expiry_noise_multiplier * (1.0 - hours_to_expiry / 24.0).max(0.0));

    // Predict step
    let predicted_variance = state.variance + process_noise;

    // Update step
    let kalman_gain = predicted_variance / (predicted_variance + config.measurement_noise);
    state.estimate = state.estimate + kalman_gain * (measurement - state.estimate);
    state.variance = (1.0 - kalman_gain) * predicted_variance;
}
```

### Cross-Platform Fusion

When both Polymarket and Kalshi have prices for the same event:

```rust
fn fuse_estimates(poly_price: f64, poly_volume: f64, kalshi_price: f64, kalshi_volume: f64) -> f64 {
    // Volume-weighted average
    let total_volume = poly_volume + kalshi_volume;
    (poly_price * poly_volume + kalshi_price * kalshi_volume) / total_volume
}
```

---

## Position Management

### Kelly Criterion Sizing

```rust
fn kelly_size(edge: f64, odds: f64, capital: f64, config: &KellyConfig) -> f64 {
    // Kelly formula: f* = (p * b - q) / b
    // where p = probability of win, b = odds, q = 1 - p
    let p = edge + (1.0 / (1.0 + odds));  // Our estimated true probability
    let q = 1.0 - p;
    let b = odds;

    let kelly_fraction = (p * b - q) / b;

    // Apply fractional Kelly (half Kelly is common for risk management)
    let adjusted_fraction = kelly_fraction * config.kelly_multiplier;

    // Cap at maximum position size
    let position_size = (capital * adjusted_fraction).min(capital * config.max_position_pct);

    position_size.max(0.0)
}

pub struct KellyConfig {
    /// Fraction of full Kelly to use (0.5 = half Kelly)
    pub kelly_multiplier: f64,  // 0.5

    /// Maximum single position as fraction of capital
    pub max_position_pct: f64,  // 1.0 (no limit, but correlation limits apply)
}
```

### Correlation-Adjusted Sizing

```rust
fn adjusted_position_size(
    base_size: f64,
    existing_correlated_exposure: f64,
    capital: f64,
    max_correlated_pct: f64
) -> f64 {
    let max_allowed = capital * max_correlated_pct;
    let remaining_capacity = max_allowed - existing_correlated_exposure;
    base_size.min(remaining_capacity).max(0.0)
}
```

---

## Order Execution

### Adaptive Order Routing

```rust
pub enum OrderStrategy {
    /// Use limit orders, wait for fill
    Limit { price: f64, timeout_secs: u64 },

    /// Market order, immediate fill
    Market,

    /// Time-weighted average price
    Twap { chunks: u32, interval_secs: u64 },
}

fn select_order_strategy(order_size_usd: f64, urgency: Urgency, liquidity: &Liquidity) -> OrderStrategy {
    match (order_size_usd, urgency) {
        // Small orders or urgent: market order
        (size, _) if size < 50.0 => OrderStrategy::Market,
        (_, Urgency::High) => OrderStrategy::Market,

        // Medium orders: limit with timeout
        (size, _) if size < 500.0 => OrderStrategy::Limit {
            price: liquidity.best_ask * 1.001,  // 0.1% above best ask
            timeout_secs: 30,
        },

        // Large orders: TWAP
        _ => OrderStrategy::Twap {
            chunks: 5,
            interval_secs: 60,
        },
    }
}
```

### Slippage Protection

```rust
pub struct SlippageConfig {
    /// Maximum acceptable slippage (0.02 = 2%)
    pub max_slippage_pct: f64,  // 0.02

    /// Abort if orderbook depth insufficient
    pub min_depth_usd: f64,  // 100.0
}
```

---

## Thesis Invalidation & Exit Logic

### Combined Signal Detection

A position's thesis is considered **invalidated** when abnormal market activity suggests new information has emerged.

```rust
pub struct ThesisInvalidation {
    /// Price velocity threshold (auto-calibrated per market)
    pub price_velocity_std_devs: f64,  // 2.0

    /// Volume spike threshold
    pub volume_spike_multiplier: f64,  // 3.0

    /// Minimum observations for calibration
    pub min_calibration_samples: u32,  // 100

    /// Rolling window for baseline calculation (seconds)
    pub baseline_window_secs: u64,  // 3600
}
```

### Auto-Calibration

For each market, we track:

```rust
pub struct MarketVolatility {
    /// Rolling mean of price changes
    pub price_change_mean: f64,

    /// Rolling std dev of price changes
    pub price_change_std: f64,

    /// Rolling mean of volume
    pub volume_mean: f64,

    /// Sample count
    pub samples: u32,
}
```

**Invalidation triggers when**:
```rust
fn is_thesis_invalidated(
    current_price: f64,
    prev_price: f64,
    current_volume: f64,
    stats: &MarketVolatility,
    config: &ThesisInvalidation
) -> bool {
    if stats.samples < config.min_calibration_samples {
        return false;  // Not enough data to judge
    }

    let price_change = (current_price - prev_price).abs();
    let price_z_score = (price_change - stats.price_change_mean) / stats.price_change_std;

    let volume_ratio = current_volume / stats.volume_mean;

    // Invalidate if EITHER condition is extreme
    price_z_score > config.price_velocity_std_devs
        || volume_ratio > config.volume_spike_multiplier
}
```

### Exit Logic

```rust
enum ExitDecision {
    /// Hold position to resolution
    Hold,

    /// Exit immediately at market
    ExitMarket { reason: String },

    /// Exit with limit order
    ExitLimit { price: f64, timeout_secs: u64 },
}

fn decide_exit(position: &Position, market: &Market, invalidated: bool) -> ExitDecision {
    if invalidated {
        // New information detected - exit to preserve capital
        return ExitDecision::ExitMarket {
            reason: "Thesis invalidated by abnormal market activity".into()
        };
    }

    // Check if position is underwater
    let pnl_pct = (market.current_price - position.entry_price) / position.entry_price;

    if pnl_pct < -0.5 && !invalidated {
        // Large loss but no new info - likely noise, hold
        ExitDecision::Hold
    } else {
        ExitDecision::Hold
    }
}
```

---

## Data Persistence

### SQLite Schema

```sql
-- Markets table
CREATE TABLE markets (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,  -- 'polymarket' | 'kalshi'
    title TEXT NOT NULL,
    category TEXT,
    resolution_time DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,

    -- Kalman state
    kalman_estimate REAL,
    kalman_variance REAL,
    kalman_updated_at DATETIME,

    -- Volatility stats
    price_change_mean REAL,
    price_change_std REAL,
    volume_mean REAL,
    volatility_samples INTEGER DEFAULT 0
);

-- Positions table
CREATE TABLE positions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    market_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    side TEXT NOT NULL,  -- 'yes' | 'no'
    entry_price REAL NOT NULL,
    quantity REAL NOT NULL,
    entry_time DATETIME DEFAULT CURRENT_TIMESTAMP,
    exit_price REAL,
    exit_time DATETIME,
    pnl REAL,
    status TEXT DEFAULT 'open',  -- 'open' | 'closed' | 'resolved'

    FOREIGN KEY (market_id) REFERENCES markets(id)
);

-- Orders table
CREATE TABLE orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    position_id INTEGER,
    market_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    side TEXT NOT NULL,
    order_type TEXT NOT NULL,  -- 'market' | 'limit'
    price REAL,
    quantity REAL NOT NULL,
    status TEXT DEFAULT 'pending',  -- 'pending' | 'filled' | 'cancelled' | 'failed'
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    filled_at DATETIME,
    platform_order_id TEXT,
    error_message TEXT,

    FOREIGN KEY (position_id) REFERENCES positions(id),
    FOREIGN KEY (market_id) REFERENCES markets(id)
);

-- Price history (for backtesting)
CREATE TABLE price_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    market_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    timestamp DATETIME NOT NULL,
    bid REAL,
    ask REAL,
    last_price REAL,
    volume_24h REAL,

    FOREIGN KEY (market_id) REFERENCES markets(id)
);

-- Correlation mappings
CREATE TABLE correlations (
    market_id_1 TEXT NOT NULL,
    market_id_2 TEXT NOT NULL,
    correlation_type TEXT NOT NULL,  -- 'same_event' | 'same_resolution' | 'category_time'
    confidence REAL DEFAULT 1.0,

    PRIMARY KEY (market_id_1, market_id_2),
    FOREIGN KEY (market_id_1) REFERENCES markets(id),
    FOREIGN KEY (market_id_2) REFERENCES markets(id)
);

-- Daily snapshots for P&L tracking
CREATE TABLE daily_snapshots (
    date DATE PRIMARY KEY,
    starting_capital REAL NOT NULL,
    ending_capital REAL NOT NULL,
    realized_pnl REAL NOT NULL,
    unrealized_pnl REAL NOT NULL,
    trades_count INTEGER NOT NULL,
    win_count INTEGER NOT NULL,
    max_drawdown REAL NOT NULL
);

-- System state for recovery
CREATE TABLE system_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
```

### Recovery on Restart

```rust
async fn recover_state(db: &Database, poly: &PolymarketClient, kalshi: &KalshiClient) -> Result<()> {
    // 1. Load persisted state
    let saved_positions = db.get_open_positions().await?;

    // 2. Query actual positions from exchanges
    let poly_positions = poly.get_positions().await?;
    let kalshi_positions = kalshi.get_positions().await?;

    // 3. Reconcile
    for saved in saved_positions {
        let actual = match saved.platform {
            Platform::Polymarket => poly_positions.get(&saved.market_id),
            Platform::Kalshi => kalshi_positions.get(&saved.market_id),
        };

        match actual {
            Some(pos) if pos.quantity != saved.quantity => {
                // Quantity mismatch - update DB
                db.update_position_quantity(saved.id, pos.quantity).await?;
                log::warn!("Reconciled position {} quantity: {} -> {}", saved.id, saved.quantity, pos.quantity);
            }
            None => {
                // Position closed externally - mark as closed
                db.close_position(saved.id, None).await?;
                log::warn!("Position {} closed externally", saved.id);
            }
            _ => {}
        }
    }

    Ok(())
}
```

---

## Web Dashboard

### Technology Stack

- **Backend**: Rust (Axum)
- **Frontend**: HTMX + Tailwind CSS
- **Real-time**: Server-Sent Events (SSE)
- **Charts**: Chart.js

### Routes

```rust
// Dashboard routes
GET  /                      -> Dashboard home
GET  /positions             -> Open positions view
GET  /opportunities         -> Live opportunity scanner
GET  /history               -> Trade history
GET  /analytics             -> P&L charts, performance metrics
GET  /settings              -> Configuration panel

// API routes
GET  /api/positions         -> JSON list of positions
GET  /api/opportunities     -> JSON list of current opportunities
GET  /api/stats             -> Current P&L, drawdown, etc.
GET  /api/events            -> SSE stream of real-time updates

// Control routes
POST /api/kill-switch       -> Emergency halt all trading
POST /api/mode              -> Toggle paper/live trading
POST /api/config            -> Update runtime configuration
```

### Dashboard Panels

#### 1. Core Trading Panel
- Current capital (live)
- Open positions table (market, side, entry, current, P&L)
- Today's realized P&L
- All-time P&L

#### 2. Opportunity Scanner Panel
- Live-updating table of opportunities
- Columns: Market, Strategy, Expected Value, Kelly Size, Expiry
- Sort by EV descending
- Filter by strategy type

#### 3. Risk Gauges Panel
- Current drawdown % (gauge chart)
- Exposure by category (pie chart)
- Correlation heatmap (when multiple positions)
- Circuit breaker status

#### 4. Historical Charts Panel
- Cumulative P&L over time (line chart)
- Trade distribution by strategy (bar chart)
- Win rate over time (line chart)
- Daily returns histogram

---

## Alerting & Notifications

### Discord Webhook Integration

```rust
pub struct DiscordConfig {
    pub webhook_url: String,
    pub mention_user_id: Option<String>,  // For critical alerts
}

pub enum AlertLevel {
    Info,      // Trades, opportunities
    Warning,   // Rate limits, minor errors
    Critical,  // Circuit breaker, crashes
}

pub enum AlertType {
    TradeExecuted { position: Position },
    OpportunityFound { market: String, ev: f64 },
    PositionClosed { position: Position, pnl: f64 },
    CircuitBreakerTriggered { reason: String },
    Error { message: String },
    DailySummary { stats: DailyStats },
}
```

### Alert Formatting

```rust
fn format_trade_alert(position: &Position) -> String {
    format!(
        "**Trade Executed**\n\
        Market: {}\n\
        Side: {} @ {:.2}¢\n\
        Size: ${:.2}\n\
        Strategy: {}",
        position.market_title,
        position.side,
        position.entry_price * 100.0,
        position.quantity * position.entry_price,
        position.strategy
    )
}

fn format_daily_summary(stats: &DailyStats) -> String {
    format!(
        "**Daily Summary ({})**\n\
        Starting Capital: ${:.2}\n\
        Ending Capital: ${:.2}\n\
        Realized P&L: ${:+.2} ({:+.1}%)\n\
        Trades: {} ({} wins, {} losses)\n\
        Win Rate: {:.1}%\n\
        Max Drawdown: {:.2}%",
        stats.date,
        stats.starting_capital,
        stats.ending_capital,
        stats.realized_pnl,
        stats.realized_pnl / stats.starting_capital * 100.0,
        stats.trades_count,
        stats.win_count,
        stats.trades_count - stats.win_count,
        stats.win_count as f64 / stats.trades_count as f64 * 100.0,
        stats.max_drawdown * 100.0
    )
}
```

---

## Operational Controls

### Paper Trading Mode

```rust
pub struct TradingMode {
    pub live: bool,
    pub paper_capital: f64,  // Simulated capital for paper trading
}

// Paper trading simulates fills at mid-price with realistic slippage
fn simulate_fill(order: &Order, orderbook: &Orderbook) -> SimulatedFill {
    let base_price = (orderbook.best_bid + orderbook.best_ask) / 2.0;
    let slippage = rand::thread_rng().gen_range(0.0..0.005);  // 0-0.5% slippage
    let fill_price = match order.side {
        Side::Buy => base_price * (1.0 + slippage),
        Side::Sell => base_price * (1.0 - slippage),
    };

    SimulatedFill {
        price: fill_price,
        quantity: order.quantity,
        timestamp: Utc::now(),
    }
}
```

### Kill Switch

```rust
/// Emergency halt all trading and optionally close positions
async fn kill_switch(
    db: &Database,
    poly: &PolymarketClient,
    kalshi: &KalshiClient,
    close_positions: bool
) -> Result<()> {
    // 1. Set global halt flag
    db.set_system_state("trading_halted", "true").await?;

    // 2. Cancel all pending orders
    poly.cancel_all_orders().await?;
    kalshi.cancel_all_orders().await?;

    // 3. Optionally close all positions
    if close_positions {
        let positions = db.get_open_positions().await?;
        for pos in positions {
            match pos.platform {
                Platform::Polymarket => {
                    poly.market_sell(pos.market_id, pos.quantity).await?;
                }
                Platform::Kalshi => {
                    kalshi.market_sell(pos.market_id, pos.quantity).await?;
                }
            }
            db.close_position(pos.id, None).await?;
        }
    }

    // 4. Send alert
    send_discord_alert(AlertType::CircuitBreakerTriggered {
        reason: "Manual kill switch activated".into()
    }).await?;

    Ok(())
}
```

### Circuit Breaker (Automatic)

```rust
pub struct CircuitBreaker {
    pub max_drawdown_pct: f64,      // 0.05 (5%)
    pub max_consecutive_losses: u32, // 5
    pub cooldown_secs: u64,          // 3600 (1 hour)
}

fn check_circuit_breaker(stats: &TradingStats, config: &CircuitBreaker) -> Option<String> {
    if stats.current_drawdown > config.max_drawdown_pct {
        return Some(format!(
            "Max drawdown exceeded: {:.2}% > {:.2}%",
            stats.current_drawdown * 100.0,
            config.max_drawdown_pct * 100.0
        ));
    }

    if stats.consecutive_losses >= config.max_consecutive_losses {
        return Some(format!(
            "Max consecutive losses: {} >= {}",
            stats.consecutive_losses,
            config.max_consecutive_losses
        ));
    }

    None
}
```

---

## Development Phases

### Phase 1: Core Engine (Week 1-2)
- [ ] Evaluate existing codebase, identify reusable components
- [ ] Implement Kalman filter probability estimation
- [ ] Build SQLite persistence layer
- [ ] Implement paper trading mode
- [ ] Add basic logging and error handling

### Phase 2: Trading Strategies (Week 3-4)
- [ ] Implement high-probability grinding strategy
- [ ] Port existing cross-platform arbitrage
- [ ] Implement multi-outcome bundle arbitrage
- [ ] Build Kelly Criterion position sizing
- [ ] Add correlation tracking

### Phase 3: Web Dashboard (Week 5-6)
- [ ] Set up Axum server with HTMX
- [ ] Build core trading panel
- [ ] Build opportunity scanner
- [ ] Build risk gauges
- [ ] Add historical charts
- [ ] Implement SSE real-time updates

### Phase 4: Advanced Features (Week 7-8)
- [ ] Discord webhook integration
- [ ] Kill switch (CLI + Dashboard)
- [ ] Backtesting framework
- [ ] Auto-calibrating thesis invalidation
- [ ] Performance optimization

---

## Configuration

### Environment Variables

```bash
# === PLATFORM CREDENTIALS ===
KALSHI_API_KEY_ID=your-kalshi-api-key
KALSHI_PRIVATE_KEY_PATH=/path/to/kalshi_private_key.pem
POLY_PRIVATE_KEY=0xYOUR_WALLET_PRIVATE_KEY
POLY_FUNDER=0xYOUR_WALLET_ADDRESS

# === SYSTEM CONFIGURATION ===
DRY_RUN=1                          # 1 = paper trading, 0 = live
RUST_LOG=info                      # Logging level
DISCORD_WEBHOOK_URL=               # Optional Discord webhook
DATABASE_PATH=./data/trading.db    # SQLite database path
DASHBOARD_PORT=8080                # Web dashboard port

# === RISK PARAMETERS ===
MAX_DRAWDOWN_PCT=0.05             # 5% max drawdown
STARTING_CAPITAL=100.0            # Initial capital in USD
```

### Config File (config.toml)

```toml
[risk]
max_drawdown_pct = 0.05
max_consecutive_losses = 5
circuit_breaker_cooldown_secs = 3600

[grinding]
base_probability_threshold = 0.90
max_hours_to_expiry = 24.0
base_min_ev_pct = 0.005
min_liquidity_usd = 10.0

[kalman]
initial_variance = 0.1
measurement_noise = 0.01
base_process_noise = 0.001
expiry_noise_multiplier = 10.0

[kelly]
kelly_multiplier = 0.5
max_position_pct = 1.0

[execution]
max_slippage_pct = 0.02
min_depth_usd = 100.0

[thesis_invalidation]
price_velocity_std_devs = 2.0
volume_spike_multiplier = 3.0
min_calibration_samples = 100
baseline_window_secs = 3600

[alerts]
discord_webhook_url = ""
alert_on_trades = true
alert_on_opportunities = true
alert_on_errors = true
daily_summary_utc_hour = 0
```

---

## Backtesting

### Data Collection

```rust
// Continuously collect price data for future backtesting
async fn collect_price_history(db: &Database, markets: &[Market]) {
    for market in markets {
        db.insert_price_history(PriceHistory {
            market_id: market.id.clone(),
            platform: market.platform,
            timestamp: Utc::now(),
            bid: market.orderbook.best_bid,
            ask: market.orderbook.best_ask,
            last_price: market.last_price,
            volume_24h: market.volume_24h,
        }).await?;
    }
}
```

### Backtest Runner

```rust
pub struct BacktestConfig {
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub initial_capital: f64,
    pub strategies: Vec<StrategyType>,
}

pub struct BacktestResult {
    pub final_capital: f64,
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub win_rate: f64,
    pub trades: Vec<BacktestTrade>,
}

async fn run_backtest(config: BacktestConfig, db: &Database) -> Result<BacktestResult> {
    let price_data = db.get_price_history(config.start_date, config.end_date).await?;

    let mut simulator = TradingSimulator::new(config.initial_capital);

    for timestamp in price_data.timestamps() {
        let markets = price_data.at(timestamp);

        // Run strategy logic
        for strategy in &config.strategies {
            let signals = strategy.generate_signals(&markets, &simulator.state);
            for signal in signals {
                simulator.execute(signal, &markets)?;
            }
        }

        // Update positions with new prices
        simulator.mark_to_market(&markets);
    }

    Ok(simulator.results())
}
```

---

## Appendix: Market Categories

All categories are enabled by default. No legal restrictions applied (non-US user).

| Category | Examples | Notes |
|----------|----------|-------|
| Sports | NFL, NBA, Soccer, Tennis | Frequent events, clear resolution |
| Politics | Elections, policy decisions | High volume, longer duration |
| Crypto | BTC price, ETF approvals | Volatile, 24/7 |
| Entertainment | Oscars, TV shows | Niche, less liquid |
| Economics | Fed rates, GDP | Macro events |
| Weather | Temperature records | Niche |
| Science | SpaceX launches | Infrequent |

---

*Generated: 2024-12-30*
*Version: 2.0*
