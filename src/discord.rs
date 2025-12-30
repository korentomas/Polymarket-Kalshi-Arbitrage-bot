#![allow(dead_code)]

//! Discord webhook integration for trading notifications and alerts.
//!
//! This module provides:
//! - Discord webhook notifications with rich embeds
//! - Alert level classification (Info, Warning, Critical)
//! - Alert type formatting (trades, opportunities, errors, etc.)
//! - Rate limiting to avoid Discord API limits
//! - Daily summary formatting with stats
//! - Optional configuration (gracefully disabled when no webhook URL)
//!
//! # Example
//! ```rust,ignore
//! use prediction_market_arbitrage::discord::{DiscordNotifier, DiscordConfig, AlertLevel, AlertType};
//!
//! let config = DiscordConfig {
//!     webhook_url: Some("https://discord.com/api/webhooks/...".to_string()),
//!     mention_user_id: Some("123456789".to_string()),
//!     enabled: true,
//! };
//!
//! let notifier = DiscordNotifier::new(config);
//!
//! // Send a trade alert
//! notifier.send_alert(
//!     AlertLevel::Info,
//!     AlertType::TradeExecuted {
//!         market_title: "Chelsea vs Arsenal".to_string(),
//!         side: "YES".to_string(),
//!         price_cents: 45,
//!         size_usd: 50.0,
//!         strategy: "grinding".to_string(),
//!         platform: "polymarket".to_string(),
//!     },
//! ).await?;
//! ```

use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, error, warn};

// ============================================================================
// CONSTANTS
// ============================================================================

/// Discord API rate limit: 30 requests per minute per webhook
const RATE_LIMIT_REQUESTS: u32 = 30;
const RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// Maximum retries for failed webhook calls
const MAX_RETRIES: u32 = 3;

/// Initial backoff delay for retries (milliseconds)
const INITIAL_BACKOFF_MS: u64 = 1000;

/// Maximum backoff delay (milliseconds)
const MAX_BACKOFF_MS: u64 = 30000;

/// Backoff multiplier for exponential backoff
const BACKOFF_MULTIPLIER: f64 = 2.0;

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Discord webhook configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Discord webhook URL (optional - if None, notifications are disabled)
    pub webhook_url: Option<String>,

    /// Discord user ID to mention for critical alerts (optional)
    /// Format: "123456789012345678" (snowflake ID)
    pub mention_user_id: Option<String>,

    /// Whether Discord notifications are enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Minimum alert level to send (default: Info)
    #[serde(default)]
    pub min_alert_level: AlertLevel,

    /// Whether to send trade execution alerts
    #[serde(default = "default_true")]
    pub alert_on_trades: bool,

    /// Whether to send opportunity found alerts
    #[serde(default = "default_true")]
    pub alert_on_opportunities: bool,

    /// Whether to send error alerts
    #[serde(default = "default_true")]
    pub alert_on_errors: bool,

    /// Whether to send daily summary
    #[serde(default = "default_true")]
    pub send_daily_summary: bool,

    /// UTC hour to send daily summary (0-23)
    #[serde(default)]
    pub daily_summary_utc_hour: u32,
}

fn default_enabled() -> bool {
    true
}

fn default_true() -> bool {
    true
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            mention_user_id: None,
            enabled: true,
            min_alert_level: AlertLevel::Info,
            alert_on_trades: true,
            alert_on_opportunities: true,
            alert_on_errors: true,
            send_daily_summary: true,
            daily_summary_utc_hour: 0,
        }
    }
}

impl DiscordConfig {
    /// Create config from environment variables
    pub fn from_env() -> Self {
        let webhook_url = std::env::var("DISCORD_WEBHOOK_URL").ok().filter(|s| !s.is_empty());
        let mention_user_id = std::env::var("DISCORD_MENTION_USER_ID").ok().filter(|s| !s.is_empty());
        let enabled = std::env::var("DISCORD_ENABLED")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(webhook_url.is_some());

        Self {
            webhook_url,
            mention_user_id,
            enabled,
            ..Default::default()
        }
    }

    /// Check if notifications are effectively enabled
    pub fn is_active(&self) -> bool {
        self.enabled && self.webhook_url.is_some()
    }
}

// ============================================================================
// ALERT TYPES
// ============================================================================

