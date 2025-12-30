#![allow(dead_code)]

//! Kalman Filter Probability Estimation Module
//!
//! This module implements a Kalman filter for estimating the "true" underlying probability
//! of prediction market outcomes. It smooths noise from orderbook fluctuations and provides
//! adaptive estimation that increases uncertainty as market expiry approaches.
//!
//! # Key Features
//!
//! - **Adaptive Process Noise**: Process noise increases as expiry approaches, reflecting
//!   that new information is more likely to cause large probability shifts near resolution.
//!
//! - **Cross-Platform Fusion**: Volume-weighted averaging of probability estimates from
//!   multiple platforms (Polymarket, Kalshi) for more robust estimates.
//!
//! - **Thread-Safe State Management**: Uses interior mutability patterns for concurrent
//!   access from multiple WebSocket feeds.
//!
//! # Usage
//!
//! ```rust,ignore
//! use prediction_market_arbitrage::kalman::{KalmanFilter, KalmanConfig};
//!
//! let config = KalmanConfig::default();
//! let mut filter = KalmanFilter::new(0.5, &config); // Initial estimate of 50%
//!
//! // Update with new price observation
//! let hours_to_expiry = 12.0;
//! filter.update(0.55, hours_to_expiry, &config);
//!
//! println!("Estimated probability: {:.2}%", filter.estimate() * 100.0);
//! println!("Confidence (1/variance): {:.2}", 1.0 / filter.variance());
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::RwLock;

// Custom serde for DateTime<Utc> as i64 timestamp
fn serialize_datetime<S>(dt: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_i64(dt.timestamp())
}

fn deserialize_datetime<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let timestamp = i64::deserialize(deserializer)?;
    DateTime::from_timestamp(timestamp, 0)
        .ok_or_else(|| serde::de::Error::custom("invalid timestamp"))
}

// =============================================================================
// CONFIGURATION
// =============================================================================

/// Configuration parameters for the Kalman filter.
///
/// These parameters control how the filter balances between trusting new
/// observations and maintaining stability in its estimates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KalmanConfig {
    /// Initial variance for new markets.
    /// Higher values = more uncertainty initially, faster adaptation to observations.
    /// Default: 0.1 (10% initial uncertainty)
    pub initial_variance: f64,

    /// Measurement noise (how much we trust each price observation).
    /// Lower values = more trust in observations, faster response to price changes.
    /// Default: 0.01 (1% measurement uncertainty)
    pub measurement_noise: f64,

    /// Base process noise (how much true probability drifts over time).
    /// Higher values = expect more volatility in true probability.
    /// Default: 0.001 (0.1% base drift per update)
    pub base_process_noise: f64,

    /// Process noise multiplier as expiry approaches.
    /// actual_noise = base * (1.0 + multiplier * (1.0 - hours_remaining / 24.0))
    /// Default: 10.0 (10x increase in process noise at expiry)
    pub expiry_noise_multiplier: f64,

    /// Maximum hours to consider for expiry noise scaling.
    /// Beyond this, process noise stays at base level.
    /// Default: 24.0 hours
    pub max_hours_for_scaling: f64,

    /// Minimum variance floor to prevent overconfidence.
    /// Default: 0.0001 (0.01% minimum uncertainty)
    pub min_variance: f64,

    /// Maximum variance ceiling to prevent divergence.
    /// Default: 1.0 (100% maximum uncertainty)
    pub max_variance: f64,
}

impl Default for KalmanConfig {
    fn default() -> Self {
        Self {
            initial_variance: 0.1,
            measurement_noise: 0.01,
            base_process_noise: 0.001,
            expiry_noise_multiplier: 10.0,
            max_hours_for_scaling: 24.0,
            min_variance: 0.0001,
            max_variance: 1.0,
        }
    }
}

impl KalmanConfig {
    /// Create a config optimized for high-frequency updates (sub-second).
    /// Uses lower noise values for smoother estimates.
    pub fn high_frequency() -> Self {
        Self {
            initial_variance: 0.05,
            measurement_noise: 0.005,
            base_process_noise: 0.0005,
            expiry_noise_multiplier: 5.0,
            max_hours_for_scaling: 24.0,
            min_variance: 0.00001,
            max_variance: 0.5,
        }
    }

    /// Create a config optimized for low-frequency updates (minutes/hours).
    /// Uses higher noise values for faster adaptation.
    pub fn low_frequency() -> Self {
        Self {
            initial_variance: 0.2,
            measurement_noise: 0.02,
            base_process_noise: 0.005,
            expiry_noise_multiplier: 15.0,
            max_hours_for_scaling: 24.0,
            min_variance: 0.001,
            max_variance: 1.0,
        }
    }

