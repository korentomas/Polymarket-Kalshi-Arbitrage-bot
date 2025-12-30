//! HTTP route handlers for the dashboard.
//!
//! This module contains all the handler functions for dashboard routes,
//! including HTML pages, API endpoints, and control routes.

use crate::dashboard::sse::create_sse_stream;
use crate::dashboard::state::{
    ConfigUpdate, DashboardEvent, DashboardState, DashboardStats, DisplayPosition, Opportunity,
    PnlDataPoint, SystemConfig, TradingMode,
};
use axum::{
    extract::State,
    response::{Html, IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// HTML PAGE HANDLERS (HTMX)
// ============================================================================

/// Main dashboard page
pub async fn index_page() -> impl IntoResponse {
    Html(include_str!("../../static/index.html"))
}

/// Positions page partial (for HTMX)
pub async fn positions_page(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let positions = state.get_display_positions().await;
    Html(render_positions_table(&positions))
}

/// Opportunities page partial (for HTMX)
pub async fn opportunities_page(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let opportunities = state.opportunities.read().await;
    Html(render_opportunities_table(&opportunities))
}

/// History page partial (for HTMX)
pub async fn history_page(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let history = state.get_pnl_history().await;
    Html(render_history_table(&history))
}

/// Analytics page partial (for HTMX)
pub async fn analytics_page(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let stats = state.get_stats().await;
    let history = state.get_pnl_history().await;
    Html(render_analytics(&stats, &history))
}

/// Settings page partial (for HTMX)
pub async fn settings_page(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let config = state.config.read().await;
    let mode = *state.trading_mode.read().await;
    Html(render_settings(&config, mode))
}

// ============================================================================
// API HANDLERS (JSON)
// ============================================================================

/// Get current positions as JSON
pub async fn api_positions(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let positions = state.get_display_positions().await;
    Json(positions)
}

/// Get current opportunities as JSON
pub async fn api_opportunities(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let opportunities = state.opportunities.read().await;
    Json(opportunities.clone())
}

/// Get dashboard stats as JSON
pub async fn api_stats(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let stats = state.get_stats().await;
    Json(stats)
}

/// Get P&L history as JSON
pub async fn api_history(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let history = state.get_pnl_history().await;
    Json(history)
}

/// Get system config as JSON
pub async fn api_config(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    let config = state.config.read().await;
    Json(config.clone())
}

/// SSE events endpoint
pub async fn api_events(State(state): State<Arc<DashboardState>>) -> impl IntoResponse {
    create_sse_stream(state)
}

// ============================================================================
// CONTROL HANDLERS
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct KillSwitchRequest {
    pub action: String, // "activate" or "deactivate"
}

#[derive(Debug, Serialize)]
pub struct KillSwitchResponse {
    pub success: bool,
    pub active: bool,
    pub message: String,
}

/// Kill switch control endpoint
pub async fn api_kill_switch(
    State(state): State<Arc<DashboardState>>,
    Json(request): Json<KillSwitchRequest>,
) -> impl IntoResponse {
    let (success, active, message) = match request.action.as_str() {
        "activate" => {
            state.activate_kill_switch().await;
            (true, true, "Kill switch activated - all trading halted".to_string())
        }
        "deactivate" => {
            state.deactivate_kill_switch().await;
            (true, false, "Kill switch deactivated - trading resumed".to_string())
        }
        _ => (false, state.kill_switch_active.load(std::sync::atomic::Ordering::Relaxed),
              "Invalid action. Use 'activate' or 'deactivate'".to_string()),
    };

    Json(KillSwitchResponse {
        success,
        active,
        message,
    })
}

#[derive(Debug, Deserialize)]
pub struct ModeRequest {
    pub mode: String, // "paper" or "live"
}

#[derive(Debug, Serialize)]
pub struct ModeResponse {
    pub success: bool,
    pub mode: String,
    pub message: String,
}

/// Trading mode control endpoint
pub async fn api_mode(
    State(state): State<Arc<DashboardState>>,
    Json(request): Json<ModeRequest>,
) -> impl IntoResponse {
    let (success, mode, message) = match request.mode.to_lowercase().as_str() {
        "paper" => {
            state.set_trading_mode(TradingMode::Paper).await;
            (true, "paper".to_string(), "Switched to paper trading mode".to_string())
        }
        "live" => {
            state.set_trading_mode(TradingMode::Live).await;
            (true, "live".to_string(), "Switched to live trading mode - BE CAREFUL!".to_string())
        }
        _ => {
            let current = *state.trading_mode.read().await;
            (false, current.to_string(), "Invalid mode. Use 'paper' or 'live'".to_string())
        }
    };

    Json(ModeResponse {
        success,
        mode,
        message,
    })
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub success: bool,
    pub config: SystemConfig,
    pub message: String,
}

/// Configuration update endpoint
pub async fn api_config_update(
    State(state): State<Arc<DashboardState>>,
    Json(update): Json<ConfigUpdate>,
) -> impl IntoResponse {
    let mut config = state.config.write().await;

    if let Some(v) = update.max_drawdown_pct {
        config.max_drawdown_pct = v.clamp(0.01, 0.50);
    }
    if let Some(v) = update.base_probability_threshold {
        config.base_probability_threshold = v.clamp(0.50, 0.99);
    }
    if let Some(v) = update.max_hours_to_expiry {
        config.max_hours_to_expiry = v.clamp(1.0, 168.0);
    }
    if let Some(v) = update.kelly_multiplier {
        config.kelly_multiplier = v.clamp(0.1, 1.0);
    }
    if let Some(v) = update.min_liquidity_usd {
        config.min_liquidity_usd = v.clamp(1.0, 10000.0);
    }

    let updated_config = config.clone();
    drop(config);

    state.broadcast(DashboardEvent::Alert {
        level: "info".to_string(),
        message: "Configuration updated".to_string(),
    });

    Json(ConfigResponse {
        success: true,
        config: updated_config,
        message: "Configuration updated successfully".to_string(),
    })
}

// ============================================================================
// HTMX PARTIAL RENDERERS
// ============================================================================

fn render_positions_table(positions: &[DisplayPosition]) -> String {
    if positions.is_empty() {
        return r#"
        <div class="text-center py-8 text-gray-500">
            <svg class="mx-auto h-12 w-12 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M20 13V6a2 2 0 00-2-2H6a2 2 0 00-2 2v7m16 0v5a2 2 0 01-2 2H6a2 2 0 01-2-2v-5m16 0h-2.586a1 1 0 00-.707.293l-2.414 2.414a1 1 0 01-.707.293h-3.172a1 1 0 01-.707-.293l-2.414-2.414A1 1 0 006.586 13H4" />
            </svg>
            <p class="mt-2">No open positions</p>
        </div>
        "#.to_string();
    }

    let mut html = String::from(r#"
    <table class="min-w-full divide-y divide-gray-200">
        <thead class="bg-gray-50">
            <tr>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Market</th>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Platform</th>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Side</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Entry</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Current</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Qty</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">P&L</th>
            </tr>
        </thead>
        <tbody class="bg-white divide-y divide-gray-200">
    "#);

    for pos in positions {
        let pnl_class = if pos.pnl >= 0.0 { "text-green-600" } else { "text-red-600" };
        let pnl_sign = if pos.pnl >= 0.0 { "+" } else { "" };

        html.push_str(&format!(r#"
            <tr class="hover:bg-gray-50">
                <td class="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 text-right">{:.1}c</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 text-right">{:.1}c</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 text-right">{:.0}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm {} text-right font-medium">{}${:.2} ({}{:.1}%)</td>
            </tr>
        "#,
            html_escape(&pos.market),
            pos.platform,
            pos.side,
            pos.entry_price * 100.0,
            pos.current_price * 100.0,
            pos.quantity,
            pnl_class,
            pnl_sign,
            pos.pnl.abs(),
            pnl_sign,
            pos.pnl_percent.abs()
        ));
    }

    html.push_str("</tbody></table>");
    html
}

fn render_opportunities_table(opportunities: &[Opportunity]) -> String {
    if opportunities.is_empty() {
        return r#"
        <div class="text-center py-8 text-gray-500">
            <svg class="mx-auto h-12 w-12 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
            </svg>
            <p class="mt-2">Scanning for opportunities...</p>
        </div>
        "#.to_string();
    }

    let mut html = String::from(r#"
    <table class="min-w-full divide-y divide-gray-200">
        <thead class="bg-gray-50">
            <tr>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Market</th>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Strategy</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">EV</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Probability</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Kelly Size</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Expiry</th>
            </tr>
        </thead>
        <tbody class="bg-white divide-y divide-gray-200">
    "#);

    for opp in opportunities {
        let ev_class = if opp.expected_value >= 0.02 {
            "text-green-600 font-bold"
        } else if opp.expected_value >= 0.01 {
            "text-green-600"
        } else {
            "text-yellow-600"
        };

        html.push_str(&format!(r#"
            <tr class="hover:bg-gray-50">
                <td class="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                    <span class="px-2 py-1 text-xs font-medium rounded-full bg-blue-100 text-blue-800">{}</span>
                </td>
                <td class="px-6 py-4 whitespace-nowrap text-sm {} text-right">{:.2}%</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 text-right">{:.1}%</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 text-right">${:.2}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-500 text-right">{:.1}h</td>
            </tr>
        "#,
            html_escape(&opp.market_title),
            opp.strategy,
            ev_class,
            opp.expected_value * 100.0,
            opp.probability * 100.0,
            opp.kelly_size,
            opp.expiry_hours
        ));
    }

    html.push_str("</tbody></table>");
    html
}

fn render_history_table(history: &[PnlDataPoint]) -> String {
    if history.is_empty() {
        return r#"
        <div class="text-center py-8 text-gray-500">
            <svg class="mx-auto h-12 w-12 text-gray-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z" />
            </svg>
            <p class="mt-2">No trading history yet</p>
        </div>
        "#.to_string();
    }

    let mut html = String::from(r#"
    <table class="min-w-full divide-y divide-gray-200">
        <thead class="bg-gray-50">
            <tr>
                <th class="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Date</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Daily P&L</th>
                <th class="px-6 py-3 text-right text-xs font-medium text-gray-500 uppercase tracking-wider">Cumulative P&L</th>
            </tr>
        </thead>
        <tbody class="bg-white divide-y divide-gray-200">
    "#);

    for point in history.iter().rev().take(30) {
        let daily_class = if point.daily_pnl >= 0.0 { "text-green-600" } else { "text-red-600" };
        let cum_class = if point.cumulative_pnl >= 0.0 { "text-green-600" } else { "text-red-600" };
        let daily_sign = if point.daily_pnl >= 0.0 { "+" } else { "" };
        let cum_sign = if point.cumulative_pnl >= 0.0 { "+" } else { "" };

        html.push_str(&format!(r#"
            <tr class="hover:bg-gray-50">
                <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900">{}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm {} text-right">{}${:.2}</td>
                <td class="px-6 py-4 whitespace-nowrap text-sm {} text-right font-medium">{}${:.2}</td>
            </tr>
        "#,
            point.date,
            daily_class,
            daily_sign,
            point.daily_pnl.abs(),
            cum_class,
            cum_sign,
            point.cumulative_pnl.abs()
        ));
    }

    html.push_str("</tbody></table>");
    html
}

fn render_analytics(stats: &DashboardStats, history: &[PnlDataPoint]) -> String {
    let labels: Vec<String> = history.iter().map(|p| format!("\"{}\"", p.date)).collect();
    let pnl_data: Vec<String> = history.iter().map(|p| format!("{:.2}", p.cumulative_pnl)).collect();

    format!(r#"
    <div class="space-y-6">
        <!-- Summary Cards -->
        <div class="grid grid-cols-1 md:grid-cols-4 gap-4">
            <div class="bg-white rounded-lg shadow p-4">
                <div class="text-sm font-medium text-gray-500">Total Trades</div>
                <div class="text-2xl font-bold text-gray-900">{}</div>
            </div>
            <div class="bg-white rounded-lg shadow p-4">
                <div class="text-sm font-medium text-gray-500">Win Rate</div>
                <div class="text-2xl font-bold text-gray-900">{:.1}%</div>
            </div>
            <div class="bg-white rounded-lg shadow p-4">
                <div class="text-sm font-medium text-gray-500">Max Drawdown</div>
                <div class="text-2xl font-bold text-red-600">{:.2}%</div>
            </div>
            <div class="bg-white rounded-lg shadow p-4">
                <div class="text-sm font-medium text-gray-500">High Water Mark</div>
                <div class="text-2xl font-bold text-green-600">${:.2}</div>
            </div>
        </div>

        <!-- P&L Chart -->
        <div class="bg-white rounded-lg shadow p-6">
            <h3 class="text-lg font-medium text-gray-900 mb-4">Cumulative P&L</h3>
            <canvas id="pnlChart" height="300"></canvas>
        </div>
    </div>

    <script>
        const ctx = document.getElementById('pnlChart').getContext('2d');
        new Chart(ctx, {{
            type: 'line',
            data: {{
                labels: [{}],
                datasets: [{{
                    label: 'Cumulative P&L ($)',
                    data: [{}],
                    borderColor: 'rgb(34, 197, 94)',
                    backgroundColor: 'rgba(34, 197, 94, 0.1)',
                    fill: true,
                    tension: 0.4
                }}]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    legend: {{
                        display: false
                    }}
                }},
                scales: {{
                    y: {{
                        beginAtZero: true,
                        ticks: {{
                            callback: function(value) {{
                                return '$' + value.toFixed(2);
                            }}
                        }}
                    }}
                }}
            }}
        }});
    </script>
    "#,
        stats.total_trades,
        stats.win_rate,
        stats.drawdown_percent,
        stats.high_water_mark,
        labels.join(", "),
        pnl_data.join(", ")
    )
}

fn render_settings(config: &SystemConfig, mode: TradingMode) -> String {
    let paper_selected = if mode == TradingMode::Paper { "selected" } else { "" };
    let live_selected = if mode == TradingMode::Live { "selected" } else { "" };

    format!(r#"
    <form hx-post="/api/config" hx-swap="none" class="space-y-6">
        <!-- Trading Mode -->
        <div class="bg-white rounded-lg shadow p-6">
            <h3 class="text-lg font-medium text-gray-900 mb-4">Trading Mode</h3>
            <div class="flex items-center space-x-4">
                <select
                    id="trading-mode"
                    name="mode"
                    class="block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500"
                    hx-post="/api/mode"
                    hx-swap="none"
                    hx-vals='js:{{mode: document.getElementById("trading-mode").value}}'
                >
                    <option value="paper" {}>Paper Trading</option>
                    <option value="live" {}>Live Trading</option>
                </select>
            </div>
            <p class="mt-2 text-sm text-gray-500">
                Paper trading simulates orders without real execution. Use Live mode with caution.
            </p>
        </div>

        <!-- Risk Parameters -->
        <div class="bg-white rounded-lg shadow p-6">
            <h3 class="text-lg font-medium text-gray-900 mb-4">Risk Parameters</h3>
            <div class="grid grid-cols-1 md:grid-cols-2 gap-6">
                <div>
                    <label class="block text-sm font-medium text-gray-700">Max Drawdown %</label>
                    <input type="number" name="max_drawdown_pct" value="{:.0}" min="1" max="50" step="1"
                           class="mt-1 block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500" />
                    <p class="mt-1 text-xs text-gray-500">Circuit breaker triggers at this drawdown level</p>
                </div>
                <div>
                    <label class="block text-sm font-medium text-gray-700">Kelly Multiplier</label>
                    <input type="number" name="kelly_multiplier" value="{:.2}" min="0.1" max="1.0" step="0.1"
                           class="mt-1 block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500" />
                    <p class="mt-1 text-xs text-gray-500">Fraction of full Kelly criterion (0.5 = half Kelly)</p>
                </div>
            </div>
        </div>

        <!-- Strategy Parameters -->
        <div class="bg-white rounded-lg shadow p-6">
            <h3 class="text-lg font-medium text-gray-900 mb-4">Strategy Parameters</h3>
            <div class="grid grid-cols-1 md:grid-cols-2 gap-6">
                <div>
                    <label class="block text-sm font-medium text-gray-700">Min Probability Threshold %</label>
                    <input type="number" name="base_probability_threshold" value="{:.0}" min="50" max="99" step="1"
                           class="mt-1 block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500" />
                    <p class="mt-1 text-xs text-gray-500">Minimum probability for grinding strategy</p>
                </div>
                <div>
                    <label class="block text-sm font-medium text-gray-700">Max Hours to Expiry</label>
                    <input type="number" name="max_hours_to_expiry" value="{:.0}" min="1" max="168" step="1"
                           class="mt-1 block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500" />
                    <p class="mt-1 text-xs text-gray-500">Maximum time until market resolution</p>
                </div>
                <div>
                    <label class="block text-sm font-medium text-gray-700">Min Liquidity (USD)</label>
                    <input type="number" name="min_liquidity_usd" value="{:.0}" min="1" max="10000" step="1"
                           class="mt-1 block w-full rounded-md border-gray-300 shadow-sm focus:border-blue-500 focus:ring-blue-500" />
                    <p class="mt-1 text-xs text-gray-500">Minimum liquidity required for entry</p>
                </div>
            </div>
        </div>

        <!-- Save Button -->
        <div class="flex justify-end">
            <button type="submit" class="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700 focus:outline-none focus:ring-2 focus:ring-blue-500 focus:ring-offset-2">
                Save Configuration
            </button>
        </div>
    </form>
    "#,
        paper_selected,
        live_selected,
        config.max_drawdown_pct * 100.0,
        config.kelly_multiplier,
        config.base_probability_threshold * 100.0,
        config.max_hours_to_expiry,
        config.min_liquidity_usd
    )
}

/// Simple HTML escaping to prevent XSS
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// ============================================================================
// HEALTH CHECK
// ============================================================================

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
}

static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub async fn health_check() -> impl IntoResponse {
    let start = START_TIME.get_or_init(std::time::Instant::now);
    let uptime = start.elapsed().as_secs();

    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: uptime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape("\"test\""), "&quot;test&quot;");
    }

    #[test]
    fn test_render_empty_positions() {
        let result = render_positions_table(&[]);
        assert!(result.contains("No open positions"));
    }

    #[test]
    fn test_render_empty_opportunities() {
        let result = render_opportunities_table(&[]);
        assert!(result.contains("Scanning for opportunities"));
    }
}