/// Alert severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
pub enum AlertLevel {
    /// Informational alerts (trades, opportunities)
    #[default]
    Info,
    /// Warning alerts (rate limits, minor errors)
    Warning,
    /// Critical alerts (circuit breaker, crashes)
    Critical,
}

impl AlertLevel {
    /// Get Discord embed color for this alert level
    pub fn color(&self) -> u32 {
        match self {
            AlertLevel::Info => 0x3498DB,     // Blue
            AlertLevel::Warning => 0xF39C12,  // Orange
            AlertLevel::Critical => 0xE74C3C, // Red
        }
    }

    /// Get emoji for this alert level
    pub fn emoji(&self) -> &'static str {
        match self {
            AlertLevel::Info => "",
            AlertLevel::Warning => "",
            AlertLevel::Critical => "",
        }
    }

    /// Get human-readable name
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertLevel::Info => "Info",
            AlertLevel::Warning => "Warning",
            AlertLevel::Critical => "Critical",
        }
    }
}

impl std::fmt::Display for AlertLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Types of alerts that can be sent
#[derive(Debug, Clone)]
pub enum AlertType {
    /// Trade was executed
    TradeExecuted {
        market_title: String,
        side: String,
        price_cents: u16,
        size_usd: f64,
        strategy: String,
        platform: String,
    },

    /// Arbitrage opportunity found
    OpportunityFound {
        market_title: String,
        expected_value_pct: f64,
        profit_cents: i16,
        arb_type: String,
    },

    /// Position was closed
    PositionClosed {
        market_title: String,
        entry_price_cents: u16,
        exit_price_cents: u16,
        pnl_usd: f64,
        hold_duration: String,
    },

    /// Circuit breaker triggered
    CircuitBreakerTriggered {
        reason: String,
        current_drawdown_pct: f64,
        consecutive_losses: u32,
    },

    /// Error occurred
    Error {
        message: String,
        context: Option<String>,
    },

    /// Daily trading summary
    DailySummary {
        date: NaiveDate,
        starting_capital: f64,
        ending_capital: f64,
        realized_pnl: f64,
        trades_count: i32,
        win_count: i32,
        max_drawdown_pct: f64,
    },

    /// System startup notification
    SystemStartup {
        mode: String, // "live" or "paper"
        capital: f64,
        open_positions: usize,
    },

    /// System shutdown notification
    SystemShutdown {
        reason: String,
        final_capital: f64,
    },

    /// Custom alert with arbitrary content
    Custom {
        title: String,
        description: String,
        fields: Vec<(String, String, bool)>,
    },
}

impl AlertType {
    /// Get the default alert level for this alert type
    pub fn default_level(&self) -> AlertLevel {
        match self {
            AlertType::TradeExecuted { .. } => AlertLevel::Info,
            AlertType::OpportunityFound { .. } => AlertLevel::Info,
            AlertType::PositionClosed { pnl_usd, .. } => {
                if *pnl_usd < 0.0 {
                    AlertLevel::Warning
                } else {
                    AlertLevel::Info
                }
            }
            AlertType::CircuitBreakerTriggered { .. } => AlertLevel::Critical,
            AlertType::Error { .. } => AlertLevel::Warning,
            AlertType::DailySummary { realized_pnl, .. } => {
                if *realized_pnl < 0.0 {
                    AlertLevel::Warning
                } else {
                    AlertLevel::Info
                }
            }
            AlertType::SystemStartup { .. } => AlertLevel::Info,
            AlertType::SystemShutdown { .. } => AlertLevel::Warning,
            AlertType::Custom { .. } => AlertLevel::Info,
        }
    }

    /// Get title for this alert type
    pub fn title(&self) -> String {
        match self {
            AlertType::TradeExecuted { platform, .. } => {
                format!("Trade Executed on {}", capitalize(platform))
            }
            AlertType::OpportunityFound { arb_type, .. } => {
                format!("Opportunity Found: {}", arb_type)
            }
            AlertType::PositionClosed { pnl_usd, .. } => {
                if *pnl_usd >= 0.0 {
                    "Position Closed (Profit)".to_string()
                } else {
                    "Position Closed (Loss)".to_string()
                }
            }
            AlertType::CircuitBreakerTriggered { .. } => "Circuit Breaker Triggered".to_string(),
            AlertType::Error { .. } => "Error".to_string(),
            AlertType::DailySummary { date, .. } => format!("Daily Summary ({})", date),
            AlertType::SystemStartup { mode, .. } => {
                format!("System Started ({})", capitalize(mode))
            }
            AlertType::SystemShutdown { .. } => "System Shutdown".to_string(),
            AlertType::Custom { title, .. } => title.clone(),
        }
    }
}

