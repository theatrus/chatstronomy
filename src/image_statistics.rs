//! LIGHT-frame measurements used for an observing-session history chart.

use chrono::{DateTime, Utc};

/// A captured LIGHT frame's image-quality measurements.
///
/// Missing measurements are kept as gaps in the chart rather than being
/// converted to zero. Zero detected stars is a valid measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageStatisticsPoint {
    pub captured_at: DateTime<Utc>,
    /// True when `captured_at` is the exposure's start rather than its save time.
    pub exposure_start_known: bool,
    pub hfr: Option<f64>,
    pub stars: Option<i32>,
    pub filter: String,
}