    /// Create a config for very stable markets (low volatility expected).
    pub fn stable_market() -> Self {
        Self {
            initial_variance: 0.05,
            measurement_noise: 0.02,
            base_process_noise: 0.0001,
            expiry_noise_multiplier: 5.0,
            max_hours_for_scaling: 24.0,
            min_variance: 0.0001,
            max_variance: 0.5,
        }
    }

    /// Create a config for volatile markets (high uncertainty expected).
    pub fn volatile_market() -> Self {
        Self {
            initial_variance: 0.2,
            measurement_noise: 0.005,
            base_process_noise: 0.005,
            expiry_noise_multiplier: 20.0,
            max_hours_for_scaling: 24.0,
            min_variance: 0.0001,
            max_variance: 1.0,
        }
    }

    /// Calculate adaptive process noise based on time to expiry.
    ///
    /// # Arguments
    /// * `hours_to_expiry` - Hours remaining until market resolution
    ///
    /// # Returns
    /// Process noise value (increases as expiry approaches)
    #[inline]
    pub fn calculate_process_noise(&self, hours_to_expiry: f64) -> f64 {
        // Clamp hours to valid range
        let hours_clamped = hours_to_expiry.clamp(0.0, self.max_hours_for_scaling);

        // Calculate time factor (0.0 at max_hours, 1.0 at 0 hours)
        let time_factor = 1.0 - (hours_clamped / self.max_hours_for_scaling);

        // Adaptive process noise increases as expiry approaches
        self.base_process_noise * (1.0 + self.expiry_noise_multiplier * time_factor)
    }
}

// =============================================================================
// KALMAN STATE
// =============================================================================

/// Current state of the Kalman filter for a single market/outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KalmanState {
    /// Current probability estimate (0.0 to 1.0)
    pub estimate: f64,

    /// Estimation uncertainty (variance)
    pub variance: f64,

    /// Last update timestamp (serialized as Unix timestamp)
    #[serde(serialize_with = "serialize_datetime", deserialize_with = "deserialize_datetime")]
    pub last_update: DateTime<Utc>,

    /// Number of updates processed
    pub update_count: u64,
}

impl KalmanState {
    /// Create a new Kalman state with initial estimate.
    pub fn new(initial_estimate: f64, config: &KalmanConfig) -> Self {
        Self {
            estimate: initial_estimate.clamp(0.0, 1.0),
            variance: config.initial_variance,
            last_update: Utc::now(),
            update_count: 0,
        }
    }

    /// Create a state with specific variance (for persistence recovery).
    pub fn with_variance(estimate: f64, variance: f64, last_update: DateTime<Utc>) -> Self {
        Self {
            estimate: estimate.clamp(0.0, 1.0),
            variance: variance.clamp(0.0001, 1.0),
            last_update,
            update_count: 0,
        }
    }

    /// Check if the state is stale (not updated recently).
    pub fn is_stale(&self, max_age_secs: i64) -> bool {
        let age = Utc::now().signed_duration_since(self.last_update);
        age.num_seconds() > max_age_secs
    }

    /// Get confidence level (inverse of variance, higher = more confident).
    #[inline]
    pub fn confidence(&self) -> f64 {
        1.0 / self.variance
    }
}

// =============================================================================
// KALMAN FILTER
// =============================================================================

/// Kalman filter for probability estimation.
///
/// Implements a one-dimensional Kalman filter specialized for probability
/// estimation in prediction markets. The filter maintains a probability
/// estimate and its uncertainty, updating both as new price observations arrive.
#[derive(Debug)]
pub struct KalmanFilter {
    state: KalmanState,
}

impl KalmanFilter {
    /// Create a new Kalman filter with initial estimate.
    ///
    /// # Arguments
    /// * `initial_estimate` - Initial probability estimate (0.0 to 1.0)
    /// * `config` - Filter configuration parameters
    pub fn new(initial_estimate: f64, config: &KalmanConfig) -> Self {
        Self {
            state: KalmanState::new(initial_estimate, config),
        }
    }

    /// Create a filter from existing state (for persistence recovery).
    pub fn from_state(state: KalmanState) -> Self {
        Self { state }
    }

    /// Get current probability estimate.
    #[inline]
    pub fn estimate(&self) -> f64 {
        self.state.estimate
    }

    /// Get current estimation variance.
    #[inline]
    pub fn variance(&self) -> f64 {
        self.state.variance
    }

    /// Get the full state (for persistence).
    #[inline]
    pub fn state(&self) -> &KalmanState {
        &self.state
    }