// ============================================================================
// DISCORD EMBED STRUCTURES
// ============================================================================

/// Discord webhook message payload
#[derive(Debug, Serialize)]
struct WebhookPayload {
    /// Plain text content (for mentions)
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,

    /// Username override
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,

    /// Embedded rich content
    embeds: Vec<Embed>,
}

/// Discord embed structure
#[derive(Debug, Serialize)]
struct Embed {
    /// Embed title
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,

    /// Embed description
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    /// Embed color (integer)
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<u32>,

    /// Embed fields
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fields: Vec<EmbedField>,

    /// Footer
    #[serde(skip_serializing_if = "Option::is_none")]
    footer: Option<EmbedFooter>,

    /// Timestamp (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
}

/// Discord embed field
#[derive(Debug, Serialize)]
struct EmbedField {
    name: String,
    value: String,
    inline: bool,
}

/// Discord embed footer
#[derive(Debug, Serialize)]
struct EmbedFooter {
    text: String,
}

// ============================================================================
// RATE LIMITER
// ============================================================================

/// Simple token bucket rate limiter for Discord API
struct RateLimiter {
    /// Timestamps of recent requests
    request_times: Mutex<Vec<Instant>>,
    /// Window duration
    window: Duration,
    /// Maximum requests per window
    max_requests: u32,
}

impl RateLimiter {
    fn new(max_requests: u32, window_secs: u64) -> Self {
        Self {
            request_times: Mutex::new(Vec::with_capacity(max_requests as usize)),
            window: Duration::from_secs(window_secs),
            max_requests,
        }
    }

    /// Check if a request can be made, and if so, record it
    async fn acquire(&self) -> bool {
        let mut times = self.request_times.lock().await;
        let now = Instant::now();
        let cutoff = now - self.window;

        // Remove old timestamps
        times.retain(|&t| t > cutoff);

        if times.len() < self.max_requests as usize {
            times.push(now);
            true
        } else {
            false
        }
    }

    /// Get time until next available slot
    async fn time_until_available(&self) -> Option<Duration> {
        let times = self.request_times.lock().await;
        if times.len() < self.max_requests as usize {
            return None;
        }

        let oldest = times.first()?;
        let available_at = *oldest + self.window;
        let now = Instant::now();

        if available_at > now {
            Some(available_at - now)
        } else {
            None
        }
    }
}

// ============================================================================
// DISCORD NOTIFIER
// ============================================================================

/// Discord notification result
pub type DiscordResult<T> = Result<T, DiscordError>;

/// Discord notification errors
#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    #[error("Discord notifications not configured")]
    NotConfigured,

    #[error("Discord notifications disabled")]
    Disabled,

    #[error("Rate limited, retry after {0:?}")]
    RateLimited(Duration),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Webhook returned error status: {0}")]
    WebhookError(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Max retries exceeded")]
    MaxRetriesExceeded,
}

/// Discord webhook notifier
pub struct DiscordNotifier {
    /// Configuration
    config: DiscordConfig,

    /// HTTP client
    client: reqwest::Client,

    /// Rate limiter
    rate_limiter: RateLimiter,

    /// Count of sent messages (for statistics)
    messages_sent: AtomicU64,

    /// Count of failed messages
    messages_failed: AtomicU64,
}

