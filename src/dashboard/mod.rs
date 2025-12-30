#![allow(dead_code)]

//! Web Dashboard Module
//!
//! This module provides a web-based dashboard for monitoring and controlling
//! the prediction market trading system. Built with Axum, HTMX, and Tailwind CSS.
//!
//! # Features
//!
//! - **Real-time Updates**: Server-Sent Events (SSE) for live price and position updates
//! - **Core Trading Panel**: Capital, open positions, P&L tracking
//! - **Opportunity Scanner**: Live opportunities ranked by expected value
//! - **Risk Gauges**: Drawdown %, exposure by category
//! - **Historical Charts**: Cumulative P&L visualization with Chart.js
//! - **Kill Switch**: Emergency trading halt button
//! - **Paper/Live Mode Toggle**: Switch between paper and live trading
//!
//! # Usage
//!
//! ```rust,ignore
//! use prediction_market_arbitrage::dashboard::{DashboardServer, DashboardState};
//!
//! // Create shared state
//! let state = DashboardState::new(/* ... */);
//!
//! // Start the dashboard server
//! let server = DashboardServer::new(state);
//! server.run(8080).await?;
//! ```

pub mod handlers;
pub mod server;
pub mod sse;
pub mod state;

pub use server::DashboardServer;
pub use state::DashboardState;