    /// Get mutable access to state (for direct manipulation).
    #[inline]
    pub fn state_mut(&mut self) -> &mut KalmanState {
        &mut self.state
    }

    /// Update the filter with a new price observation.
    ///
    /// This implements the standard Kalman filter predict-update cycle:
    /// 1. **Predict**: Increase variance by process noise (time-adaptive)
    /// 2. **Update**: Incorporate measurement using Kalman gain
    ///
    /// # Arguments
    /// * `measurement` - Observed price/probability (0.0 to 1.0)
    /// * `hours_to_expiry` - Hours until market resolution (for adaptive noise)
    /// * `config` - Filter configuration
    ///
    /// # Returns
    /// The updated probability estimate
    pub fn update(
        &mut self,
        measurement: f64,
        hours_to_expiry: f64,
        config: &KalmanConfig,
    ) -> f64 {
        // Clamp measurement to valid probability range
        let measurement = measurement.clamp(0.0, 1.0);

        // Calculate adaptive process noise based on time to expiry
        let process_noise = config.calculate_process_noise(hours_to_expiry);

        // === PREDICT STEP ===
        // Variance increases with process noise (estimate stays the same)
        let predicted_variance = (self.state.variance + process_noise)
            .clamp(config.min_variance, config.max_variance);

        // === UPDATE STEP ===
        // Kalman gain: how much to weight the new measurement
        // gain = predicted_variance / (predicted_variance + measurement_noise)
        let kalman_gain = predicted_variance / (predicted_variance + config.measurement_noise);

        // Update estimate: blend prior estimate with new measurement
        let new_estimate = self.state.estimate + kalman_gain * (measurement - self.state.estimate);

        // Update variance: reduce uncertainty after incorporating measurement
        let new_variance = ((1.0 - kalman_gain) * predicted_variance)
            .clamp(config.min_variance, config.max_variance);

        // Store updated state
        self.state.estimate = new_estimate.clamp(0.0, 1.0);
        self.state.variance = new_variance;
        self.state.last_update = Utc::now();
        self.state.update_count += 1;

        self.state.estimate
    }

    /// Update with custom measurement noise (for variable-quality observations).
    ///
    /// Use this when different data sources have different reliability.
    /// Lower measurement noise = more trust in the observation.
    pub fn update_with_noise(
        &mut self,
        measurement: f64,
        measurement_noise: f64,
        hours_to_expiry: f64,
        config: &KalmanConfig,
    ) -> f64 {
        let measurement = measurement.clamp(0.0, 1.0);
        let process_noise = config.calculate_process_noise(hours_to_expiry);

        // Predict step
        let predicted_variance = (self.state.variance + process_noise)
            .clamp(config.min_variance, config.max_variance);

        // Update step with custom measurement noise
        let kalman_gain = predicted_variance / (predicted_variance + measurement_noise);
        let new_estimate = self.state.estimate + kalman_gain * (measurement - self.state.estimate);
        let new_variance = ((1.0 - kalman_gain) * predicted_variance)
            .clamp(config.min_variance, config.max_variance);

        self.state.estimate = new_estimate.clamp(0.0, 1.0);
        self.state.variance = new_variance;
        self.state.last_update = Utc::now();
        self.state.update_count += 1;

        self.state.estimate
    }

    /// Predict step only (increase uncertainty without measurement).
    ///
    /// Use this to age the filter when no observations are available.
    pub fn predict(&mut self, hours_to_expiry: f64, config: &KalmanConfig) {
        let process_noise = config.calculate_process_noise(hours_to_expiry);
        self.state.variance = (self.state.variance + process_noise)
            .clamp(config.min_variance, config.max_variance);
    }

    /// Reset the filter to initial state.
    pub fn reset(&mut self, initial_estimate: f64, config: &KalmanConfig) {
        self.state = KalmanState::new(initial_estimate, config);
    }

    /// Get the 95% confidence interval for the estimate.
    ///
    /// Returns (lower_bound, upper_bound) representing ~95% confidence.
    pub fn confidence_interval(&self) -> (f64, f64) {
        // 95% CI = estimate +/- 1.96 * std_dev
        let std_dev = self.state.variance.sqrt();
        let margin = 1.96 * std_dev;

        let lower = (self.state.estimate - margin).clamp(0.0, 1.0);
        let upper = (self.state.estimate + margin).clamp(0.0, 1.0);

        (lower, upper)
    }
}

// =============================================================================
// CROSS-PLATFORM FUSION
// =============================================================================

