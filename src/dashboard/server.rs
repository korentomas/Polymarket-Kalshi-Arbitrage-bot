//! Axum server setup and configuration.
//!
//! This module provides the main dashboard server with all routes configured,
//! CORS middleware, static file serving, and graceful shutdown support.

use crate::dashboard::handlers::{
    api_config, api_config_update, api_events, api_history, api_kill_switch, api_mode,
    api_opportunities, api_positions, api_stats, analytics_page, health_check, history_page,
    index_page, opportunities_page, positions_page, settings_page,
};
use crate::dashboard::sse::{heartbeat_broadcaster, stats_broadcaster};
use crate::dashboard::state::DashboardState;
use axum::{
    http::{header, Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
    trace::TraceLayer,
};
use tracing::{error, info};

/// Dashboard server configuration
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Port to listen on
    pub port: u16,
    /// Host to bind to
    pub host: String,
    /// Path to static files directory
    pub static_dir: PathBuf,
    /// Enable CORS for development
    pub enable_cors: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            host: "127.0.0.1".to_string(),
            static_dir: PathBuf::from("static"),
            enable_cors: true,
        }
    }
}

impl DashboardConfig {
    /// Create config from environment variables
    pub fn from_env() -> Self {
        Self {
            port: std::env::var("DASHBOARD_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            host: std::env::var("DASHBOARD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            static_dir: std::env::var("DASHBOARD_STATIC_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("static")),
            enable_cors: std::env::var("DASHBOARD_CORS")
                .map(|v| v == "1" || v.to_lowercase() == "true")
                .unwrap_or(true),
        }
    }
}

/// Dashboard server
pub struct DashboardServer {
    state: Arc<DashboardState>,
    config: DashboardConfig,
}

impl DashboardServer {
    /// Create a new dashboard server with state
    pub fn new(state: Arc<DashboardState>) -> Self {
        Self {
            state,
            config: DashboardConfig::default(),
        }
    }

    /// Create with custom configuration
    pub fn with_config(state: Arc<DashboardState>, config: DashboardConfig) -> Self {
        Self { state, config }
    }

    /// Build the router with all routes
    fn build_router(&self) -> Router {
        // Create CORS layer if enabled
        let cors = if self.config.enable_cors {
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers([header::CONTENT_TYPE, header::ACCEPT])
        } else {
            CorsLayer::new()
        };

        // Static file service
        let static_service = ServeDir::new(&self.config.static_dir)
            .append_index_html_on_directories(true);

        // Build the router
        Router::new()
            // HTML pages
            .route("/", get(index_page))
            .route("/positions", get(positions_page))
            .route("/opportunities", get(opportunities_page))
            .route("/history", get(history_page))
            .route("/analytics", get(analytics_page))
            .route("/settings", get(settings_page))
            // API routes
            .route("/api/positions", get(api_positions))
            .route("/api/opportunities", get(api_opportunities))
            .route("/api/stats", get(api_stats))
            .route("/api/history", get(api_history))
            .route("/api/config", get(api_config).post(api_config_update))
            .route("/api/events", get(api_events))
            // Control routes
            .route("/api/kill-switch", post(api_kill_switch))
            .route("/api/mode", post(api_mode))
            // Health check
            .route("/health", get(health_check))
            // Fallback for static files and styles
            .nest_service("/static", static_service)
            .route("/styles.css", get(serve_styles))
            // Add state and middleware
            .with_state(self.state.clone())
            .layer(cors)
            .layer(TraceLayer::new_for_http())
    }

    /// Run the server
    pub async fn run(self) -> anyhow::Result<()> {
        let addr: SocketAddr = format!("{}:{}", self.config.host, self.config.port).parse()?;

        info!("Starting dashboard server at http://{}", addr);
        info!("Static files directory: {:?}", self.config.static_dir);

        let router = self.build_router();
        let state = self.state.clone();

        // Spawn background tasks
        let state_for_stats = state.clone();
        tokio::spawn(async move {
            stats_broadcaster(state_for_stats).await;
        });

        let state_for_heartbeat = state.clone();
        tokio::spawn(async move {
            heartbeat_broadcaster(state_for_heartbeat).await;
        });

        // Create TCP listener
        let listener = tokio::net::TcpListener::bind(addr).await?;

        info!("Dashboard ready at http://{}", addr);

        // Run the server with graceful shutdown
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await?;

        info!("Dashboard server shut down");
        Ok(())
    }

    /// Run the server on a specific port (convenience method)
    pub async fn run_on_port(self, port: u16) -> anyhow::Result<()> {
        let mut server = self;
        server.config.port = port;
        server.run().await
    }
}

/// Serve the CSS styles (embedded or from file)
async fn serve_styles() -> impl IntoResponse {
    let css = include_str!("../../static/styles.css");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css")],
        css,
    )
}

/// Shutdown signal handler for graceful shutdown
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = terminate => {
            info!("Received SIGTERM, shutting down...");
        }
    }
}

/// Start the dashboard server in a background task
pub fn spawn_dashboard_server(state: Arc<DashboardState>, port: u16) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let server = DashboardServer::new(state);
        if let Err(e) = server.run_on_port(port).await {
            error!("Dashboard server error: {}", e);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DashboardConfig::default();
        assert_eq!(config.port, 8080);
        assert_eq!(config.host, "127.0.0.1");
        assert!(config.enable_cors);
    }

    #[tokio::test]
    async fn test_server_creation() {
        let state = DashboardState::new();
        let server = DashboardServer::new(state);
        let _router = server.build_router();
        // Router should build without panicking
    }
}
