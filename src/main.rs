//! Prediction Market Arbitrage Trading System
//!
//! A high-performance, production-ready arbitrage trading system for cross-platform
//! prediction markets. This system monitors price discrepancies between Kalshi and
//! Polymarket, executing risk-free arbitrage opportunities in real-time.
//!
//! ## Strategy
//!
//! The core arbitrage strategy exploits the fundamental property of prediction markets:
//! YES + NO = $1.00 (guaranteed). Arbitrage opportunities exist when:
//!
//! ```
//! Best YES ask (Platform A) + Best NO ask (Platform B) < $1.00
//! ```
//!
//! ## Architecture
//!
//! - **Real-time price monitoring** via WebSocket connections to both platforms
//! - **Lock-free orderbook cache** using atomic operations for zero-copy updates
//! - **SIMD-accelerated arbitrage detection** for sub-millisecond latency
//! - **Concurrent order execution** with automatic position reconciliation
//! - **Circuit breaker protection** with configurable risk limits
//! - **Market discovery system** with intelligent caching and incremental updates
//! - **Kalman filter probability estimation** for grinding strategy
//! - **Paper trading mode** for risk-free strategy testing
//! - **Discord notifications** for trade alerts and system status
//! - **SQLite persistence** for state recovery and position tracking

mod cache;
mod circuit_breaker;
mod config;
mod dashboard;
mod discord;
mod discovery;
mod execution;
mod grinding;
mod kalman;
mod kalshi;
mod paper_trading;
mod persistence;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod types;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use cache::TeamCache;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{ARB_THRESHOLD, ENABLED_LEAGUES, WS_RECONNECT_DELAY_SECS};
use discord::{AlertType, DiscordNotifier};
use discovery::DiscoveryClient;
use execution::{create_execution_channel, run_execution_loop, ExecutionEngine};
use grinding::{
    run_grinding_strategy, GrindingConfig, GrindingExecutionRequest, GrindingRunnerConfig,
    GrindingStrategy,
};
use kalman::{KalmanConfig, MarketFilterMap};
use kalshi::{KalshiApiClient, KalshiConfig};
use paper_trading::{HybridExecutor, TradingMode};
use persistence::Database;
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use position_tracker::{create_position_channel, position_writer_loop, PositionTracker};
use types::{GlobalState, PriceCents};
use dashboard::{DashboardServer, DashboardState};

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

/// Default database path
const DEFAULT_DB_PATH: &str = "./data/trading.db";

/// Default dashboard port
const DEFAULT_DASHBOARD_PORT: u16 = 8080;


