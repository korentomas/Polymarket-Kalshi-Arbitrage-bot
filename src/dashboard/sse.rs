//! Server-Sent Events (SSE) implementation for real-time dashboard updates.
//!
//! This module provides SSE streaming for pushing real-time data to the dashboard,
//! including price updates, position changes, and system status.

use crate::dashboard::state::{DashboardEvent, DashboardState};
use async_stream::stream;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Create an SSE stream for a client connection
pub fn create_sse_stream(
    state: Arc<DashboardState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.subscribe();

    let stream = stream! {
        // Send initial state on connection
        if let Ok(stats) = serde_json::to_string(&state.get_stats().await) {
            yield Ok(Event::default()
                .event("stats")
                .data(stats));
        }

        if let Ok(positions) = serde_json::to_string(&state.get_display_positions().await) {
            yield Ok(Event::default()
                .event("positions")
                .data(positions));
        }

        let opportunities = state.opportunities.read().await;
        if let Ok(opps) = serde_json::to_string(&*opportunities) {
            yield Ok(Event::default()
                .event("opportunities")
                .data(opps));
        }
        drop(opportunities);

        let status_event = DashboardEvent::Status {
            status: *state.system_status.read().await,
            mode: *state.trading_mode.read().await,
            kill_switch_active: state.kill_switch_active.load(std::sync::atomic::Ordering::Relaxed),
        };
        if let Ok(status) = serde_json::to_string(&status_event) {
            yield Ok(Event::default()
                .event("status")
                .data(status));
        }

        // Stream events from broadcast channel
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let (event_type, data) = match &event {
                        DashboardEvent::Stats(stats) => {
                            ("stats", serde_json::to_string(stats))
                        }
                        DashboardEvent::Positions(positions) => {
                            ("positions", serde_json::to_string(positions))
                        }
                        DashboardEvent::Opportunities(opportunities) => {
                            ("opportunities", serde_json::to_string(opportunities))
                        }
                        DashboardEvent::Status { .. } => {
                            ("status", serde_json::to_string(&event))
                        }
                        DashboardEvent::PriceUpdate { .. } => {
                            ("price_update", serde_json::to_string(&event))
                        }
                        DashboardEvent::Alert { .. } => {
                            ("alert", serde_json::to_string(&event))
                        }
                    };

                    match data {
                        Ok(json) => {
                            debug!("SSE sending event: {}", event_type);
                            yield Ok(Event::default()
                                .event(event_type)
                                .data(json));
                        }
                        Err(e) => {
                            warn!("Failed to serialize SSE event: {}", e);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("SSE client lagged by {} messages", n);
                    // Continue receiving
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!("SSE broadcast channel closed");
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// Background task that periodically broadcasts stats updates
pub async fn stats_broadcaster(state: Arc<DashboardState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        interval.tick().await;

        // Broadcast current stats
        let stats = state.get_stats().await;
        state.broadcast(DashboardEvent::Stats(stats));

        // Broadcast positions
        let positions = state.get_display_positions().await;
        state.broadcast(DashboardEvent::Positions(positions));
    }
}

/// Background task that sends heartbeat status updates
pub async fn heartbeat_broadcaster(state: Arc<DashboardState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        interval.tick().await;

        let status = DashboardEvent::Status {
            status: *state.system_status.read().await,
            mode: *state.trading_mode.read().await,
            kill_switch_active: state
                .kill_switch_active
                .load(std::sync::atomic::Ordering::Relaxed),
        };

        state.broadcast(status);
    }
}

/// Helper to format SSE event data
pub fn format_sse_event(event_type: &str, data: &str) -> String {
    format!("event: {}\ndata: {}\n\n", event_type, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_sse_event() {
        let result = format_sse_event("test", r#"{"key": "value"}"#);
        assert!(result.contains("event: test"));
        assert!(result.contains("data: {\"key\": \"value\"}"));
    }

    #[tokio::test]
    async fn test_sse_stream_creation() {
        let state = DashboardState::new();
        let _sse = create_sse_stream(state);
        // Stream should be created without panic
    }
}