impl DiscordNotifier {
    /// Create a new Discord notifier
    pub fn new(config: DiscordConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            config,
            client,
            rate_limiter: RateLimiter::new(RATE_LIMIT_REQUESTS, RATE_LIMIT_WINDOW_SECS),
            messages_sent: AtomicU64::new(0),
            messages_failed: AtomicU64::new(0),
        }
    }

    /// Create from environment variables
    pub fn from_env() -> Self {
        Self::new(DiscordConfig::from_env())
    }

    /// Check if the notifier is active
    pub fn is_active(&self) -> bool {
        self.config.is_active()
    }

    /// Get statistics
    pub fn stats(&self) -> (u64, u64) {
        (
            self.messages_sent.load(Ordering::Relaxed),
            self.messages_failed.load(Ordering::Relaxed),
        )
    }

    /// Send an alert with automatic level detection
    pub async fn send_alert(&self, alert: AlertType) -> DiscordResult<()> {
        let level = alert.default_level();
        self.send_alert_with_level(level, alert).await
    }

    /// Send an alert with explicit level
    pub async fn send_alert_with_level(
        &self,
        level: AlertLevel,
        alert: AlertType,
    ) -> DiscordResult<()> {
        // Check if notifications are enabled
        if !self.config.enabled {
            debug!("Discord notifications disabled");
            return Err(DiscordError::Disabled);
        }

        let webhook_url = match &self.config.webhook_url {
            Some(url) => url,
            None => {
                debug!("Discord webhook URL not configured");
                return Err(DiscordError::NotConfigured);
            }
        };

        // Check minimum alert level
        if level < self.config.min_alert_level {
            debug!("Alert level {:?} below minimum {:?}", level, self.config.min_alert_level);
            return Ok(());
        }

        // Check alert type filtering
        match &alert {
            AlertType::TradeExecuted { .. } if !self.config.alert_on_trades => {
                debug!("Trade alerts disabled");
                return Ok(());
            }
            AlertType::OpportunityFound { .. } if !self.config.alert_on_opportunities => {
                debug!("Opportunity alerts disabled");
                return Ok(());
            }
            AlertType::Error { .. } if !self.config.alert_on_errors => {
                debug!("Error alerts disabled");
                return Ok(());
            }
            AlertType::DailySummary { .. } if !self.config.send_daily_summary => {
                debug!("Daily summary disabled");
                return Ok(());
            }
            _ => {}
        }

        // Build the payload
        let payload = self.build_payload(level, &alert);

        // Send with retry logic
        self.send_with_retry(webhook_url, &payload).await
    }

    /// Build webhook payload from alert
    fn build_payload(&self, level: AlertLevel, alert: &AlertType) -> WebhookPayload {
        let embed = self.build_embed(level, alert);

        // Add mention for critical alerts
        let content = if level == AlertLevel::Critical {
            self.config
                .mention_user_id
                .as_ref()
                .map(|id| format!("<@{}>", id))
        } else {
            None
        };

        WebhookPayload {
            content,
            username: Some("Trading Bot".to_string()),
            embeds: vec![embed],
        }
    }

    /// Build Discord embed from alert
    fn build_embed(&self, level: AlertLevel, alert: &AlertType) -> Embed {
        let title = format!("{} {}", level.emoji(), alert.title());
        let color = level.color();
        let timestamp = Utc::now().to_rfc3339();

        let (description, fields) = match alert {
            AlertType::TradeExecuted {
                market_title,
                side,
                price_cents,
                size_usd,
                strategy,
                platform,
            } => {
                let desc = format!("**{}**", market_title);
                let fields = vec![
                    EmbedField {
                        name: "Side".to_string(),
                        value: side.to_uppercase(),
                        inline: true,
                    },
                    EmbedField {
                        name: "Price".to_string(),
                        value: format!("{}c", price_cents),
                        inline: true,
                    },
                    EmbedField {
                        name: "Size".to_string(),
                        value: format!("${:.2}", size_usd),
                        inline: true,
                    },
                    EmbedField {
                        name: "Platform".to_string(),
                        value: capitalize(platform),
                        inline: true,
                    },
                    EmbedField {
                        name: "Strategy".to_string(),
                        value: capitalize(strategy),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::OpportunityFound {
                market_title,
                expected_value_pct,
                profit_cents,
                arb_type,
            } => {
                let desc = format!("**{}**", market_title);
                let fields = vec![
                    EmbedField {
                        name: "Type".to_string(),
                        value: arb_type.clone(),
                        inline: true,
                    },
                    EmbedField {
                        name: "Expected Value".to_string(),
                        value: format!("{:+.2}%", expected_value_pct),
                        inline: true,
                    },
                    EmbedField {
                        name: "Profit".to_string(),
                        value: format!("{}c/contract", profit_cents),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::PositionClosed {
                market_title,
                entry_price_cents,
                exit_price_cents,
                pnl_usd,
                hold_duration,
            } => {
                let desc = format!("**{}**", market_title);
                let pnl_str = if *pnl_usd >= 0.0 {
                    format!("+${:.2}", pnl_usd)
                } else {
                    format!("-${:.2}", pnl_usd.abs())
                };
                let fields = vec![
                    EmbedField {
                        name: "Entry".to_string(),
                        value: format!("{}c", entry_price_cents),
                        inline: true,
                    },
                    EmbedField {
                        name: "Exit".to_string(),
                        value: format!("{}c", exit_price_cents),
                        inline: true,
                    },
                    EmbedField {
                        name: "P&L".to_string(),
                        value: pnl_str,
                        inline: true,
                    },
                    EmbedField {
                        name: "Duration".to_string(),
                        value: hold_duration.clone(),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::CircuitBreakerTriggered {
                reason,
                current_drawdown_pct,
                consecutive_losses,
            } => {
                let desc = format!("**Trading halted: {}**", reason);
                let fields = vec![
                    EmbedField {
                        name: "Current Drawdown".to_string(),
                        value: format!("{:.2}%", current_drawdown_pct * 100.0),
                        inline: true,
                    },
                    EmbedField {
                        name: "Consecutive Losses".to_string(),
                        value: consecutive_losses.to_string(),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::Error { message, context } => {
                let desc = format!("```\n{}\n```", message);
                let fields = context
                    .as_ref()
                    .map(|ctx| {
                        vec![EmbedField {
                            name: "Context".to_string(),
                            value: ctx.clone(),
                            inline: false,
                        }]
                    })
                    .unwrap_or_default();
                (Some(desc), fields)
            }

            AlertType::DailySummary {
                date,
                starting_capital,
                ending_capital,
                realized_pnl,
                trades_count,
                win_count,
                max_drawdown_pct,
            } => {
                let win_rate = if *trades_count > 0 {
                    (*win_count as f64 / *trades_count as f64) * 100.0
                } else {
                    0.0
                };
                let loss_count = trades_count - win_count;
                let pnl_pct = if *starting_capital > 0.0 {
                    (realized_pnl / starting_capital) * 100.0
                } else {
                    0.0
                };

                let pnl_str = if *realized_pnl >= 0.0 {
                    format!("+${:.2} (+{:.1}%)", realized_pnl, pnl_pct)
                } else {
                    format!("-${:.2} ({:.1}%)", realized_pnl.abs(), pnl_pct)
                };

                let desc = self.format_daily_summary_table(
                    *date,
                    *starting_capital,
                    *ending_capital,
                    *realized_pnl,
                    *trades_count,
                    *win_count,
                    *max_drawdown_pct,
                );

                let fields = vec![
                    EmbedField {
                        name: "Starting Capital".to_string(),
                        value: format!("${:.2}", starting_capital),
                        inline: true,
                    },
                    EmbedField {
                        name: "Ending Capital".to_string(),
                        value: format!("${:.2}", ending_capital),
                        inline: true,
                    },
                    EmbedField {
                        name: "Realized P&L".to_string(),
                        value: pnl_str,
                        inline: true,
                    },
                    EmbedField {
                        name: "Trades".to_string(),
                        value: format!("{} ({} W / {} L)", trades_count, win_count, loss_count),
                        inline: true,
                    },
                    EmbedField {
                        name: "Win Rate".to_string(),
                        value: format!("{:.1}%", win_rate),
                        inline: true,
                    },
                    EmbedField {
                        name: "Max Drawdown".to_string(),
                        value: format!("{:.2}%", max_drawdown_pct * 100.0),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::SystemStartup {
                mode,
                capital,
                open_positions,
            } => {
                let desc = format!(
                    "Trading system initialized in **{}** mode",
                    mode.to_uppercase()
                );
                let fields = vec![
                    EmbedField {
                        name: "Capital".to_string(),
                        value: format!("${:.2}", capital),
                        inline: true,
                    },
                    EmbedField {
                        name: "Open Positions".to_string(),
                        value: open_positions.to_string(),
                        inline: true,
                    },
                ];
                (Some(desc), fields)
            }

            AlertType::SystemShutdown {
                reason,
                final_capital,
            } => {
                let desc = format!("**Reason:** {}", reason);
                let fields = vec![EmbedField {
                    name: "Final Capital".to_string(),
                    value: format!("${:.2}", final_capital),
                    inline: true,
                }];
                (Some(desc), fields)
            }

            AlertType::Custom {
                title: _,
                description,
                fields,
            } => {
                let embed_fields = fields
                    .iter()
                    .map(|(name, value, inline)| EmbedField {
                        name: name.clone(),
                        value: value.clone(),
                        inline: *inline,
                    })
                    .collect();
                (Some(description.clone()), embed_fields)
            }
        };

        Embed {
            title: Some(title),
            description,
            color: Some(color),
            fields,
            footer: Some(EmbedFooter {
                text: "Prediction Market Trading Bot".to_string(),
            }),
            timestamp: Some(timestamp),
        }
    }

    /// Format daily summary as a nice table
    fn format_daily_summary_table(
        &self,
        date: NaiveDate,
        starting_capital: f64,
        ending_capital: f64,
        realized_pnl: f64,
        trades_count: i32,
        win_count: i32,
        max_drawdown_pct: f64,
    ) -> String {
        let win_rate = if trades_count > 0 {
            (win_count as f64 / trades_count as f64) * 100.0
        } else {
            0.0
        };
        let loss_count = trades_count - win_count;
        let return_pct = if starting_capital > 0.0 {
            ((ending_capital - starting_capital) / starting_capital) * 100.0
        } else {
            0.0
        };

        format!(
            "```\n\
             +---------------------------+\n\
             |     DAILY REPORT {}    |\n\
             +---------------------------+\n\
             | Starting    | ${:>10.2} |\n\
             | Ending      | ${:>10.2} |\n\
             | P&L         | ${:>+10.2} |\n\
             | Return      | {:>+10.1}% |\n\
             +---------------------------+\n\
             | Trades      | {:>10}  |\n\
             | Wins        | {:>10}  |\n\
             | Losses      | {:>10}  |\n\
             | Win Rate    | {:>9.1}%  |\n\
             +---------------------------+\n\
             | Max DD      | {:>9.2}%  |\n\
             +---------------------------+\n\
             ```",
            date,
            starting_capital,
            ending_capital,
            realized_pnl,
            return_pct,
            trades_count,
            win_count,
            loss_count,
            win_rate,
            max_drawdown_pct * 100.0
        )
    }

    /// Send webhook with retry logic and exponential backoff
    async fn send_with_retry(
        &self,
        webhook_url: &str,
        payload: &WebhookPayload,
    ) -> DiscordResult<()> {
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        for attempt in 0..MAX_RETRIES {
            // Check rate limit
            if !self.rate_limiter.acquire().await {
                if let Some(wait_time) = self.rate_limiter.time_until_available().await {
                    warn!(
                        "Discord rate limited, waiting {:?} (attempt {})",
                        wait_time, attempt
                    );
                    tokio::time::sleep(wait_time).await;
                    continue;
                }
            }

            // Attempt to send
            match self.send_once(webhook_url, payload).await {
                Ok(()) => {
                    self.messages_sent.fetch_add(1, Ordering::Relaxed);
                    debug!("Discord message sent successfully");
                    return Ok(());
                }
                Err(DiscordError::RateLimited(duration)) => {
                    warn!("Discord API rate limited, waiting {:?}", duration);
                    tokio::time::sleep(duration).await;
                }
                Err(e) if attempt < MAX_RETRIES - 1 => {
                    warn!(
                        "Discord send failed (attempt {}): {}, retrying in {}ms",
                        attempt + 1,
                        e,
                        backoff_ms
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms as f64 * BACKOFF_MULTIPLIER)
                        .min(MAX_BACKOFF_MS as f64) as u64;
                }
                Err(e) => {
                    self.messages_failed.fetch_add(1, Ordering::Relaxed);
                    error!("Discord send failed after {} attempts: {}", MAX_RETRIES, e);
                    return Err(e);
                }
            }
        }

        self.messages_failed.fetch_add(1, Ordering::Relaxed);
        Err(DiscordError::MaxRetriesExceeded)
    }

    /// Send a single webhook request
    async fn send_once(&self, webhook_url: &str, payload: &WebhookPayload) -> DiscordResult<()> {
        let response = self
            .client
            .post(webhook_url)
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await?;

        let status = response.status();

        if status.is_success() {
            return Ok(());
        }

        // Handle rate limiting
        if status.as_u16() == 429 {
            // Try to get retry-after header
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|secs| Duration::from_secs_f64(secs))
                .unwrap_or(Duration::from_secs(5));

            return Err(DiscordError::RateLimited(retry_after));
        }

        // Get error body for debugging
        let error_body = response.text().await.unwrap_or_default();
        Err(DiscordError::WebhookError(format!(
            "Status {}: {}",
            status, error_body
        )))
    }

    /// Send a simple text message (no embed)
    pub async fn send_text(&self, message: &str) -> DiscordResult<()> {
        if !self.config.enabled {
            return Err(DiscordError::Disabled);
        }

        let webhook_url = match &self.config.webhook_url {
            Some(url) => url,
            None => return Err(DiscordError::NotConfigured),
        };

        #[derive(Serialize)]
        struct TextPayload<'a> {
            content: &'a str,
        }

        let payload = TextPayload { content: message };

        // Check rate limit
        if !self.rate_limiter.acquire().await {
            if let Some(wait_time) = self.rate_limiter.time_until_available().await {
                return Err(DiscordError::RateLimited(wait_time));
            }
        }

        let response = self
            .client
            .post(webhook_url)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        if response.status().is_success() {
            self.messages_sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        } else {
            self.messages_failed.fetch_add(1, Ordering::Relaxed);
            let error_body = response.text().await.unwrap_or_default();
            Err(DiscordError::WebhookError(error_body))
        }
    }
}

// ============================================================================
// CONVENIENCE FUNCTIONS
// ============================================================================

/// Create a shared Discord notifier
pub fn create_notifier() -> Arc<DiscordNotifier> {
    Arc::new(DiscordNotifier::from_env())
}

/// Helper to capitalize first letter
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// ============================================================================
// DAILY STATS STRUCT (for compatibility with spec)
// ============================================================================

/// Daily trading statistics for summary alerts
#[derive(Debug, Clone, Default)]
pub struct DailyStats {
    pub date: NaiveDate,
    pub starting_capital: f64,
    pub ending_capital: f64,
    pub realized_pnl: f64,
    pub trades_count: i32,
    pub win_count: i32,
    pub max_drawdown: f64,
}

impl From<DailyStats> for AlertType {
    fn from(stats: DailyStats) -> Self {
        AlertType::DailySummary {
            date: stats.date,
            starting_capital: stats.starting_capital,
            ending_capital: stats.ending_capital,
            realized_pnl: stats.realized_pnl,
            trades_count: stats.trades_count,
            win_count: stats.win_count,
            max_drawdown_pct: stats.max_drawdown,
        }
    }
}

// ============================================================================
// FORMATTING HELPERS (spec compatibility)
// ============================================================================

/// Format a trade alert message (spec-compatible)
pub fn format_trade_alert(
    market_title: &str,
    side: &str,
    price_cents: u16,
    size_usd: f64,
    strategy: &str,
) -> String {
    format!(
        "**Trade Executed**\n\
        Market: {}\n\
        Side: {} @ {:.0}c\n\
        Size: ${:.2}\n\
        Strategy: {}",
        market_title, side, price_cents, size_usd, strategy
    )
}

/// Format a daily summary message (spec-compatible)
pub fn format_daily_summary(stats: &DailyStats) -> String {
    let win_rate = if stats.trades_count > 0 {
        (stats.win_count as f64 / stats.trades_count as f64) * 100.0
    } else {
        0.0
    };
    let loss_count = stats.trades_count - stats.win_count;
    let return_pct = if stats.starting_capital > 0.0 {
        (stats.realized_pnl / stats.starting_capital) * 100.0
    } else {
        0.0
    };

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
        return_pct,
        stats.trades_count,
        stats.win_count,
        loss_count,
        win_rate,
        stats.max_drawdown * 100.0
    )
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alert_level_ordering() {
        assert!(AlertLevel::Info < AlertLevel::Warning);
        assert!(AlertLevel::Warning < AlertLevel::Critical);
    }

    #[test]
    fn test_alert_level_colors() {
        assert_eq!(AlertLevel::Info.color(), 0x3498DB);
        assert_eq!(AlertLevel::Warning.color(), 0xF39C12);
        assert_eq!(AlertLevel::Critical.color(), 0xE74C3C);
    }

    #[test]
    fn test_config_default() {
        let config = DiscordConfig::default();
        assert!(config.webhook_url.is_none());
        assert!(config.mention_user_id.is_none());
        assert!(config.enabled);
        assert!(!config.is_active());
    }

    #[test]
    fn test_config_is_active() {
        let mut config = DiscordConfig::default();
        assert!(!config.is_active());

        config.webhook_url = Some("https://discord.com/api/webhooks/test".to_string());
        assert!(config.is_active());

        config.enabled = false;
        assert!(!config.is_active());
    }

    #[test]
    fn test_alert_type_default_level() {
        let trade = AlertType::TradeExecuted {
            market_title: "Test".to_string(),
            side: "YES".to_string(),
            price_cents: 50,
            size_usd: 100.0,
            strategy: "grinding".to_string(),
            platform: "polymarket".to_string(),
        };
        assert_eq!(trade.default_level(), AlertLevel::Info);

        let circuit_breaker = AlertType::CircuitBreakerTriggered {
            reason: "Max drawdown".to_string(),
            current_drawdown_pct: 0.06,
            consecutive_losses: 3,
        };
        assert_eq!(circuit_breaker.default_level(), AlertLevel::Critical);

        // Losing position should be warning
        let position_loss = AlertType::PositionClosed {
            market_title: "Test".to_string(),
            entry_price_cents: 60,
            exit_price_cents: 40,
            pnl_usd: -10.0,
            hold_duration: "1h".to_string(),
        };
        assert_eq!(position_loss.default_level(), AlertLevel::Warning);
    }

    #[test]
    fn test_format_trade_alert() {
        let msg = format_trade_alert("Chelsea vs Arsenal", "YES", 45, 50.0, "grinding");
        assert!(msg.contains("Chelsea vs Arsenal"));
        assert!(msg.contains("YES"));
        assert!(msg.contains("45c"));
        assert!(msg.contains("$50.00"));
    }

    #[test]
    fn test_format_daily_summary() {
        let stats = DailyStats {
            date: NaiveDate::from_ymd_opt(2024, 12, 30).unwrap(),
            starting_capital: 100.0,
            ending_capital: 110.0,
            realized_pnl: 10.0,
            trades_count: 10,
            win_count: 8,
            max_drawdown: 0.02,
        };

        let msg = format_daily_summary(&stats);
        assert!(msg.contains("2024-12-30"));
        assert!(msg.contains("$100.00"));
        assert!(msg.contains("$110.00"));
        assert!(msg.contains("80.0%")); // win rate
    }

    #[test]
    fn test_daily_stats_to_alert_type() {
        let stats = DailyStats {
            date: NaiveDate::from_ymd_opt(2024, 12, 30).unwrap(),
            starting_capital: 100.0,
            ending_capital: 110.0,
            realized_pnl: 10.0,
            trades_count: 10,
            win_count: 8,
            max_drawdown: 0.02,
        };

        let alert: AlertType = stats.into();
        match alert {
            AlertType::DailySummary {
                trades_count,
                win_count,
                ..
            } => {
                assert_eq!(trades_count, 10);
                assert_eq!(win_count, 8);
            }
            _ => panic!("Expected DailySummary"),
        }
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("polymarket"), "Polymarket");
        assert_eq!(capitalize("kalshi"), "Kalshi");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("a"), "A");
    }

    #[tokio::test]
    async fn test_rate_limiter() {
        let limiter = RateLimiter::new(3, 1);

        // First 3 requests should succeed
        assert!(limiter.acquire().await);
        assert!(limiter.acquire().await);
        assert!(limiter.acquire().await);

        // 4th should fail
        assert!(!limiter.acquire().await);

        // Wait for window to pass
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Should succeed again
        assert!(limiter.acquire().await);
    }

    #[test]
    fn test_notifier_creation() {
        let config = DiscordConfig {
            webhook_url: Some("https://test.com".to_string()),
            enabled: true,
            ..Default::default()
        };

        let notifier = DiscordNotifier::new(config);
        assert!(notifier.is_active());

        let (sent, failed) = notifier.stats();
        assert_eq!(sent, 0);
        assert_eq!(failed, 0);
    }

    #[test]
    fn test_disabled_notifier() {
        let config = DiscordConfig {
            webhook_url: Some("https://test.com".to_string()),
            enabled: false,
            ..Default::default()
        };

        let notifier = DiscordNotifier::new(config);
        assert!(!notifier.is_active());
    }
}