#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("arb_bot=info".parse().unwrap()),
        )
        .init();

    info!("Prediction Market Arbitrage System v2.0");
    info!(
        "   Profit threshold: <{:.1}c ({:.1}% minimum profit)",
        ARB_THRESHOLD * 100.0,
        (1.0 - ARB_THRESHOLD) * 100.0
    );
    info!("   Monitored leagues: {:?}", ENABLED_LEAGUES);

    // =========================================================================
    // 1. INITIALIZE TRADING MODE (Paper vs Live)
    // =========================================================================
    let trading_mode = TradingMode::from_env();
    if trading_mode.is_paper() {
        info!(
            "   Mode: PAPER TRADING (starting capital: ${:.2})",
            trading_mode.paper_capital
        );
    } else {
        warn!("   Mode: LIVE EXECUTION");
    }

    // =========================================================================
    // 2. INITIALIZE DATABASE
    // =========================================================================
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());
    info!("[DB] Initializing database at: {}", db_path);

    let db = Database::open(&db_path).context("Failed to open database")?;
    db.initialize().context("Failed to initialize database schema")?;

    let db_stats = db.get_stats()?;
    info!(
        "[DB] Database initialized: {} markets, {} positions ({} open), {} orders",
        db_stats.market_count,
        db_stats.position_count,
        db_stats.open_position_count,
        db_stats.order_count
    );

    let db = Arc::new(db);

    // =========================================================================
    // 3. INITIALIZE DISCORD NOTIFIER
    // =========================================================================
    let discord = Arc::new(DiscordNotifier::from_env());
    if discord.is_active() {
        info!("[DISCORD] Notifications enabled");
    } else {
        info!("[DISCORD] Notifications disabled (no webhook URL configured)");
    }

    // =========================================================================
    // 4. LOAD EXCHANGE CREDENTIALS
    // =========================================================================
    let kalshi_config = KalshiConfig::from_env()?;
    info!("[KALSHI] API key loaded");

    dotenvy::dotenv().ok();
    let poly_private_key =
        std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder =
        std::env::var("POLY_FUNDER").context("POLY_FUNDER not set (your wallet address)")?;

    // Create async Polymarket client and derive API credentials
    info!("[POLYMARKET] Creating async client and deriving API credentials...");
    let poly_async_client = PolymarketAsyncClient::new(
        POLY_CLOB_HOST,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )?;
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;
    let poly_async = Arc::new(SharedAsyncClient::new(
        poly_async_client,
        prepared_creds,
        POLYGON_CHAIN_ID,
    ));

    // Load neg_risk cache from Python script output
    match poly_async.load_cache(".clob_market_cache.json") {
        Ok(count) => info!("[POLYMARKET] Loaded {} neg_risk entries from cache", count),
        Err(e) => warn!("[POLYMARKET] Could not load neg_risk cache: {}", e),
    }

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // Create Kalshi API client
    let kalshi_api = Arc::new(KalshiApiClient::new(kalshi_config));

    // =========================================================================
    // 5. INITIALIZE HYBRID EXECUTOR (Paper or Live)
    // =========================================================================
    let hybrid_executor = Arc::new(HybridExecutor::new(
        &trading_mode,
        kalshi_api.clone(),
        poly_async.clone(),
    ));

    let paper_tracker = hybrid_executor.paper_tracker();
    if let Some(ref tracker) = paper_tracker {
        let guard = tracker.read().await;
        info!(
            "[PAPER] Paper trading initialized: ${:.2} capital, {} positions",
            guard.get_capital(),
            guard.positions.len()
        );
    }

    // =========================================================================
    // 6. MARKET DISCOVERY
    // =========================================================================
    let team_cache = TeamCache::load();
    info!("Loaded {} team code mappings", team_cache.len());

    let force_discovery = std::env::var("FORCE_DISCOVERY")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    info!(
        "Market discovery{}...",
        if force_discovery {
            " (forced refresh)"
        } else {
            ""
        }
    );

    let discovery = DiscoveryClient::new(
        KalshiApiClient::new(KalshiConfig::from_env()?),
        team_cache,
    );

    let result = if force_discovery {
        discovery.discover_all_force(ENABLED_LEAGUES).await
    } else {
        discovery.discover_all(ENABLED_LEAGUES).await
    };

    info!("Market discovery complete:");
    info!("   - Matched market pairs: {}", result.pairs.len());

    if !result.errors.is_empty() {
        for err in &result.errors {
            warn!("   Warning: {}", err);
        }
    }

    if result.pairs.is_empty() {
        error!("No market pairs found!");
        return Ok(());
    }

    // Display discovered market pairs
    info!("Discovered market pairs:");
    for pair in &result.pairs {
        info!(
            "   {} | {} | Kalshi: {}",
            pair.description, pair.market_type, pair.kalshi_market_ticker
        );
    }

    // =========================================================================
    // 7. BUILD GLOBAL STATE
    // =========================================================================
    let state = Arc::new({
        let mut s = GlobalState::new();
        for pair in result.pairs {
            s.add_pair(pair);
        }
        info!(
            "Global state initialized: tracking {} markets",
            s.market_count()
        );
        s
    });

    // =========================================================================
    // 8. INITIALIZE KALMAN FILTERS
    // =========================================================================
    let kalman_config = KalmanConfig::default();
    let kalman_filters = Arc::new(MarketFilterMap::new(kalman_config));
    info!("[KALMAN] Probability estimation filters initialized");

    // =========================================================================
    // 9. INITIALIZE MARKET EXPIRIES (for grinding strategy)
    // =========================================================================
    let market_expiries: Arc<std::sync::RwLock<HashMap<u16, DateTime<Utc>>>> =
        Arc::new(std::sync::RwLock::new(HashMap::new()));

    // TODO: Populate expiries from market discovery data when available
    // For now, markets without expiry info will use max_hours_to_expiry default

    // =========================================================================
    // 10. INITIALIZE CAPITAL TRACKING
    // =========================================================================
    let initial_capital_cents = if trading_mode.is_paper() {
        (trading_mode.paper_capital * 100.0) as u64
    } else {
        // For live mode, could fetch from exchange APIs
        (db.get_current_capital().unwrap_or(100.0) * 100.0) as u64
    };
    let capital_cents = Arc::new(AtomicU64::new(initial_capital_cents));

    // =========================================================================
    // 11. INITIALIZE EXECUTION INFRASTRUCTURE
    // =========================================================================
    let (exec_tx, exec_rx) = create_execution_channel();
    let circuit_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::from_env()));

    let position_tracker = Arc::new(RwLock::new(PositionTracker::new()));
    let (position_channel, position_rx) = create_position_channel();

    tokio::spawn(position_writer_loop(position_rx, position_tracker.clone()));

    let threshold_cents: PriceCents = ((ARB_THRESHOLD * 100.0).round() as u16).max(1);
    info!("   Execution threshold: {} cents", threshold_cents);

    // Use dry_run based on trading mode
    let dry_run = trading_mode.is_paper();

    let engine = Arc::new(ExecutionEngine::new(
        kalshi_api.clone(),
        poly_async.clone(),
        state.clone(),
        circuit_breaker.clone(),
        position_channel,
        dry_run,
    ));

    let exec_handle = tokio::spawn(run_execution_loop(exec_rx, engine));

    // =========================================================================
    // 12. INITIALIZE GRINDING STRATEGY
    // =========================================================================
    let grinding_config = GrindingConfig::from_env();
    let grinding_strategy = Arc::new(GrindingStrategy::new(grinding_config, Some(kalman_config)));

    // Create grinding execution channel
    let (grinding_tx, mut grinding_rx) = tokio::sync::mpsc::channel::<GrindingExecutionRequest>(100);

    // Spawn grinding strategy runner
    let grinding_state = state.clone();
    let grinding_strategy_clone = grinding_strategy.clone();
    let grinding_capital = capital_cents.clone();
    let grinding_expiries = market_expiries.clone();
    let grinding_runner_config = GrindingRunnerConfig::default();

    let grinding_handle = tokio::spawn(async move {
        run_grinding_strategy(
            grinding_strategy_clone,
            grinding_state,
            grinding_tx,
            grinding_runner_config,
            grinding_capital,
            grinding_expiries,
        )
        .await;
    });

    // Spawn grinding execution handler
    let grinding_exec_hybrid = hybrid_executor.clone();
    let grinding_exec_state = state.clone();
    let grinding_discord = discord.clone();
    let grinding_exec_handle = tokio::spawn(async move {
        while let Some(request) = grinding_rx.recv().await {
            // Get market info for execution
            let market = grinding_exec_state.get_by_id(request.market_id);
            let pair = market.and_then(|m| m.pair.as_ref());

            if let Some(pair) = pair {
                let result = match request.platform {
                    grinding::Platform::Kalshi => {
                        let side = match request.side {
                            grinding::Side::Yes => "yes",
                            grinding::Side::No => "no",
                        };
                        grinding_exec_hybrid
                            .kalshi_buy(
                                &pair.kalshi_market_ticker,
                                side,
                                (request.max_price * 100.0) as i64,
                                request.contracts as i64,
                            )
                            .await
                    }
                    grinding::Platform::Polymarket => {
                        let token = match request.side {
                            grinding::Side::Yes => &pair.poly_yes_token,
                            grinding::Side::No => &pair.poly_no_token,
                        };
                        grinding_exec_hybrid
                            .poly_buy(token, request.max_price, request.contracts)
                            .await
                    }
                };

                match result {
                    Ok(fill) => {
                        info!(
                            "[GRINDING] Executed: {} {} {} @ {}c x{} (cost: {}c)",
                            fill.platform,
                            request.side,
                            pair.description,
                            fill.fill_price_cents,
                            fill.filled_contracts,
                            fill.fill_cost_cents
                        );

                        // Send Discord notification
                        let _ = grinding_discord
                            .send_alert(AlertType::TradeExecuted {
                                market_title: pair.description.to_string(),
                                side: format!("{}", request.side),
                                price_cents: fill.fill_price_cents,
                                size_usd: fill.fill_cost_cents as f64 / 100.0,
                                strategy: "grinding".to_string(),
                                platform: fill.platform.clone(),
                            })
                            .await;
                    }
                    Err(e) => {
                        error!(
                            "[GRINDING] Execution failed for {}: {}",
                            pair.description, e
                        );
                    }
                }
            }
        }
    });

    info!("[GRINDING] Strategy runner spawned");

    // =========================================================================
    // 13. RECOVERY ON STARTUP
    // =========================================================================
    info!("[RECOVERY] Checking for state recovery...");

    // Get current positions from exchanges (simplified - in production would query APIs)
    let poly_positions: HashMap<String, f64> = HashMap::new();
    let kalshi_positions: HashMap<String, f64> = HashMap::new();

    let recovery_report = db.recover_state(&poly_positions, &kalshi_positions)?;

    if !recovery_report.verified_positions.is_empty() {
        info!(
            "[RECOVERY] Verified {} positions",
            recovery_report.verified_positions.len()
        );
    }
    if !recovery_report.quantity_mismatches.is_empty() {
        warn!(
            "[RECOVERY] Fixed {} quantity mismatches",
            recovery_report.quantity_mismatches.len()
        );
    }
    if !recovery_report.closed_positions.is_empty() {
        warn!(
            "[RECOVERY] Closed {} orphaned DB positions",
            recovery_report.closed_positions.len()
        );
    }
    if !recovery_report.orphaned_positions.is_empty() {
        warn!(
            "[RECOVERY] Found {} orphaned exchange positions (manual review needed)",
            recovery_report.orphaned_positions.len()
        );
    }

    // =========================================================================
    // 14. SEND STARTUP NOTIFICATION
    // =========================================================================
    let open_positions = recovery_report.verified_positions.len();
    let startup_capital = if trading_mode.is_paper() {
        trading_mode.paper_capital
    } else {
        db.get_current_capital().unwrap_or(100.0)
    };

    let _ = discord
        .send_alert(AlertType::SystemStartup {
            mode: if trading_mode.is_paper() {
                "paper".to_string()
            } else {
                "live".to_string()
            },
            capital: startup_capital,
            open_positions,
        })
        .await;

    // =========================================================================
    // 15. TEST MODE: Synthetic arbitrage injection
    // =========================================================================
    let test_arb = std::env::var("TEST_ARB")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if test_arb {
        let test_state = state.clone();
        let test_exec_tx = exec_tx.clone();
        let test_dry_run = dry_run;

        let arb_type_str =
            std::env::var("TEST_ARB_TYPE").unwrap_or_else(|_| "poly_yes_kalshi_no".to_string());

        tokio::spawn(async move {
            use types::{ArbType, FastExecutionRequest};

            info!("[TEST] Injecting synthetic arbitrage opportunity in 10 seconds...");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

            let arb_type = match arb_type_str.to_lowercase().as_str() {
                "poly_yes_kalshi_no" | "pykn" | "0" => ArbType::PolyYesKalshiNo,
                "kalshi_yes_poly_no" | "kypn" | "1" => ArbType::KalshiYesPolyNo,
                "poly_only" | "poly" | "2" => ArbType::PolyOnly,
                "kalshi_only" | "kalshi" | "3" => ArbType::KalshiOnly,
                _ => {
                    warn!(
                        "[TEST] Unknown TEST_ARB_TYPE='{}', defaulting to PolyYesKalshiNo",
                        arb_type_str
                    );
                    ArbType::PolyYesKalshiNo
                }
            };

            let (yes_price, no_price, description) = match arb_type {
                ArbType::PolyYesKalshiNo => {
                    (40, 50, "P_yes=40c + K_no=50c + fee~2c = 92c -> 8c profit")
                }
                ArbType::KalshiYesPolyNo => {
                    (40, 50, "K_yes=40c + P_no=50c + fee~2c = 92c -> 8c profit")
                }
                ArbType::PolyOnly => {
                    (48, 50, "P_yes=48c + P_no=50c + fee=0c = 98c -> 2c profit")
                }
                ArbType::KalshiOnly => {
                    (44, 44, "K_yes=44c + K_no=44c + fee~4c = 92c -> 8c profit")
                }
            };

            let market_count = test_state.market_count();
            for market_id in 0..market_count {
                if let Some(market) = test_state.get_by_id(market_id as u16) {
                    if let Some(pair) = &market.pair {
                        let fake_req = FastExecutionRequest {
                            market_id: market_id as u16,
                            yes_price,
                            no_price,
                            yes_size: 1000,
                            no_size: 1000,
                            arb_type,
                            detected_ns: 0,
                        };

                        warn!(
                            "[TEST] Injecting synthetic {:?} arbitrage for: {}",
                            arb_type, pair.description
                        );
                        warn!("[TEST]    Scenario: {}", description);
                        warn!("[TEST]    Execution mode: DRY_RUN={}", test_dry_run);

                        if let Err(e) = test_exec_tx.send(fake_req).await {
                            error!("[TEST] Failed to send fake arb: {}", e);
                        }
                        break;
                    }
                }
            }
        });
    }

    // =========================================================================
    // 16. INITIALIZE WEBSOCKET CONNECTIONS
    // =========================================================================
    let kalshi_state = state.clone();
    let kalshi_exec_tx = exec_tx.clone();
    let kalshi_threshold = threshold_cents;
    let _kalshi_filters = kalman_filters.clone();
    // Create a fresh KalshiConfig for WebSocket (since it can't be cloned)
    let kalshi_handle = tokio::spawn(async move {
        loop {
            // Re-create config on each reconnect attempt
            match KalshiConfig::from_env() {
                Ok(kalshi_ws_config) => {
                    if let Err(e) = kalshi::run_ws(
                        &kalshi_ws_config,
                        kalshi_state.clone(),
                        kalshi_exec_tx.clone(),
                        kalshi_threshold,
                    )
                    .await
                    {
                        error!("[KALSHI] WebSocket disconnected: {} - reconnecting...", e);
                    }
                }
                Err(e) => {
                    error!("[KALSHI] Failed to load config: {} - retrying...", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    let poly_state = state.clone();
    let poly_exec_tx = exec_tx.clone();
    let poly_threshold = threshold_cents;
    let _poly_filters = kalman_filters.clone();
    let poly_handle = tokio::spawn(async move {
        loop {
            if let Err(e) =
                polymarket::run_ws(poly_state.clone(), poly_exec_tx.clone(), poly_threshold).await
            {
                error!(
                    "[POLYMARKET] WebSocket disconnected: {} - reconnecting...",
                    e
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // =========================================================================
    // 17. SPAWN DASHBOARD SERVER
    // =========================================================================
    let dashboard_port: u16 = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DASHBOARD_PORT);

    // Create dashboard state with all components
    let dashboard_state = DashboardState::with_components(
        db.clone(),
        position_tracker.clone(),
        Some(circuit_breaker.clone()),
        if trading_mode.is_paper() {
            trading_mode.paper_capital
        } else {
            100.0
        },
    );

    // Set initial trading mode in dashboard
    {
        let mode = if trading_mode.is_paper() {
            dashboard::state::TradingMode::Paper
        } else {
            dashboard::state::TradingMode::Live
        };
        dashboard_state.set_trading_mode(mode).await;
    }

    // Spawn dashboard server
    let dashboard_state_clone = dashboard_state.clone();
    let dashboard_handle = tokio::spawn(async move {
        let server = DashboardServer::new(dashboard_state_clone);
        if let Err(e) = server.run_on_port(dashboard_port).await {
            error!("[DASHBOARD] Server error: {}", e);
        }
    });

    info!("[DASHBOARD] Server starting on http://127.0.0.1:{}", dashboard_port);

    // =========================================================================
    // 18. SYSTEM HEARTBEAT AND ARBITRAGE DIAGNOSTICS
    // =========================================================================
    let heartbeat_state = state.clone();
    let heartbeat_threshold = threshold_cents;
    let heartbeat_db = db.clone();
    let heartbeat_kalman = kalman_filters.clone();
    let heartbeat_dashboard = dashboard_state.clone();
    let heartbeat_discord = discord.clone();
    let heartbeat_handle = tokio::spawn(async move {
        use crate::types::kalshi_fee_cents;
        use crate::dashboard::state::Opportunity;
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;

            // Record heartbeat in database
            let _ = heartbeat_db.record_heartbeat();

            let market_count = heartbeat_state.market_count();
            let mut with_kalshi = 0;
            let mut with_poly = 0;
            let mut with_both = 0;
            let mut best_arb: Option<(u16, u16, u16, u16, u16, u16, u16, bool, String)> = None;
            let mut opportunities: Vec<Opportunity> = Vec::new();

            for market in heartbeat_state.markets.iter().take(market_count) {
                let (k_yes, k_no, _, _) = market.kalshi.load();
                let (p_yes, p_no, _, _) = market.poly.load();
                let has_k = k_yes > 0 && k_no > 0;
                let has_p = p_yes > 0 && p_no > 0;
                if k_yes > 0 || k_no > 0 {
                    with_kalshi += 1;
                }
                if p_yes > 0 || p_no > 0 {
                    with_poly += 1;
                }
                if has_k && has_p {
                    with_both += 1;

                    let fee1 = kalshi_fee_cents(k_no);
                    let cost1 = p_yes + k_no + fee1;

                    let fee2 = kalshi_fee_cents(k_yes);
                    let cost2 = k_yes + fee2 + p_no;

                    let (best_cost, best_fee, is_poly_yes) = if cost1 <= cost2 {
                        (cost1, fee1, true)
                    } else {
                        (cost2, fee2, false)
                    };

                    let desc = heartbeat_state
                        .get_by_id(market.market_id)
                        .and_then(|m| m.pair.as_ref())
                        .map(|p| p.description.to_string())
                        .unwrap_or_else(|| "Unknown".to_string());

                    // Track best arbitrage opportunity
                    if best_arb.is_none() || best_cost < best_arb.as_ref().unwrap().0 {
                        best_arb = Some((
                            best_cost,
                            market.market_id,
                            p_yes,
                            k_no,
                            k_yes,
                            p_no,
                            best_fee,
                            is_poly_yes,
                            desc.clone(),
                        ));
                    }

                    // Add to opportunities list if gap is reasonable (<10c)
                    let gap = best_cost as i16 - heartbeat_threshold as i16;
                    if gap <= 10 {
                        let expected_value = if best_cost < 100 {
                            (100.0 - best_cost as f64) / best_cost as f64 * 100.0
                        } else {
                            -(best_cost as f64 - 100.0) / 100.0 * 100.0
                        };

                        opportunities.push(Opportunity {
                            id: format!("arb-{}", market.market_id),
                            market_title: desc,
                            strategy: "cross-platform-arb".to_string(),
                            expected_value,
                            kelly_size: 0.0, // Would calculate based on edge
                            probability: if is_poly_yes { p_yes as f64 / 100.0 } else { k_yes as f64 / 100.0 },
                            expiry_hours: 24.0, // Default
                            platform: if is_poly_yes { "Poly→Kalshi".to_string() } else { "Kalshi→Poly".to_string() },
                            timestamp: chrono::Utc::now().timestamp(),
                        });
                    }
                }

                // Update Kalman filter with mid-price
                if has_p {
                    let mid_price = (p_yes as f64 + (100 - p_no) as f64) / 2.0 / 100.0;
                    // Use default hours (would need expiry info for accurate filtering)
                    heartbeat_kalman.update(market.market_id, mid_price, 24.0);
                }
            }

            // Sort opportunities by expected value (best first)
            opportunities.sort_by(|a, b| b.expected_value.partial_cmp(&a.expected_value).unwrap_or(std::cmp::Ordering::Equal));
            opportunities.truncate(10); // Keep top 10

            // Update dashboard with opportunities
            {
                let mut opps = heartbeat_dashboard.opportunities.write().await;
                *opps = opportunities;
            }

            info!(
                "System heartbeat | Markets: {} total, {} with Kalshi prices, {} with Polymarket prices, {} with both | threshold={}c",
                market_count, with_kalshi, with_poly, with_both, heartbeat_threshold
            );

            // Report Kalman filter stats
            let filter_count = heartbeat_kalman.len();
            if filter_count > 0 {
                info!(
                    "   Kalman filters: {} markets tracked",
                    filter_count
                );
            }

            if let Some((cost, _market_id, p_yes, k_no, k_yes, p_no, fee, is_poly_yes, desc)) = best_arb {
                let gap = cost as i16 - heartbeat_threshold as i16;
                let leg_breakdown = if is_poly_yes {
                    format!(
                        "P_yes({}c) + K_no({}c) + K_fee({}c) = {}c",
                        p_yes, k_no, fee, cost
                    )
                } else {
                    format!(
                        "K_yes({}c) + P_no({}c) + K_fee({}c) = {}c",
                        k_yes, p_no, fee, cost
                    )
                };
                if gap <= 10 {
                    info!(
                        "   Best opportunity: {} | {} | gap={:+}c | [Poly_yes={}c Kalshi_no={}c Kalshi_yes={}c Poly_no={}c]",
                        desc, leg_breakdown, gap, p_yes, k_no, k_yes, p_no
                    );

                    // Send Discord alert for profitable opportunities (gap <= 0 means profit)
                    if gap <= 0 {
                        let _ = heartbeat_discord.send_alert(AlertType::OpportunityFound {
                            market_title: desc.clone(),
                            expected_value_pct: -gap as f64,
                            profit_cents: -gap,
                            arb_type: if is_poly_yes { "Poly_YES + Kalshi_NO".to_string() } else { "Kalshi_YES + Poly_NO".to_string() },
                        }).await;
                    }
                } else {
                    info!(
                        "   Best opportunity: {} | {} | gap={:+}c (market efficient)",
                        desc, leg_breakdown, gap
                    );
                }
            } else if with_both == 0 {
                warn!("   No markets with both Kalshi and Polymarket prices - verify WebSocket connections");
            }
        }
    });

    // =========================================================================
    // 19. GRACEFUL SHUTDOWN HANDLER
    // =========================================================================
    let shutdown_discord = discord.clone();
    let shutdown_db = db.clone();
    let shutdown_paper_tracker = paper_tracker.clone();
    let shutdown_trading_mode = trading_mode;

    // Set up Ctrl+C handler
    let shutdown_handle = tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("[SHUTDOWN] Received shutdown signal, cleaning up...");

                // Get final capital
                let final_capital = if shutdown_trading_mode.is_paper() {
                    if let Some(ref tracker) = shutdown_paper_tracker {
                        tracker.read().await.get_capital()
                    } else {
                        shutdown_trading_mode.paper_capital
                    }
                } else {
                    shutdown_db.get_current_capital().unwrap_or(0.0)
                };

                // Send shutdown notification
                let _ = shutdown_discord
                    .send_alert(AlertType::SystemShutdown {
                        reason: "User requested shutdown (Ctrl+C)".to_string(),
                        final_capital,
                    })
                    .await;

                // Save paper trading state if applicable
                if let Some(ref tracker) = shutdown_paper_tracker {
                    let guard = tracker.read().await;
                    if let Err(e) = guard.save("paper_trading_state.json") {
                        error!("[SHUTDOWN] Failed to save paper trading state: {}", e);
                    } else {
                        info!("[SHUTDOWN] Paper trading state saved");
                    }
                }

                // Record final heartbeat
                let _ = shutdown_db.record_heartbeat();

                info!("[SHUTDOWN] Cleanup complete");
            }
            Err(e) => {
                error!("[SHUTDOWN] Error setting up signal handler: {}", e);
            }
        }
    });

    // =========================================================================
    // 20. MAIN EVENT LOOP
    // =========================================================================
    info!("All systems operational - entering main event loop");

    tokio::select! {
        _ = kalshi_handle => {
            error!("Kalshi WebSocket handler exited unexpectedly");
        }
        _ = poly_handle => {
            error!("Polymarket WebSocket handler exited unexpectedly");
        }
        _ = heartbeat_handle => {
            error!("Heartbeat handler exited unexpectedly");
        }
        _ = exec_handle => {
            error!("Execution handler exited unexpectedly");
        }
        _ = grinding_handle => {
            error!("Grinding strategy handler exited unexpectedly");
        }
        _ = grinding_exec_handle => {
            error!("Grinding execution handler exited unexpectedly");
        }
        _ = dashboard_handle => {
            error!("Dashboard server exited unexpectedly");
        }
        _ = shutdown_handle => {
            info!("Shutdown complete");
        }
    }

    Ok(())
}