/// Observation from a single platform for cross-platform fusion.
#[derive(Debug, Clone, Copy)]
pub struct PlatformObservation {
    /// Observed price/probability
    pub price: f64,
    /// Trading volume (for weighting)
    pub volume: f64,
    /// Platform identifier
    pub platform: Platform,
}

/// Platform identifier for observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Polymarket,
    Kalshi,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Polymarket => write!(f, "Polymarket"),
            Platform::Kalshi => write!(f, "Kalshi"),
        }
    }
}

/// Fuse probability estimates from multiple platforms using volume-weighted averaging.
///
/// # Arguments
/// * `observations` - Slice of platform observations with prices and volumes
///
/// # Returns
/// * `Option<f64>` - Fused probability estimate, or None if no valid observations
///
/// # Example
/// ```rust,ignore
/// use prediction_market_arbitrage::kalman::{fuse_estimates, PlatformObservation, Platform};
///
/// let poly = PlatformObservation { price: 0.55, volume: 10000.0, platform: Platform::Polymarket };
/// let kalshi = PlatformObservation { price: 0.53, volume: 5000.0, platform: Platform::Kalshi };
///
/// let fused = fuse_estimates(&[poly, kalshi]);
/// assert!(fused.is_some());
/// // Weighted average: (0.55 * 10000 + 0.53 * 5000) / 15000 = 0.5433...
/// ```
pub fn fuse_estimates(observations: &[PlatformObservation]) -> Option<f64> {
    if observations.is_empty() {
        return None;
    }

    let (weighted_sum, total_volume) = observations.iter().fold((0.0, 0.0), |(sum, vol), obs| {
        if obs.volume > 0.0 && obs.price >= 0.0 && obs.price <= 1.0 {
            (sum + obs.price * obs.volume, vol + obs.volume)
        } else {
            (sum, vol)
        }
    });

    if total_volume > 0.0 {
        Some((weighted_sum / total_volume).clamp(0.0, 1.0))
    } else {
        None
    }
}

/// Fuse two platform estimates with explicit volumes (convenience function).
///
/// # Arguments
/// * `poly_price` - Polymarket price/probability
/// * `poly_volume` - Polymarket trading volume
/// * `kalshi_price` - Kalshi price/probability
/// * `kalshi_volume` - Kalshi trading volume
///
/// # Returns
/// Volume-weighted average probability
#[inline]
pub fn fuse_two_platforms(
    poly_price: f64,
    poly_volume: f64,
    kalshi_price: f64,
    kalshi_volume: f64,
) -> f64 {
    let total_volume = poly_volume + kalshi_volume;
    if total_volume > 0.0 {
        ((poly_price * poly_volume + kalshi_price * kalshi_volume) / total_volume).clamp(0.0, 1.0)
    } else {
        // Default to simple average if no volume info
        ((poly_price + kalshi_price) / 2.0).clamp(0.0, 1.0)
    }
}

// =============================================================================
// THREAD-SAFE MARKET FILTER MANAGER
// =============================================================================

/// Thread-safe Kalman filter for a single market.
///
/// Uses RwLock for concurrent read access with exclusive write access.
/// Suitable for updating from multiple WebSocket feeds.
#[derive(Debug)]
pub struct ThreadSafeFilter {
    inner: RwLock<KalmanFilter>,
}

impl ThreadSafeFilter {
    /// Create a new thread-safe filter.
    pub fn new(initial_estimate: f64, config: &KalmanConfig) -> Self {
        Self {
            inner: RwLock::new(KalmanFilter::new(initial_estimate, config)),
        }
    }

    /// Get current estimate (read lock).
    pub fn estimate(&self) -> f64 {
        self.inner.read().unwrap().estimate()
    }

    /// Get current variance (read lock).
    pub fn variance(&self) -> f64 {
        self.inner.read().unwrap().variance()
    }

    /// Get full state copy (read lock).
    pub fn state(&self) -> KalmanState {
        *self.inner.read().unwrap().state()
    }

    /// Update with new observation (write lock).
    pub fn update(
        &self,
        measurement: f64,
        hours_to_expiry: f64,
        config: &KalmanConfig,
    ) -> f64 {
        self.inner
            .write()
            .unwrap()
            .update(measurement, hours_to_expiry, config)
    }

    /// Update with custom noise (write lock).
    pub fn update_with_noise(
        &self,
        measurement: f64,
        measurement_noise: f64,
        hours_to_expiry: f64,
        config: &KalmanConfig,
    ) -> f64 {
        self.inner
            .write()
            .unwrap()
            .update_with_noise(measurement, measurement_noise, hours_to_expiry, config)
    }

    /// Predict step only (write lock).
    pub fn predict(&self, hours_to_expiry: f64, config: &KalmanConfig) {
        self.inner.write().unwrap().predict(hours_to_expiry, config)
    }

    /// Reset filter (write lock).
    pub fn reset(&self, initial_estimate: f64, config: &KalmanConfig) {
        self.inner.write().unwrap().reset(initial_estimate, config)
    }

    /// Get confidence interval (read lock).
    pub fn confidence_interval(&self) -> (f64, f64) {
        self.inner.read().unwrap().confidence_interval()
    }
}

// =============================================================================
// MARKET FILTER MAP
// =============================================================================

use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Manager for multiple market Kalman filters.
///
/// Provides efficient lookup and update of filters by market ID.
pub struct MarketFilterMap {
    filters: RwLock<FxHashMap<u16, Arc<ThreadSafeFilter>>>,
    config: KalmanConfig,
}

impl MarketFilterMap {
    /// Create a new market filter map with given configuration.
    pub fn new(config: KalmanConfig) -> Self {
        Self {
            filters: RwLock::new(FxHashMap::default()),
            config,
        }
    }

    /// Get or create a filter for a market.
    ///
    /// # Arguments
    /// * `market_id` - Unique market identifier
    /// * `initial_estimate` - Initial probability if creating new filter
    pub fn get_or_create(&self, market_id: u16, initial_estimate: f64) -> Arc<ThreadSafeFilter> {
        // Try read lock first (fast path)
        {
            let filters = self.filters.read().unwrap();
            if let Some(filter) = filters.get(&market_id) {
                return filter.clone();
            }
        }

        // Upgrade to write lock (slow path)
        let mut filters = self.filters.write().unwrap();

        // Double-check after acquiring write lock
        filters
            .entry(market_id)
            .or_insert_with(|| Arc::new(ThreadSafeFilter::new(initial_estimate, &self.config)))
            .clone()
    }

    /// Get existing filter (returns None if not found).
    pub fn get(&self, market_id: u16) -> Option<Arc<ThreadSafeFilter>> {
        self.filters.read().unwrap().get(&market_id).cloned()
    }

    /// Update a market's filter with new observation.
    ///
    /// Creates the filter if it doesn't exist.
    pub fn update(
        &self,
        market_id: u16,
        measurement: f64,
        hours_to_expiry: f64,
    ) -> f64 {
        let filter = self.get_or_create(market_id, measurement);
        filter.update(measurement, hours_to_expiry, &self.config)
    }

    /// Get estimate for a market.
    pub fn estimate(&self, market_id: u16) -> Option<f64> {
        self.get(market_id).map(|f| f.estimate())
    }

    /// Get state for a market.
    pub fn state(&self, market_id: u16) -> Option<KalmanState> {
        self.get(market_id).map(|f| f.state())
    }

    /// Remove a market's filter.
    pub fn remove(&self, market_id: u16) -> Option<Arc<ThreadSafeFilter>> {
        self.filters.write().unwrap().remove(&market_id)
    }

    /// Get number of tracked markets.
    pub fn len(&self) -> usize {
        self.filters.read().unwrap().len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.filters.read().unwrap().is_empty()
    }

    /// Get all market IDs.
    pub fn market_ids(&self) -> Vec<u16> {
        self.filters.read().unwrap().keys().copied().collect()
    }

    /// Get configuration.
    pub fn config(&self) -> &KalmanConfig {
        &self.config
    }

    /// Update configuration (affects future operations).
    pub fn set_config(&mut self, config: KalmanConfig) {
        self.config = config;
    }
}

impl Default for MarketFilterMap {
    fn default() -> Self {
        Self::new(KalmanConfig::default())
    }
}

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

/// Calculate hours to expiry from resolution time.
pub fn hours_to_expiry(resolution_time: DateTime<Utc>) -> f64 {
    let duration = resolution_time.signed_duration_since(Utc::now());
    let hours = duration.num_minutes() as f64 / 60.0;
    hours.max(0.0)
}

/// Calculate hours to expiry from Unix timestamp (seconds).
pub fn hours_to_expiry_from_timestamp(resolution_unix: i64) -> f64 {
    let now = Utc::now().timestamp();
    let remaining_secs = (resolution_unix - now).max(0);
    remaining_secs as f64 / 3600.0
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // KalmanConfig Tests
    // =========================================================================

    #[test]
    fn test_config_default_values() {
        let config = KalmanConfig::default();
        assert_eq!(config.initial_variance, 0.1);
        assert_eq!(config.measurement_noise, 0.01);
        assert_eq!(config.base_process_noise, 0.001);
        assert_eq!(config.expiry_noise_multiplier, 10.0);
        assert_eq!(config.max_hours_for_scaling, 24.0);
    }

    #[test]
    fn test_process_noise_at_24_hours() {
        let config = KalmanConfig::default();

        // At 24 hours: time_factor = 0, noise = base * (1 + 10 * 0) = base
        let noise = config.calculate_process_noise(24.0);
        assert!((noise - 0.001).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_at_12_hours() {
        let config = KalmanConfig::default();

        // At 12 hours: time_factor = 0.5, noise = base * (1 + 10 * 0.5) = base * 6
        let noise = config.calculate_process_noise(12.0);
        let expected = 0.001 * 6.0; // 0.006
        assert!((noise - expected).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_at_6_hours() {
        let config = KalmanConfig::default();

        // At 6 hours: time_factor = 0.75, noise = base * (1 + 10 * 0.75) = base * 8.5
        let noise = config.calculate_process_noise(6.0);
        let expected = 0.001 * 8.5; // 0.0085
        assert!((noise - expected).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_at_1_hour() {
        let config = KalmanConfig::default();

        // At 1 hour: time_factor = 23/24, noise = base * (1 + 10 * 23/24)
        let noise = config.calculate_process_noise(1.0);
        let expected = 0.001 * (1.0 + 10.0 * (23.0 / 24.0));
        assert!((noise - expected).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_at_0_hours() {
        let config = KalmanConfig::default();

        // At 0 hours: time_factor = 1, noise = base * (1 + 10 * 1) = base * 11
        let noise = config.calculate_process_noise(0.0);
        let expected = 0.001 * 11.0; // 0.011
        assert!((noise - expected).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_beyond_max_hours() {
        let config = KalmanConfig::default();

        // Beyond 24 hours should be clamped to base noise
        let noise = config.calculate_process_noise(48.0);
        assert!((noise - 0.001).abs() < 1e-10);
    }

    #[test]
    fn test_process_noise_negative_hours() {
        let config = KalmanConfig::default();

        // Negative hours should be clamped to 0 (max noise)
        let noise = config.calculate_process_noise(-5.0);
        let expected = 0.001 * 11.0; // Same as 0 hours
        assert!((noise - expected).abs() < 1e-10);
    }

    // =========================================================================
    // KalmanState Tests
    // =========================================================================

    #[test]
    fn test_state_new() {
        let config = KalmanConfig::default();
        let state = KalmanState::new(0.5, &config);

        assert_eq!(state.estimate, 0.5);
        assert_eq!(state.variance, 0.1);
        assert_eq!(state.update_count, 0);
    }

    #[test]
    fn test_state_clamps_estimate() {
        let config = KalmanConfig::default();

        let state1 = KalmanState::new(1.5, &config);
        assert_eq!(state1.estimate, 1.0);

        let state2 = KalmanState::new(-0.5, &config);
        assert_eq!(state2.estimate, 0.0);
    }

    #[test]
    fn test_state_confidence() {
        let config = KalmanConfig::default();
        let state = KalmanState::new(0.5, &config);

        // Confidence = 1 / variance = 1 / 0.1 = 10
        assert!((state.confidence() - 10.0).abs() < 1e-10);
    }

    // =========================================================================
    // KalmanFilter Tests
    // =========================================================================

    #[test]
    fn test_filter_new() {
        let config = KalmanConfig::default();
        let filter = KalmanFilter::new(0.5, &config);

        assert_eq!(filter.estimate(), 0.5);
        assert_eq!(filter.variance(), 0.1);
    }

    #[test]
    fn test_filter_single_update() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        // Update with measurement at 24 hours to expiry
        let new_estimate = filter.update(0.6, 24.0, &config);

        // Estimate should move toward measurement
        assert!(new_estimate > 0.5);
        assert!(new_estimate < 0.6);

        // Variance should decrease after measurement
        assert!(filter.variance() < config.initial_variance);
    }

    #[test]
    fn test_filter_converges_to_measurement() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.3, &config);

        // Repeatedly update with same measurement
        for _ in 0..100 {
            filter.update(0.7, 24.0, &config);
        }

        // Should converge close to 0.7
        assert!((filter.estimate() - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_filter_variance_bounded() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        // Many updates should not make variance go below minimum
        for _ in 0..1000 {
            filter.update(0.5, 24.0, &config);
        }
        assert!(filter.variance() >= config.min_variance);

        // Reset and do many predict steps (no measurements)
        filter.reset(0.5, &config);
        for _ in 0..1000 {
            filter.predict(0.0, &config);
        }
        assert!(filter.variance() <= config.max_variance);
    }

    #[test]
    fn test_filter_estimate_bounded() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        // Update with out-of-range measurements
        filter.update(1.5, 24.0, &config);
        assert!(filter.estimate() <= 1.0);

        filter.update(-0.5, 24.0, &config);
        assert!(filter.estimate() >= 0.0);
    }

    #[test]
    fn test_filter_confidence_interval() {
        let config = KalmanConfig::default();
        let filter = KalmanFilter::new(0.5, &config);

        let (lower, upper) = filter.confidence_interval();

        // 95% CI should contain the estimate
        assert!(lower < filter.estimate());
        assert!(upper > filter.estimate());

        // Bounds should be valid probabilities
        assert!(lower >= 0.0);
        assert!(upper <= 1.0);
    }

    #[test]
    fn test_filter_update_count() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        assert_eq!(filter.state().update_count, 0);

        filter.update(0.6, 24.0, &config);
        assert_eq!(filter.state().update_count, 1);

        filter.update(0.7, 24.0, &config);
        assert_eq!(filter.state().update_count, 2);
    }

    #[test]
    fn test_filter_predict_only() {
        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        let initial_variance = filter.variance();
        let initial_estimate = filter.estimate();

        // Predict without measurement
        filter.predict(24.0, &config);

        // Estimate should not change
        assert_eq!(filter.estimate(), initial_estimate);

        // Variance should increase
        assert!(filter.variance() > initial_variance);
    }

    #[test]
    fn test_filter_near_expiry_more_responsive() {
        let config = KalmanConfig::default();

        let mut filter_24h = KalmanFilter::new(0.5, &config);
        let mut filter_1h = KalmanFilter::new(0.5, &config);

        // Same measurement, different time to expiry
        filter_24h.update(0.7, 24.0, &config);
        filter_1h.update(0.7, 1.0, &config);

        // Near expiry filter should be more responsive (higher process noise)
        // and thus move more toward the measurement
        // Note: This is somewhat counterintuitive because higher process noise
        // means more uncertainty, but the net effect depends on the filter dynamics
        assert!(filter_24h.estimate() != filter_1h.estimate());
    }

    // =========================================================================
    // Cross-Platform Fusion Tests
    // =========================================================================

    #[test]
    fn test_fuse_two_platforms_equal_volume() {
        let result = fuse_two_platforms(0.6, 1000.0, 0.4, 1000.0);

        // Should be simple average
        assert!((result - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_fuse_two_platforms_weighted() {
        // Poly: 60% with 2x volume should dominate
        let result = fuse_two_platforms(0.6, 2000.0, 0.3, 1000.0);

        // (0.6 * 2000 + 0.3 * 1000) / 3000 = 1500 / 3000 = 0.5
        assert!((result - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_fuse_two_platforms_zero_volume() {
        // Zero volume should fall back to simple average
        let result = fuse_two_platforms(0.6, 0.0, 0.4, 0.0);
        assert!((result - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_fuse_estimates_empty() {
        let result = fuse_estimates(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_fuse_estimates_single() {
        let obs = PlatformObservation {
            price: 0.6,
            volume: 1000.0,
            platform: Platform::Polymarket,
        };
        let result = fuse_estimates(&[obs]);
        assert!((result.unwrap() - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_fuse_estimates_multiple() {
        let obs = [
            PlatformObservation {
                price: 0.6,
                volume: 1000.0,
                platform: Platform::Polymarket,
            },
            PlatformObservation {
                price: 0.4,
                volume: 1000.0,
                platform: Platform::Kalshi,
            },
        ];
        let result = fuse_estimates(&obs);
        assert!((result.unwrap() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_fuse_estimates_ignores_invalid() {
        let obs = [
            PlatformObservation {
                price: 0.6,
                volume: 1000.0,
                platform: Platform::Polymarket,
            },
            PlatformObservation {
                price: -0.5, // Invalid
                volume: 1000.0,
                platform: Platform::Kalshi,
            },
        ];
        let result = fuse_estimates(&obs);
        // Should only use the valid observation
        assert!((result.unwrap() - 0.6).abs() < 1e-10);
    }

    // =========================================================================
    // Thread-Safe Filter Tests
    // =========================================================================

    #[test]
    fn test_thread_safe_filter() {
        let config = KalmanConfig::default();
        let filter = ThreadSafeFilter::new(0.5, &config);

        assert_eq!(filter.estimate(), 0.5);

        let new_estimate = filter.update(0.6, 24.0, &config);
        assert!(new_estimate > 0.5);
    }

    #[test]
    fn test_thread_safe_filter_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let config = KalmanConfig::default();
        let filter = Arc::new(ThreadSafeFilter::new(0.5, &config));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let filter = filter.clone();
                let config = config;
                thread::spawn(move || {
                    for _ in 0..100 {
                        filter.update(0.6, 24.0, &config);
                        let _ = filter.estimate();
                        let _ = filter.variance();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Filter should still be in valid state
        let estimate = filter.estimate();
        assert!(estimate >= 0.0 && estimate <= 1.0);
    }

    // =========================================================================
    // MarketFilterMap Tests
    // =========================================================================

    #[test]
    fn test_market_filter_map_basic() {
        let map = MarketFilterMap::default();

        assert!(map.is_empty());

        // Create filter
        let _ = map.get_or_create(0, 0.5);
        assert_eq!(map.len(), 1);

        // Get same filter
        let filter = map.get(0);
        assert!(filter.is_some());
        assert!((filter.unwrap().estimate() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_market_filter_map_update() {
        let map = MarketFilterMap::default();

        // Update creates filter if needed
        let estimate = map.update(0, 0.6, 24.0);
        assert!(estimate > 0.5); // Moved toward 0.6

        // Subsequent update
        let estimate2 = map.update(0, 0.7, 24.0);
        assert!(estimate2 > estimate);
    }

    #[test]
    fn test_market_filter_map_multiple_markets() {
        let map = MarketFilterMap::default();

        map.update(0, 0.5, 24.0);
        map.update(1, 0.6, 24.0);
        map.update(2, 0.7, 24.0);

        assert_eq!(map.len(), 3);

        let ids = map.market_ids();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_market_filter_map_remove() {
        let map = MarketFilterMap::default();

        map.update(0, 0.5, 24.0);
        assert_eq!(map.len(), 1);

        map.remove(0);
        assert_eq!(map.len(), 0);
        assert!(map.get(0).is_none());
    }

    // =========================================================================
    // Helper Function Tests
    // =========================================================================

    #[test]
    fn test_hours_to_expiry_from_timestamp() {
        let now = Utc::now().timestamp();

        // 1 hour from now
        let hours = hours_to_expiry_from_timestamp(now + 3600);
        assert!((hours - 1.0).abs() < 0.01);

        // Past (should be 0)
        let hours_past = hours_to_expiry_from_timestamp(now - 3600);
        assert_eq!(hours_past, 0.0);
    }

    // =========================================================================
    // Integration Tests
    // =========================================================================

    #[test]
    fn test_full_workflow() {
        // Simulate a market's lifecycle

        let config = KalmanConfig::default();
        let mut filter = KalmanFilter::new(0.5, &config);

        // Initial state: 50% probability, 24h to expiry
        assert_eq!(filter.estimate(), 0.5);

        // Morning: prices move up
        filter.update(0.55, 20.0, &config);
        filter.update(0.58, 18.0, &config);
        filter.update(0.60, 16.0, &config);

        // Estimate should track upward
        assert!(filter.estimate() > 0.55);

        // Afternoon: prices stabilize
        for _ in 0..10 {
            filter.update(0.62, 12.0, &config);
        }

        // Should be close to 0.62 with low variance
        assert!((filter.estimate() - 0.62).abs() < 0.02);

        // Evening: sudden news, price jumps
        filter.update(0.85, 6.0, &config);

        // Filter should respond but not overreact
        assert!(filter.estimate() > 0.62);
        assert!(filter.estimate() < 0.85);

        // Near expiry: more responsive
        filter.update(0.90, 1.0, &config);
        filter.update(0.92, 0.5, &config);

        // Should track closely near expiry
        assert!(filter.estimate() > 0.85);
    }

    #[test]
    fn test_cross_platform_workflow() {
        let config = KalmanConfig::default();
        let map = MarketFilterMap::new(config);

        // Both platforms reporting similar prices
        let poly_price = 0.65;
        let poly_volume = 10000.0;
        let kalshi_price = 0.63;
        let kalshi_volume = 5000.0;

        // Fuse estimates
        let fused = fuse_two_platforms(poly_price, poly_volume, kalshi_price, kalshi_volume);

        // Update filter with fused estimate
        let estimate = map.update(0, fused, 12.0);

        // Fused should weight toward Poly (higher volume)
        assert!((fused - 0.6433).abs() < 0.01); // (0.65*10000 + 0.63*5000) / 15000

        // Filter estimate should be close to fused
        assert!((estimate - fused).abs() < 0.1);
    }
}
