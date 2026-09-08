use crate::autofocus::AutofocusResponse;
use crate::camera::CameraInfo;
use crate::chat::{ChatAttachment, ChatField, ChatMessage, ChatServiceManager, ChatTarget};
use crate::discord::colors;
use crate::events::{
    Event, EventDeliveryScope, EventDetails, FilterInfo, TargetCoordinates, WeatherConditions,
    event_delivery_scope, event_types,
};
use crate::images::ImageMetadata;
use crate::sequence::{
    PlateSolveOutput, SequenceOperation, SequenceOperationKind, SequenceResponse,
    extract_current_target_with_delivery, extract_meridian_flip_time, extract_sequence_operations,
    extract_suppressed_sequence_operation_keys, meridian_flip_time_formatted_with_clock,
};
use crate::source::{RigSourceError, SharedRigSource};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::Instant as TokioInstant;
use tokio::time::sleep;

/// Default first-retry wait when a telescope is unreachable at startup. A rig
/// that's powered off (or whose plugin is not connected yet) should not kill
/// its monitoring task — we keep re-checking until it comes back, starting
/// here and backing off exponentially.
pub(crate) const DEFAULT_RECONNECT_INITIAL: Duration = Duration::from_secs(60);
/// Default ceiling for the exponential reconnect backoff.
pub(crate) const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(600);

/// Number of consecutive failed poll cycles before we treat a telescope as
/// offline and post a chat alert. Debounces against a single transient blip;
/// because the loop already backs off after the first failure, these cycles
/// are spaced out (≈60s, then 120s, …), so a small count still means minutes.
const OFFLINE_FAILURE_THRESHOLD: u32 = 3;

/// Autofocus completion is a two-step Direct operation: N.I.N.A. first emits
/// `AUTOFOCUS-FINISHED`, then Chatstronomy asks for the saved report used to
/// render the graph. A brief transport or report-read failure must not consume
/// the completion forever. Report reads are queued behind the normal poll
/// cycle so they cannot contend with event, sequence, image, or status reads;
/// only graph rendering and chat delivery run in an updater-owned task.
const DEFAULT_AUTOFOCUS_RETRY: AutofocusRetryPolicy = AutofocusRetryPolicy {
    max_attempts: 5,
    initial_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(8),
    not_ready_timeout: Duration::from_secs(120),
    overall_timeout: Duration::from_secs(120),
};

#[derive(Debug, Clone, Copy)]
struct AutofocusRetryPolicy {
    max_attempts: usize,
    initial_delay: Duration,
    max_delay: Duration,
    /// `resource_not_ready` is a healthy response from N.I.N.A., so it does
    /// not consume the ordinary malformed/rejected-response budget. Bound
    /// that special treatment by elapsed time so a report that is never
    /// published cannot remain queued forever.
    not_ready_timeout: Duration,
    /// Absolute lifetime of a queued completion across every failure class.
    /// Transport outages do not consume `max_attempts`, but they must not keep
    /// a stale report read (or a repeatedly timing-out endpoint) alive forever.
    overall_timeout: Duration,
}

/// Double `current`, capped at `max` — but never below `initial`, so a
/// misconfigured `max < initial` can't shrink the wait. Shared by the startup
/// baseline retry and the mid-run reconnect loop so both back off identically.
fn backoff_delay(current: Duration, initial: Duration, max: Duration) -> Duration {
    (current * 2).min(max.max(initial))
}

fn completed_milestone(progress: u8) -> u8 {
    [75, 50, 25]
        .into_iter()
        .find(|milestone| progress >= *milestone)
        .unwrap_or(0)
}

fn format_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_utc_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn add_wait_endpoint_field(message: ChatMessage, label: &str, end: DateTime<Utc>) -> ChatMessage {
    let until = format_utc_timestamp(end);
    let discord_until = format!("<t:{}:F>", end.timestamp());
    message.field_with_discord_value(label, &until, &discord_until, false)
}

/// Add one scheduled endpoint in a form each chat adapter can render well.
/// Discord receives native timestamp markup, so its relative value counts down
/// in the client without polling or editing a message. Matrix receives only
/// canonical UTC text and a remaining-duration snapshot.
fn add_wait_timing_fields(
    message: ChatMessage,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> ChatMessage {
    let (countdown, discord_countdown) = if end > now {
        (
            format!(
                "{} remaining",
                format_duration(end.signed_duration_since(now))
            ),
            format!("<t:{}:R>", end.timestamp()),
        )
    } else {
        (
            "Scheduled time reached".to_string(),
            "Scheduled time reached".to_string(),
        )
    };
    add_wait_endpoint_field(message, "Until", end).field_with_discord_value(
        "Countdown",
        &countdown,
        &discord_countdown,
        true,
    )
}

#[cfg(test)]
mod wait_timing_tests {
    use super::*;

    #[test]
    fn wait_fields_normalize_offsets_and_include_a_discord_live_countdown() {
        let started = DateTime::parse_from_rfc3339("2026-09-02T01:55:00.6436653+00:00")
            .expect("valid start")
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-09-01T21:25:51.5404816-05:00")
            .expect("valid end")
            .with_timezone(&Utc);

        let message = add_wait_timing_fields(ChatMessage::new("Wait"), end, started);
        let until = message
            .fields
            .iter()
            .find(|field| field.name == "Until")
            .expect("Until field");
        let countdown = message
            .fields
            .iter()
            .find(|field| field.name == "Countdown")
            .expect("Countdown field");

        assert_eq!(until.value, "2026-09-02 02:25:51 UTC");
        assert_eq!(
            until.discord_value.as_deref(),
            Some(format!("<t:{}:F>", end.timestamp()).as_str())
        );
        assert_eq!(countdown.value, "30m 50s remaining");
        assert_eq!(
            countdown.discord_value.as_deref(),
            Some(format!("<t:{}:R>", end.timestamp()).as_str())
        );
    }

    #[test]
    fn elapsed_wait_is_described_truthfully() {
        let end = DateTime::parse_from_rfc3339("2026-09-02T02:25:51Z")
            .expect("valid end")
            .with_timezone(&Utc);
        let now = end + chrono::Duration::seconds(1);
        let message = add_wait_timing_fields(ChatMessage::new("Wait"), end, now);
        let countdown = message
            .fields
            .iter()
            .find(|field| field.name == "Countdown")
            .expect("Countdown field");

        assert_eq!(countdown.value, "Scheduled time reached");
        assert_eq!(
            countdown.discord_value.as_deref(),
            Some("Scheduled time reached")
        );
        assert!(!countdown.value.to_ascii_lowercase().contains("success"));
    }
}

/// Information about the current observation target
#[derive(Debug, Clone)]
struct TargetInfo {
    name: String,
    source: TargetSource,
    coordinates: Option<TargetCoordinates>,
    project: Option<String>,
    rotation: Option<f64>,
    target_end_time: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, PartialEq)]
enum TargetSource {
    Sequence,
    TsTargetStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SafetyState {
    Unknown,
    ConnectedUnknown,
    Safe,
    Unsafe,
    Disconnected,
}

impl SafetyState {
    fn status_text(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::ConnectedUnknown => Some("🛡️ Safety monitor connected; state unknown"),
            Self::Safe => Some("🛡️ Conditions safe"),
            Self::Unsafe => Some("⚠️ Conditions unsafe"),
            Self::Disconnected => Some("⚠️ Safety monitor disconnected"),
        }
    }
}

fn has_usable_weather_details(event: &Event) -> bool {
    match (event.event.as_str(), &event.details) {
        (event_types::WEATHER_CHANGED, Some(EventDetails::WeatherChanged { conditions, .. })) => {
            !conditions.is_empty()
        }
        (
            event_types::WEATHER_HIGH_WIND,
            Some(EventDetails::WeatherHighWind { conditions, .. }),
        ) => conditions.has_wind_reading(),
        (event_types::WEATHER_CHANGED | event_types::WEATHER_HIGH_WIND, _) => false,
        _ => true,
    }
}

impl WeatherConditions {
    /// High-wind transitions intentionally carry only wind readings. Merge
    /// those available values into the last full WEATHER-CHANGED snapshot so
    /// temperature/cloud state is not erased by an alert edge.
    fn merge_available(&mut self, update: &Self) {
        macro_rules! replace_some {
            ($field:ident) => {
                if update.$field.is_some() {
                    self.$field = update.$field;
                }
            };
        }
        replace_some!(wind_speed_meters_per_second);
        replace_some!(wind_gust_meters_per_second);
        replace_some!(wind_direction_degrees);
        replace_some!(temperature_celsius);
        replace_some!(dew_point_celsius);
        replace_some!(humidity_percent);
        replace_some!(pressure_hectopascals);
        replace_some!(cloud_cover_percent);
        replace_some!(rain_rate_millimeters_per_hour);
        replace_some!(sky_temperature_celsius);
        replace_some!(sky_brightness_lux);
        replace_some!(sky_quality_magnitudes_per_square_arcsecond);
        replace_some!(star_fwhm_arcseconds);
    }

    /// Compact Discord groups with units in every value. Grouping keeps a
    /// complete snapshot below Discord's field-count limit.
    fn chat_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = Vec::new();

        let mut wind = Vec::new();
        if let Some(value) = self.wind_speed_meters_per_second {
            wind.push(format!("{value:.1} m/s speed"));
        }
        if let Some(value) = self.wind_gust_meters_per_second {
            wind.push(format!("{value:.1} m/s gust"));
        }
        if let Some(value) = self.wind_direction_degrees {
            wind.push(format!("{value:.0}°"));
        }
        if !wind.is_empty() {
            fields.push(("Wind", wind.join(" · ")));
        }

        let mut atmosphere = Vec::new();
        if let Some(value) = self.temperature_celsius {
            atmosphere.push(format!("{value:.1} °C"));
        }
        if let Some(value) = self.dew_point_celsius {
            atmosphere.push(format!("dew point {value:.1} °C"));
        }
        if let Some(value) = self.humidity_percent {
            atmosphere.push(format!("{value:.0}% RH"));
        }
        if let Some(value) = self.pressure_hectopascals {
            atmosphere.push(format!("{value:.1} hPa"));
        }
        if !atmosphere.is_empty() {
            fields.push(("Atmosphere", atmosphere.join(" · ")));
        }

        let mut sky = Vec::new();
        if let Some(value) = self.cloud_cover_percent {
            sky.push(format!("{value:.0}% cloud"));
        }
        if let Some(value) = self.sky_temperature_celsius {
            sky.push(format!("sky {value:.1} °C"));
        }
        if let Some(value) = self.sky_brightness_lux {
            sky.push(format!("{value:.2} lux"));
        }
        if let Some(value) = self.sky_quality_magnitudes_per_square_arcsecond {
            sky.push(format!("{value:.2} mag/arcsec²"));
        }
        if let Some(value) = self.star_fwhm_arcseconds {
            sky.push(format!("{value:.2}″ FWHM"));
        }
        if !sky.is_empty() {
            fields.push(("Sky", sky.join(" · ")));
        }

        if let Some(value) = self.rain_rate_millimeters_per_hour {
            fields.push(("Rain", format!("{value:.2} mm/h")));
        }
        fields
    }

    fn status_summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(value) = self.wind_speed_meters_per_second {
            parts.push(format!("wind {value:.1} m/s"));
        }
        if let Some(value) = self.wind_gust_meters_per_second {
            parts.push(format!("gust {value:.1} m/s"));
        }
        if let Some(value) = self.temperature_celsius {
            parts.push(format!("{value:.1} °C"));
        }
        if let Some(value) = self.humidity_percent {
            parts.push(format!("{value:.0}% RH"));
        }
        if let Some(value) = self.cloud_cover_percent {
            parts.push(format!("{value:.0}% cloud"));
        }
        if let Some(value) = self.rain_rate_millimeters_per_hour {
            parts.push(format!("rain {value:.2} mm/h"));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceFailureInfo {
    entity: String,
    entity_type: String,
    error: String,
}

#[derive(Debug, Clone)]
struct TrackedSequenceOperation {
    operation: SequenceOperation,
    started_at: DateTime<Utc>,
    estimated_end: Option<DateTime<Utc>>,
    initial_temperature: Option<f64>,
    camera: Option<CameraInfo>,
    last_milestone: u8,
    last_output_key: Option<String>,
}

impl TrackedSequenceOperation {
    fn new(operation: SequenceOperation, now: DateTime<Utc>, camera: Option<CameraInfo>) -> Self {
        let estimated_end = match &operation.kind {
            SequenceOperationKind::TimeWait {
                target_time: Some(target),
                ..
            } => Some(target.with_timezone(&Utc)),
            SequenceOperationKind::TimeWait {
                configured_duration: Some(duration),
                ..
            } => Some(now + *duration),
            SequenceOperationKind::CameraWarming {
                minimum_duration: Some(duration),
            } => Some(now + *duration),
            _ => None,
        };
        let initial_temperature = camera
            .as_ref()
            .map(|info| info.temperature)
            .filter(|value| value.is_finite());
        Self {
            operation,
            started_at: now,
            estimated_end,
            initial_temperature,
            camera,
            last_milestone: 0,
            last_output_key: None,
        }
    }

    fn restore_intrinsic_wait_estimate(&mut self) {
        self.estimated_end = match &self.operation.kind {
            SequenceOperationKind::TimeWait {
                target_time: Some(target),
                ..
            } => Some(target.with_timezone(&Utc)),
            SequenceOperationKind::TimeWait {
                configured_duration: Some(duration),
                ..
            } => Some(self.started_at + *duration),
            _ => None,
        };
    }

    fn progress_percent(&self, now: DateTime<Utc>) -> Option<u8> {
        match &self.operation.kind {
            SequenceOperationKind::TimeWait { .. } => {
                let end = self.estimated_end?;
                let total = end
                    .signed_duration_since(self.started_at)
                    .num_milliseconds();
                if total <= 0 {
                    return Some(100);
                }
                let elapsed = now
                    .signed_duration_since(self.started_at)
                    .num_milliseconds()
                    .clamp(0, total);
                Some(((elapsed as f64 / total as f64) * 100.0).round() as u8)
            }
            SequenceOperationKind::CameraCooling {
                target_temperature, ..
            } => {
                let initial = self.initial_temperature?;
                let camera = self.camera.as_ref()?;
                if camera.at_target_temp {
                    return Some(100);
                }
                let total = (initial - target_temperature).abs();
                if total < 0.1 || !camera.temperature.is_finite() {
                    return None;
                }
                let remaining = (camera.temperature - target_temperature).abs();
                Some(((1.0 - remaining / total).clamp(0.0, 1.0) * 100.0).round() as u8)
            }
            SequenceOperationKind::CameraWarming { minimum_duration } => {
                let end = self
                    .estimated_end
                    .or_else(|| minimum_duration.map(|duration| self.started_at + duration))?;
                let total = end
                    .signed_duration_since(self.started_at)
                    .num_milliseconds();
                if total <= 0 {
                    return Some(100);
                }
                let elapsed = now
                    .signed_duration_since(self.started_at)
                    .num_milliseconds()
                    .clamp(0, total);
                Some(((elapsed as f64 / total as f64) * 100.0).round() as u8)
            }
            SequenceOperationKind::MountSlew { .. }
            | SequenceOperationKind::MountCenter { .. }
            | SequenceOperationKind::PlateSolve { .. }
            | SequenceOperationKind::AstronomicalWait { .. } => None,
            SequenceOperationKind::SafetyWait { .. }
            | SequenceOperationKind::ConditionWait { .. }
            | SequenceOperationKind::ManualWait => None,
        }
    }

    fn next_milestone(&self, now: DateTime<Utc>) -> Option<u8> {
        let progress = self.progress_percent(now)?;
        [75, 50, 25]
            .into_iter()
            .find(|milestone| progress >= *milestone && self.last_milestone < *milestone)
    }
}

fn operation_plate_solve_output(kind: &SequenceOperationKind) -> Option<&PlateSolveOutput> {
    match kind {
        SequenceOperationKind::MountCenter {
            output: Some(output),
            ..
        }
        | SequenceOperationKind::PlateSolve {
            output: Some(output),
            ..
        } => Some(output),
        _ => None,
    }
}

fn add_plate_solve_output_fields(
    mut message: ChatMessage,
    output: &PlateSolveOutput,
) -> ChatMessage {
    if let Some(success) = output.success {
        message = message.field(
            "Plate solve",
            if success { "Succeeded" } else { "Failed" },
            true,
        );
    }
    if let Some(coordinates) = &output.coordinates {
        message = message.field("Solved position", &coordinates.display(), false);
    }
    if let Some(angle) = output.position_angle {
        message = message.field("Position angle", &format!("{angle:.2}°"), true);
    }
    if let Some(scale) = output.pixel_scale {
        message = message.field("Image scale", &format!("{scale:.2} arcsec/px"), true);
    }
    if let Some(radius) = output.radius_degrees {
        message = message.field("Solve radius", &format!("{radius:.2}°"), true);
    }
    if let Some(separation) = output.separation_arcseconds {
        message = message.field("Pointing error", &format!("{separation:.1} arcsec"), true);
    }
    if output.ra_error.is_some() || output.dec_error.is_some() {
        message = message.field(
            "Axis error",
            &format!(
                "RA {} · Dec {}",
                output.ra_error.as_deref().unwrap_or("--"),
                output.dec_error.as_deref().unwrap_or("--")
            ),
            false,
        );
    }
    if output.ra_pixel_error.is_some() || output.dec_pixel_error.is_some() {
        message = message.field(
            "Pixel error",
            &format!(
                "RA {} · Dec {}",
                output
                    .ra_pixel_error
                    .map(|value| format!("{value:.2} px"))
                    .unwrap_or_else(|| "--".to_string()),
                output
                    .dec_pixel_error
                    .map(|value| format!("{value:.2} px"))
                    .unwrap_or_else(|| "--".to_string())
            ),
            false,
        );
    }
    if output.flipped == Some(true) {
        message = message.field("Orientation", "Flipped", true);
    }
    message
}

fn plate_solve_output_key(hasher: &RandomState, operation: &SequenceOperation) -> Option<String> {
    let output = operation_plate_solve_output(&operation.kind)?;
    Some(format!(
        "p:{:016x}",
        hasher.hash_one((
            &output.solve_time,
            output.success.map(|value| value as u8),
            output.position_angle.map(f64::to_bits),
            output.separation_arcseconds.map(f64::to_bits),
            output.thumbnail.as_ref().map(Vec::len),
        ))
    ))
}

fn promote_ambiguous_slew_to_center(operation: &mut SequenceOperation) -> bool {
    let SequenceOperationKind::MountSlew {
        coordinates,
        may_be_center: true,
    } = &operation.kind
    else {
        return false;
    };
    operation.kind = SequenceOperationKind::MountCenter {
        coordinates: coordinates.clone(),
        rotation: None,
        output: None,
    };
    true
}

#[derive(Debug, Clone, Copy)]
enum OperationUpdate {
    Started,
    Progress(u8),
    Output,
    Finished { attach_output: bool },
    Ended { attach_output: bool },
    Failed { attach_output: bool },
}

/// Insert-only dedup set with a bounded memory footprint.
///
/// Enabled-event keys embed payload text — for `NINA-LOG` events, a whole log
/// line — and a local updater lives for the entire N.I.N.A. session, so an
/// unbounded set grows all night. Disabled records are never inserted.
/// Evicting the oldest key can at worst re-announce something older than the
/// whole retained window, which the source histories have dropped long before.
/// Eviction is least-recently-*seen*, not insertion order: a key that is still
/// present in the source history gets re-observed on every poll, and dropping
/// it would re-announce an event the user has already been told about. Ordering
/// by a monotonic sequence number keeps both the touch and the eviction
/// logarithmic, which matters when a single poll re-checks thousands of keys.
#[derive(Debug)]
struct BoundedSeenSet {
    seen: HashMap<String, u64>,
    order: BTreeMap<u64, String>,
    next_seq: u64,
    capacity: usize,
}

impl BoundedSeenSet {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashMap::new(),
            order: BTreeMap::new(),
            next_seq: 0,
            capacity: capacity.max(1),
        }
    }

    /// Record `key`, returning true when it had already been recorded. A
    /// repeat sighting refreshes the key's position so it outlives keys that
    /// have genuinely fallen out of the source history.
    fn check_and_insert(&mut self, key: String) -> bool {
        let seq = self.next_seq;
        self.next_seq += 1;

        if let Some(previous) = self.seen.insert(key.clone(), seq) {
            self.order.remove(&previous);
            self.order.insert(seq, key);
            return true;
        }

        self.order.insert(seq, key);
        while self.seen.len() > self.capacity {
            let Some((_, evicted)) = self.order.pop_first() else {
                break;
            };
            self.seen.remove(&evicted);
        }
        false
    }

    /// Record a key without caring whether it was already present.
    fn insert(&mut self, key: String) {
        self.check_and_insert(key);
    }

    fn len(&self) -> usize {
        self.seen.len()
    }
}

/// Claim a plate-solve output for delivery, returning true the first time it is
/// seen. An operation whose delivery is switched off must not claim a key:
/// doing so burns it, so the image could never be posted if the user
/// re-enabled that category before the next solve.
///
/// Takes the set directly rather than `&mut UpdaterState` so callers can hold a
/// mutable borrow of a sibling field at the same time.
fn claim_plate_solve_output(
    seen: &mut BoundedSeenSet,
    chat_enabled: bool,
    key: Option<&String>,
) -> bool {
    if !chat_enabled {
        return false;
    }
    key.is_some_and(|key| !seen.check_and_insert(key.clone()))
}

/// Ceiling for the event and image dedup sets. Comfortably above the largest
/// history either side returns in one poll, so nothing is re-announced while
/// it is still visible in the source history.
const SEEN_SET_CAPACITY: usize = 20_000;

#[derive(Debug, Clone, Copy)]
struct SchedulerWaitState {
    end_at: DateTime<Utc>,
}

/// State management for the chat updater
struct UpdaterState {
    /// Per-updater keyed hasher for dedup identities. History keys must never
    /// retain event details, target/filter labels, or camera names in memory.
    dedup_hasher: RandomState,
    events_seen: BoundedSeenSet,
    /// Privacy-policy tombstones from legacy payload-v3 peers. Keys contain
    /// only the delivery scope and event timestamp, never disabled details.
    disabled_event_tombstones: BoundedSeenSet,
    images_seen: BoundedSeenSet,
    current_target: Option<TargetInfo>,
    meridian_flip_time: Option<f64>,
    /// Only the chat-visible aggregate needed by sequence notifications. The
    /// raw snapshot may contain legacy `ChatEnabled: false` nodes and is never
    /// retained.
    sequence_container_counts: Option<(usize, usize)>,
    last_image_time: Option<Instant>,
    skipped_images_count: u32,
    last_filter: Option<FilterInfo>,
    /// Latest mount-state event we've observed (PARKED, UNPARKED, HOMED, etc.).
    last_mount_event: Option<String>,
    /// Latest guider-state event we've observed (START, STOP, DITHER).
    last_guider_event: Option<String>,
    /// True if the last sequence event was STARTING (not FINISHED).
    sequence_running: bool,
    /// Failure from the current or most recently ended sequence. Cleared by a
    /// subsequent sequence start so a normal finish is never called success.
    sequence_failure: Option<SequenceFailureInfo>,
    sequence_outcome: Option<String>,
    /// Latest Target Scheduler wait plan, normalized to UTC. Its endpoint is
    /// an estimate, not a completion signal, and is independent from N.I.N.A.
    /// sequence `WaitForTime` operations.
    scheduler_wait: Option<SchedulerWaitState>,
    /// Newest wait or terminal transition applied to scheduler state. This
    /// prevents delayed history from resurrecting or replacing newer state.
    scheduler_wait_latest_at: Option<DateTime<Utc>>,
    safety_state: SafetyState,
    dome_connected: Option<bool>,
    dome_shutter_open: Option<bool>,
    dome_azimuth: Option<f64>,
    dome_parked: Option<bool>,
    dome_homed: Option<bool>,
    flat_connected: Option<bool>,
    flat_cover_state: Option<String>,
    flat_light_on: Option<bool>,
    flat_brightness: Option<i32>,
    weather_connected: Option<bool>,
    /// Latest opt-in observing-conditions snapshot. It contains only numeric
    /// measurements with explicit units — never a device identity, site
    /// coordinate, or raw driver payload.
    weather_conditions: Option<WeatherConditions>,
    weather_conditions_at: Option<DateTime<Utc>>,
    weather_high_wind: Option<bool>,
    weather_high_wind_conditions: Option<WeatherConditions>,
    weather_high_wind_conditions_at: Option<DateTime<Utc>>,
    weather_high_wind_threshold_meters_per_second: Option<f64>,
    switch_connected: Option<bool>,
    /// A recent legacy signal that the otherwise-ambiguous coordinate
    /// operation is a center rather than a plain slew.
    center_event_seen_at: Option<DateTime<Utc>>,
    /// Long-running operations reconstructed from the live sequence tree.
    sequence_operations: HashMap<String, TrackedSequenceOperation>,
    /// Solve attempts already announced during this updater lifetime. NINA
    /// retains a Center item's last result when a sequence loop restarts, so
    /// operation-local state alone would resend the stale image.
    plate_solve_outputs_seen: BoundedSeenSet,
    /// Fingerprint of the last live-status embed posted. Lets us skip the
    /// `upsert_status` call when nothing meaningful has changed since the
    /// previous poll cycle.
    last_status_fingerprint: Option<String>,
    /// Whether the telescope is currently *reported* as connected. Drives the
    /// offline/reconnect logging and chat alerts. Set true once the startup
    /// baseline succeeds; flips to false only after the failure debounce.
    connected: bool,
    /// Consecutive failed poll cycles since the last successful one. Used to
    /// debounce the offline alert (see `OFFLINE_FAILURE_THRESHOLD`).
    consecutive_failures: u32,
}

impl UpdaterState {
    fn new() -> Self {
        Self {
            dedup_hasher: RandomState::new(),
            events_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            disabled_event_tombstones: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            images_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            current_target: None,
            meridian_flip_time: None,
            sequence_container_counts: None,
            last_image_time: None,
            skipped_images_count: 0,
            last_filter: None,
            last_mount_event: None,
            last_guider_event: None,
            sequence_running: false,
            sequence_failure: None,
            sequence_outcome: None,
            scheduler_wait: None,
            scheduler_wait_latest_at: None,
            safety_state: SafetyState::Unknown,
            dome_connected: None,
            dome_shutter_open: None,
            dome_azimuth: None,
            dome_parked: None,
            dome_homed: None,
            flat_connected: None,
            flat_cover_state: None,
            flat_light_on: None,
            flat_brightness: None,
            weather_connected: None,
            weather_conditions: None,
            weather_conditions_at: None,
            weather_high_wind: None,
            weather_high_wind_conditions: None,
            weather_high_wind_conditions_at: None,
            weather_high_wind_threshold_meters_per_second: None,
            switch_connected: None,
            center_event_seen_at: None,
            sequence_operations: HashMap::new(),
            plate_solve_outputs_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            last_status_fingerprint: None,
            connected: false,
            consecutive_failures: 0,
        }
    }

    fn scheduler_wait_end(&self) -> Option<DateTime<Utc>> {
        self.scheduler_wait.map(|wait| wait.end_at)
    }

    fn record_scheduler_wait(&mut self, event: &Event) -> bool {
        let occurred_at =
            parse_nina_timestamp(&event.time).map(|timestamp| timestamp.with_timezone(&Utc));
        if !self.accept_scheduler_transition(occurred_at) {
            return false;
        }

        // A new wait announcement supersedes the prior plan even when this
        // legacy payload cannot provide a usable endpoint.
        self.scheduler_wait = None;
        let Some(EventDetails::WaitStart { wait_end_time }) = event.details.as_ref() else {
            return true;
        };
        let Some(end_at) = parse_nina_timestamp_with_context(wait_end_time, Some(&event.time))
            .map(|timestamp| timestamp.with_timezone(&Utc))
        else {
            return true;
        };
        self.scheduler_wait = Some(SchedulerWaitState { end_at });
        true
    }

    fn clear_scheduler_waits(&mut self) {
        self.scheduler_wait = None;
    }

    fn clear_scheduler_waits_for_privacy(&mut self, event_time: &str) {
        // A local privacy revocation is authoritative even if a legacy peer's
        // timestamp is malformed. Keep or advance the ordering watermark so
        // delayed enabled history cannot reconstruct the redacted wait.
        self.scheduler_wait = None;
        if let Some(candidate) =
            parse_nina_timestamp(event_time).map(|timestamp| timestamp.with_timezone(&Utc))
            && self
                .scheduler_wait_latest_at
                .is_none_or(|current| candidate > current)
        {
            self.scheduler_wait_latest_at = Some(candidate);
        }
    }

    fn clear_scheduler_waits_at(&mut self, event_time: &str) {
        let occurred_at =
            parse_nina_timestamp(event_time).map(|timestamp| timestamp.with_timezone(&Utc));
        if occurred_at.is_none() {
            // The terminal event type is still authoritative. Preserve the
            // known watermark, but do not leave a possibly completed wait
            // visible merely because a legacy timestamp was malformed.
            self.scheduler_wait = None;
            return;
        }
        if self.accept_scheduler_transition(occurred_at) {
            self.scheduler_wait = None;
        }
    }

    fn accept_scheduler_transition(&mut self, occurred_at: Option<DateTime<Utc>>) -> bool {
        match (self.scheduler_wait_latest_at, occurred_at) {
            (Some(current), Some(candidate)) if candidate < current => false,
            // An unparseable occurrence must not displace state whose ordering
            // is known. Legacy-to-legacy evidence remains arrival ordered.
            (Some(_), None) => false,
            (_, Some(candidate)) => {
                self.scheduler_wait_latest_at = Some(candidate);
                true
            }
            (None, None) => true,
        }
    }

    /// Fingerprint of the state that should drive a live-status edit.
    /// Deliberately excludes the live mount RA/Dec — those drift every
    /// cycle during tracking and would force constant edits. We only
    /// re-render when discrete state transitions happen (target changes,
    /// filter switches, mount events, guider events, wait timers, etc.).
    fn status_fingerprint(&self) -> String {
        let target = self
            .current_target
            .as_ref()
            .map(|t| t.name.as_str())
            .unwrap_or("");
        let filter = self
            .last_filter
            .as_ref()
            .map(|f| f.name.as_str())
            .unwrap_or("");
        let mount = self.last_mount_event.as_deref().unwrap_or("");
        let guider = self.last_guider_event.as_deref().unwrap_or("");
        let now = Utc::now();
        let scheduler_wait = self.scheduler_wait_end().map_or_else(
            || "none".to_string(),
            |end| {
                let remaining = end.signed_duration_since(now);
                if remaining > chrono::Duration::zero() {
                    format!("pending:{}", remaining.num_minutes())
                } else {
                    "scheduled-time-reached".to_string()
                }
            },
        );
        let mut operations = self
            .sequence_operations
            .iter()
            .filter(|(_, operation)| operation.operation.chat_enabled)
            .map(|(key, operation)| {
                let bucket = match &operation.operation.kind {
                    SequenceOperationKind::TimeWait { .. } => format!(
                        "wait:{}",
                        operation
                            .estimated_end
                            .map(|end| end.signed_duration_since(Utc::now()).num_minutes())
                            .unwrap_or(-1)
                    ),
                    SequenceOperationKind::CameraCooling { .. } => format!(
                        "cool:{}",
                        operation
                            .camera
                            .as_ref()
                            .map(|camera| (camera.temperature * 2.0).round() as i64)
                            .unwrap_or(i64::MIN)
                    ),
                    SequenceOperationKind::CameraWarming { .. } => format!(
                        "warm:{}",
                        operation
                            .camera
                            .as_ref()
                            .map(|camera| (camera.temperature * 2.0).round() as i64)
                            .unwrap_or(i64::MIN)
                    ),
                    SequenceOperationKind::MountSlew { coordinates, .. } => format!(
                        "slew:{}",
                        coordinates.as_ref().map_or("", |coordinates| {
                            coordinates.ra_string.as_deref().unwrap_or("")
                        })
                    ),
                    SequenceOperationKind::MountCenter { output, .. } => format!(
                        "center:{}",
                        output
                            .as_ref()
                            .and_then(|output| output.solve_time.as_deref())
                            .unwrap_or("")
                    ),
                    SequenceOperationKind::PlateSolve { output, .. } => format!(
                        "solve:{}",
                        output
                            .as_ref()
                            .and_then(|output| output.solve_time.as_deref())
                            .unwrap_or("")
                    ),
                    SequenceOperationKind::AstronomicalWait {
                        target_altitude_degrees,
                        current_altitude_degrees,
                        comparator,
                        expected_time,
                    } => format!(
                        "astro:{:?}:{:?}:{}:{}",
                        target_altitude_degrees.map(f64::to_bits),
                        current_altitude_degrees.map(f64::to_bits),
                        comparator.as_deref().unwrap_or(""),
                        expected_time.as_deref().unwrap_or("")
                    ),
                    SequenceOperationKind::SafetyWait { is_safe, .. } => format!(
                        "safety:{}",
                        is_safe
                            .map(|safe| if safe { "safe" } else { "unsafe" })
                            .unwrap_or("disconnected")
                    ),
                    SequenceOperationKind::ConditionWait { wait_interval } => format!(
                        "condition:{}",
                        wait_interval
                            .map(|duration| duration.num_seconds())
                            .unwrap_or(-1)
                    ),
                    SequenceOperationKind::ManualWait => "manual".to_string(),
                };
                format!("{key}:{bucket}")
            })
            .collect::<Vec<_>>();
        operations.sort();
        // Round the meridian-flip ETA to whole minutes; second-by-second
        // drift shouldn't trigger an edit.
        let flip_minutes = self
            .meridian_flip_time
            .map(|h| (h * 60.0).round() as i64)
            .unwrap_or(-1);
        let target_end = self
            .current_target
            .as_ref()
            .and_then(|target| target.target_end_time)
            .map(|end| end.timestamp())
            .unwrap_or_default();
        let failure = self
            .sequence_failure
            .as_ref()
            .map_or("", |failure| failure.error.as_str());
        format!(
            "t={target}|te={target_end}|f={filter}|m={mount}|g={guider}|w={scheduler_wait}|sr={}|sf={failure}|so={:?}|safe={:?}|dc={:?}|dso={:?}|daz={:?}|dp={:?}|dh={:?}|fc={:?}|fcs={:?}|fl={:?}|fb={:?}|wc={:?}|wx={:?}|wh={:?}|whx={:?}|wht={:?}|sw={:?}|flip={flip_minutes}|ops={}",
            self.sequence_running,
            self.sequence_outcome,
            self.safety_state,
            self.dome_connected,
            self.dome_shutter_open,
            self.dome_azimuth,
            self.dome_parked,
            self.dome_homed,
            self.flat_connected,
            self.flat_cover_state,
            self.flat_light_on,
            self.flat_brightness,
            self.weather_connected,
            self.weather_conditions,
            self.weather_high_wind,
            self.weather_high_wind_conditions,
            self.weather_high_wind_threshold_meters_per_second,
            self.switch_connected,
            operations.join(",")
        )
    }

    fn event_key(&self, event: &Event) -> String {
        format!(
            "e:{:016x}",
            self.dedup_hasher
                .hash_one((&event.time, &event.event, format!("{:?}", event.details)))
        )
    }

    fn image_key(&self, image: &ImageMetadata) -> String {
        format!(
            "i:{:016x}",
            self.dedup_hasher.hash_one(format!("{image:?}"))
        )
    }

    fn has_seen_event(&mut self, event: &Event) -> bool {
        if !event.chat_enabled {
            // Disabled compatibility records have no side effects. Treat them
            // as consumed without retaining any of their wire contents.
            return true;
        }
        let key = self.event_key(event);
        self.events_seen.check_and_insert(key)
    }

    fn has_seen_disabled_event(&mut self, event: &Event) -> bool {
        debug_assert!(!event.chat_enabled);
        let key = format!(
            "d:{:016x}",
            self.dedup_hasher
                .hash_one((&event.time, event_delivery_scope(&event.event)))
        );
        self.disabled_event_tombstones.check_and_insert(key)
    }

    fn has_seen_image(&mut self, image: &ImageMetadata) -> bool {
        let key = self.image_key(image);
        self.images_seen.check_and_insert(key)
    }
}

/// Per-telescope chat updater. Holds a reference to the process-wide chat
/// service manager and a `ChatTarget` describing where this telescope's posts
/// should be routed (Discord webhook override, Matrix room override).
pub struct ChatUpdater {
    source: SharedRigSource,
    state: UpdaterState,
    chat_manager: Arc<ChatServiceManager>,
    chat_target: ChatTarget,
    image_cooldown: Duration,
    /// First-retry wait when the telescope is unreachable at startup.
    reconnect_initial: Duration,
    /// Ceiling for the exponential reconnect backoff.
    reconnect_max: Duration,
    /// Telescope name — used to prefix chat message titles and console logs
    /// so users running multiple telescopes can tell rigs apart.
    telescope_name: String,
    /// Post lifecycle messages (startup welcome, offline/back-online
    /// alerts) to chat. True for self-hosted mode, where this updater's
    /// lifetime IS the process lifetime. The hub sets false: its updaters
    /// restart on every deploy, rig reconnect, and config change, and it
    /// announces scope presence from the connection layer instead.
    announce_lifecycle: bool,
    autofocus_retry: AutofocusRetryPolicy,
    pending_autofocus_deliveries: Vec<PendingAutofocusDelivery>,
    /// Initialization is retried stream-by-stream. Once event history has
    /// established its baseline, every later event response is a live delta
    /// even if another capability (such as image history) is still offline.
    event_baseline_complete: bool,
}

struct PendingAutofocusDelivery {
    event_time: String,
    report_timestamp: Option<String>,
    filter: Option<String>,
    position: Option<f64>,
    temperature: Option<f64>,
    attempts: usize,
    queued_at: TokioInstant,
    not_ready_since: Option<TokioInstant>,
    next_attempt_at: TokioInstant,
    retry_delay: Duration,
    retry: AutofocusRetryPolicy,
}

struct AutofocusNotificationTask {
    chat_manager: Arc<ChatServiceManager>,
    chat_target: ChatTarget,
    telescope_name: String,
    autofocus_data: AutofocusResponse,
}

fn sequence_container_counts(sequence: &SequenceResponse) -> (usize, usize) {
    let containers = sequence.get_containers();
    let running = containers
        .iter()
        .filter(|container| container.status.eq_ignore_ascii_case("RUNNING"))
        .count();
    (containers.len(), running)
}

impl AutofocusNotificationTask {
    async fn run(self) {
        ChatUpdater::display_autofocus_results(&self.autofocus_data);
        if self.chat_manager.service_count() > 0 {
            ChatUpdater::send_autofocus_notification_to(
                &self.chat_manager,
                &self.chat_target,
                &self.telescope_name,
                &self.autofocus_data,
            )
            .await;
        }
    }
}

impl ChatUpdater {
    pub fn new(
        source: SharedRigSource,
        telescope_name: String,
        chat_target: ChatTarget,
        chat_manager: Arc<ChatServiceManager>,
    ) -> Self {
        Self {
            source,
            state: UpdaterState::new(),
            chat_manager,
            chat_target,
            image_cooldown: Duration::from_secs(60),
            reconnect_initial: DEFAULT_RECONNECT_INITIAL,
            reconnect_max: DEFAULT_RECONNECT_MAX,
            telescope_name,
            announce_lifecycle: true,
            autofocus_retry: DEFAULT_AUTOFOCUS_RETRY,
            pending_autofocus_deliveries: Vec::new(),
            event_baseline_complete: false,
        }
    }

    /// Telescope identifier this updater is wired to.
    pub fn telescope_name(&self) -> &str {
        &self.telescope_name
    }

    /// Format a chat-message title with the telescope name prefix.
    fn titled(&self, title: impl Into<String>) -> String {
        format!("[{}] {}", self.telescope_name, title.into())
    }

    pub fn with_image_cooldown(mut self, cooldown_seconds: u64) -> Self {
        self.image_cooldown = Duration::from_secs(cooldown_seconds);
        self
    }

    /// Set the exponential-backoff schedule for baseline reconnect attempts:
    /// the first retry waits `initial_seconds`, doubling each failure up to
    /// `max_seconds`. Values are not clamped — a large `max_seconds` is honored.
    pub fn with_reconnect_backoff(mut self, initial_seconds: u64, max_seconds: u64) -> Self {
        self.reconnect_initial = Duration::from_secs(initial_seconds);
        self.reconnect_max = Duration::from_secs(max_seconds);
        self
    }

    /// Enable or disable lifecycle chat messages (startup welcome,
    /// offline/back-online alerts). Event and image notifications are not
    /// affected.
    pub fn with_lifecycle_announcements(mut self, announce: bool) -> Self {
        self.announce_lifecycle = announce;
        self
    }

    /// First-retry wait for an unreachable telescope's baseline.
    pub fn reconnect_initial(&self) -> Duration {
        self.reconnect_initial
    }

    /// Next backoff delay after a failed reconnect attempt: double `current`,
    /// capped at `reconnect_max` (but never below `reconnect_initial`, so a
    /// misconfigured `max < initial` can't shrink the wait).
    pub fn next_reconnect_delay(&self, current: Duration) -> Duration {
        backoff_delay(current, self.reconnect_initial, self.reconnect_max)
    }

    pub async fn start_polling(&mut self, poll_interval: Duration) {
        let n = self.telescope_name.clone();
        println!("[{n}] Starting chat updater loop (events and images)...");
        println!(
            "[{n}] Chat services configured: {}",
            self.chat_manager.service_count()
        );
        println!("[{n}] Polling interval: {poll_interval:?}");

        // If the telescope is unreachable at startup, don't give up forever.
        // Retry the baseline until it succeeds, backing off exponentially — in
        // a multi-telescope setup one offline rig must not kill its own task.
        let mut delay = self.reconnect_initial;
        loop {
            // Map the (non-Send) error to a String in the scrutinee so no
            // `Box<dyn Error>` is bound across the await point below.
            match self.initialize_baseline().await.map_err(|e| e.to_string()) {
                Ok(()) => break,
                Err(msg) => {
                    eprintln!("[{n}] Failed to initialize baseline: {msg}; retrying in {delay:?}");
                    sleep(delay).await;
                    delay = self.next_reconnect_delay(delay);
                }
            }
        }

        // Steady-state Direct reconciliation. Each reader reports whether the
        // plugin answered, so a rig that drops mid-session is noticed without a
        // separate health probe. Failed cycles use the shared backoff schedule
        // instead of hammering the plugin.
        // Catching up on missed bounded history is automatic because readers
        // deduplicate against seen state, so no re-baseline is needed.
        let mut reconnect_delay = self.reconnect_initial;
        loop {
            // Run every reader so live state stays current; the cycle counts as
            // reachable if any Direct query answered.
            let events_ok = self.poll_events().await;
            let seq_ok = self.poll_sequence().await;
            let images_ok = self.poll_images().await;
            let reachable = seq_ok || events_ok || images_ok;

            self.record_reachability(reachable).await;

            if reachable {
                self.refresh_status_message().await;
                reconnect_delay = self.reconnect_initial;
                self.poll_autofocus_delivery().await;
                sleep(poll_interval).await;
            } else {
                self.poll_autofocus_delivery().await;
                sleep(reconnect_delay).await;
                reconnect_delay = self.next_reconnect_delay(reconnect_delay);
            }
        }
    }

    /// Record the outcome of a poll cycle and manage the reported-connection
    /// state. Logs and posts a chat alert on each transition, debouncing the
    /// offline direction until `OFFLINE_FAILURE_THRESHOLD` consecutive cycles
    /// have failed (so a single transient blip stays quiet); reconnect fires as
    /// soon as the plugin answers again.
    pub async fn record_reachability(&mut self, reachable: bool) {
        if reachable {
            self.state.consecutive_failures = 0;
            if !self.state.connected {
                eprintln!(
                    "[{}] Telescope reconnected; resuming updates.",
                    self.telescope_name
                );
                self.state.connected = true;
                if self.chat_manager.service_count() > 0 {
                    self.send_connectivity_notification(true).await;
                }
            }
        } else {
            self.state.consecutive_failures += 1;
            if self.state.connected && self.state.consecutive_failures >= OFFLINE_FAILURE_THRESHOLD
            {
                eprintln!(
                    "[{}] Telescope offline after {} failed cycles; backing off.",
                    self.telescope_name, self.state.consecutive_failures
                );
                self.state.connected = false;
                if self.chat_manager.service_count() > 0 {
                    self.send_connectivity_notification(false).await;
                }
            }
        }
    }

    /// Post an offline/back-online connectivity alert to chat.
    async fn send_connectivity_notification(&self, online: bool) {
        if !self.announce_lifecycle {
            return;
        }
        let message = if online {
            ChatMessage::new(&self.titled("✅ Telescope back online"))
                .color(colors::GREEN)
                .field("Status", "Reconnected; resuming monitoring.", false)
        } else {
            ChatMessage::new(&self.titled("🔌 Telescope offline"))
                .color(colors::RED)
                .field(
                    "Status",
                    &format!(
                        "No response after {} consecutive poll cycles; retrying with backoff.",
                        self.state.consecutive_failures
                    ),
                    false,
                )
        };
        let message = message.footer(&format!(
            "{}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        ));
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    /// Build a live-status embed from current state and push it to any
    /// service that supports editing in place (currently only the Discord
    /// bot). No-op for telescopes routed only through webhooks/Matrix, or
    /// when the state fingerprint hasn't changed since the last cycle.
    pub async fn refresh_status_message(&mut self) {
        self.expire_target_scheduler_target(Utc::now());
        if !self.chat_manager.has_status_upsert(&self.chat_target) {
            return;
        }
        let fingerprint = self.state.status_fingerprint();
        if self.state.last_status_fingerprint.as_ref() == Some(&fingerprint) {
            return;
        }
        let message = self.build_status_message().await;
        self.chat_manager
            .upsert_status(&self.telescope_name, &self.chat_target, &message)
            .await;
        self.state.last_status_fingerprint = Some(fingerprint);
    }

    /// Compose the live-status `ChatMessage`. Pulls cheap state from
    /// `self.state` and adds a fresh mount snapshot per cycle (the most
    /// useful single fetch for at-a-glance status).
    async fn build_status_message(&self) -> ChatMessage {
        let mut message = ChatMessage::new(&self.titled("📡 Live status"));
        message = message.color(colors::CYAN);

        let summary = self.format_startup_status();
        if !summary.is_empty() {
            message = message.field("State", &summary, false);
        }

        if let Some(target) = &self.state.current_target {
            message = message.field("Target", &target.name, false);
            if let Some(coords) = &target.coordinates
                && let Some(s) = coords.display()
            {
                message = message.field("Coordinates", &s, false);
            }
        }

        if let Some(filter) = &self.state.last_filter
            && !filter.is_unknown()
        {
            message = message.field("Filter", &filter.name, true);
        }

        if let Some(flip_hours) = self.state.meridian_flip_time {
            message = message.field(
                "Meridian flip in",
                &meridian_flip_time_formatted_with_clock(flip_hours),
                true,
            );
        }

        // Fresh mount snapshot — small payload, very useful at a glance.
        if let Ok(mount_info) = self.source.get_mount_info().await
            && mount_info.is_connected()
        {
            let (ra, dec) = mount_info.get_coordinates();
            message = message
                .field("Mount RA/Dec", &format!("RA: {ra}\nDec: {dec}"), true)
                .field("Pier", mount_info.get_side_of_pier(), true);
        }

        message.footer(&format!(
            "Updated {}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        ))
    }

    async fn camera_snapshot_for(&self, operations: &[SequenceOperation]) -> Option<CameraInfo> {
        let cooling = operations.iter().any(|operation| {
            operation.is_active()
                && matches!(
                    operation.kind,
                    SequenceOperationKind::CameraCooling { .. }
                        | SequenceOperationKind::CameraWarming { .. }
                )
        }) || self.state.sequence_operations.values().any(|tracked| {
            matches!(
                tracked.operation.kind,
                SequenceOperationKind::CameraCooling { .. }
                    | SequenceOperationKind::CameraWarming { .. }
            )
        });
        if !cooling || !self.source.capabilities().equipment_snapshots {
            return None;
        }
        self.source
            .get_camera_info()
            .await
            .ok()
            .filter(|response| response.success && response.response.connected)
            .map(|response| response.response)
    }

    async fn reconcile_sequence_operations(
        &mut self,
        operations: Vec<SequenceOperation>,
        suppressed_operation_keys: HashSet<String>,
        camera: Option<CameraInfo>,
        announce: bool,
    ) {
        let now = Utc::now();
        if self
            .state
            .center_event_seen_at
            .is_some_and(|seen| now.signed_duration_since(seen) > chrono::Duration::minutes(2))
        {
            self.state.center_event_seen_at = None;
        }
        let mut incoming = operations
            .into_iter()
            .filter(|operation| !suppressed_operation_keys.contains(&operation.key))
            .map(|mut operation| {
                // Once a recent MOUNT-CENTER event has identified an
                // legacy coordinate item, retain that classification
                // on later snapshots even when an older Direct payload omits its type.
                if self
                    .state
                    .sequence_operations
                    .get(&operation.key)
                    .is_some_and(|previous| {
                        matches!(
                            previous.operation.kind,
                            SequenceOperationKind::MountCenter { .. }
                        )
                    })
                {
                    promote_ambiguous_slew_to_center(&mut operation);
                }
                (operation.key.clone(), operation)
            })
            .collect::<HashMap<_, _>>();
        let mut center_event_operation = None;
        if self.state.center_event_seen_at.is_some() {
            for (key, operation) in &mut incoming {
                if !operation.is_active() {
                    continue;
                }
                let is_center = matches!(operation.kind, SequenceOperationKind::MountCenter { .. })
                    || promote_ambiguous_slew_to_center(operation);
                if !is_center {
                    continue;
                }
                if let Some(previous) = self.state.sequence_operations.get_mut(key) {
                    promote_ambiguous_slew_to_center(&mut previous.operation);
                }
                center_event_operation = Some(key.clone());
                self.state.center_event_seen_at = None;
                break;
            }
        }
        let mut notifications = Vec::new();

        let existing_keys = self
            .state
            .sequence_operations
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for key in existing_keys {
            if suppressed_operation_keys.contains(&key) {
                self.state.sequence_operations.remove(&key);
                continue;
            }
            let Some(next) = incoming.get(&key) else {
                if let Some(mut previous) = self.state.sequence_operations.remove(&key) {
                    if matches!(
                        previous.operation.kind,
                        SequenceOperationKind::CameraCooling { .. }
                            | SequenceOperationKind::CameraWarming { .. }
                    ) {
                        previous.camera = camera.clone();
                    }
                    notifications.push((
                        previous,
                        OperationUpdate::Ended {
                            attach_output: false,
                        },
                    ));
                }
                continue;
            };
            let identity_changed =
                self.state
                    .sequence_operations
                    .get(&key)
                    .is_some_and(|previous| {
                        previous.operation.name != next.name
                            || std::mem::discriminant(&previous.operation.kind)
                                != std::mem::discriminant(&next.kind)
                    });
            if identity_changed {
                if let Some(mut previous) = self.state.sequence_operations.remove(&key) {
                    if matches!(
                        previous.operation.kind,
                        SequenceOperationKind::CameraCooling { .. }
                            | SequenceOperationKind::CameraWarming { .. }
                    ) {
                        previous.camera = camera.clone();
                    }
                    notifications.push((
                        previous,
                        OperationUpdate::Ended {
                            attach_output: false,
                        },
                    ));
                }
                // The second pass treats the replacement at this path as a
                // newly started operation instead of retaining stale timing
                // or camera progress from the old sequence item.
                continue;
            }
            if !next.is_active() {
                if let Some(mut previous) = self.state.sequence_operations.remove(&key) {
                    if matches!(
                        previous.operation.kind,
                        SequenceOperationKind::CameraCooling { .. }
                            | SequenceOperationKind::CameraWarming { .. }
                    ) {
                        previous.camera = camera.clone();
                    }
                    let output_key = plate_solve_output_key(&self.state.dedup_hasher, next);
                    let attach_output = claim_plate_solve_output(
                        &mut self.state.plate_solve_outputs_seen,
                        next.chat_enabled,
                        output_key.as_ref(),
                    );
                    previous.last_output_key = output_key;
                    previous.operation = next.clone();
                    notifications.push((
                        previous,
                        if next.is_failed() {
                            OperationUpdate::Failed { attach_output }
                        } else if next.is_finished() {
                            OperationUpdate::Finished { attach_output }
                        } else {
                            OperationUpdate::Ended { attach_output }
                        },
                    ));
                }
                continue;
            }

            if let Some(tracked) = self.state.sequence_operations.get_mut(&key) {
                tracked.operation = next.clone();
                if matches!(
                    tracked.operation.kind,
                    SequenceOperationKind::CameraCooling { .. }
                        | SequenceOperationKind::CameraWarming { .. }
                ) {
                    if tracked.initial_temperature.is_none() {
                        tracked.initial_temperature = camera
                            .as_ref()
                            .map(|info| info.temperature)
                            .filter(|value| value.is_finite());
                    }
                    tracked.camera = camera.clone();
                } else if let SequenceOperationKind::TimeWait {
                    target_time: Some(target),
                    ..
                } = &tracked.operation.kind
                {
                    tracked.estimated_end = Some(target.with_timezone(&Utc));
                } else if matches!(
                    tracked.operation.kind,
                    SequenceOperationKind::TimeWait { .. }
                ) {
                    tracked.restore_intrinsic_wait_estimate();
                }

                if let Some(milestone) = tracked.next_milestone(now) {
                    tracked.last_milestone = milestone;
                    notifications.push((tracked.clone(), OperationUpdate::Progress(milestone)));
                }
                let output_key =
                    plate_solve_output_key(&self.state.dedup_hasher, &tracked.operation);
                if output_key.is_some() && output_key != tracked.last_output_key {
                    tracked.last_output_key = output_key.clone();
                    if claim_plate_solve_output(
                        &mut self.state.plate_solve_outputs_seen,
                        tracked.operation.chat_enabled,
                        output_key.as_ref(),
                    ) {
                        notifications.push((tracked.clone(), OperationUpdate::Output));
                    }
                }
            }
        }

        for (key, operation) in incoming {
            if !operation.is_active() || self.state.sequence_operations.contains_key(&key) {
                continue;
            }
            let suppress_duplicate_center = center_event_operation.as_deref() == Some(key.as_str());
            let operation_camera = matches!(
                operation.kind,
                SequenceOperationKind::CameraCooling { .. }
                    | SequenceOperationKind::CameraWarming { .. }
            )
            .then(|| camera.clone())
            .flatten();
            let mut tracked = TrackedSequenceOperation::new(operation, now, operation_camera);
            if !announce {
                tracked.last_milestone = tracked
                    .progress_percent(now)
                    .map(completed_milestone)
                    .unwrap_or(0);
            }
            let output_key = plate_solve_output_key(&self.state.dedup_hasher, &tracked.operation);
            let output_is_new = claim_plate_solve_output(
                &mut self.state.plate_solve_outputs_seen,
                tracked.operation.chat_enabled,
                output_key.as_ref(),
            );
            tracked.last_output_key = output_key;
            if announce && !suppress_duplicate_center {
                notifications.push((tracked.clone(), OperationUpdate::Started));
            }
            if announce && output_is_new {
                notifications.push((tracked.clone(), OperationUpdate::Output));
            }
            self.state.sequence_operations.insert(key, tracked);
        }

        if announce && self.chat_manager.service_count() > 0 {
            for (operation, update) in notifications {
                if operation.operation.chat_enabled {
                    self.send_sequence_operation_update(&operation, update)
                        .await;
                }
            }
        }
    }

    async fn send_sequence_operation_update(
        &self,
        tracked: &TrackedSequenceOperation,
        update: OperationUpdate,
    ) {
        let (operation_name, title) = match (&tracked.operation.kind, update) {
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Started) => {
                ("Camera cooling", "❄️ Camera cooling started")
            }
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Progress(_)) => {
                ("Camera cooling", "❄️ Camera cooling update")
            }
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Finished { .. }) => {
                ("Camera cooling", "✅ Camera cooling finished")
            }
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Ended { .. }) => {
                ("Camera cooling", "Camera cooling ended")
            }
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Failed { .. }) => {
                ("Camera cooling", "❌ Camera cooling failed")
            }
            (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Started) => {
                ("Camera warming", "🌡️ Camera warming started")
            }
            (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Progress(_)) => {
                ("Camera warming", "🌡️ Camera warming update")
            }
            (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Finished { .. }) => {
                ("Camera warming", "✅ Camera warming finished")
            }
            (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Ended { .. }) => {
                ("Camera warming", "Camera warming ended")
            }
            (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Failed { .. }) => {
                ("Camera warming", "❌ Camera warming failed")
            }
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Started) => {
                ("Timed wait", "⏳ Timed wait started")
            }
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Progress(_)) => {
                ("Timed wait", "⏳ Timed wait update")
            }
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Finished { .. }) => {
                ("Timed wait", "✅ Timed wait finished")
            }
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Ended { .. }) => {
                ("Timed wait", "Timed wait ended")
            }
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Failed { .. }) => {
                ("Timed wait", "❌ Timed wait failed")
            }
            (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Started) => {
                ("Astronomical wait", "🌌 Astronomical wait started")
            }
            (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Finished { .. }) => {
                ("Astronomical wait", "✅ Astronomical condition reached")
            }
            (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Ended { .. }) => {
                ("Astronomical wait", "Astronomical wait ended")
            }
            (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Failed { .. }) => {
                ("Astronomical wait", "❌ Astronomical wait failed")
            }
            (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Started) => {
                ("Safety wait", "🛡️ Waiting for safe conditions")
            }
            (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Finished { .. }) => {
                ("Safety wait", "✅ Safe conditions reached")
            }
            (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Ended { .. }) => {
                ("Safety wait", "Safety wait ended")
            }
            (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Failed { .. }) => {
                ("Safety wait", "❌ Safety wait failed")
            }
            (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Started) => {
                ("Condition wait", "⏳ Waiting for a sequence condition")
            }
            (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Finished { .. }) => {
                ("Condition wait", "✅ Sequence condition reached")
            }
            (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Ended { .. }) => {
                ("Condition wait", "Sequence condition wait ended")
            }
            (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Failed { .. }) => {
                ("Condition wait", "❌ Sequence condition wait failed")
            }
            (SequenceOperationKind::ManualWait, OperationUpdate::Started) => {
                ("Manual wait", "⏸️ Waiting for manual sequence resume")
            }
            (SequenceOperationKind::ManualWait, OperationUpdate::Finished { .. }) => {
                ("Manual wait", "▶️ Sequence manually resumed")
            }
            (SequenceOperationKind::ManualWait, OperationUpdate::Ended { .. }) => {
                ("Manual wait", "Manual sequence wait ended")
            }
            (SequenceOperationKind::ManualWait, OperationUpdate::Failed { .. }) => {
                ("Manual wait", "❌ Manual sequence wait failed")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Started) => {
                ("Mount slew", "🔭 Mount slew started")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Finished { .. }) => {
                ("Mount slew", "✅ Mount slew finished")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Ended { .. }) => {
                ("Mount slew", "Mount slew ended")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Failed { .. }) => {
                ("Mount slew", "❌ Mount slew failed")
            }
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Started) => {
                ("Center", "🎯 Centering started")
            }
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Output) => {
                ("Center", "🔎 Plate solve result")
            }
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Finished { .. }) => {
                ("Center", "✅ Centering finished")
            }
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Ended { .. }) => {
                ("Center", "Centering ended")
            }
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Failed { .. }) => {
                ("Center", "❌ Centering failed")
            }
            (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Started) => {
                ("Plate solve", "🔎 Plate solve started")
            }
            (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Output) => {
                ("Plate solve", "🔎 Plate solve result")
            }
            (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Finished { .. }) => {
                ("Plate solve", "✅ Plate solve finished")
            }
            (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Ended { .. }) => {
                ("Plate solve", "Plate solve ended")
            }
            (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Failed { .. }) => {
                ("Plate solve", "❌ Plate solve failed")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::PlateSolve { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::CameraWarming { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::AstronomicalWait { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::SafetyWait { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::ConditionWait { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::ManualWait, OperationUpdate::Progress(_))
            | (SequenceOperationKind::ManualWait, OperationUpdate::Output) => {
                ("Sequence operation", "Sequence operation update")
            }
        };
        let color = match update {
            OperationUpdate::Finished { .. } => colors::GREEN,
            OperationUpdate::Ended { .. } => colors::GRAY,
            OperationUpdate::Failed { .. } => colors::RED,
            OperationUpdate::Output
                if operation_plate_solve_output(&tracked.operation.kind)
                    .is_some_and(|output| output.success == Some(false)) =>
            {
                colors::RED
            }
            OperationUpdate::Output => colors::CYAN,
            OperationUpdate::Started | OperationUpdate::Progress(_) => colors::YELLOW,
        };
        let mut message = ChatMessage::new(&self.titled(title))
            .color(color)
            .field("Operation", operation_name, true)
            .field("Sequence item", &tracked.operation.name, true);

        if let OperationUpdate::Progress(percent) = update {
            message = message.field("Progress", &format!("{percent}%"), true);
        }
        match &tracked.operation.kind {
            SequenceOperationKind::CameraCooling {
                target_temperature,
                minimum_duration,
            } => {
                message = message.field(
                    "Target temperature",
                    &format!("{target_temperature:.1} °C"),
                    true,
                );
                if let Some(duration) = minimum_duration {
                    message = message.field("Minimum time", &format_duration(*duration), true);
                }
                if let Some(camera) = &tracked.camera {
                    if camera.temperature.is_finite() {
                        message = message.field(
                            "Current temperature",
                            &format!("{:.1} °C", camera.temperature),
                            true,
                        );
                    }
                    if camera.cooler_power.is_finite() {
                        message = message.field(
                            "Cooler power",
                            &format!("{:.0}%", camera.cooler_power),
                            true,
                        );
                    }
                }
            }
            SequenceOperationKind::CameraWarming { minimum_duration } => {
                if let Some(duration) = minimum_duration {
                    message = message.field("Minimum time", &format_duration(*duration), true);
                }
                if let Some(camera) = &tracked.camera
                    && camera.temperature.is_finite()
                {
                    message = message.field(
                        "Current temperature",
                        &format!("{:.1} °C", camera.temperature),
                        true,
                    );
                }
            }
            SequenceOperationKind::TimeWait { .. } => {
                if let Some(end) = tracked.estimated_end {
                    message = match update {
                        OperationUpdate::Started | OperationUpdate::Progress(_) => {
                            add_wait_timing_fields(message, end, Utc::now())
                        }
                        OperationUpdate::Finished { .. }
                        | OperationUpdate::Ended { .. }
                        | OperationUpdate::Failed { .. }
                        | OperationUpdate::Output => {
                            add_wait_endpoint_field(message, "Planned until", end)
                        }
                    };
                }
            }
            SequenceOperationKind::AstronomicalWait {
                target_altitude_degrees,
                current_altitude_degrees,
                comparator,
                expected_time,
            } => {
                if let Some(target) = target_altitude_degrees {
                    let comparison = comparator
                        .as_deref()
                        .map(|value| format!("{value} "))
                        .unwrap_or_default();
                    message = message.field(
                        "Target altitude",
                        &format!("{comparison}{target:.2}°"),
                        true,
                    );
                }
                if let Some(current) = current_altitude_degrees {
                    message = message.field("Current altitude", &format!("{current:.2}°"), true);
                }
                if let Some(expected) = expected_time {
                    message = message.field("Expected", &truncate_chat_value(expected), false);
                }
            }
            SequenceOperationKind::SafetyWait {
                is_safe,
                wait_interval,
            } => {
                let state = match is_safe {
                    Some(true) => "Safe",
                    Some(false) => "Unsafe",
                    None => "Monitor disconnected",
                };
                message = message.field("Safety monitor", state, true);
                if let Some(interval) = wait_interval {
                    message = message.field("Check interval", &format_duration(*interval), true);
                }
            }
            SequenceOperationKind::ConditionWait { wait_interval } => {
                if let Some(interval) = wait_interval {
                    message = message.field("Check interval", &format_duration(*interval), true);
                }
            }
            SequenceOperationKind::ManualWait => {}
            SequenceOperationKind::MountSlew { coordinates, .. } => {
                if let Some(coordinates) = coordinates {
                    message = message.field("Destination", &coordinates.display(), false);
                }
            }
            SequenceOperationKind::MountCenter {
                coordinates,
                rotation,
                ..
            } => {
                if let Some(coordinates) = coordinates {
                    message = message.field("Target", &coordinates.display(), false);
                }
                if let Some(rotation) = rotation {
                    message = message.field("Target rotation", &format!("{rotation:.1}°"), true);
                }
            }
            SequenceOperationKind::PlateSolve {
                coordinates,
                rotation,
                ..
            } => {
                if let Some(coordinates) = coordinates {
                    message = message.field("Requested position", &coordinates.display(), false);
                }
                if let Some(rotation) = rotation {
                    message = message.field("Requested rotation", &format!("{rotation:.1}°"), true);
                }
            }
        }
        if let Some(output) = operation_plate_solve_output(&tracked.operation.kind) {
            message = add_plate_solve_output_fields(message, output);
        }
        let attach_output = matches!(update, OperationUpdate::Output)
            || matches!(
                update,
                OperationUpdate::Finished {
                    attach_output: true
                } | OperationUpdate::Ended {
                    attach_output: true
                } | OperationUpdate::Failed {
                    attach_output: true
                }
            );
        let attachments = if attach_output {
            operation_plate_solve_output(&tracked.operation.kind)
                .and_then(|output| {
                    output
                        .thumbnail
                        .as_ref()
                        .map(|thumbnail| (output, thumbnail))
                })
                .map(|(output, thumbnail)| {
                    vec![ChatAttachment {
                        data: thumbnail.clone(),
                        filename: if output.thumbnail_media_type.as_deref() == Some("image/png") {
                            "plate_solve.png".to_string()
                        } else {
                            "plate_solve.jpg".to_string()
                        },
                    }]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.chat_manager
            .send_message_with_attachments(&message, &self.chat_target, &attachments)
            .await;
    }

    pub async fn initialize_baseline(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let n = self.telescope_name.clone();
        let capabilities = self.source.capabilities();
        println!("[{n}] Fetching initial baseline...");

        // Load events and find latest TS-TARGETSTART
        if capabilities.event_history {
            let events = self.source.get_event_history().await?;
            if self.event_baseline_complete {
                self.process_live_events(events.response).await;
            } else {
                self.process_baseline_events(&events.response);
                self.event_baseline_complete = true;
            }
        }

        // Load sequence to get meridian flip time and potential sequence target
        if capabilities.sequence {
            match self.source.get_sequence().await {
                Ok(sequence) => {
                    self.state.meridian_flip_time = extract_meridian_flip_time(&sequence);
                    let operations = extract_sequence_operations(&sequence);
                    let suppressed_operation_keys =
                        extract_suppressed_sequence_operation_keys(&sequence);
                    let camera = self.camera_snapshot_for(&operations).await;
                    self.reconcile_sequence_operations(
                        operations,
                        suppressed_operation_keys,
                        camera,
                        false,
                    )
                    .await;

                    // Apply both an enabled identity and an explicit local
                    // revocation before formatting startup status or welcome
                    // output. The latter must clear any historical Target
                    // Scheduler event reconstructed above.
                    self.reconcile_sequence_target(extract_current_target_with_delivery(&sequence));

                    self.state.sequence_container_counts =
                        Some(sequence_container_counts(&sequence));
                }
                Err(e) => {
                    println!("[{n}] Could not load sequence during initialization: {e}");
                }
            }
        }

        // Load images
        if capabilities.image_history {
            let images = self.source.get_all_image_history().await?;
            let privacy_boundary = images
                .response
                .iter()
                .rposition(|image| !image.chat_enabled);
            for (index, image) in images.response.iter().enumerate() {
                if privacy_boundary.is_some_and(|boundary| index < boundary) {
                    continue;
                }
                if image.chat_enabled {
                    let key = self.state.image_key(image);
                    self.state.images_seen.insert(key);
                }
            }
        }

        self.expire_target_scheduler_target(Utc::now());

        println!(
            "[{n}] Baseline: {} events, {} images",
            self.state.events_seen.len(),
            self.state.images_seen.len()
        );

        if let Some(target) = &self.state.current_target {
            println!(
                "[{n}] Current target: {} (from {:?})",
                target.name, target.source
            );
        }

        let status = self.format_startup_status();
        if !status.is_empty() {
            println!("[{n}] Inferred NINA state:\n{status}");
        }

        println!("[{n}] Now monitoring for new events and images.");

        // Send welcome message to chat services
        if self.announce_lifecycle && self.chat_manager.service_count() > 0 {
            self.send_welcome_message().await;
        }

        self.state.connected = true;
        Ok(())
    }

    /// Apply a legacy payload-v3 privacy tombstone without ever reading its
    /// details. New plugin sessions omit disabled records entirely, but an
    /// older peer can retain an enabled value followed by a disabled marker;
    /// the marker must revoke that category's reconstructed state rather than
    /// allowing the older value to remain visible.
    fn revoke_state_for_disabled_event(&mut self, event_type: &str, event_time: Option<&str>) {
        match event_delivery_scope(event_type) {
            EventDeliveryScope::TargetScheduler => {
                self.state.current_target = None;
                if let Some(event_time) = event_time {
                    self.state.clear_scheduler_waits_for_privacy(event_time);
                } else {
                    self.state.clear_scheduler_waits();
                }
            }
            EventDeliveryScope::FilterFocuserRotator => {
                self.state.last_filter = None;
            }
            EventDeliveryScope::Mount => {
                self.state.last_mount_event = None;
                self.state.center_event_seen_at = None;
                self.state.meridian_flip_time = None;
                self.state.sequence_operations.retain(|_, tracked| {
                    !matches!(
                        tracked.operation.kind,
                        SequenceOperationKind::MountSlew { .. }
                            | SequenceOperationKind::MountCenter { .. }
                            | SequenceOperationKind::PlateSolve { .. }
                    )
                });
            }
            // Start-event scopes are new and transient. Existing completion
            // names deliberately retain their legacy broad scopes so a
            // payload-v3 privacy tombstone still revokes all older state that
            // was captured under those switches.
            EventDeliveryScope::SlewMotion | EventDeliveryScope::RotatorMotion => {}
            EventDeliveryScope::Guiding => {
                self.state.last_guider_event = None;
            }
            EventDeliveryScope::Sequence => {
                self.state.sequence_running = false;
                self.state.sequence_failure = None;
                self.state.sequence_outcome = None;
                self.state.sequence_container_counts = None;
                self.state.sequence_operations.retain(|_, tracked| {
                    matches!(
                        tracked.operation.kind,
                        SequenceOperationKind::MountSlew { .. }
                            | SequenceOperationKind::MountCenter { .. }
                            | SequenceOperationKind::PlateSolve { .. }
                    )
                });
            }
            EventDeliveryScope::Safety => {
                self.state.safety_state = SafetyState::Unknown;
                self.state.sequence_operations.retain(|_, tracked| {
                    !matches!(
                        tracked.operation.kind,
                        SequenceOperationKind::SafetyWait { .. }
                    )
                });
            }
            EventDeliveryScope::WeatherChanges => {
                self.state.weather_conditions = None;
                self.state.weather_conditions_at = None;
            }
            EventDeliveryScope::HighWindAlerts => {
                self.state.weather_high_wind = None;
                self.state.weather_high_wind_conditions = None;
                self.state.weather_high_wind_conditions_at = None;
                self.state.weather_high_wind_threshold_meters_per_second = None;
            }
            EventDeliveryScope::Autofocus => {
                self.pending_autofocus_deliveries.clear();
            }
            EventDeliveryScope::Images => {
                self.state.last_image_time = None;
                self.state.skipped_images_count = 0;
            }
            EventDeliveryScope::Observatory => {
                self.state.dome_shutter_open = None;
                self.state.dome_azimuth = None;
                self.state.dome_parked = None;
                self.state.dome_homed = None;
                self.state.flat_cover_state = None;
                self.state.flat_light_on = None;
                self.state.flat_brightness = None;
            }
            EventDeliveryScope::EquipmentConnections => {
                self.state.dome_connected = None;
                self.state.flat_connected = None;
                self.state.weather_connected = None;
                self.state.switch_connected = None;
            }
            EventDeliveryScope::CommandFailures
            | EventDeliveryScope::NinaNotifications
            | EventDeliveryScope::NinaLogs
            | EventDeliveryScope::Other => {}
        }
        self.state.last_status_fingerprint = None;
    }

    fn apply_event_state(&mut self, event: &Event) -> bool {
        if !has_usable_weather_details(event) {
            return false;
        }
        if event.event == event_types::TS_WAITSTART && !self.state.record_scheduler_wait(event) {
            return false;
        }
        match event.event.as_str() {
            event_types::MOUNT_PARKED
            | event_types::MOUNT_UNPARKED
            | event_types::MOUNT_HOMED
            | event_types::MOUNT_SLEWED => {
                self.state.last_mount_event = Some(event.event.clone());
            }
            event_types::MOUNT_CENTER => {
                self.state.last_mount_event = Some(event.event.clone());
                self.state.center_event_seen_at = Some(Utc::now());
            }
            // Motion starts and flips describe brief activity, not a stable
            // state. They are announced but must not replace the status state.
            event_types::MOUNT_SLEW_STARTED
            | event_types::MOUNT_BEFORE_FLIP
            | event_types::MOUNT_AFTER_FLIP => {}
            event_types::GUIDER_START | event_types::GUIDER_STOP => {
                self.state.last_guider_event = Some(event.event.clone());
            }
            event_types::GUIDER_DITHER => {}
            event_types::SEQUENCE_STARTING => {
                self.state.sequence_running = true;
                self.state.sequence_failure = None;
                self.state.sequence_outcome = None;
                self.state.clear_scheduler_waits_at(&event.time);
            }
            event_types::SEQUENCE_FINISHED => {
                self.state.sequence_running = false;
                self.state.clear_scheduler_waits_at(&event.time);
                if let Some(EventDetails::SequenceFinished {
                    outcome,
                    status,
                    had_failures,
                }) = &event.details
                {
                    self.state.sequence_outcome = Some(outcome.clone());
                    if (*had_failures
                        || matches!(outcome.as_str(), "failed" | "completed_with_failures"))
                        && self.state.sequence_failure.is_none()
                    {
                        self.state.sequence_failure = Some(SequenceFailureInfo {
                            entity: "Sequence".to_string(),
                            entity_type: "Sequence root".to_string(),
                            error: format!("N.I.N.A. ended the sequence with status {status}"),
                        });
                    }
                } else {
                    self.state.sequence_outcome = None;
                }
            }
            event_types::SEQUENCE_ENTITY_FAILED => {
                self.state.sequence_failure = Some(match &event.details {
                    Some(EventDetails::SequenceEntityFailed {
                        entity,
                        entity_type,
                        error,
                    }) => SequenceFailureInfo {
                        entity: entity.clone(),
                        entity_type: entity_type.clone(),
                        error: error.clone(),
                    },
                    _ => SequenceFailureInfo {
                        entity: "Sequence item".to_string(),
                        entity_type: "Unknown".to_string(),
                        error: "N.I.N.A. reported that the sequence item failed".to_string(),
                    },
                });
            }
            event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART => {
                self.state.clear_scheduler_waits_at(&event.time);
            }
            event_types::TS_WAITSTART => {}
            event_types::SAFETY_CONNECTED => {
                self.state.safety_state = SafetyState::ConnectedUnknown;
            }
            event_types::SAFETY_DISCONNECTED => {
                self.state.safety_state = SafetyState::Disconnected;
            }
            event_types::SAFETY_CHANGED => {
                if let Some(EventDetails::SafetyChanged { is_safe }) = &event.details {
                    self.state.safety_state = if *is_safe {
                        SafetyState::Safe
                    } else {
                        SafetyState::Unsafe
                    };
                }
            }
            event_types::DOME_CONNECTED => {
                self.state.dome_connected = Some(true);
                self.state.dome_shutter_open = None;
                self.state.dome_azimuth = None;
                self.state.dome_parked = None;
                self.state.dome_homed = None;
            }
            event_types::DOME_DISCONNECTED => {
                self.state.dome_connected = Some(false);
                self.state.dome_shutter_open = None;
                self.state.dome_azimuth = None;
                self.state.dome_parked = None;
                self.state.dome_homed = None;
            }
            event_types::DOME_SHUTTER_OPENED => {
                self.state.dome_shutter_open = Some(true);
            }
            event_types::DOME_SHUTTER_CLOSED => {
                self.state.dome_shutter_open = Some(false);
            }
            event_types::DOME_HOMED => {
                self.state.dome_homed = Some(true);
                self.state.dome_parked = Some(false);
                self.state.dome_azimuth = None;
            }
            event_types::DOME_PARKED => {
                self.state.dome_parked = Some(true);
                self.state.dome_homed = Some(false);
                self.state.dome_azimuth = None;
            }
            // Synchronizing changes the azimuth reference without moving the
            // shutter or changing the known park/home state. Discard the last
            // slew azimuth rather than presenting it against the new reference.
            event_types::DOME_SYNCED => self.state.dome_azimuth = None,
            event_types::DOME_SLEWED => {
                if let Some(EventDetails::DomeSlewed { to, .. }) = &event.details {
                    self.state.dome_azimuth = Some(*to);
                    self.state.dome_parked = Some(false);
                    self.state.dome_homed = Some(false);
                }
            }
            event_types::FLAT_CONNECTED => {
                self.state.flat_connected = Some(true);
                self.state.flat_cover_state = None;
                self.state.flat_light_on = None;
                self.state.flat_brightness = None;
            }
            event_types::FLAT_DISCONNECTED => {
                self.state.flat_connected = Some(false);
                self.state.flat_cover_state = None;
                self.state.flat_light_on = None;
                self.state.flat_brightness = None;
            }
            event_types::FLAT_COVER_OPENED => {
                self.state.flat_cover_state = Some("Open".to_string());
            }
            event_types::FLAT_COVER_CLOSED => {
                self.state.flat_cover_state = Some("Closed".to_string());
            }
            event_types::FLAT_LIGHT_TOGGLED => {
                if let Some(EventDetails::FlatLightToggled { on }) = &event.details {
                    self.state.flat_light_on = *on;
                } else {
                    self.state.flat_light_on = None;
                }
            }
            event_types::FLAT_BRIGHTNESS_CHANGED => {
                if let Some(EventDetails::FlatBrightnessChanged { new, .. }) = &event.details {
                    self.state.flat_brightness = Some(*new);
                }
            }
            event_types::WEATHER_CONNECTED => {
                self.state.weather_connected = Some(true);
                self.clear_weather_connection_epoch();
            }
            event_types::WEATHER_DISCONNECTED => {
                self.state.weather_connected = Some(false);
                self.clear_weather_connection_epoch();
            }
            event_types::WEATHER_CHANGED => {
                if let Some(EventDetails::WeatherChanged { conditions, .. }) = &event.details {
                    self.state.weather_conditions =
                        (!conditions.is_empty()).then(|| conditions.clone());
                    self.state.weather_conditions_at = self
                        .state
                        .weather_conditions
                        .as_ref()
                        .and_then(|_| parse_nina_timestamp(&event.time))
                        .map(|time| time.with_timezone(&Utc));
                }
            }
            event_types::WEATHER_HIGH_WIND => {
                if let Some(EventDetails::WeatherHighWind {
                    is_high_wind,
                    threshold_meters_per_second,
                    conditions,
                }) = &event.details
                {
                    self.state.weather_high_wind = Some(*is_high_wind);
                    self.state.weather_high_wind_threshold_meters_per_second =
                        *threshold_meters_per_second;
                    self.state.weather_high_wind_conditions =
                        (!conditions.is_empty()).then(|| conditions.clone());
                    self.state.weather_high_wind_conditions_at = self
                        .state
                        .weather_high_wind_conditions
                        .as_ref()
                        .and_then(|_| parse_nina_timestamp(&event.time))
                        .map(|time| time.with_timezone(&Utc));
                }
            }
            event_types::SWITCH_CONNECTED => self.state.switch_connected = Some(true),
            event_types::SWITCH_DISCONNECTED => self.state.switch_connected = Some(false),
            _ => {}
        }
        self.state.last_status_fingerprint = None;
        true
    }

    fn clear_weather_connection_epoch(&mut self) {
        self.state.weather_conditions = None;
        self.state.weather_conditions_at = None;
        // A disconnected or newly connected station cannot prove that an
        // active alert recovered, so retain that latch. A prior recovery is
        // not durable across the new observation epoch; clear its old safe
        // reading and threshold instead of presenting them as current.
        if self.state.weather_high_wind != Some(true) {
            self.state.weather_high_wind = None;
            self.state.weather_high_wind_conditions = None;
            self.state.weather_high_wind_conditions_at = None;
            self.state.weather_high_wind_threshold_meters_per_second = None;
        }
    }

    fn process_baseline_events(&mut self, events: &[Event]) {
        let mut latest_ts_target: Option<(Option<DateTime<FixedOffset>>, usize, TargetInfo)> = None;
        let mut privacy_boundaries = HashMap::new();
        for (index, event) in events.iter().enumerate() {
            if !event.chat_enabled {
                privacy_boundaries.insert(event_delivery_scope(&event.event), index);
            }
        }

        for (index, event) in events.iter().enumerate() {
            let scope = event_delivery_scope(&event.event);
            if event.chat_enabled
                && privacy_boundaries
                    .get(&scope)
                    .is_some_and(|boundary| index < *boundary)
            {
                continue;
            }
            if !event.chat_enabled {
                // Mixed payload-v3 peers may still carry details on an event
                // whose local N.I.N.A. category is disabled. Retain only a
                // detail-free policy tombstone and revoke prior category
                // state; none of the event details may reconstruct status.
                self.state.has_seen_disabled_event(event);
                if scope == EventDeliveryScope::TargetScheduler {
                    latest_ts_target = None;
                }
                // Baseline initialization can be retried after a later query
                // fails. Reapply every tombstone on every attempt so enabled
                // history earlier in this same pass cannot resurrect state.
                self.revoke_state_for_disabled_event(&event.event, Some(&event.time));
                continue;
            }

            // Skip redundant filterwheel events
            if event.event == event_types::FILTERWHEEL_CHANGED
                && let Some(EventDetails::FilterWheelChange { new, previous }) = &event.details
                && new.name == previous.name
                && !new.is_unknown()
            {
                continue;
            }

            // Remember the last known good filter seen, so when NINA sends
            // empty-array fields later we still have a 'previous' to show.
            if event.event == event_types::FILTERWHEEL_CHANGED
                && let Some(EventDetails::FilterWheelChange { new, .. }) = &event.details
                && !new.is_unknown()
            {
                self.state.last_filter = Some(new.clone());
            }

            // Track TS-TARGETSTART events
            if matches!(
                event.event.as_str(),
                event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART
            ) && let Some(EventDetails::TargetStart {
                target_name,
                coordinates,
                project_name,
                rotation,
                target_end_time,
            }) = &event.details
                && target_name != "Sequential Instruction Set"
            {
                let target_info = TargetInfo {
                    name: target_name.clone(),
                    source: TargetSource::TsTargetStart,
                    coordinates: coordinates.clone(),
                    project: project_name.clone(),
                    rotation: *rotation,
                    target_end_time: target_end_time
                        .as_deref()
                        .and_then(|end| parse_nina_timestamp_with_context(end, Some(&event.time))),
                };

                let parsed_time = parse_nina_timestamp(&event.time);
                let is_newer =
                    latest_ts_target
                        .as_ref()
                        .is_none_or(|(latest_time, latest_index, _)| {
                            match (parsed_time, *latest_time) {
                                (Some(candidate), Some(latest)) => candidate > latest,
                                (Some(_), None) => true,
                                (None, Some(_)) => false,
                                (None, None) => index > *latest_index,
                            }
                        });
                if is_newer {
                    latest_ts_target = Some((parsed_time, index, target_info));
                }
            }

            self.apply_event_state(event);

            let key = self.state.event_key(event);
            self.state.events_seen.insert(key);
        }

        // Set the latest TS target if found
        if let Some((_, _, target)) = latest_ts_target {
            self.state.current_target = Some(target);
        }
    }

    /// Returns whether the Direct source responded, so the update loop can
    /// detect a mid-run disconnect without a separate health probe.
    pub async fn poll_events(&mut self) -> bool {
        if !self.source.capabilities().event_history {
            return false;
        }
        match self.source.get_event_history().await {
            Ok(events) => {
                self.process_live_events(events.response).await;
                self.expire_target_scheduler_target(Utc::now());
                true
            }
            Err(e) => {
                eprintln!("Error fetching events: {e}");
                false
            }
        }
    }

    async fn process_live_events(&mut self, events: Vec<Event>) {
        let mut privacy_boundaries = HashMap::new();
        for (index, event) in events.iter().enumerate() {
            if !event.chat_enabled {
                privacy_boundaries.insert(event_delivery_scope(&event.event), index);
            }
        }
        for (index, event) in events.into_iter().enumerate() {
            let scope = event_delivery_scope(&event.event);
            if event.chat_enabled
                && privacy_boundaries
                    .get(&scope)
                    .is_some_and(|boundary| index < *boundary)
            {
                continue;
            }
            if !self.should_process_event(&event) {
                continue;
            }

            if !event.chat_enabled {
                if !self.state.has_seen_disabled_event(&event) {
                    self.revoke_state_for_disabled_event(&event.event, Some(&event.time));
                }
                continue;
            }

            if !self.state.has_seen_event(&event) {
                self.print_new_event(&event);
                self.handle_event(&event).await;
            }
        }
    }

    fn should_process_event(&self, event: &Event) -> bool {
        if !event.chat_enabled {
            return true;
        }

        // These names drive durable state and user-visible alerts. Never
        // guess from absent, malformed, or sensor-empty typed details.
        if !has_usable_weather_details(event) {
            return false;
        }

        // Skip redundant filterwheel events, but only when both filters are
        // known — empty/unknown payloads need to be enriched, not dropped.
        if event.event == event_types::FILTERWHEEL_CHANGED
            && let Some(EventDetails::FilterWheelChange { new, previous }) = &event.details
            && !new.is_unknown()
            && !previous.is_unknown()
        {
            return new.name != previous.name;
        }
        true
    }

    async fn handle_event(&mut self, event: &Event) {
        if !has_usable_weather_details(event) {
            return;
        }
        if !event.chat_enabled {
            if !self.state.has_seen_disabled_event(event) {
                self.revoke_state_for_disabled_event(&event.event, Some(&event.time));
            }
            return;
        }

        // The plugin republishes WEATHER-HIGH-WIND after a threshold setting
        // changes so durable Hub state receives the new limit. When the rig
        // was already high and remains high this is a state refresh, not a
        // second alert; update state but do not post duplicate chat noise.
        let suppress_high_wind_refresh = matches!(
            &event.details,
            Some(EventDetails::WeatherHighWind {
                is_high_wind: true,
                ..
            }) if self.state.weather_high_wind == Some(true)
        );

        if !self.apply_event_state(event) {
            return;
        }
        if suppress_high_wind_refresh {
            return;
        }

        match event.event.as_str() {
            event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART => {
                self.handle_ts_targetstart(event).await;
                return;
            }
            event_types::FILTERWHEEL_CHANGED => {
                self.handle_filterwheel_changed(event).await;
                return;
            }
            _ => {}
        }

        match event.event.as_str() {
            event_types::AUTOFOCUS_FINISHED => self.handle_autofocus_finished(event).await,
            event_types::MOUNT_BEFORE_FLIP
            | event_types::MOUNT_AFTER_FLIP
            | event_types::MOUNT_PARKED
            | event_types::MOUNT_UNPARKED
            | event_types::MOUNT_HOMED
            | event_types::MOUNT_CENTER
            | event_types::MOUNT_SLEW_STARTED
            | event_types::MOUNT_SLEWED => self.handle_mount_event(event).await,
            event_types::GUIDER_START | event_types::GUIDER_DITHER => {
                self.handle_guider_event(event).await
            }
            event_types::SEQUENCE_STARTING
            | event_types::SEQUENCE_FINISHED
            | event_types::SEQUENCE_ENTITY_FAILED => self.handle_sequence_event(event).await,
            event_types::ROTATOR_SYNCED => self.handle_rotator_synced(event).await,
            event_types::FOCUSER_USER_FOCUSED => self.handle_focuser_user_focused(event).await,
            event_types::IMAGE_SAVE => {} // Handled in image polling
            _ => self.handle_generic_event(event).await,
        }
    }

    /// Filter wheel change events from NINA sometimes arrive with empty Name/Id
    /// arrays. When that happens, fetch the live filterwheel state to recover
    /// the actual current filter, and use the cached previous filter for the
    /// 'from' side. Always update the cache after handling.
    async fn handle_filterwheel_changed(&mut self, event: &Event) {
        let (mut new, mut previous) =
            if let Some(EventDetails::FilterWheelChange { new, previous }) = &event.details {
                (new.clone(), previous.clone())
            } else {
                return;
            };

        if new.is_unknown() {
            match self.source.get_filterwheel_info().await {
                Ok(info) => {
                    if let Some(selected) = info.response.selected_filter {
                        new = selected;
                    }
                }
                Err(e) => eprintln!("Failed to enrich filterwheel info: {e}"),
            }
        }

        if previous.is_unknown()
            && let Some(cached) = &self.state.last_filter
        {
            previous = cached.clone();
        }

        // No useful change to report (same filter, both known).
        if !new.is_unknown() && !previous.is_unknown() && new.name == previous.name {
            self.state.last_filter = Some(new);
            return;
        }

        if !new.is_unknown() {
            self.state.last_filter = Some(new.clone());
        }

        if event.chat_enabled && self.chat_manager.service_count() > 0 {
            self.send_filterwheel_change_notification(event, &previous, &new)
                .await;
        }
    }

    async fn send_filterwheel_change_notification(
        &self,
        event: &Event,
        previous: &FilterInfo,
        new: &FilterInfo,
    ) {
        let fmt = |f: &FilterInfo| {
            if f.is_unknown() {
                "(unknown)".to_string()
            } else {
                format!("{} (ID: {})", f.name, f.id)
            }
        };
        let arrow = format!(
            "{} → {}",
            if previous.is_unknown() {
                "(unknown)".to_string()
            } else {
                previous.name.clone()
            },
            if new.is_unknown() {
                "(unknown)".to_string()
            } else {
                new.name.clone()
            },
        );

        let message = ChatMessage::new(&self.titled("🔄 Filter Changed"))
            .color(colors::BLUE)
            .field("Time", &event.time, false)
            .field("Filter Change", &arrow, false)
            .field("Previous", &fmt(previous), true)
            .field("New", &fmt(new), true);

        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn handle_ts_targetstart(&mut self, event: &Event) {
        // `ChatEnabled` is a local N.I.N.A. transmission boundary. Current
        // plugins filter disabled Target Scheduler events before they cross
        // Direct, but older payload-v3 peers can still include the flag and
        // target details. Never retain those details in status state.
        if !event.chat_enabled {
            return;
        }

        if let Some(EventDetails::TargetStart {
            target_name,
            coordinates,
            project_name,
            rotation,
            target_end_time,
        }) = &event.details
        {
            if target_name == "Sequential Instruction Set" {
                return;
            }

            let new_target = TargetInfo {
                name: target_name.clone(),
                source: TargetSource::TsTargetStart,
                coordinates: coordinates.clone(),
                project: project_name.clone(),
                rotation: *rotation,
                target_end_time: target_end_time
                    .as_deref()
                    .and_then(|end| parse_nina_timestamp_with_context(end, Some(&event.time))),
            };

            let old_target = self.state.current_target.clone();
            let target_changed = old_target
                .as_ref()
                .map(|t| t.name != new_target.name)
                .unwrap_or(true);

            self.state.current_target = Some(new_target.clone());
            if target_changed {
                println!("[TS-TARGETSTART] Target: {}", target_name);

                if event.chat_enabled && self.chat_manager.service_count() > 0 {
                    if let Some(old) = old_target {
                        self.send_target_change_notification(&old, &new_target)
                            .await;
                    } else {
                        self.send_target_start_notification(&new_target).await;
                    }
                }
            }
        }
    }

    async fn handle_autofocus_finished(&mut self, event: &Event) {
        println!("[AUTOFOCUS FINISHED] {}", event.time);
        println!("Queued autofocus results for delivery after the poll cycle.");

        let (report_timestamp, filter, position, temperature) = match &event.details {
            Some(EventDetails::AutofocusFinished {
                report_timestamp,
                filter,
                position,
                temperature,
            }) => (
                Some(report_timestamp.clone()),
                filter.clone(),
                *position,
                *temperature,
            ),
            _ => (None, None, None, None),
        };
        let queued_at = TokioInstant::now();
        // Direct exposes only N.I.N.A.'s latest completed report. Once a newer
        // completion arrives, an older pending delivery can no longer be
        // fetched reliably and would otherwise retry against the new report
        // before emitting a false "Report Unavailable" warning. Prefer the
        // newest truthful result and supersede older undelivered completions
        // silently.
        if !self.pending_autofocus_deliveries.is_empty() {
            println!(
                "Superseded {} pending autofocus result(s) with the newer completion.",
                self.pending_autofocus_deliveries.len()
            );
            self.pending_autofocus_deliveries.clear();
        }
        self.pending_autofocus_deliveries
            .push(PendingAutofocusDelivery {
                event_time: event.time.clone(),
                report_timestamp,
                filter,
                position,
                temperature,
                attempts: 0,
                queued_at,
                not_ready_since: None,
                next_attempt_at: queued_at,
                retry_delay: self.autofocus_retry.initial_delay,
                retry: self.autofocus_retry,
            });
    }

    /// Run at most one due report read after the regular source poll cycle.
    /// Keeping the read on the updater task avoids racing the serialized local
    /// Direct pipe, while the potentially slower graph/chat work remains in an
    /// updater-owned task that is aborted when this updater is dropped.
    async fn poll_autofocus_delivery(&mut self) {
        let now = TokioInstant::now();
        let Some(index) = self
            .pending_autofocus_deliveries
            .iter()
            .rposition(|delivery| delivery.next_attempt_at <= now)
        else {
            return;
        };
        let mut delivery = self.pending_autofocus_deliveries.remove(index);
        if self
            .expire_autofocus_delivery_if_timed_out(
                &delivery,
                now,
                "the overall report-retry deadline elapsed before the next read",
            )
            .await
        {
            return;
        }

        let remaining = delivery
            .retry
            .overall_timeout
            .saturating_sub(now.saturating_duration_since(delivery.queued_at));
        let result = match tokio::time::timeout(remaining, self.source.get_last_autofocus()).await {
            Ok(result) => result,
            Err(_) => {
                self.expire_autofocus_delivery(
                    &delivery,
                    "the Direct autofocus read did not finish before the overall deadline",
                )
                .await;
                return;
            }
        };
        if let Err(RigSourceError::Unavailable { reason, .. }) = &result {
            // A transport outage says nothing about whether N.I.N.A. has
            // finished publishing this report. Preserve the attempt budget
            // and resume after the updater's source can answer again, but keep
            // the delivery's absolute lifetime bounded.
            let now = TokioInstant::now();
            if self
                .expire_autofocus_delivery_if_timed_out(&delivery, now, reason)
                .await
            {
                return;
            }
            let remaining = delivery
                .retry
                .overall_timeout
                .saturating_sub(now.saturating_duration_since(delivery.queued_at));
            let initial_delay = delivery.retry.initial_delay.max(Duration::from_millis(1));
            let delay = delivery.retry_delay.max(initial_delay).min(remaining);
            eprintln!(
                "[{}] Autofocus report for {} is paused while N.I.N.A. is unavailable: {reason}; retrying in {delay:?}",
                self.telescope_name, delivery.event_time
            );
            delivery.next_attempt_at = now + delay;
            delivery.retry_delay = backoff_delay(delay, initial_delay, delivery.retry.max_delay);
            self.pending_autofocus_deliveries.push(delivery);
            return;
        }

        match result {
            Ok(autofocus_data)
                if autofocus_report_matches(
                    delivery.report_timestamp.as_deref(),
                    &autofocus_data.response.timestamp,
                ) =>
            {
                // Keep graph rendering and delivery inside the updater task.
                // Awaiting the updater handle is then a complete cancellation
                // barrier for route removal or consent revocation: no detached
                // child can post after that barrier returns.
                AutofocusNotificationTask {
                    chat_manager: self.chat_manager.clone(),
                    chat_target: self.chat_target.clone(),
                    telescope_name: self.telescope_name.clone(),
                    autofocus_data,
                }
                .run()
                .await;
            }
            Ok(autofocus_data) => {
                delivery.attempts += 1;
                let expected = delivery
                    .report_timestamp
                    .as_deref()
                    .unwrap_or("(legacy event)");
                let reason = format!(
                    "received report {} instead of {expected}",
                    autofocus_data.response.timestamp
                );
                self.retry_autofocus_delivery(delivery, &reason).await;
            }
            Err(RigSourceError::NotReady { reason, .. }) => {
                self.retry_autofocus_not_ready(delivery, &reason).await;
            }
            Err(error) => {
                delivery.attempts += 1;
                self.retry_autofocus_delivery(delivery, &error.to_string())
                    .await
            }
        }
    }

    async fn expire_autofocus_delivery_if_timed_out(
        &self,
        delivery: &PendingAutofocusDelivery,
        now: TokioInstant,
        reason: &str,
    ) -> bool {
        let timeout = delivery.retry.overall_timeout;
        let elapsed = now.saturating_duration_since(delivery.queued_at);
        if elapsed < timeout {
            return false;
        }

        self.expire_autofocus_delivery(delivery, reason).await;
        true
    }

    async fn expire_autofocus_delivery(&self, delivery: &PendingAutofocusDelivery, reason: &str) {
        let timeout = delivery.retry.overall_timeout;
        eprintln!(
            "[{}] Autofocus report for {} exceeded its overall retry window of {timeout:?}: {reason}",
            self.telescope_name, delivery.event_time
        );
        self.send_autofocus_unavailable(delivery).await;
    }

    async fn retry_autofocus_not_ready(
        &mut self,
        mut delivery: PendingAutofocusDelivery,
        reason: &str,
    ) {
        let now = TokioInstant::now();
        let not_ready_since = *delivery.not_ready_since.get_or_insert(now);
        let elapsed = now.duration_since(not_ready_since);
        let timeout = delivery.retry.not_ready_timeout;
        if elapsed >= timeout {
            eprintln!(
                "[{}] Autofocus report for {} remained unavailable for {timeout:?}: {reason}",
                self.telescope_name, delivery.event_time
            );
            self.send_autofocus_unavailable(&delivery).await;
            return;
        }
        if self
            .expire_autofocus_delivery_if_timed_out(&delivery, now, reason)
            .await
        {
            return;
        }

        let remaining = timeout.saturating_sub(elapsed);
        let overall_remaining = delivery
            .retry
            .overall_timeout
            .saturating_sub(now.saturating_duration_since(delivery.queued_at));
        let delay = delivery.retry_delay.min(remaining).min(overall_remaining);
        eprintln!(
            "[{}] Autofocus report for {} is not ready after {elapsed:?}: {reason}; retrying in {delay:?}",
            self.telescope_name, delivery.event_time
        );
        delivery.next_attempt_at = now + delay;
        delivery.retry_delay = backoff_delay(
            delivery.retry_delay,
            delivery.retry.initial_delay,
            delivery.retry.max_delay,
        );
        self.pending_autofocus_deliveries.push(delivery);
    }

    async fn retry_autofocus_delivery(
        &mut self,
        mut delivery: PendingAutofocusDelivery,
        reason: &str,
    ) {
        let attempts = delivery.retry.max_attempts.max(1);
        if delivery.attempts >= attempts {
            eprintln!(
                "[{}] Failed to fetch autofocus report for {} after {attempts} attempts: {reason}",
                self.telescope_name, delivery.event_time
            );
            self.send_autofocus_unavailable(&delivery).await;
            return;
        }

        let now = TokioInstant::now();
        if self
            .expire_autofocus_delivery_if_timed_out(&delivery, now, reason)
            .await
        {
            return;
        }
        let overall_remaining = delivery
            .retry
            .overall_timeout
            .saturating_sub(now.saturating_duration_since(delivery.queued_at));
        let delay = delivery.retry_delay.min(overall_remaining);
        eprintln!(
            "[{}] Autofocus report for {} was not available on attempt {}/{attempts}: {reason}; retrying in {delay:?}",
            self.telescope_name, delivery.event_time, delivery.attempts
        );
        delivery.next_attempt_at = now + delay;
        delivery.retry_delay = backoff_delay(
            delay,
            delivery.retry.initial_delay,
            delivery.retry.max_delay,
        );
        self.pending_autofocus_deliveries.push(delivery);
    }

    async fn send_autofocus_unavailable(&self, delivery: &PendingAutofocusDelivery) {
        if self.chat_manager.service_count() == 0 {
            return;
        }
        let mut message =
            ChatMessage::new(&self.titled("⚠️ Autofocus Completed · Report Unavailable"))
                .color(colors::ORANGE)
                .field("Time", &delivery.event_time, false)
                .field(
                    "Report",
                    "N.I.N.A. completed autofocus, but the saved report could not be retrieved.",
                    false,
                );
        if let Some(filter) = delivery.filter.as_deref() {
            let filter = filter.trim();
            message = message.field(
                "Filter",
                if filter.is_empty() {
                    "No filter"
                } else {
                    filter
                },
                true,
            );
        }
        if let Some(position) = delivery.position {
            message = message.field("Position", &format!("{position:.0}"), true);
        }
        if let Some(temperature) = delivery.temperature {
            message = message.field("Temperature", &format!("{temperature:.1} °C"), true);
        }
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn handle_mount_event(&self, event: &Event) {
        if self.chat_manager.service_count() > 0 {
            self.send_mount_event_notification(event).await;
        }
    }

    async fn handle_guider_event(&self, event: &Event) {
        if self.chat_manager.service_count() == 0 {
            return;
        }
        let info = self.source.get_guider_info().await.ok();
        self.send_guider_event_notification(event, info.as_ref())
            .await;
    }

    async fn handle_sequence_event(&self, event: &Event) {
        if self.chat_manager.service_count() == 0 {
            return;
        }
        // Use the freshest sequence we have. The poll_sequence loop refreshes
        // this every cycle, so it's typically <interval seconds stale.
        self.send_sequence_event_notification(event).await;
    }

    /// ROTATOR-SYNCED ships only `{Time, Event}`. Query the Direct equipment
    /// snapshot to surface angle and mechanical position in the notification.
    async fn handle_rotator_synced(&self, event: &Event) {
        if self.chat_manager.service_count() == 0 {
            return;
        }
        let info = self.source.get_rotator_info().await.ok();
        self.send_rotator_synced_notification(event, info.as_ref())
            .await;
    }

    /// FOCUSER-USER-FOCUSED ships only `{Time, Event}` (someone tweaked focus
    /// manually). Query the Direct equipment snapshot for position and
    /// temperature.
    async fn handle_focuser_user_focused(&self, event: &Event) {
        if self.chat_manager.service_count() == 0 {
            return;
        }
        let info = self.source.get_focuser_info().await.ok();
        self.send_focuser_user_focused_notification(event, info.as_ref())
            .await;
    }

    async fn handle_generic_event(&self, event: &Event) {
        if self.chat_manager.service_count() > 0 {
            self.send_generic_event_notification(event).await;
        }
    }

    /// Returns whether the Direct source responded (see [`Self::poll_events`]).
    pub async fn poll_sequence(&mut self) -> bool {
        if !self.source.capabilities().sequence {
            return false;
        }
        match self.source.get_sequence().await {
            Ok(sequence) => {
                let new_sequence_target = extract_current_target_with_delivery(&sequence);
                let new_meridian_flip_time = extract_meridian_flip_time(&sequence);
                let operations = extract_sequence_operations(&sequence);
                let suppressed_operation_keys =
                    extract_suppressed_sequence_operation_keys(&sequence);
                let camera = self.camera_snapshot_for(&operations).await;
                self.reconcile_sequence_operations(
                    operations,
                    suppressed_operation_keys,
                    camera,
                    true,
                )
                .await;

                self.state.meridian_flip_time = new_meridian_flip_time;
                self.state.sequence_container_counts = Some(sequence_container_counts(&sequence));

                if let Some((old_target, new_target)) =
                    self.reconcile_sequence_target(new_sequence_target)
                {
                    println!("[SEQUENCE TARGET] {}", new_target.name);

                    if self.chat_manager.service_count() > 0 {
                        if let Some(old) = old_target {
                            self.send_target_change_notification(&old, &new_target)
                                .await;
                        } else {
                            self.send_target_start_notification(&new_target).await;
                        }
                    }
                }
                true
            }
            Err(e) => {
                if self.state.sequence_container_counts.is_none() {
                    eprintln!("Error fetching sequence (will retry silently): {e}");
                }
                false
            }
        }
    }

    /// Apply the target identity carried by a sequence snapshot.
    ///
    /// An explicit `ChatEnabled: false` is also a revocation signal for a
    /// target retained from an earlier poll or Target Scheduler event. Clear
    /// it immediately so live status and slash commands cannot keep exposing
    /// the old name. `None` remains the legacy "no active target observed"
    /// case and does not erase a Target Scheduler override.
    fn reconcile_sequence_target(
        &mut self,
        projection: Option<(String, bool)>,
    ) -> Option<(Option<TargetInfo>, TargetInfo)> {
        self.expire_target_scheduler_target(Utc::now());
        let (target_name, chat_enabled) = projection?;
        if !chat_enabled {
            self.state.current_target = None;
            return None;
        }
        if self
            .state
            .current_target
            .as_ref()
            .is_some_and(|target| target.source == TargetSource::TsTargetStart)
        {
            return None;
        }

        let new_target = TargetInfo {
            name: target_name,
            source: TargetSource::Sequence,
            coordinates: None,
            project: None,
            rotation: None,
            target_end_time: None,
        };
        let old_target = self.state.current_target.clone();
        if old_target
            .as_ref()
            .is_some_and(|target| target.name == new_target.name)
        {
            return None;
        }

        self.state.current_target = Some(new_target.clone());
        Some((old_target, new_target))
    }

    fn expire_target_scheduler_target(&mut self, now: DateTime<Utc>) -> bool {
        let expired = self.state.current_target.as_ref().is_some_and(|target| {
            target.source == TargetSource::TsTargetStart
                && target
                    .target_end_time
                    .is_some_and(|end| end.with_timezone(&Utc) <= now)
        });
        if expired {
            self.state.current_target = None;
            self.state.last_status_fingerprint = None;
        }
        expired
    }

    /// Returns whether the Direct source responded (see [`Self::poll_events`]).
    pub async fn poll_images(&mut self) -> bool {
        if !self.source.capabilities().image_history {
            return false;
        }
        match self.source.get_all_image_history().await {
            Ok(images) => {
                let privacy_boundary = images
                    .response
                    .iter()
                    .rposition(|image| !image.chat_enabled);
                for (index, image) in images.response.iter().enumerate() {
                    if privacy_boundary.is_some_and(|boundary| index < boundary) {
                        continue;
                    }
                    if !image.chat_enabled {
                        if !self.state.has_seen_image(image) {
                            self.revoke_state_for_disabled_event(event_types::IMAGE_SAVE, None);
                        }
                        continue;
                    }
                    if !self.state.has_seen_image(image) {
                        if image.chat_enabled {
                            self.print_new_image(image);
                        }

                        if image.chat_enabled && self.chat_manager.service_count() > 0 {
                            self.handle_new_image(image, index).await;
                        }
                    }
                }
                true
            }
            Err(e) => {
                eprintln!("Error fetching images: {e}");
                false
            }
        }
    }

    async fn handle_new_image(&mut self, image: &ImageMetadata, index: usize) {
        let should_send = match self.state.last_image_time {
            None => true,
            Some(last_time) => last_time.elapsed() >= self.image_cooldown,
        };

        if should_send {
            self.send_image_notification(image, index, self.state.skipped_images_count)
                .await;
            self.state.last_image_time = Some(Instant::now());
            if self.state.skipped_images_count > 0 {
                println!(
                    "  Sent image notification (including {} skipped images)",
                    self.state.skipped_images_count
                );
            }
            self.state.skipped_images_count = 0;
        } else {
            self.state.skipped_images_count += 1;
            let remaining = self.image_cooldown - self.state.last_image_time.unwrap().elapsed();
            println!(
                "  Skipping chat notification (cooldown: {:.0}s remaining)",
                remaining.as_secs_f32()
            );
        }
    }

    fn print_new_event(&self, event: &Event) {
        println!("[NEW EVENT] {}", event.time);
        println!("  Type: {}", event.event);
        if let Some(details) = &event.details {
            println!("  Details: {details:?}");
        }
        println!();
    }

    fn print_new_image(&self, image: &ImageMetadata) {
        println!("[NEW IMAGE] {}", image.date);
        if let Some(target) = &self.state.current_target {
            println!("  Target: {}", target.name);
        }
        if let Some(meridian_flip_hours) = self.state.meridian_flip_time {
            let formatted_time = meridian_flip_time_formatted_with_clock(meridian_flip_hours);
            println!("  Meridian flip in: {formatted_time}");
        }
        println!("  Camera: {}", image.camera_name);
        println!("  Type: {}", image.image_type);
        println!("  Filter: {}", image.filter);
        println!("  Exposure: {}s", image.exposure_time);
        println!("  Temperature: {:.1}°C", image.temperature);
        println!("  Stars: {}, HFR: {:.2}", image.stars, image.hfr);
        println!("  RMS: {}", image.rms_text);
        println!();
    }

    fn display_autofocus_results(af: &AutofocusResponse) {
        if !af.success {
            println!("❌ Autofocus failed: {}", af.error);
            return;
        }

        let af_data = &af.response;
        let success_indicator = if af.is_successful() { "✅" } else { "⚠️" };

        println!("{success_indicator} Autofocus Summary");
        println!("  Filter: {}", af_data.filter_name());
        println!("  Mode: {}", af_data.method_summary());
        if af_data.temperature.is_finite() {
            println!("  Temperature: {:.1}°C", af_data.temperature);
        } else {
            println!("  Temperature: n/a");
        }
        println!("  Duration: {}", af_data.duration);
        println!(
            "  Position Change: {:.0}",
            af_data.calculated_focus_point.position - af_data.initial_focus_point.position
        );
        if let Some(r_squared) = af_data.fit_quality_summary() {
            println!("  Fit R-squared: {r_squared}");
        } else {
            println!("  Fit R-squared: n/a");
        }
        if let Some(stars) = af_data.accepted_star_count_summary() {
            println!("  Accepted stars per point: {stars}");
        }
        if let Some(uncertainty) = af_data.hyperbolic_minimum_std_error {
            println!("  Focus precision: ±{uncertainty:.2} steps");
        }
        if let Some(stability) = af_data.hyperbolic_leave_one_out_std_error {
            println!("  Leave-one-out stability: {stability:.2} steps");
        }
        if let Some(chi_squared) = af_data.hyperbolic_reduced_chi_squared {
            println!("  Reduced chi-squared: {chi_squared:.4}");
        }
        if let Some(region) = af_data.region_summary() {
            println!("  Detection region: {region}");
        }
    }

    // Chat notification methods
    async fn send_welcome_message(&self) {
        let mut message =
            ChatMessage::new(&self.titled("🚀 Chatstronomy — observatory monitor started"))
                .color(colors::GREEN);

        // Inferred NINA state from event history
        let summary = self.format_startup_status();
        if !summary.is_empty() {
            message = message.field("Status", &summary, false);
        }

        // Add current target information
        if let Some(target) = &self.state.current_target {
            message = message.field("Current Target", &target.name, false);

            if let Some(project) = &target.project {
                message = message.field("Project", project, true);
            }

            if let Some(coords) = &target.coordinates
                && let Some(s) = coords.display()
            {
                message = message.field("Coordinates", &s, false);
            }

            if let Some(rotation) = &target.rotation {
                message = message.field("Rotation", &format!("{}°", rotation), true);
            }

            let source_text = match target.source {
                TargetSource::TsTargetStart => "TS-TARGETSTART event",
                TargetSource::Sequence => "Sequence file",
            };
            message = message.field("Target Source", source_text, true);
        } else {
            message = message.field("Current Target", "None detected", false);
        }

        if let Some(filter) = &self.state.last_filter
            && !filter.is_unknown()
        {
            message = message.field("Last Filter", &filter.name, true);
        }

        // Add baseline information
        message = message
            .field(
                "Events in History",
                &self.state.events_seen.len().to_string(),
                true,
            )
            .field(
                "Images in History",
                &self.state.images_seen.len().to_string(),
                true,
            )
            .field(
                "Chat Services",
                &self.chat_manager.service_count().to_string(),
                true,
            );

        // Add meridian flip info if available
        self.add_meridian_flip_info(&mut message);

        // Add mount info
        self.add_mount_info(&mut message).await;

        message = message.footer(&format!(
            "{} {} — ready to monitor telescope events and images",
            crate::version::WORDMARK,
            crate::version::VERSION_STRING
        ));

        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    /// Build a one-paragraph summary of NINA's state, inferred from recent events.
    /// Includes wait timer, sequence running, mount state, guider state.
    fn format_startup_status(&self) -> String {
        self.format_startup_status_at(Utc::now())
    }

    fn format_startup_status_at(&self, now: DateTime<Utc>) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(end) = self
            .state
            .current_target
            .as_ref()
            .and_then(|target| target.target_end_time)
        {
            parts.push(format!(
                "🎯 Target scheduled until {}",
                end.format("%Y-%m-%d %H:%M %Z")
            ));
        }

        let mut operations = self
            .state
            .sequence_operations
            .values()
            .filter(|tracked| tracked.operation.chat_enabled)
            .collect::<Vec<_>>();
        operations.sort_by(|left, right| left.operation.key.cmp(&right.operation.key));
        for tracked in operations {
            match &tracked.operation.kind {
                SequenceOperationKind::CameraCooling {
                    target_temperature, ..
                } => {
                    let detail = tracked
                        .camera
                        .as_ref()
                        .filter(|camera| camera.temperature.is_finite())
                        .map_or_else(
                            || format!("target {target_temperature:.1} °C"),
                            |camera| {
                                let power = if camera.cooler_power.is_finite() {
                                    format!(", cooler {:.0}%", camera.cooler_power)
                                } else {
                                    String::new()
                                };
                                format!(
                                    "{:.1} → {target_temperature:.1} °C{power}",
                                    camera.temperature
                                )
                            },
                        );
                    parts.push(format!("❄️ Camera cooling ({detail})"));
                }
                SequenceOperationKind::CameraWarming { minimum_duration } => {
                    let mut detail = tracked
                        .camera
                        .as_ref()
                        .filter(|camera| camera.temperature.is_finite())
                        .map(|camera| format!("{:.1} °C", camera.temperature))
                        .unwrap_or_else(|| "temperature unavailable".to_string());
                    if let Some(duration) = minimum_duration {
                        detail.push_str(&format!(", minimum {}", format_duration(*duration)));
                    }
                    parts.push(format!("🌡️ Camera warming ({detail})"));
                }
                SequenceOperationKind::TimeWait { .. } => {
                    if let Some(end) = tracked.estimated_end {
                        let remaining = end.signed_duration_since(now);
                        if remaining > chrono::Duration::zero() {
                            parts.push(format!(
                                "⏳ Sequence wait until {} ({} remaining)",
                                end.format("%H:%M UTC"),
                                format_duration(remaining)
                            ));
                        } else {
                            parts.push("⏳ Sequence wait reached its scheduled time".to_string());
                        }
                    } else {
                        parts.push("⏳ Timed wait in progress".to_string());
                    }
                }
                SequenceOperationKind::AstronomicalWait {
                    target_altitude_degrees,
                    current_altitude_degrees,
                    comparator,
                    expected_time,
                } => {
                    let current =
                        current_altitude_degrees.map(|value| format!("current {value:.2}°"));
                    let target = target_altitude_degrees.map(|value| {
                        format!(
                            "target {}{value:.2}°",
                            comparator
                                .as_deref()
                                .map(|comparison| format!("{comparison} "))
                                .unwrap_or_default()
                        )
                    });
                    let expected = expected_time
                        .as_deref()
                        .map(|value| format!("expected {value}"));
                    let details = [current, target, expected]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(", ");
                    parts.push(if details.is_empty() {
                        format!("🌌 {}", tracked.operation.name)
                    } else {
                        format!("🌌 {} ({details})", tracked.operation.name)
                    });
                }
                SequenceOperationKind::SafetyWait {
                    is_safe,
                    wait_interval,
                } => {
                    let state = match is_safe {
                        Some(true) => "safe; resolving",
                        Some(false) => "unsafe",
                        None => "monitor disconnected",
                    };
                    let interval = wait_interval
                        .map(|duration| format!(", checks every {}", format_duration(duration)))
                        .unwrap_or_default();
                    parts.push(format!(
                        "🛡️ Waiting for safe conditions ({state}{interval})"
                    ));
                }
                SequenceOperationKind::ConditionWait { wait_interval } => {
                    let interval = wait_interval
                        .map(|duration| format!(", checks every {}", format_duration(duration)))
                        .unwrap_or_default();
                    parts.push(format!("⏳ Waiting for a sequence condition{interval}"));
                }
                SequenceOperationKind::ManualWait => {
                    parts.push("⏸️ Waiting for manual sequence resume".to_string());
                }
                SequenceOperationKind::MountSlew { coordinates, .. } => {
                    parts.push(coordinates.as_ref().map_or_else(
                        || "🔭 Mount slew in progress".to_string(),
                        |coordinates| format!("🔭 Slewing to {}", coordinates.display()),
                    ));
                }
                SequenceOperationKind::MountCenter {
                    coordinates,
                    output,
                    ..
                } => {
                    let target = coordinates
                        .as_ref()
                        .map_or_else(String::new, |coordinates| {
                            format!(" on {}", coordinates.display())
                        });
                    let solve = output
                        .as_ref()
                        .and_then(|output| output.success)
                        .map_or_else(String::new, |success| {
                            if success {
                                "; latest plate solve succeeded".to_string()
                            } else {
                                "; latest plate solve failed".to_string()
                            }
                        });
                    parts.push(format!("🎯 Centering{target}{solve}"));
                }
                SequenceOperationKind::PlateSolve {
                    coordinates,
                    output,
                    ..
                } => {
                    let target = coordinates
                        .as_ref()
                        .map_or_else(String::new, |coordinates| {
                            format!(" near {}", coordinates.display())
                        });
                    let result = output
                        .as_ref()
                        .and_then(|output| output.success)
                        .map_or_else(String::new, |success| {
                            if success {
                                "; latest result succeeded".to_string()
                            } else {
                                "; latest result failed".to_string()
                            }
                        });
                    parts.push(format!("🔎 Plate solving{target}{result}"));
                }
            }
        }

        if let Some(end) = self.state.scheduler_wait_end() {
            let remaining = end.signed_duration_since(now);
            if remaining > chrono::Duration::zero() {
                parts.push(format!(
                    "⏳ Target Scheduler wait until {} ({} remaining)",
                    end.format("%H:%M UTC"),
                    format_duration(remaining)
                ));
            } else {
                parts.push(
                    "⏳ Target Scheduler reached its scheduled time; awaiting its next transition"
                        .to_string(),
                );
            }
        }

        if self.state.sequence_running {
            parts.push("▶️ Sequence running".to_string());
        } else if self.state.sequence_failure.is_none() {
            match self.state.sequence_outcome.as_deref() {
                Some("completed") => parts.push("🏁 Sequence completed".to_string()),
                Some("stopped") => parts.push("⏹️ Sequence stopped".to_string()),
                Some("cancelled_or_not_started") => {
                    parts.push("⏹️ Sequence cancelled or not started".to_string())
                }
                Some("ended") => parts.push("🏁 Sequence ended".to_string()),
                _ => {}
            }
        }
        if let Some(failure) = &self.state.sequence_failure {
            let entity = if failure.entity.trim().is_empty() {
                "Sequence item"
            } else {
                &failure.entity
            };
            parts.push(format!(
                "❌ {} failed: {}",
                truncate_chat_title(entity),
                truncate_chat_value(&failure.error)
            ));
        }

        if let Some(safety) = self.state.safety_state.status_text() {
            parts.push(safety.to_string());
        }

        let mut dome_detail = Vec::new();
        if let Some(connected) = self.state.dome_connected {
            dome_detail.push(
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
                .to_string(),
            );
        }
        if let Some(open) = self.state.dome_shutter_open {
            dome_detail.push(
                if open {
                    "shutter open"
                } else {
                    "shutter closed"
                }
                .to_string(),
            );
        }
        if self.state.dome_parked == Some(true) {
            dome_detail.push("parked".to_string());
        } else if self.state.dome_homed == Some(true) {
            dome_detail.push("homed".to_string());
        }
        if let Some(azimuth) = self.state.dome_azimuth {
            dome_detail.push(format!("azimuth {azimuth:.2}°"));
        }
        if !dome_detail.is_empty() {
            let icon = if self.state.dome_connected == Some(false) {
                "⚠️"
            } else {
                "🏠"
            };
            parts.push(format!("{icon} Dome · {}", dome_detail.join(", ")));
        }

        let mut flat_detail = Vec::new();
        if let Some(connected) = self.state.flat_connected {
            flat_detail.push(
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
                .to_string(),
            );
        }
        if let Some(cover) = self.state.flat_cover_state.as_deref() {
            flat_detail.push(format!("cover {cover}"));
        }
        if let Some(on) = self.state.flat_light_on {
            flat_detail.push(if on { "light on" } else { "light off" }.to_string());
        }
        if let Some(brightness) = self.state.flat_brightness {
            flat_detail.push(format!("brightness {brightness}"));
        }
        if !flat_detail.is_empty() {
            let icon = if self.state.flat_connected == Some(false) {
                "⚠️"
            } else {
                "💡"
            };
            parts.push(format!("{icon} Flat panel · {}", flat_detail.join(", ")));
        }
        let mut weather_detail = Vec::new();
        if let Some(connected) = self.state.weather_connected {
            weather_detail.push(
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
                .to_string(),
            );
        }
        if self.state.weather_high_wind == Some(true) {
            weather_detail.push(
                match self.state.weather_high_wind_threshold_meters_per_second {
                    Some(threshold) => format!("HIGH WIND (limit {threshold:.1} m/s)"),
                    None => "HIGH WIND".to_string(),
                },
            );
        }
        let mut current_weather = self.state.weather_conditions.clone().unwrap_or_default();
        let weather_has_wind = self
            .state
            .weather_conditions
            .as_ref()
            .is_some_and(WeatherConditions::has_wind_reading);
        let high_wind_has_wind = self
            .state
            .weather_high_wind_conditions
            .as_ref()
            .is_some_and(WeatherConditions::has_wind_reading);
        let high_wind_reading_is_current = high_wind_has_wind
            && (!weather_has_wind
                || match (
                    self.state.weather_conditions_at,
                    self.state.weather_high_wind_conditions_at,
                ) {
                    (Some(weather), Some(high_wind)) => high_wind >= weather,
                    (None, Some(_)) => true,
                    (Some(_), None) => false,
                    (None, None) => self.state.weather_high_wind == Some(true),
                });
        if high_wind_reading_is_current
            && let Some(high_wind_conditions) = &self.state.weather_high_wind_conditions
        {
            current_weather.merge_available(high_wind_conditions);
        }
        if let Some(summary) = current_weather.status_summary() {
            weather_detail.push(format!("last report: {summary}"));
        }
        if !weather_detail.is_empty() {
            let icon = if self.state.weather_connected == Some(false)
                || self.state.weather_high_wind == Some(true)
            {
                "⚠️"
            } else {
                "🌦️"
            };
            parts.push(format!("{icon} Weather · {}", weather_detail.join(", ")));
        }
        if let Some(connected) = self.state.switch_connected {
            parts.push(format!(
                "{} Switch device {}",
                if connected { "🔌" } else { "⚠️" },
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
            ));
        }

        if let Some(ev) = &self.state.last_mount_event {
            let label = match ev.as_str() {
                event_types::MOUNT_PARKED => "🅿️ Mount parked",
                event_types::MOUNT_UNPARKED => "🔭 Mount unparked",
                event_types::MOUNT_HOMED => "🏠 Mount homed",
                event_types::MOUNT_BEFORE_FLIP => "🔄 Mount pre-flip",
                event_types::MOUNT_AFTER_FLIP => "✅ Mount post-flip",
                event_types::MOUNT_CENTER => "🎯 Centering started",
                event_types::MOUNT_SLEWED => "🔭 Mount slew ended",
                _ => "🔭 Mount active",
            };
            parts.push(label.to_string());
        }

        if let Some(ev) = &self.state.last_guider_event {
            let label = match ev.as_str() {
                event_types::GUIDER_START => "🎯 Guiding",
                event_types::GUIDER_DITHER => "🎯 Dithering",
                event_types::GUIDER_STOP => "🛑 Guider stopped",
                _ => "🎯 Guider active",
            };
            parts.push(label.to_string());
        }

        parts.join("\n")
    }

    async fn send_target_change_notification(
        &self,
        old_target: &TargetInfo,
        new_target: &TargetInfo,
    ) {
        let mut message = ChatMessage::new(&self.titled("🎯 Target Change"))
            .color(colors::CYAN)
            .field("Previous Target", &old_target.name, true)
            .field("New Target", &new_target.name, true);

        if let Some(project) = &new_target.project {
            message = message.field("Project", project, true);
        }

        if let Some(coords) = &new_target.coordinates
            && let Some(s) = coords.display()
        {
            message = message.field("Coordinates", &s, false);
        }

        if let Some(rotation) = &new_target.rotation {
            message = message.field("Rotation", &format!("{}°", rotation), true);
        }
        if let Some(end) = new_target.target_end_time {
            message = message.field(
                "Scheduled End",
                &end.format("%Y-%m-%d %H:%M %Z").to_string(),
                true,
            );
        }

        self.add_meridian_flip_info(&mut message);
        self.add_mount_info(&mut message).await;
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_target_start_notification(&self, target: &TargetInfo) {
        let mut message = ChatMessage::new(&self.titled("🎯 Target Started"))
            .color(colors::GREEN)
            .field("Target", &target.name, false);

        if let Some(project) = &target.project {
            message = message.field("Project", project, true);
        }

        if let Some(coords) = &target.coordinates
            && let Some(s) = coords.display()
        {
            message = message.field("Coordinates", &s, false);
        }

        if let Some(rotation) = &target.rotation {
            message = message.field("Rotation", &format!("{}°", rotation), true);
        }
        if let Some(end) = target.target_end_time {
            message = message.field(
                "Scheduled End",
                &end.format("%Y-%m-%d %H:%M %Z").to_string(),
                true,
            );
        }

        self.add_meridian_flip_info(&mut message);
        self.add_mount_info(&mut message).await;
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_autofocus_notification_to(
        chat_manager: &ChatServiceManager,
        chat_target: &ChatTarget,
        telescope_name: &str,
        af: &AutofocusResponse,
    ) {
        if !af.success {
            return;
        }

        let af_data = &af.response;
        let color = if af.is_successful() {
            colors::GREEN
        } else {
            colors::ORANGE
        };
        let success_indicator = if af.is_successful() { "✅" } else { "⚠️" };

        let position_change =
            af_data.calculated_focus_point.position - af_data.initial_focus_point.position;
        let position_change_text = if position_change > 0.0 {
            format!("+{position_change:.0}")
        } else {
            format!("{position_change:.0}")
        };
        let temperature_text = if af_data.temperature.is_finite() {
            format!("{:.1}°C", af_data.temperature)
        } else {
            "n/a".to_string()
        };
        let fit_quality_text = af_data
            .fit_quality_summary()
            .unwrap_or_else(|| "n/a".to_string());

        let measurement_name = af_data.measurement_name();
        let mut message = ChatMessage::new(&format!(
            "[{telescope_name}] {success_indicator} Autofocus Completed"
        ))
        .color(color)
        .field("Filter", af_data.filter_name(), true)
        .field("Mode", &af_data.method_summary(), true)
        .field("Duration", &af_data.duration, true)
        .field("Temperature", &temperature_text, true)
        .field(
            "Focus Position",
            &format!("{:.0}", af_data.calculated_focus_point.position),
            true,
        )
        .field("Position Change", &position_change_text, true)
        .field(
            &format!("{measurement_name} Before"),
            &af_data
                .initial_hfr()
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "n/a".to_string()),
            true,
        )
        .field(
            &af_data.final_measurement_label(),
            &af_data
                .final_hfr()
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "n/a".to_string()),
            true,
        )
        .field("Fit R²", &fit_quality_text, true)
        .field(
            "Measurements",
            &af_data.measure_points.len().to_string(),
            true,
        )
        .footer(&format!("Focuser: {}", af_data.auto_focuser_name));

        if let Some(stars) = af_data.accepted_star_count_summary() {
            message = message.field("Accepted stars / point", &stars, true);
        }
        if let Some(uncertainty) = af_data.hyperbolic_minimum_std_error {
            message = message.field("Focus precision", &format!("±{uncertainty:.2} steps"), true);
        }
        if let Some(stability) = af_data.hyperbolic_leave_one_out_std_error {
            message = message.field("LOO stability", &format!("{stability:.2} steps"), true);
        }
        if let Some(chi_squared) = af_data.hyperbolic_reduced_chi_squared {
            message = message.field("Reduced χ²", &format!("{chi_squared:.4}"), true);
        }
        if let Some(region) = af_data.region_summary() {
            message = message.field("Detection region", &region, true);
        }
        if let Some(acceptance) = af_data.fit_acceptance_summary() {
            message = message.field("Acceptance", &acceptance, true);
        }
        if let Some(detection) = af_data.detection_summary() {
            message = message.field("Star detection", &detection, false);
        }

        // Attach the rendered autofocus graph; failures are non-fatal and
        // the notification just goes out without it.
        let attachments = match crate::charts::render_autofocus_graph_png(af_data) {
            Ok(png) => vec![ChatAttachment {
                data: png,
                filename: "autofocus.png".to_string(),
            }],
            Err(e) => {
                eprintln!("Failed to render autofocus graph: {e}");
                Vec::new()
            }
        };
        chat_manager
            .send_message_with_attachments(&message, chat_target, &attachments)
            .await;
    }

    async fn send_mount_event_notification(&self, event: &Event) {
        let (title, color) = match event.event.as_str() {
            event_types::MOUNT_BEFORE_FLIP => {
                ("🔄 Mount Preparing for Meridian Flip", colors::ORANGE)
            }
            event_types::MOUNT_AFTER_FLIP => ("✅ Mount Meridian Flip Completed", colors::GREEN),
            event_types::MOUNT_PARKED => ("🅿️ Mount Parked", colors::YELLOW),
            event_types::MOUNT_UNPARKED => ("🔭 Mount Unparked", colors::YELLOW),
            event_types::MOUNT_HOMED => ("🏠 Mount Homed", colors::CYAN),
            event_types::MOUNT_CENTER => ("🎯 Centering Started", colors::CYAN),
            event_types::MOUNT_SLEW_STARTED
                if matches!(
                    &event.details,
                    Some(EventDetails::MountSlewStarted {
                        observed_in_progress: Some(true),
                        ..
                    })
                ) =>
            {
                ("🔭 Mount Slew Recovered", colors::BLUE)
            }
            event_types::MOUNT_SLEW_STARTED => ("🔭 Mount Slew Started", colors::BLUE),
            event_types::MOUNT_SLEWED => ("🔭 Mount Slew Ended", colors::CYAN),
            _ => ("🔭 Mount Event", colors::GRAY),
        };

        let mut message = ChatMessage::new(&self.titled(title))
            .color(color)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);

        if let Some(target) = &self.state.current_target {
            message = message.field("Current Target", &target.name, true);
        }
        match &event.details {
            Some(EventDetails::MountSlewStarted {
                from,
                target,
                motion_id,
                observed_in_progress,
            }) => {
                message = add_coordinate_field(message, "From position", from);
                if let Some(target) = target {
                    message = add_coordinate_field(message, "Requested destination", target);
                }
                message =
                    add_motion_metadata(message, *motion_id, None, None, *observed_in_progress);
            }
            Some(EventDetails::MountSlewed {
                from,
                to,
                target,
                motion_id,
                duration_seconds,
                end_detection,
                observed_in_progress,
            }) => {
                message = add_coordinate_field(message, "From position", from);
                if let Some(target) = target {
                    message = add_coordinate_field(message, "Requested destination", target);
                }
                message = add_coordinate_field(message, "To position", to);
                message = add_motion_metadata(
                    message,
                    *motion_id,
                    *duration_seconds,
                    end_detection.as_deref(),
                    *observed_in_progress,
                );
            }
            _ => {}
        }

        // Motion diagnostics carry snapshots associated with their state edges
        // (or callback coordinates when capture recovered late). A live
        // query here could replace that useful evidence with a later position
        // and could stall delivery while the driver is unhealthy, so enrich
        // only the other mount events.
        if !matches!(
            event.event.as_str(),
            event_types::MOUNT_SLEW_STARTED | event_types::MOUNT_SLEWED
        ) {
            self.add_mount_info(&mut message).await;
        }
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_guider_event_notification(
        &self,
        event: &Event,
        info: Option<&crate::guider::GuiderInfoResponse>,
    ) {
        let (title, color) = match event.event.as_str() {
            event_types::GUIDER_START => ("🎯 Guiding Started", colors::BLUE),
            event_types::GUIDER_DITHER => ("🎯 Guider Dither", colors::CYAN),
            _ => ("🎯 Guider Event", colors::GRAY),
        };

        let mut message = ChatMessage::new(&self.titled(title))
            .color(color)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);

        if let Some(target) = &self.state.current_target {
            message = message.field("Current Target", &target.name, true);
        }

        if let Some(info) = info
            && info.response.connected
        {
            let g = &info.response;
            message = message.field("State", &g.state, true);
            if g.pixel_scale > 0.0 {
                message = message.field(
                    "Pixel Scale",
                    &format!("{:.3} arcsec/px", g.pixel_scale),
                    true,
                );
            }
            if let Some(rms) = &g.rms_error {
                message = message.field(
                    "RMS Error",
                    &format!(
                        "Total: {:.2}\"\nRA: {:.2}\"  Dec: {:.2}\"",
                        rms.total.arcseconds, rms.ra.arcseconds, rms.dec.arcseconds
                    ),
                    false,
                );
            }
        }

        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_sequence_event_notification(&self, event: &Event) {
        let (title, color) = match event.event.as_str() {
            event_types::SEQUENCE_STARTING => ("▶️ Sequence Starting", colors::CYAN),
            event_types::SEQUENCE_FINISHED
                if matches!(
                    &event.details,
                    Some(EventDetails::SequenceFinished {
                        outcome,
                        had_failures: false,
                        ..
                    }) if outcome == "completed"
                ) =>
            {
                ("🏁 Sequence Completed", colors::GREEN)
            }
            event_types::SEQUENCE_FINISHED
                if matches!(
                    &event.details,
                    Some(EventDetails::SequenceFinished { outcome, .. })
                        if outcome == "stopped"
                ) =>
            {
                ("⏹️ Sequence Stopped", colors::YELLOW)
            }
            event_types::SEQUENCE_FINISHED
                if matches!(
                    &event.details,
                    Some(EventDetails::SequenceFinished { outcome, .. })
                        if outcome == "cancelled_or_not_started"
                ) =>
            {
                ("⏹️ Sequence Cancelled or Not Started", colors::GRAY)
            }
            event_types::SEQUENCE_FINISHED if self.state.sequence_failure.is_some() => {
                ("❌ Sequence Ended After a Failure", colors::RED)
            }
            event_types::SEQUENCE_FINISHED => ("🏁 Sequence Ended", colors::CYAN),
            event_types::SEQUENCE_ENTITY_FAILED => ("❌ Sequence Item Failed", colors::RED),
            _ => ("📋 Sequence Event", colors::GRAY),
        };

        let mut message = ChatMessage::new(&self.titled(title))
            .color(color)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);

        if let Some(target) = &self.state.current_target {
            message = message.field("Current Target", &target.name, true);
            if let Some(coords) = &target.coordinates
                && let Some(s) = coords.display()
            {
                message = message.field("Coordinates", &s, false);
            }
        }

        if let Some((total, running)) = self.state.sequence_container_counts
            && total > 0
        {
            message = message.field(
                "Containers",
                &format!("{total} total / {running} running"),
                true,
            );
        }
        if let Some(EventDetails::SequenceEntityFailed {
            entity,
            entity_type,
            error,
        }) = &event.details
        {
            message = message
                .field("Item", &truncate_chat_value(entity), true)
                .field("Type", &truncate_chat_value(entity_type), true)
                .field("Error", &truncate_chat_value(error), false);
        } else if event.event == event_types::SEQUENCE_FINISHED
            && let Some(failure) = &self.state.sequence_failure
        {
            message = message
                .field("Failed Item", &truncate_chat_value(&failure.entity), true)
                .field("Error", &truncate_chat_value(&failure.error), false);
        }
        if let Some(EventDetails::SequenceFinished {
            outcome,
            status,
            had_failures,
        }) = &event.details
        {
            message = message
                .field("Outcome", &outcome.replace('_', " "), true)
                .field("N.I.N.A. status", status, true)
                .field(
                    "Reported failures",
                    if *had_failures { "Yes" } else { "No" },
                    true,
                );
        }

        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_rotator_synced_notification(
        &self,
        event: &Event,
        info: Option<&crate::rotator::RotatorInfoResponse>,
    ) {
        let mut message = ChatMessage::new(&self.titled("🧭 Rotator Synced"))
            .color(colors::CYAN)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);
        if let Some(info) = info
            && info.response.connected
        {
            let r = &info.response;
            message = message
                .field("Position", &format!("{:.2}°", r.position), true)
                .field(
                    "Mechanical",
                    &format!("{:.2}°", r.mechanical_position),
                    true,
                );
            if r.synced {
                message = message.field("Sync", "✅", true);
            }
        }
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_focuser_user_focused_notification(
        &self,
        event: &Event,
        info: Option<&crate::focuser::FocuserInfoResponse>,
    ) {
        let mut message = ChatMessage::new(&self.titled("🔧 Focuser User-Focused"))
            .color(colors::PURPLE)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);
        if let Some(info) = info
            && info.response.connected
        {
            let f = &info.response;
            message = message.field("Position", &f.position.to_string(), true);
            if !f.temperature.is_nan() {
                message = message.field("Temperature", &format!("{:.1}°C", f.temperature), true);
            }
            if f.temp_comp_available {
                message = message.field("Temp comp", if f.temp_comp { "on" } else { "off" }, true);
            }
        }
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_generic_event_notification(&self, event: &Event) {
        self.send_generic_event_notification_at(event, Utc::now())
            .await;
    }

    async fn send_generic_event_notification_at(&self, event: &Event, now: DateTime<Utc>) {
        let (color, title) = match &event.details {
            Some(EventDetails::RotatorMoveStarted {
                observed_in_progress: Some(true),
                ..
            }) => (
                get_event_color(&event.event),
                "🧭 Rotator move recovered".to_string(),
            ),
            Some(EventDetails::CommandFailed { command, .. }) => (
                colors::RED,
                format!("❌ Command failed · {}", truncate_chat_title(command)),
            ),
            Some(EventDetails::NinaNotification { level, header, .. }) => (
                nina_level_color(level),
                if header.trim().is_empty() {
                    "🔔 N.I.N.A. notification".to_string()
                } else {
                    format!("🔔 N.I.N.A. · {}", truncate_chat_title(header))
                },
            ),
            Some(EventDetails::NinaLog { level, .. }) => (
                nina_level_color(level),
                format!("📝 N.I.N.A. log · {}", level.to_ascii_uppercase()),
            ),
            Some(EventDetails::SafetyChanged { is_safe }) => (
                if *is_safe { colors::GREEN } else { colors::RED },
                if *is_safe {
                    "🛡️ Safety monitor reports safe".to_string()
                } else {
                    "⚠️ Safety monitor reports unsafe".to_string()
                },
            ),
            Some(EventDetails::WeatherHighWind { is_high_wind, .. }) => (
                if *is_high_wind {
                    colors::RED
                } else {
                    colors::GREEN
                },
                if *is_high_wind {
                    "⚠️ High wind reported".to_string()
                } else {
                    "✅ Wind conditions recovered".to_string()
                },
            ),
            _ => (get_event_color(&event.event), get_event_title(&event.event)),
        };

        let is_scheduler_wait = event.event == event_types::TS_WAITSTART;
        let mut message = ChatMessage::new(&self.titled(title)).color(color);
        if is_scheduler_wait {
            if let Some(event_time) = parse_nina_timestamp(&event.time) {
                message = message.occurred_at("Started", event_time.with_timezone(&Utc));
            } else {
                // Compatibility fallback for malformed legacy payloads: keep
                // the source value visible rather than silently replacing it.
                message = message.field("Started", &event.time, false);
            }
        } else {
            message = message.field("Time", &event.time, false);
        }

        // Add event-specific details
        if let Some(details) = &event.details {
            match details {
                EventDetails::FilterWheelChange { new, previous } => {
                    message = message
                        .field(
                            "Filter Change",
                            &format!("{} → {}", previous.name, new.name),
                            false,
                        )
                        .field(
                            "Previous",
                            &format!("{} (ID: {})", previous.name, previous.id),
                            true,
                        )
                        .field("New", &format!("{} (ID: {})", new.name, new.id), true);
                }
                EventDetails::TargetStart { .. } => {
                    // Already handled in handle_ts_targetstart
                    return;
                }
                EventDetails::WaitStart { wait_end_time } => {
                    if let Some(end) =
                        parse_nina_timestamp_with_context(wait_end_time, Some(&event.time))
                    {
                        message = add_wait_timing_fields(message, end.with_timezone(&Utc), now);
                    } else {
                        message = message.field("Until", wait_end_time, false);
                    }
                }
                EventDetails::AutofocusPointAdded { position, hfr } => {
                    message = message
                        .field("Position", &position.to_string(), true)
                        .field("HFR", &format!("{hfr:.3}"), true);
                }
                EventDetails::AutofocusFinished {
                    filter,
                    position,
                    temperature,
                    report_timestamp,
                } => {
                    message = message.field("Report", report_timestamp, false);
                    if let Some(filter) = filter {
                        message = message.field("Filter", filter, true);
                    }
                    if let Some(position) = position {
                        message = message.field("Position", &format!("{position:.0}"), true);
                    }
                    if let Some(temperature) = temperature {
                        message =
                            message.field("Temperature", &format!("{temperature:.1} °C"), true);
                    }
                }
                EventDetails::SafetyChanged { is_safe } => {
                    message =
                        message.field("State", if *is_safe { "Safe" } else { "Unsafe" }, true);
                }
                EventDetails::WeatherChanged {
                    changed_fields,
                    summary,
                    conditions,
                } => {
                    if !changed_fields.trim().is_empty() {
                        message =
                            message.field("Changed", &truncate_chat_value(changed_fields), false);
                    }
                    if let Some(summary) =
                        summary.as_deref().filter(|value| !value.trim().is_empty())
                    {
                        message = message.field("Summary", &truncate_chat_value(summary), false);
                    }
                    for (name, value) in conditions.chat_fields() {
                        message = message.field(name, &truncate_chat_value(&value), false);
                    }
                }
                EventDetails::WeatherHighWind {
                    is_high_wind,
                    threshold_meters_per_second,
                    conditions,
                } => {
                    message = message.field(
                        "State",
                        if *is_high_wind {
                            "Above high-wind threshold"
                        } else {
                            "Below high-wind threshold"
                        },
                        false,
                    );
                    for (name, value) in conditions.chat_fields() {
                        message = message.field(name, &truncate_chat_value(&value), false);
                    }
                    if let Some(threshold) = threshold_meters_per_second {
                        message = message.field(
                            "Configured threshold",
                            &format!("{threshold:.1} m/s"),
                            true,
                        );
                    }
                }
                EventDetails::RotatorMoveStarted {
                    position,
                    mechanical_position,
                    motion_id,
                    observed_in_progress,
                } => {
                    if let Some(position) = position {
                        message = message.field("From position", &format!("{position:.2}°"), true);
                    }
                    if let Some(position) = mechanical_position {
                        message =
                            message.field("Mechanical from", &format!("{position:.2}°"), true);
                    }
                    message =
                        add_motion_metadata(message, *motion_id, None, None, *observed_in_progress);
                }
                EventDetails::RotatorMoved {
                    from,
                    to,
                    position,
                    mechanical_from,
                    mechanical_to,
                    mechanical_position,
                    motion_id,
                    duration_seconds,
                    end_detection,
                    observed_in_progress,
                } => {
                    message = add_rotator_end_fields(
                        message,
                        &event.event,
                        *from,
                        *to,
                        *position,
                        *mechanical_from,
                        *mechanical_to,
                        *mechanical_position,
                    );
                    message = add_motion_metadata(
                        message,
                        *motion_id,
                        *duration_seconds,
                        end_detection.as_deref(),
                        *observed_in_progress,
                    );
                }
                EventDetails::MountSlewStarted {
                    from,
                    target,
                    motion_id,
                    observed_in_progress,
                } => {
                    message = add_coordinate_field(message, "From position", from);
                    if let Some(target) = target {
                        message = add_coordinate_field(message, "Requested destination", target);
                    }
                    message =
                        add_motion_metadata(message, *motion_id, None, None, *observed_in_progress);
                }
                EventDetails::MountSlewed {
                    from,
                    to,
                    target,
                    motion_id,
                    duration_seconds,
                    end_detection,
                    observed_in_progress,
                } => {
                    message = add_coordinate_field(message, "From position", from);
                    if let Some(target) = target {
                        message = add_coordinate_field(message, "Requested destination", target);
                    }
                    message = add_coordinate_field(message, "To position", to);
                    message = add_motion_metadata(
                        message,
                        *motion_id,
                        *duration_seconds,
                        end_detection.as_deref(),
                        *observed_in_progress,
                    );
                }
                EventDetails::DomeSlewed { from, to } => {
                    message = message
                        .field("From azimuth", &format!("{from:.2}°"), true)
                        .field("To azimuth", &format!("{to:.2}°"), true)
                        .field("Δ", &format!("{:+.2}°", to - from), true);
                }
                EventDetails::SequenceEntityFailed {
                    entity,
                    entity_type,
                    error,
                } => {
                    message = message
                        .field("Item", &truncate_chat_value(entity), true)
                        .field("Type", &truncate_chat_value(entity_type), true)
                        .field("Error", &truncate_chat_value(error), false);
                }
                EventDetails::SequenceFinished {
                    outcome,
                    status,
                    had_failures,
                } => {
                    message = message
                        .field("Outcome", &outcome.replace('_', " "), true)
                        .field("N.I.N.A. status", status, true)
                        .field(
                            "Reported failures",
                            if *had_failures { "Yes" } else { "No" },
                            true,
                        );
                }
                EventDetails::ImageSaveFailed {
                    stage,
                    disk_full,
                    error,
                } => {
                    message = message
                        .field("Stage", &truncate_chat_value(stage), true)
                        .field("Disk full", if *disk_full { "Yes" } else { "No" }, true)
                        .field("Error", &truncate_chat_value(error), false);
                }
                EventDetails::FlatBrightnessChanged { previous, new } => {
                    message = message
                        .field("Previous", &previous.to_string(), true)
                        .field("New", &new.to_string(), true);
                }
                EventDetails::FlatLightToggled { on } => {
                    message = message.field(
                        "Light",
                        match on {
                            Some(true) => "On",
                            Some(false) => "Off",
                            None => "State unavailable",
                        },
                        true,
                    );
                }
                EventDetails::CommandFailed { command, error } => {
                    message = message
                        .field("Command", &truncate_chat_value(command), true)
                        .field("Error", &truncate_chat_value(error), false);
                }
                EventDetails::NinaNotification {
                    level,
                    message: notification_message,
                    ..
                } => {
                    message = message.field("Level", level, true).field(
                        "Message",
                        &truncate_chat_value(notification_message),
                        false,
                    );
                }
                EventDetails::NinaLog {
                    level,
                    source,
                    member,
                    line,
                    message: log_message,
                } => {
                    let location = match (member.is_empty(), *line > 0) {
                        (false, true) => format!("{source}:{member}:{line}"),
                        (false, false) => format!("{source}:{member}"),
                        (true, true) => format!("{source}:{line}"),
                        (true, false) => source.clone(),
                    };
                    message = message
                        .field("Level", level, true)
                        .field("Source", &truncate_chat_value(&location), true)
                        .field("Message", &truncate_chat_value(log_message), false);
                }
                EventDetails::Unknown(_) => {}
            }
        }

        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_image_notification(
        &self,
        image: &ImageMetadata,
        index: usize,
        skipped_count: u32,
    ) {
        let color = match image.image_type.as_str() {
            "LIGHT" => colors::GREEN,
            "DARK" => colors::GRAY,
            "FLAT" => colors::BLUE,
            "BIAS" => colors::PURPLE,
            _ => colors::CYAN,
        };

        let title = if skipped_count > 0 {
            format!(
                "📸 New {} Frame Captured (+{} skipped)",
                image.image_type, skipped_count
            )
        } else {
            format!("📸 New {} Frame Captured", image.image_type)
        };

        let mut message = ChatMessage::new(&self.titled(title)).color(color);

        if let Some(target) = &self.state.current_target {
            message = message.field("Target", &target.name, true);
        }

        if skipped_count > 0 {
            message = message.field(
                "Images Since Last Post",
                &format!("{} images", skipped_count + 1),
                true,
            );
        }

        message = message
            .field("Camera", &image.camera_name, true)
            .field("Tracking RMS", &image.rms_text, true)
            .field("Filter", &image.filter, true)
            .field("Exposure", &format!("{}s", image.exposure_time), true)
            .field("Temperature", &format!("{:.1}°C", image.temperature), true)
            .field("Stars", &image.stars.to_string(), true)
            .field("HFR", &format!("{:.2}", image.hfr), true)
            .field("Mean", &format!("{:.1}", image.mean), true)
            .field("Median", &format!("{:.1}", image.median), true)
            .field("StDev", &format!("{:.1}", image.st_dev), true)
            .footer(&format!("Telescope: {}", image.telescope_name));

        if self
            .state
            .meridian_flip_time
            .as_ref()
            .map(|&h| h <= 1.0)
            .unwrap_or(false)
        {
            self.add_meridian_flip_info(&mut message);
        }

        // Send message with thumbnail plus, when the guider has data, a
        // rendered guiding graph
        let capabilities = self.source.capabilities();
        let extra_attachments = if capabilities.guider_graph {
            self.render_guiding_graph_attachment(index).await
        } else {
            Vec::new()
        };
        if capabilities.thumbnails {
            self.chat_manager
                .send_message_with_image(
                    &message,
                    &self.chat_target,
                    &self.source,
                    index as u32,
                    extra_attachments,
                )
                .await;
        } else {
            self.chat_manager
                .send_message_with_attachments(&message, &self.chat_target, &extra_attachments)
                .await;
        }
    }

    /// Fetch the guide graph and render it as a PNG attachment. Any
    /// failure (guider disconnected, empty history, render error) is
    /// non-fatal — the image notification just goes out without a graph.
    async fn render_guiding_graph_attachment(&self, index: usize) -> Vec<ChatAttachment> {
        let graph = match self.source.get_guider_graph().await {
            Ok(graph) => graph,
            Err(e) => {
                eprintln!("Guiding graph unavailable: {e}");
                return Vec::new();
            }
        };
        if !graph.success || !graph.response.has_graph_data() {
            return Vec::new();
        }
        match crate::charts::render_guider_graph_png(&graph.response) {
            Ok(png) => vec![ChatAttachment {
                data: png,
                filename: format!("guiding_{index}.png"),
            }],
            Err(e) => {
                eprintln!("Failed to render guiding graph: {e}");
                Vec::new()
            }
        }
    }
}

impl ChatUpdater {
    /// Add meridian flip information to a message
    fn add_meridian_flip_info(&self, message: &mut ChatMessage) {
        if let Some(hours) = self.state.meridian_flip_time {
            let formatted = meridian_flip_time_formatted_with_clock(hours);
            message.fields.push(ChatField {
                name: "Meridian Flip In".to_string(),
                value: formatted,
                discord_value: None,
                inline: true,
            });
        }
    }

    /// Add mount information to a message
    async fn add_mount_info(&self, message: &mut ChatMessage) {
        if let Ok(mount_info) = self.source.get_mount_info().await
            && mount_info.is_connected()
        {
            let (ra, dec) = mount_info.get_coordinates();
            let (alt, az) = mount_info.get_alt_az();

            message.fields.push(ChatField {
                name: "Mount Position".to_string(),
                value: format!("RA: {ra}\nDec: {dec}"),
                discord_value: None,
                inline: true,
            });
            if !alt.is_empty() && !az.is_empty() {
                message.fields.push(ChatField {
                    name: "Alt/Az".to_string(),
                    value: format!("Alt: {alt}\nAz: {az}"),
                    discord_value: None,
                    inline: true,
                });
            }
            message.fields.push(ChatField {
                name: "Pier Side".to_string(),
                value: mount_info.get_side_of_pier().to_string(),
                discord_value: None,
                inline: true,
            });

            let tracking_status = if mount_info.response.tracking_enabled {
                "✅ Enabled"
            } else {
                "❌ Disabled"
            };
            message.fields.push(ChatField {
                name: "Tracking".to_string(),
                value: tracking_status.to_string(),
                discord_value: None,
                inline: true,
            });
        }
    }
}

fn add_coordinate_field(
    message: ChatMessage,
    name: &str,
    coordinates: &crate::events::EventCoordinates,
) -> ChatMessage {
    let display = coordinates.display();
    let display = if display.is_empty() {
        "Position unavailable".to_string()
    } else {
        truncate_chat_value(&display)
    };
    message.field(name, &display, false)
}

fn motion_end_detection_display(value: &str) -> String {
    match value {
        "motion_state" => "Equipment first reported idle".to_string(),
        "nina_slewed" => "N.I.N.A. slew event".to_string(),
        "nina_moved" => "N.I.N.A. move event".to_string(),
        _ => truncate_chat_value(value),
    }
}

fn add_motion_metadata(
    mut message: ChatMessage,
    motion_id: Option<i64>,
    duration_seconds: Option<f64>,
    end_detection: Option<&str>,
    observed_in_progress: Option<bool>,
) -> ChatMessage {
    if let Some(motion_id) = motion_id {
        message = message.field("Motion", &format!("#{motion_id}"), true);
    }
    if let Some(duration_seconds) = duration_seconds {
        message = message.field(
            "Observed interval",
            &format!("{duration_seconds:.2} s"),
            true,
        );
    }
    if let Some(end_detection) = end_detection.filter(|value| !value.trim().is_empty()) {
        message = message.field(
            "End detected by",
            &motion_end_detection_display(end_detection),
            true,
        );
    }
    if observed_in_progress == Some(true) {
        message = message.field("Capture", "Recovered after motion began", false);
    }
    message
}

#[allow(clippy::too_many_arguments)]
fn add_rotator_end_fields(
    mut message: ChatMessage,
    event_name: &str,
    from: Option<f64>,
    to: Option<f64>,
    position: Option<f64>,
    mechanical_from: Option<f64>,
    mechanical_to: Option<f64>,
    mechanical_position: Option<f64>,
) -> ChatMessage {
    let mechanical_event = event_name == event_types::ROTATOR_MOVED_MECHANICAL;
    let logical_from = (!mechanical_event).then_some(from).flatten();
    let logical_to = position.or((!mechanical_event).then_some(to).flatten());
    let mechanical_from = mechanical_from.or(mechanical_event.then_some(from).flatten());
    let mechanical_to = mechanical_to
        .or(mechanical_position)
        .or(mechanical_event.then_some(to).flatten());

    if let Some(value) = logical_from {
        message = message.field("From position", &format!("{value:.2}°"), true);
    }
    if let Some(value) = logical_to {
        message = message.field("To position", &format!("{value:.2}°"), true);
    }
    if let (Some(from), Some(to)) = (logical_from, logical_to) {
        message = message.field("Position change", &format!("{:+.2}°", to - from), true);
    }
    if let Some(value) = mechanical_from {
        message = message.field("Mechanical from", &format!("{value:.2}°"), true);
    }
    if let Some(value) = mechanical_to {
        message = message.field("Mechanical to", &format!("{value:.2}°"), true);
    }
    if let (Some(from), Some(to)) = (mechanical_from, mechanical_to) {
        message = message.field("Mechanical change", &format!("{:+.2}°", to - from), true);
    }
    message
}

fn get_event_color(event: &str) -> u32 {
    match event {
        // Camera events
        event_types::CAMERA_CONNECTED => colors::GREEN,
        event_types::CAMERA_DISCONNECTED => colors::RED,
        event_types::CAMERA_DOWNLOAD_TIMEOUT | event_types::IMAGE_SAVE_FAILED => colors::RED,

        // Filterwheel events
        event_types::FILTERWHEEL_CONNECTED => colors::BLUE,
        event_types::FILTERWHEEL_DISCONNECTED => colors::RED,
        event_types::FILTERWHEEL_CHANGED => colors::BLUE,

        // Mount events
        event_types::MOUNT_CONNECTED => colors::GREEN,
        event_types::MOUNT_DISCONNECTED => colors::RED,
        event_types::MOUNT_PARKED => colors::YELLOW,
        event_types::MOUNT_UNPARKED => colors::YELLOW,
        event_types::MOUNT_HOMED => colors::CYAN,
        event_types::MOUNT_CENTER => colors::CYAN,
        event_types::MOUNT_SLEW_STARTED => colors::BLUE,
        event_types::MOUNT_SLEWED => colors::CYAN,

        // Focuser events
        event_types::FOCUSER_CONNECTED => colors::GREEN,
        event_types::FOCUSER_DISCONNECTED => colors::RED,
        event_types::FOCUSER_USER_FOCUSED => colors::PURPLE,
        event_types::AUTOFOCUS_STARTING => colors::PURPLE,
        event_types::AUTOFOCUS_FINISHED => colors::PURPLE,
        event_types::AUTOFOCUS_POINT_ADDED => colors::PURPLE,
        event_types::ERROR_AF => colors::RED,

        // Rotator events
        event_types::ROTATOR_CONNECTED => colors::GREEN,
        event_types::ROTATOR_DISCONNECTED => colors::RED,
        event_types::ROTATOR_MOVE_STARTED => colors::BLUE,
        event_types::ROTATOR_MOVED => colors::CYAN,
        event_types::ROTATOR_MOVED_MECHANICAL => colors::CYAN,
        event_types::ROTATOR_SYNCED => colors::CYAN,

        // Guider events
        event_types::GUIDER_CONNECTED => colors::GREEN,
        event_types::GUIDER_DISCONNECTED => colors::RED,
        event_types::GUIDER_START => colors::BLUE,
        event_types::GUIDER_STOP => colors::YELLOW,
        event_types::GUIDER_DITHER => colors::CYAN,

        // Sequence events
        event_types::SEQUENCE_STARTING => colors::CYAN,
        event_types::SEQUENCE_FINISHED => colors::CYAN,
        event_types::SEQUENCE_ENTITY_FAILED => colors::RED,
        event_types::CHATSTRONOMY_COMMAND_FAILED => colors::RED,

        // System events
        event_types::FLAT_DISCONNECTED
        | event_types::WEATHER_DISCONNECTED
        | event_types::SWITCH_DISCONNECTED
        | event_types::DOME_DISCONNECTED
        | event_types::SAFETY_DISCONNECTED => colors::RED,
        event_types::FLAT_CONNECTED
        | event_types::WEATHER_CONNECTED
        | event_types::SWITCH_CONNECTED
        | event_types::DOME_CONNECTED
        | event_types::SAFETY_CONNECTED => colors::GREEN,
        event_types::WEATHER_CHANGED => colors::CYAN,
        event_types::WEATHER_HIGH_WIND => colors::ORANGE,
        event_types::DOME_SHUTTER_OPENED
        | event_types::DOME_SHUTTER_CLOSED
        | event_types::DOME_HOMED
        | event_types::DOME_PARKED
        | event_types::DOME_SLEWED
        | event_types::DOME_SYNCED
        | event_types::FLAT_COVER_OPENED
        | event_types::FLAT_COVER_CLOSED
        | event_types::FLAT_BRIGHTNESS_CHANGED => colors::CYAN,
        event_types::FLAT_LIGHT_TOGGLED => colors::YELLOW,
        event_types::SAFETY_CHANGED => colors::ORANGE,
        event_types::ERROR_PLATESOLVE => colors::RED,

        // Target events
        event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART => colors::CYAN,
        event_types::TS_WAITSTART => colors::YELLOW,

        // Fallback patterns
        _ if event.contains("ERROR") => colors::RED,
        _ if event.contains("WARNING") => colors::ORANGE,
        _ => colors::GRAY,
    }
}

fn nina_level_color(level: &str) -> u32 {
    match level.to_ascii_uppercase().as_str() {
        "FATAL" | "ERROR" => colors::RED,
        "WARN" | "WARNING" => colors::ORANGE,
        "SUCCESS" => colors::GREEN,
        "INFO" | "INFORMATION" => colors::BLUE,
        "DEBUG" | "TRACE" | "VERBOSE" => colors::GRAY,
        _ => colors::CYAN,
    }
}

/// Parse a timestamp N.I.N.A. put on the wire.
///
/// Most carry an offset and parse as RFC 3339. A `DateTime` with
/// `DateTimeKind.Unspecified` serializes without one, and those used to be
/// dropped silently — leaving the sequence "waiting until" state unset. Use
/// an explicit companion event offset when available; otherwise preserve the
/// legacy ninaAPI convention deterministically as UTC.
fn parse_nina_timestamp_with_context(
    value: &str,
    context_event_time: Option<&str>,
) -> Option<DateTime<FixedOffset>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed);
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()?;

    // Older N.I.N.A. payloads serialize some local DateTimes without an
    // offset. The event timestamp still carries the observatory offset, so
    // use it rather than the Hub host's timezone. Fixed offsets are
    // unambiguous; explicit offsets above always take precedence.
    if let Some(context) = context_event_time.and_then(|time| {
        DateTime::parse_from_rfc3339(time)
            .ok()
            .map(|parsed| *parsed.offset())
    }) {
        return context.from_local_datetime(&naive).single();
    }

    // Legacy ninaAPI/Target Scheduler payloads also emitted both timestamps
    // without offsets, but those values were UTC (see the checked-in event
    // history fixtures). Current plugin payloads always carry an explicit
    // offset. UTC is therefore the only deterministic compatibility fallback;
    // using `Local` here would reinterpret the same telescope payload
    // differently on each Hub host.
    FixedOffset::east_opt(0)?
        .from_local_datetime(&naive)
        .single()
}

fn parse_nina_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    parse_nina_timestamp_with_context(value, None)
}

fn autofocus_report_matches(expected: Option<&str>, actual: &str) -> bool {
    let Some(expected) = expected else {
        // Payload v1/v2 completions carry no report identity. Preserve their
        // historical "last report" behavior while v3 uses exact correlation.
        return true;
    };
    match (parse_nina_timestamp(expected), parse_nina_timestamp(actual)) {
        // N.I.N.A. and Hocus Focus copy the report timestamp into the
        // completion callback. Filename timestamps have a bounded skew, but
        // v3 payload identity must name the exact completed report.
        (Some(expected), Some(actual)) => expected == actual,
        _ => expected.trim() == actual.trim(),
    }
}

fn truncate_to(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut truncated = value.chars().take(limit - 1).collect::<String>();
    truncated.push('…');
    truncated
}

/// Embed *field values* cap at 1024 in Discord; stay under it.
fn truncate_chat_value(value: &str) -> String {
    truncate_to(value, 1_000)
}

/// Embed *titles* cap at 256 in Discord, and an over-long title fails the whole
/// message with a 400 rather than being trimmed. Titles are built from
/// remote-supplied text (notification headers, unknown event names), so they
/// need their own, much smaller budget. The caller prepends `[telescope] `,
/// so leave room for that too.
fn truncate_chat_title(value: &str) -> String {
    truncate_to(value, 180)
}

fn get_event_title(event: &str) -> String {
    match event {
        event_types::FILTERWHEEL_CHANGED => "🔄 Filter Changed".to_string(),
        event_types::TS_TARGETSTART => "🎯 Target Started".to_string(),
        event_types::TS_WAITSTART => "⏳ Target Scheduler wait".to_string(),
        event_types::AUTOFOCUS_POINT_ADDED => "📈 Autofocus Point".to_string(),
        event_types::ROTATOR_MOVE_STARTED => "🧭 Rotator move started".to_string(),
        event_types::ROTATOR_MOVED => "🧭 Rotator move ended".to_string(),
        event_types::ROTATOR_MOVED_MECHANICAL => "🧭 Rotator mechanical move ended".to_string(),
        event_types::MOUNT_SLEW_STARTED => "🔭 Mount slew started".to_string(),
        event_types::MOUNT_SLEWED => "🔭 Mount slew ended".to_string(),
        event_types::SEQUENCE_STARTING => "▶️ Sequence starting".to_string(),
        event_types::SEQUENCE_FINISHED => "🏁 Sequence ended".to_string(),
        event_types::SEQUENCE_ENTITY_FAILED => "❌ Sequence item failed".to_string(),
        event_types::IMAGE_SAVE_FAILED => "❌ Image save failed".to_string(),
        event_types::CAMERA_DOWNLOAD_TIMEOUT => "❌ Camera download timed out".to_string(),
        event_types::DOME_CONNECTED => "🏠 Dome connected".to_string(),
        event_types::DOME_DISCONNECTED => "⚠️ Dome disconnected".to_string(),
        event_types::DOME_SHUTTER_OPENED => "🏠 Dome shutter opened".to_string(),
        event_types::DOME_SHUTTER_CLOSED => "🏠 Dome shutter closed".to_string(),
        event_types::DOME_HOMED => "🏠 Dome homed".to_string(),
        event_types::DOME_PARKED => "🏠 Dome parked".to_string(),
        event_types::DOME_SLEWED => "🏠 Dome slew completed".to_string(),
        event_types::DOME_SYNCED => "🏠 Dome synchronized".to_string(),
        event_types::FLAT_CONNECTED => "💡 Flat panel connected".to_string(),
        event_types::FLAT_DISCONNECTED => "⚠️ Flat panel disconnected".to_string(),
        event_types::FLAT_COVER_OPENED => "💡 Flat-panel cover opened".to_string(),
        event_types::FLAT_COVER_CLOSED => "💡 Flat-panel cover closed".to_string(),
        event_types::FLAT_LIGHT_TOGGLED => "💡 Flat-panel light changed".to_string(),
        event_types::FLAT_BRIGHTNESS_CHANGED => "💡 Flat-panel brightness changed".to_string(),
        event_types::WEATHER_CONNECTED => "🌦️ Weather station connected".to_string(),
        event_types::WEATHER_DISCONNECTED => "⚠️ Weather station disconnected".to_string(),
        event_types::WEATHER_CHANGED => "🌦️ Weather changed".to_string(),
        event_types::WEATHER_HIGH_WIND => "⚠️ High wind reported".to_string(),
        event_types::SWITCH_CONNECTED => "🔌 Switch device connected".to_string(),
        event_types::SWITCH_DISCONNECTED => "⚠️ Switch device disconnected".to_string(),
        event_types::NINA_NOTIFICATION => "🔔 N.I.N.A. notification".to_string(),
        event_types::NINA_LOG => "📝 N.I.N.A. log".to_string(),
        event_types::CHATSTRONOMY_COMMAND_FAILED => "❌ Telescope command failed".to_string(),
        event_types::SAFETY_CONNECTED => "🛡️ Safety monitor connected".to_string(),
        event_types::SAFETY_DISCONNECTED => "⚠️ Safety monitor disconnected".to_string(),
        event_types::SAFETY_CHANGED => "🛡️ Safety state changed".to_string(),
        // The event name comes from the plugin, so it is not length-bounded.
        _ => format!("📡 {}", truncate_chat_title(event)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::CommandResponse;
    use crate::camera::CameraInfoResponse;
    use crate::chat::ChatService;
    use crate::error::ChatError;
    use crate::events::EventHistoryResponse;
    use crate::filterwheel::FilterWheelInfoResponse;
    use crate::focuser::FocuserInfoResponse;
    use crate::guider::{GuiderGraphResponse, GuiderInfoResponse};
    use crate::images::{ImageHistoryResponse, ThumbnailResponse};
    use crate::mount::MountInfoResponse;
    use crate::rotator::RotatorInfoResponse;
    use crate::source::{
        RigCapabilities, RigCommand, RigSource, RigSourceError, RigSourceKind, RigSourceResult,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyAutofocusSource {
        event: Event,
        event_queries: AtomicUsize,
        image_queries: AtomicUsize,
        autofocus_queries: AtomicUsize,
        autofocus_delay: Duration,
        unavailable_queries: usize,
        not_ready_queries: usize,
        invalid_response: bool,
        baseline_retry: bool,
        hocus_response: bool,
    }

    impl FlakyAutofocusSource {
        fn new(event: Event) -> Self {
            Self::unavailable_for(event, 1)
        }

        fn unavailable_for(event: Event, unavailable_queries: usize) -> Self {
            Self {
                event,
                event_queries: AtomicUsize::new(0),
                image_queries: AtomicUsize::new(0),
                autofocus_queries: AtomicUsize::new(0),
                autofocus_delay: Duration::ZERO,
                unavailable_queries,
                not_ready_queries: 0,
                invalid_response: false,
                baseline_retry: false,
                hocus_response: false,
            }
        }

        fn invalid(event: Event) -> Self {
            Self {
                event,
                event_queries: AtomicUsize::new(0),
                image_queries: AtomicUsize::new(0),
                autofocus_queries: AtomicUsize::new(0),
                autofocus_delay: Duration::ZERO,
                unavailable_queries: 0,
                not_ready_queries: 0,
                invalid_response: true,
                baseline_retry: false,
                hocus_response: false,
            }
        }

        fn baseline_retry(event: Event) -> Self {
            Self {
                event,
                event_queries: AtomicUsize::new(0),
                image_queries: AtomicUsize::new(0),
                autofocus_queries: AtomicUsize::new(0),
                autofocus_delay: Duration::ZERO,
                unavailable_queries: 0,
                not_ready_queries: 0,
                invalid_response: false,
                baseline_retry: true,
                hocus_response: false,
            }
        }

        fn not_ready_for(event: Event, not_ready_queries: usize) -> Self {
            Self {
                event,
                event_queries: AtomicUsize::new(0),
                image_queries: AtomicUsize::new(0),
                autofocus_queries: AtomicUsize::new(0),
                autofocus_delay: Duration::ZERO,
                unavailable_queries: 0,
                not_ready_queries,
                invalid_response: false,
                baseline_retry: false,
                hocus_response: false,
            }
        }

        fn delayed(event: Event, autofocus_delay: Duration) -> Self {
            Self {
                event,
                event_queries: AtomicUsize::new(0),
                image_queries: AtomicUsize::new(0),
                autofocus_queries: AtomicUsize::new(0),
                autofocus_delay,
                unavailable_queries: 0,
                not_ready_queries: 0,
                invalid_response: false,
                baseline_retry: false,
                hocus_response: false,
            }
        }

        fn hocus(event: Event) -> Self {
            let mut source = Self::unavailable_for(event, 0);
            source.hocus_response = true;
            source
        }

        fn unexpected<T>() -> RigSourceResult<T> {
            panic!("unexpected RigSource query in autofocus delivery test")
        }
    }

    #[async_trait]
    impl RigSource for FlakyAutofocusSource {
        fn kind(&self) -> RigSourceKind {
            RigSourceKind::NinaDirect
        }

        fn capabilities(&self) -> RigCapabilities {
            if self.baseline_retry {
                let mut capabilities = RigCapabilities::none();
                capabilities.event_history = true;
                capabilities.image_history = true;
                capabilities
            } else {
                RigCapabilities::all()
            }
        }

        async fn get_event_history(&self) -> RigSourceResult<EventHistoryResponse> {
            let query = self.event_queries.fetch_add(1, Ordering::SeqCst);
            let response = if self.baseline_retry && query == 0 {
                Vec::new()
            } else if self.baseline_retry {
                vec![
                    self.event.clone(),
                    Event {
                        time: "2026-08-25T22:30:01Z".to_string(),
                        event: event_types::SAFETY_CHANGED.to_string(),
                        chat_enabled: true,
                        details: Some(EventDetails::SafetyChanged { is_safe: false }),
                    },
                ]
            } else {
                vec![self.event.clone()]
            };
            Ok(EventHistoryResponse {
                response,
                error: String::new(),
                status_code: 200,
                success: true,
                response_type: "API".to_string(),
            })
        }

        async fn get_all_image_history(&self) -> RigSourceResult<ImageHistoryResponse> {
            if !self.baseline_retry {
                return Self::unexpected();
            }
            if self.image_queries.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(RigSourceError::Unavailable {
                    kind: RigSourceKind::NinaDirect,
                    reason: "image history is temporarily unavailable".to_string(),
                });
            }
            Ok(ImageHistoryResponse {
                response: Vec::new(),
                error: String::new(),
                status_code: 200,
                success: true,
                response_type: "API".to_string(),
            })
        }

        async fn get_sequence(&self) -> RigSourceResult<SequenceResponse> {
            Self::unexpected()
        }

        async fn get_thumbnail(&self, _index: u32) -> RigSourceResult<ThumbnailResponse> {
            Self::unexpected()
        }

        async fn get_last_autofocus(&self) -> RigSourceResult<AutofocusResponse> {
            let query = self.autofocus_queries.fetch_add(1, Ordering::SeqCst);
            if !self.autofocus_delay.is_zero() {
                tokio::time::sleep(self.autofocus_delay).await;
            }
            if query < self.unavailable_queries {
                return Err(RigSourceError::Unavailable {
                    kind: RigSourceKind::NinaDirect,
                    reason: "report is still being published".to_string(),
                });
            }
            if query
                < self
                    .unavailable_queries
                    .saturating_add(self.not_ready_queries)
            {
                return Err(RigSourceError::NotReady {
                    kind: RigSourceKind::NinaDirect,
                    reason: "report is still being published".to_string(),
                });
            }
            if self.invalid_response {
                return Err(RigSourceError::InvalidResponse {
                    kind: RigSourceKind::NinaDirect,
                    reason: "malformed autofocus payload".to_string(),
                });
            }
            let fixture = if self.hocus_response {
                include_str!("../example_last_af_hocus_modern.json")
            } else {
                include_str!("../example_last_af.json")
            };
            Ok(serde_json::from_str(fixture).expect("valid autofocus fixture"))
        }

        async fn get_mount_info(&self) -> RigSourceResult<MountInfoResponse> {
            Self::unexpected()
        }

        async fn get_camera_info(&self) -> RigSourceResult<CameraInfoResponse> {
            Self::unexpected()
        }

        async fn get_filterwheel_info(&self) -> RigSourceResult<FilterWheelInfoResponse> {
            Self::unexpected()
        }

        async fn get_guider_info(&self) -> RigSourceResult<GuiderInfoResponse> {
            Self::unexpected()
        }

        async fn get_guider_graph(&self) -> RigSourceResult<GuiderGraphResponse> {
            Self::unexpected()
        }

        async fn get_rotator_info(&self) -> RigSourceResult<RotatorInfoResponse> {
            Self::unexpected()
        }

        async fn get_focuser_info(&self) -> RigSourceResult<FocuserInfoResponse> {
            Self::unexpected()
        }

        async fn execute_command(&self, _command: RigCommand) -> RigSourceResult<CommandResponse> {
            Self::unexpected()
        }
    }

    #[derive(Default)]
    struct RecordingChatState {
        deliveries: Mutex<Vec<(ChatMessage, Vec<ChatAttachment>)>>,
    }

    struct RecordingChatService {
        state: Arc<RecordingChatState>,
    }

    #[async_trait]
    impl ChatService for RecordingChatService {
        async fn send_message(
            &self,
            message: &ChatMessage,
            _target: &ChatTarget,
        ) -> Result<(), ChatError> {
            self.state
                .deliveries
                .lock()
                .unwrap()
                .push((message.clone(), Vec::new()));
            Ok(())
        }

        async fn send_message_with_image(
            &self,
            message: &ChatMessage,
            _target: &ChatTarget,
            image_data: &[u8],
            filename: &str,
        ) -> Result<(), ChatError> {
            self.state.deliveries.lock().unwrap().push((
                message.clone(),
                vec![ChatAttachment {
                    data: image_data.to_vec(),
                    filename: filename.to_string(),
                }],
            ));
            Ok(())
        }

        async fn send_message_with_attachments(
            &self,
            message: &ChatMessage,
            _target: &ChatTarget,
            attachments: &[ChatAttachment],
        ) -> Result<(), ChatError> {
            self.state
                .deliveries
                .lock()
                .unwrap()
                .push((message.clone(), attachments.to_vec()));
            Ok(())
        }

        fn service_name(&self) -> &'static str {
            "recording chat"
        }

        fn can_route(&self, _target: &ChatTarget) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct BlockingChatState {
        started: AtomicUsize,
        cancelled: AtomicUsize,
    }

    struct BlockingChatService {
        state: Arc<BlockingChatState>,
    }

    struct BlockingDeliveryGuard(Arc<BlockingChatState>);

    impl Drop for BlockingDeliveryGuard {
        fn drop(&mut self) {
            self.0.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl BlockingChatService {
        async fn block(&self) -> Result<(), ChatError> {
            self.state.started.fetch_add(1, Ordering::SeqCst);
            let _guard = BlockingDeliveryGuard(self.state.clone());
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    #[async_trait]
    impl ChatService for BlockingChatService {
        async fn send_message(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
        ) -> Result<(), ChatError> {
            self.block().await
        }

        async fn send_message_with_image(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
            _image_data: &[u8],
            _filename: &str,
        ) -> Result<(), ChatError> {
            self.block().await
        }

        async fn send_message_with_attachments(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
            _attachments: &[ChatAttachment],
        ) -> Result<(), ChatError> {
            self.block().await
        }

        fn service_name(&self) -> &'static str {
            "blocking chat"
        }

        fn can_route(&self, _target: &ChatTarget) -> bool {
            true
        }
    }

    fn operation(kind: SequenceOperationKind) -> SequenceOperation {
        SequenceOperation {
            key: "1/0".to_string(),
            name: "Test operation".to_string(),
            status: "RUNNING".to_string(),
            chat_enabled: true,
            kind,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn autofocus_completion_retries_off_poller_and_delivers_one_graph() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                // N.I.N.A.'s callback can precede the matching report timestamp
                // slightly; the plugin accepts this exact run with a 5s window.
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::new(event));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        )
        .with_lifecycle_announcements(false);
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_secs(1),
        };

        // Event polling only queues the potentially slow report fetch. Seeing
        // the same retained event on the next poll must not queue another task.
        tokio::time::timeout(Duration::from_millis(1), updater.poll_events())
            .await
            .expect("autofocus delivery blocked the event poller");
        tokio::time::timeout(Duration::from_millis(1), updater.poll_events())
            .await
            .expect("deduplicating autofocus blocked the event poller");

        // The source read is owned by the updater and starts only after the
        // normal poll methods have completed.
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 0);
        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 1);
        assert!(chat_state.deliveries.lock().unwrap().is_empty());

        tokio::time::advance(Duration::from_millis(10)).await;
        updater.poll_autofocus_delivery().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if !chat_state.deliveries.lock().unwrap().is_empty() {
                break;
            }
        }

        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 2);
        {
            let deliveries = chat_state.deliveries.lock().unwrap();
            assert_eq!(deliveries.len(), 1);
            let (message, attachments) = &deliveries[0];
            assert!(message.title.contains("Autofocus Completed"));
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].filename, "autofocus.png");
            assert_eq!(
                attachments[0].data.get(..8),
                Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..])
            );
            assert!(message.fields.iter().any(|field| {
                field.name == "Mode" && field.value == "Star HFR · Trend + hyperbolic"
            }));
            for hocus_only in [
                "Accepted stars / point",
                "Focus precision",
                "LOO stability",
                "Reduced χ²",
                "Detection region",
                "Acceptance",
                "Star detection",
            ] {
                assert!(
                    message.fields.iter().all(|field| field.name != hocus_only),
                    "native fallback unexpectedly contained {hocus_only}"
                );
            }
        }

        // Advancing beyond the entire retry window cannot produce a duplicate.
        tokio::time::advance(Duration::from_secs(60)).await;
        updater.poll_autofocus_delivery().await;
        tokio::task::yield_now().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 2);
        assert_eq!(chat_state.deliveries.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hocus_autofocus_completion_delivers_enriched_feedback_and_graph() {
        let event = Event {
            time: "2026-08-28T05:15:42.250Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2026-08-27T22:15:42.2500000-07:00".to_string(),
                filter: Some("L".to_string()),
                position: Some(4188.955065493704),
                temperature: Some(12.4),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::hocus(event));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Hocus Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        )
        .with_lifecycle_announcements(false);

        updater.poll_events().await;
        updater.poll_autofocus_delivery().await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        let (message, attachments) = &deliveries[0];
        assert!(message.title.contains("Autofocus Completed"));
        assert!(!message.title.contains("Report Unavailable"));
        for (name, expected) in [
            ("Mode", "Star HFR · Trend + hyperbolic · Tilted Hyperbola"),
            ("HFR After (measured)", "2.220"),
            ("Accepted stars / point", "83–119"),
            ("Focus precision", "±0.70 steps"),
            ("LOO stability", "0.57 steps"),
            ("Reduced χ²", "0.0017"),
            ("Detection region", "Region 3 · 50% × 50%"),
            ("Acceptance", "Reduced χ² ≤ 5.000"),
            (
                "Star detection",
                "Optimized · Mean + outlier detection · 2× binning",
            ),
        ] {
            assert!(
                message
                    .fields
                    .iter()
                    .any(|field| field.name == name && field.value == expected),
                "missing {name}={expected}"
            );
        }
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "autofocus.png");
        assert_eq!(
            attachments[0].data.get(..8),
            Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn newer_autofocus_completion_silently_supersedes_an_unfetchable_older_report() {
        let older = Event {
            time: "2025-08-11T22:28:30Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T15:28:30-07:00".to_string(),
                filter: Some("L".to_string()),
                position: Some(4000.0),
                temperature: Some(21.0),
            }),
        };
        let newer = Event {
            time: "2025-08-12T06:28:30.5478817Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::unavailable_for(newer.clone(), 0));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        )
        .with_lifecycle_announcements(false);

        updater.handle_autofocus_finished(&older).await;
        updater.handle_autofocus_finished(&newer).await;
        assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
        assert_eq!(
            updater.pending_autofocus_deliveries[0]
                .report_timestamp
                .as_deref(),
            Some("2025-08-11T23:28:30.5478817-07:00")
        );

        updater.poll_autofocus_delivery().await;
        tokio::time::advance(Duration::from_secs(300)).await;
        updater.poll_autofocus_delivery().await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].0.title.contains("Autofocus Completed"));
        assert!(!deliveries[0].0.title.contains("Report Unavailable"));
    }

    #[tokio::test(start_paused = true)]
    async fn autofocus_completion_never_delivers_a_different_reports_graph() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2026-08-25T22:29:59Z".to_string(),
                filter: Some("  ".to_string()),
                position: Some(5000.0),
                temperature: Some(-5.0),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::unavailable_for(event, 0));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_secs(1),
        };

        updater.poll_events().await;
        updater.poll_autofocus_delivery().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        updater.poll_autofocus_delivery().await;
        tokio::task::yield_now().await;

        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 2);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        let (message, attachments) = &deliveries[0];
        assert!(message.title.contains("Report Unavailable"));
        assert!(attachments.is_empty());
        assert!(
            message
                .fields
                .iter()
                .any(|field| { field.name == "Filter" && field.value == "No filter" })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_event_baseline_is_live_on_later_initialization_retry() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::baseline_retry(event));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        )
        .with_lifecycle_announcements(false);

        assert!(updater.initialize_baseline().await.is_err());
        assert!(updater.event_baseline_complete);
        assert!(updater.pending_autofocus_deliveries.is_empty());

        updater.initialize_baseline().await.unwrap();

        assert_eq!(source.event_queries.load(Ordering::SeqCst), 2);
        assert_eq!(source.image_queries.load(Ordering::SeqCst), 2);
        assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
        let deliveries = chat_state.deliveries.lock().unwrap();
        assert!(
            deliveries
                .iter()
                .any(|(message, _)| message.title.contains("Safety"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn autofocus_outage_does_not_consume_report_attempt_budget() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::unavailable_for(event, 5));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_millis(100),
        };

        updater.poll_events().await;
        for _ in 0..5 {
            updater.poll_autofocus_delivery().await;
            tokio::time::advance(Duration::from_millis(10)).await;
        }
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 5);
        assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
        assert_eq!(updater.pending_autofocus_deliveries[0].attempts, 0);

        updater.poll_autofocus_delivery().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if !chat_state.deliveries.lock().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 6);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        assert_eq!(chat_state.deliveries.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn permanently_unavailable_autofocus_stops_at_overall_deadline() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::unavailable_for(event, usize::MAX));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            // Transport failures retain this ordinary attempt budget, while
            // the independent overall deadline still bounds the queue entry.
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_millis(50),
        };

        updater.poll_events().await;
        for (expected_queries, expected_wait) in [
            (1, Duration::from_millis(10)),
            (2, Duration::from_millis(20)),
            (3, Duration::from_millis(20)),
        ] {
            updater.poll_autofocus_delivery().await;
            assert_eq!(
                source.autofocus_queries.load(Ordering::SeqCst),
                expected_queries
            );
            assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
            assert_eq!(updater.pending_autofocus_deliveries[0].attempts, 0);
            let wait = updater.pending_autofocus_deliveries[0]
                .next_attempt_at
                .saturating_duration_since(TokioInstant::now());
            assert_eq!(wait, expected_wait);
            tokio::time::advance(wait).await;
        }

        // At the absolute deadline, expire without issuing a fourth query.
        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 3);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        {
            let deliveries = chat_state.deliveries.lock().unwrap();
            assert_eq!(deliveries.len(), 1);
            assert!(deliveries[0].0.title.contains("Report Unavailable"));
        }

        tokio::time::advance(Duration::from_secs(1)).await;
        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_autofocus_read_cannot_outlive_overall_deadline() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::delayed(
            event,
            Duration::from_millis(50),
        ));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_millis(25),
        };

        updater.poll_events().await;
        let started = TokioInstant::now();
        updater.poll_autofocus_delivery().await;

        assert_eq!(
            TokioInstant::now().duration_since(started),
            Duration::from_millis(25)
        );
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 1);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].0.title.contains("Report Unavailable"));
    }

    #[tokio::test(start_paused = true)]
    async fn autofocus_not_ready_preserves_attempt_budget_until_report_arrives() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::not_ready_for(event, 3));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            // A single ordinary failure would exhaust this budget. The three
            // readiness responses must leave it untouched.
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_secs(1),
        };

        updater.poll_events().await;
        for expected_queries in 1..=3 {
            updater.poll_autofocus_delivery().await;
            assert_eq!(
                source.autofocus_queries.load(Ordering::SeqCst),
                expected_queries
            );
            assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
            assert_eq!(updater.pending_autofocus_deliveries[0].attempts, 0);
            assert!(chat_state.deliveries.lock().unwrap().is_empty());
            let wait = updater.pending_autofocus_deliveries[0]
                .next_attempt_at
                .saturating_duration_since(TokioInstant::now());
            tokio::time::advance(wait).await;
        }

        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 4);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].0.title.contains("Autofocus Completed"));
        assert_eq!(deliveries[0].1.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn autofocus_not_ready_stops_at_elapsed_deadline() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::not_ready_for(event, usize::MAX));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            not_ready_timeout: Duration::from_millis(25),
            overall_timeout: Duration::from_secs(1),
        };

        updater.poll_events().await;
        for expected_queries in 1..=3 {
            updater.poll_autofocus_delivery().await;
            assert_eq!(
                source.autofocus_queries.load(Ordering::SeqCst),
                expected_queries
            );
            assert_eq!(updater.pending_autofocus_deliveries.len(), 1);
            assert_eq!(updater.pending_autofocus_deliveries[0].attempts, 0);
            let wait = updater.pending_autofocus_deliveries[0]
                .next_attempt_at
                .saturating_duration_since(TokioInstant::now());
            tokio::time::advance(wait).await;
        }

        // The fourth readiness response lands exactly on the 25 ms elapsed
        // deadline. It terminates the pending delivery without consuming the
        // ordinary one-attempt failure budget.
        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 4);
        assert!(updater.pending_autofocus_deliveries.is_empty());
        {
            let deliveries = chat_state.deliveries.lock().unwrap();
            assert_eq!(deliveries.len(), 1);
            assert!(deliveries[0].0.title.contains("Report Unavailable"));
        }

        tokio::time::advance(Duration::from_secs(1)).await;
        updater.poll_autofocus_delivery().await;
        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_autofocus_report_exhausts_attempt_budget() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::invalid(event));
        let mut updater = ChatUpdater::new(
            source.clone(),
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_secs(1),
        };

        updater.poll_events().await;
        updater.poll_autofocus_delivery().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        updater.poll_autofocus_delivery().await;

        assert_eq!(source.autofocus_queries.load(Ordering::SeqCst), 2);
        assert!(updater.pending_autofocus_deliveries.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn aborting_updater_task_cancels_inflight_autofocus_notification() {
        let event = Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::AUTOFOCUS_FINISHED.to_string(),
            chat_enabled: true,
            details: Some(EventDetails::AutofocusFinished {
                report_timestamp: "2025-08-11T23:28:30.5478817-07:00".to_string(),
                filter: Some("OIII".to_string()),
                position: Some(4068.0),
                temperature: Some(21.3),
            }),
        };
        let source = Arc::new(FlakyAutofocusSource::new(event));
        let chat_state = Arc::new(BlockingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(BlockingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        updater.autofocus_retry = AutofocusRetryPolicy {
            max_attempts: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            not_ready_timeout: Duration::from_millis(100),
            overall_timeout: Duration::from_secs(1),
        };

        updater.poll_events().await;
        updater.poll_autofocus_delivery().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        let delivery = tokio::spawn(async move {
            updater.poll_autofocus_delivery().await;
            updater
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if chat_state.started.load(Ordering::SeqCst) == 1 {
                break;
            }
        }
        assert_eq!(chat_state.started.load(Ordering::SeqCst), 1);

        delivery.abort();
        let _ = delivery.await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if chat_state.cancelled.load(Ordering::SeqCst) == 1 {
                break;
            }
        }
        assert_eq!(chat_state.cancelled.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sequencer_plus_wait_lifecycle_and_status_stay_privacy_safe() {
        let source = Arc::new(FlakyAutofocusSource::new(Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        }));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        let now = Utc::now();
        let condition = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::ConditionWait {
                wait_interval: Some(chrono::Duration::seconds(17)),
            }),
            now,
            None,
        );
        let mut manual_operation = operation(SequenceOperationKind::ManualWait);
        manual_operation.key = "0/2".to_string();
        let manual = TrackedSequenceOperation::new(manual_operation, now, None);
        let mut disabled_safety_operation = operation(SequenceOperationKind::SafetyWait {
            is_safe: Some(false),
            wait_interval: Some(chrono::Duration::seconds(5)),
        });
        disabled_safety_operation.key = "0/3".to_string();
        disabled_safety_operation.chat_enabled = false;
        let disabled_safety = TrackedSequenceOperation::new(disabled_safety_operation, now, None);

        updater
            .send_sequence_operation_update(&condition, OperationUpdate::Started)
            .await;
        updater
            .send_sequence_operation_update(
                &manual,
                OperationUpdate::Finished {
                    attach_output: false,
                },
            )
            .await;

        updater
            .state
            .sequence_operations
            .insert(condition.operation.key.clone(), condition);
        updater
            .state
            .sequence_operations
            .insert(manual.operation.key.clone(), manual);
        updater
            .state
            .sequence_operations
            .insert(disabled_safety.operation.key.clone(), disabled_safety);
        let status_time = DateTime::parse_from_rfc3339("2099-08-26T06:00:00Z")
            .expect("status time")
            .with_timezone(&Utc);
        let status = updater.format_startup_status_at(status_time);
        assert!(status.contains("Waiting for a sequence condition"));
        assert!(status.contains("checks every 17s"));
        assert!(status.contains("Waiting for manual sequence resume"));
        assert!(!status.contains("Waiting for safe conditions"));
        assert!(!updater.state.status_fingerprint().contains("0/3"));

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(
            deliveries[0]
                .0
                .title
                .contains("Waiting for a sequence condition")
        );
        assert!(
            deliveries[0]
                .0
                .fields
                .iter()
                .any(|field| field.name == "Check interval")
        );
        assert!(deliveries[1].0.title.contains("Sequence manually resumed"));
        let rendered = format!("{deliveries:?}\n{status}");
        for private_label in ["Expression", "Predicate", "Reason"] {
            assert!(!rendered.contains(private_label));
        }
    }

    #[tokio::test]
    async fn disappearing_safety_wait_does_not_claim_conditions_became_safe() {
        let source = Arc::new(FlakyAutofocusSource::new(Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        }));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        let safety_wait = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::SafetyWait {
                is_safe: Some(false),
                wait_interval: Some(chrono::Duration::seconds(5)),
            }),
            Utc::now(),
            None,
        );
        updater
            .state
            .sequence_operations
            .insert(safety_wait.operation.key.clone(), safety_wait);

        updater
            .reconcile_sequence_operations(Vec::new(), HashSet::new(), None, true)
            .await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].0.title.contains("Safety wait ended"));
        assert!(!deliveries[0].0.title.contains("Safe conditions reached"));
    }

    #[tokio::test]
    async fn suppressed_active_safety_wait_is_removed_without_delivery() {
        let source = Arc::new(FlakyAutofocusSource::new(Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        }));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        let safety_wait = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::SafetyWait {
                is_safe: Some(false),
                wait_interval: Some(chrono::Duration::seconds(5)),
            }),
            Utc::now(),
            None,
        );
        let key = safety_wait.operation.key.clone();
        updater
            .state
            .sequence_operations
            .insert(key.clone(), safety_wait);
        let mut suppressed_operation_keys = HashSet::new();
        suppressed_operation_keys.insert(key);

        updater
            .reconcile_sequence_operations(Vec::new(), suppressed_operation_keys, None, true)
            .await;

        assert!(updater.state.sequence_operations.is_empty());
        assert!(chat_state.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn suppressed_time_wait_preserves_independent_scheduler_wait_without_delivery() {
        let source = Arc::new(FlakyAutofocusSource::new(Event {
            time: "2026-08-25T22:30:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        }));
        let chat_state = Arc::new(RecordingChatState::default());
        let mut chat_manager = ChatServiceManager::new();
        chat_manager.add_service(Box::new(RecordingChatService {
            state: chat_state.clone(),
        }));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(chat_manager),
        );
        let time_wait = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::TimeWait {
                target_time: None,
                configured_duration: Some(chrono::Duration::minutes(5)),
            }),
            Utc::now(),
            None,
        );
        let key = time_wait.operation.key.clone();
        updater
            .state
            .sequence_operations
            .insert(key.clone(), time_wait);
        let scheduler_end = Utc::now() + chrono::Duration::minutes(5);
        updater.state.scheduler_wait = Some(SchedulerWaitState {
            end_at: scheduler_end,
        });
        let mut suppressed_operation_keys = HashSet::new();
        suppressed_operation_keys.insert(key);

        updater
            .reconcile_sequence_operations(Vec::new(), suppressed_operation_keys, None, true)
            .await;

        assert!(updater.state.sequence_operations.is_empty());
        assert_eq!(updater.state.scheduler_wait_end(), Some(scheduler_end));
        assert!(chat_state.deliveries.lock().unwrap().is_empty());
    }

    fn target_event(chat_enabled: bool, name: &str) -> Event {
        Event {
            time: "2026-08-26T06:00:00Z".to_string(),
            event: event_types::TS_TARGETSTART.to_string(),
            chat_enabled,
            details: Some(EventDetails::TargetStart {
                target_name: name.to_string(),
                project_name: None,
                rotation: None,
                target_end_time: None,
                coordinates: None,
            }),
        }
    }

    fn target_scheduler_wait_event(
        event_time: &str,
        wait_end_time: &str,
        chat_enabled: bool,
    ) -> Event {
        Event {
            time: event_time.to_string(),
            event: event_types::TS_WAITSTART.to_string(),
            chat_enabled,
            details: chat_enabled.then(|| EventDetails::WaitStart {
                wait_end_time: wait_end_time.to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn scheduler_and_sequence_waits_with_the_same_endpoint_are_independent() {
        let (mut updater, chat_state) = recording_test_updater();
        let endpoint = DateTime::parse_from_rfc3339("2099-08-26T08:00:00Z").expect("wait endpoint");
        let scheduler_event =
            target_scheduler_wait_event("2099-08-26T06:00:00Z", "2099-08-26T08:00:00Z", true);

        updater.handle_event(&scheduler_event).await;
        updater
            .reconcile_sequence_operations(
                vec![operation(SequenceOperationKind::TimeWait {
                    target_time: Some(endpoint),
                    configured_duration: None,
                })],
                HashSet::new(),
                None,
                true,
            )
            .await;

        let scheduler_endpoint = endpoint.with_timezone(&Utc);
        assert_eq!(updater.state.scheduler_wait_end(), Some(scheduler_endpoint));
        let tracked = updater
            .state
            .sequence_operations
            .values()
            .next()
            .expect("generic time wait");
        assert_eq!(tracked.estimated_end, Some(scheduler_endpoint));
        {
            let deliveries = chat_state.deliveries.lock().unwrap();
            assert_eq!(deliveries.len(), 2);
            assert!(
                deliveries
                    .iter()
                    .any(|(message, _)| message.title.contains("Target Scheduler wait"))
            );
            assert!(
                deliveries
                    .iter()
                    .any(|(message, _)| message.title.contains("Timed wait started"))
            );
        }
        let status_time = DateTime::parse_from_rfc3339("2099-08-26T06:00:00Z")
            .expect("status time")
            .with_timezone(&Utc);
        let status = updater.format_startup_status_at(status_time);
        assert!(status.contains("Sequence wait until"));
        assert!(status.contains("Target Scheduler wait until"));
        let elapsed_status =
            updater.format_startup_status_at(scheduler_endpoint + chrono::Duration::seconds(1));
        assert!(elapsed_status.contains("Sequence wait reached its scheduled time"));
        assert!(elapsed_status.contains("Target Scheduler reached its scheduled time"));

        updater
            .reconcile_sequence_operations(Vec::new(), HashSet::new(), None, true)
            .await;

        assert!(updater.state.sequence_operations.is_empty());
        assert_eq!(updater.state.scheduler_wait_end(), Some(scheduler_endpoint));
        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 3);
        assert!(deliveries[2].0.title.contains("Timed wait ended"));
        assert!(
            deliveries[2]
                .0
                .fields
                .iter()
                .any(|field| field.name == "Planned until")
        );
        assert!(
            !deliveries[2]
                .0
                .fields
                .iter()
                .any(|field| field.name == "Countdown")
        );
    }

    #[tokio::test]
    async fn scheduler_wait_replacement_and_lifecycle_boundaries_are_explicit() {
        let mut updater = state_test_updater();
        let first_endpoint =
            DateTime::parse_from_rfc3339("2099-08-26T07:00:00Z").expect("first endpoint");
        let later_endpoint =
            DateTime::parse_from_rfc3339("2099-08-26T08:00:00Z").expect("later endpoint");
        let first =
            target_scheduler_wait_event("2099-08-26T05:00:00Z", "2099-08-26T07:00:00Z", true);
        let later =
            target_scheduler_wait_event("2099-08-26T06:00:00Z", "2099-08-26T08:00:00Z", true);

        updater.apply_event_state(&first);
        updater.apply_event_state(&later);
        updater.apply_event_state(&first);
        assert_eq!(
            updater.state.scheduler_wait_end(),
            Some(later_endpoint.with_timezone(&Utc))
        );

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:05:00Z",
            "malformed-endpoint",
            true,
        ));
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:10:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        let mut target_started = target_event(true, "M42");
        target_started.time = "2099-08-26T06:11:00Z".to_string();
        updater.apply_event_state(&target_started);
        assert!(updater.state.scheduler_wait.is_none());
        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:10:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:20:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        updater.apply_event_state(&Event {
            time: "2099-08-26T06:21:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:30:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        updater.apply_event_state(&Event {
            time: "2099-08-26T06:31:00Z".to_string(),
            event: event_types::SEQUENCE_FINISHED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:40:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        updater
            .handle_event(&target_scheduler_wait_event(
                "2099-08-26T06:41:00Z",
                "2099-08-26T08:00:00Z",
                false,
            ))
            .await;
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:50:00Z",
            "2099-08-26T07:00:00Z",
            true,
        ));
        assert_eq!(
            updater.state.scheduler_wait_end(),
            Some(first_endpoint.with_timezone(&Utc))
        );
        assert!(
            updater
                .format_startup_status_at(first_endpoint.with_timezone(&Utc))
                .contains("reached its scheduled time")
        );
    }

    #[tokio::test]
    async fn stale_scheduler_wait_after_terminal_transition_is_not_delivered() {
        let (mut updater, chat_state) = recording_test_updater();
        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:00:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));
        let mut target_started = target_event(true, "M42");
        target_started.time = "2099-08-26T06:10:00Z".to_string();
        updater.apply_event_state(&target_started);

        updater
            .handle_event(&target_scheduler_wait_event(
                "2099-08-26T05:00:00Z",
                "2099-08-26T07:00:00Z",
                true,
            ))
            .await;

        assert!(updater.state.scheduler_wait.is_none());
        assert!(chat_state.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn baseline_scheduler_wait_restores_status_without_replaying_start() {
        let (mut updater, chat_state) = recording_test_updater();
        let endpoint = DateTime::parse_from_rfc3339("2099-08-26T08:00:00Z").expect("wait endpoint");
        let event =
            target_scheduler_wait_event("2099-08-26T06:00:00Z", "2099-08-26T08:00:00Z", true);

        updater.process_baseline_events(std::slice::from_ref(&event));

        assert_eq!(
            updater.state.scheduler_wait_end(),
            Some(endpoint.with_timezone(&Utc))
        );
        let status_time = DateTime::parse_from_rfc3339("2099-08-26T06:00:00Z")
            .expect("status time")
            .with_timezone(&Utc);
        assert!(
            updater
                .format_startup_status_at(status_time)
                .contains("Target Scheduler wait until")
        );
        assert!(chat_state.deliveries.lock().unwrap().is_empty());

        updater.process_live_events(vec![event]).await;
        assert!(chat_state.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scheduler_wait_notification_uses_one_normalized_instant_and_a_countdown() {
        let (updater, chat_state) = recording_test_updater();
        let event = target_scheduler_wait_event(
            "2026-09-02T01:55:00.6436653+00:00",
            "2026-09-01T21:25:51.5404816-05:00",
            true,
        );
        let now = DateTime::parse_from_rfc3339("2026-09-02T01:55:00.6436653Z")
            .expect("fixed delivery time")
            .with_timezone(&Utc);

        updater
            .send_generic_event_notification_at(&event, now)
            .await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        let message = &deliveries[0].0;
        assert!(message.title.contains("Target Scheduler wait"));
        assert_eq!(message.matrix_timestamp_label.as_deref(), Some("Started"));
        let occurred_at = DateTime::parse_from_rfc3339(
            message
                .timestamp
                .as_deref()
                .expect("source occurrence timestamp"),
        )
        .expect("normalized occurrence timestamp");
        assert_eq!(
            occurred_at.with_timezone(&Utc).to_rfc3339(),
            "2026-09-02T01:55:00.643665300+00:00"
        );
        assert!(
            !message
                .fields
                .iter()
                .any(|field| matches!(field.name.as_str(), "Time" | "Wait Until"))
        );

        let end = DateTime::parse_from_rfc3339("2026-09-02T02:25:51.5404816Z")
            .expect("normalized wait endpoint")
            .with_timezone(&Utc);
        let until = message
            .fields
            .iter()
            .find(|field| field.name == "Until")
            .expect("Until field");
        assert_eq!(until.value, "2026-09-02 02:25:51 UTC");
        assert_eq!(
            until.discord_value.as_deref(),
            Some(format!("<t:{}:F>", end.timestamp()).as_str())
        );
        let countdown = message
            .fields
            .iter()
            .find(|field| field.name == "Countdown")
            .expect("Countdown field");
        assert_eq!(countdown.value, "30m 50s remaining");
        assert_eq!(
            countdown.discord_value.as_deref(),
            Some(format!("<t:{}:R>", end.timestamp()).as_str())
        );
    }

    #[tokio::test]
    async fn malformed_scheduler_wait_timestamps_remain_visible_without_native_markup() {
        let (updater, chat_state) = recording_test_updater();
        let event = target_scheduler_wait_event("legacy-start", "legacy-end", true);

        updater.send_generic_event_notification(&event).await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        let message = &deliveries[0].0;
        assert!(message.matrix_timestamp_label.is_none());
        assert!(message.fields.iter().any(|field| {
            field.name == "Started"
                && field.value == "legacy-start"
                && field.discord_value.is_none()
        }));
        assert!(message.fields.iter().any(|field| {
            field.name == "Until" && field.value == "legacy-end" && field.discord_value.is_none()
        }));
        assert!(!message.fields.iter().any(|field| field.name == "Countdown"));
    }

    #[tokio::test]
    async fn disabled_legacy_event_details_never_enter_baseline_or_live_status_state() {
        let source = Arc::new(FlakyAutofocusSource::new(target_event(
            false,
            "Private target",
        )));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        );

        let disabled_events = vec![
            target_event(false, "Private baseline target"),
            Event {
                time: "2026-08-26T06:00:01Z".to_string(),
                event: event_types::FILTERWHEEL_CHANGED.to_string(),
                chat_enabled: false,
                details: Some(EventDetails::FilterWheelChange {
                    new: FilterInfo {
                        name: "Private filter".to_string(),
                        id: 4,
                    },
                    previous: FilterInfo {
                        name: "Old private filter".to_string(),
                        id: 3,
                    },
                }),
            },
            Event {
                time: "2026-08-26T06:00:02Z".to_string(),
                event: event_types::TS_WAITSTART.to_string(),
                chat_enabled: false,
                details: Some(EventDetails::WaitStart {
                    wait_end_time: "2026-08-26T07:00:00Z".to_string(),
                }),
            },
            Event {
                time: "2026-08-26T06:00:03Z".to_string(),
                event: event_types::MOUNT_PARKED.to_string(),
                chat_enabled: false,
                details: None,
            },
            Event {
                time: "2026-08-26T06:00:04Z".to_string(),
                event: event_types::GUIDER_START.to_string(),
                chat_enabled: false,
                details: None,
            },
            Event {
                time: "2026-08-26T06:00:05Z".to_string(),
                event: event_types::SEQUENCE_STARTING.to_string(),
                chat_enabled: false,
                details: None,
            },
        ];

        updater.process_baseline_events(&disabled_events);
        assert!(updater.state.current_target.is_none());
        assert!(updater.state.last_filter.is_none());
        assert!(updater.state.scheduler_wait.is_none());
        assert!(updater.state.last_mount_event.is_none());
        assert!(updater.state.last_guider_event.is_none());
        assert!(!updater.state.sequence_running);
        let retained_keys = updater
            .state
            .events_seen
            .seen
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!retained_keys.contains("Private"));
        assert!(!retained_keys.contains("07:00:00"));

        for event in &disabled_events {
            updater.handle_event(event).await;
        }
        assert!(updater.state.current_target.is_none());
        assert!(updater.state.last_filter.is_none());
        assert!(updater.state.scheduler_wait.is_none());
        assert!(updater.state.last_mount_event.is_none());
        assert!(updater.state.last_guider_event.is_none());
        assert!(!updater.state.sequence_running);

        updater.state.current_target = Some(TargetInfo {
            name: "Previously shared target".to_string(),
            source: TargetSource::TsTargetStart,
            coordinates: None,
            project: None,
            rotation: None,
            target_end_time: None,
        });
        assert!(
            updater
                .reconcile_sequence_target(Some(("Private projection".to_string(), false)))
                .is_none()
        );
        assert!(updater.state.current_target.is_none());
    }

    #[test]
    fn enabled_sequence_target_still_updates_when_no_scheduler_override_exists() {
        let source = Arc::new(FlakyAutofocusSource::new(target_event(true, "M42")));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        );

        let (_, target) = updater
            .reconcile_sequence_target(Some(("M42".to_string(), true)))
            .expect("enabled target should update state");
        assert_eq!(target.name, "M42");
        assert_eq!(
            updater
                .state
                .current_target
                .as_ref()
                .map(|target| target.name.as_str()),
            Some("M42")
        );
    }

    #[test]
    fn new_target_event_reconstructs_target_during_baseline() {
        let mut event = target_event(true, "M31");
        event.event = event_types::TS_NEWTARGETSTART.to_string();
        let source = Arc::new(FlakyAutofocusSource::new(event.clone()));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        );

        updater.process_baseline_events(&[event]);

        assert_eq!(
            updater
                .state
                .current_target
                .as_ref()
                .map(|target| target.name.as_str()),
            Some("M31")
        );
    }

    #[test]
    fn target_scheduler_tombstone_clears_only_scheduler_wait_state() {
        let source = Arc::new(FlakyAutofocusSource::new(target_event(true, "M31")));
        let mut updater = ChatUpdater::new(
            source,
            "Backyard Rig".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        );
        let started = Utc::now();
        let tracked = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::TimeWait {
                target_time: None,
                configured_duration: Some(chrono::Duration::minutes(5)),
            }),
            started,
            None,
        );
        let intrinsic = tracked.estimated_end;
        updater.state.scheduler_wait = Some(SchedulerWaitState {
            end_at: started + chrono::Duration::hours(8),
        });
        updater
            .state
            .sequence_operations
            .insert(tracked.operation.key.clone(), tracked);

        updater.revoke_state_for_disabled_event(event_types::TS_WAITSTART, None);

        let retained = updater.state.sequence_operations.values().next().unwrap();
        assert_eq!(retained.estimated_end, intrinsic);
        assert!(updater.state.scheduler_wait.is_none());
    }

    #[test]
    fn sequence_tombstone_preserves_independent_scheduler_wait_state() {
        let mut updater = state_test_updater();
        let wait =
            target_scheduler_wait_event("2099-08-26T06:00:00Z", "2099-08-26T08:00:00Z", true);
        updater.apply_event_state(&wait);
        let endpoint = updater.state.scheduler_wait_end();

        updater.revoke_state_for_disabled_event(
            event_types::SEQUENCE_STARTING,
            Some("2099-08-26T06:30:00Z"),
        );

        assert_eq!(updater.state.scheduler_wait_end(), endpoint);
    }

    #[test]
    fn malformed_time_privacy_tombstone_still_clears_scheduler_wait() {
        let mut updater = state_test_updater();
        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T06:00:00Z",
            "2099-08-26T08:00:00Z",
            true,
        ));

        updater.revoke_state_for_disabled_event(event_types::TS_WAITSTART, Some("malformed"));
        assert!(updater.state.scheduler_wait.is_none());

        updater.apply_event_state(&target_scheduler_wait_event(
            "2099-08-26T05:00:00Z",
            "2099-08-26T07:00:00Z",
            true,
        ));
        assert!(updater.state.scheduler_wait.is_none());
    }

    #[tokio::test]
    async fn new_time_wait_keeps_its_intrinsic_target_over_scheduler_state() {
        let mut updater = state_test_updater();
        let scheduler_end =
            DateTime::parse_from_rfc3339("2099-08-26T08:00:00Z").expect("scheduler end");
        let intrinsic_end =
            DateTime::parse_from_rfc3339("2099-08-26T04:00:00-07:00").expect("item target");
        updater.state.scheduler_wait = Some(SchedulerWaitState {
            end_at: scheduler_end.with_timezone(&Utc),
        });

        updater
            .reconcile_sequence_operations(
                vec![operation(SequenceOperationKind::TimeWait {
                    target_time: Some(intrinsic_end),
                    configured_duration: None,
                })],
                HashSet::new(),
                None,
                false,
            )
            .await;

        let tracked = updater
            .state
            .sequence_operations
            .values()
            .next()
            .expect("tracked time wait");
        assert_eq!(
            tracked.estimated_end,
            Some(intrinsic_end.with_timezone(&Utc))
        );
    }

    #[test]
    fn nina_timestamps_parse_with_and_without_an_offset() {
        assert!(parse_nina_timestamp("2026-08-17T04:00:00-07:00").is_some());
        // DateTimeKind.Unspecified serializes without an offset; these used to
        // be dropped, leaving the sequence wait state unset.
        assert!(parse_nina_timestamp("2026-08-17T04:00:00").is_some());
        assert!(parse_nina_timestamp("2026-08-17T04:00:00.1234567").is_some());
        assert!(parse_nina_timestamp("not a timestamp").is_none());
    }

    #[test]
    fn autofocus_v3_identity_requires_the_exact_report_timestamp() {
        let expected = "2026-08-26T22:15:30.1250000-07:00";
        assert!(autofocus_report_matches(Some(expected), expected));
        assert!(autofocus_report_matches(
            Some(expected),
            "2026-08-27T05:15:30.1250000Z"
        ));
        assert!(!autofocus_report_matches(
            Some(expected),
            "2026-08-26T22:15:31.1250000-07:00"
        ));
        assert!(autofocus_report_matches(
            None,
            "2026-08-26T22:15:31.1250000-07:00"
        ));
    }

    #[test]
    fn fully_offsetless_legacy_scheduler_timestamps_are_deterministic_utc() {
        // This is the exact timestamp shape retained in
        // example_event-history_2.json by the legacy ninaAPI Target Scheduler
        // integration: both the event and scheduled end omit an offset.
        let parsed = parse_nina_timestamp_with_context(
            "2025-08-17T05:51:20.6380468",
            Some("2025-08-17T05:23:53.1567244"),
        )
        .expect("legacy scheduler timestamp");
        let expected = DateTime::parse_from_rfc3339("2025-08-17T05:51:20.6380468Z")
            .expect("expected UTC timestamp");

        assert_eq!(parsed, expected);
        assert_eq!(parsed.offset().local_minus_utc(), 0);
    }

    #[test]
    fn offsetless_nina_timestamps_use_the_event_observatory_offset() {
        let contextual = parse_nina_timestamp_with_context(
            "2026-08-26T04:00:00",
            Some("2026-08-25T23:00:00-07:00"),
        )
        .expect("contextual timestamp");
        assert_eq!(contextual.offset().local_minus_utc(), -7 * 60 * 60);
        assert_eq!(contextual.to_rfc3339(), "2026-08-26T04:00:00-07:00");

        // An explicit value is authoritative even when the event used a
        // different observatory offset.
        let explicit = parse_nina_timestamp_with_context(
            "2026-08-26T04:00:00+02:00",
            Some("2026-08-25T23:00:00-07:00"),
        )
        .expect("explicit timestamp");
        assert_eq!(explicit.offset().local_minus_utc(), 2 * 60 * 60);
    }

    #[test]
    fn scheduler_wait_uses_event_observatory_offset() {
        let mut updater = state_test_updater();
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2099-08-25T23:00:00-07:00",
            "Event": "TS-WAITSTART",
            "WaitEndTime": "2099-08-26T04:00:00"
        }))
        .unwrap();
        updater.apply_event_state(&event);
        let wait_until = updater
            .state
            .scheduler_wait_end()
            .expect("scheduler wait end");
        assert_eq!(wait_until.to_rfc3339(), "2099-08-26T11:00:00+00:00");
    }

    #[test]
    fn chat_titles_stay_within_the_discord_limit() {
        let header = "E".repeat(4_000);
        let title = format!("🔔 N.I.N.A. · {}", truncate_chat_title(&header));
        // Discord rejects the whole message when the title exceeds 256, and
        // the caller still prepends "[telescope] ".
        assert!(title.chars().count() < 256);
        assert!(get_event_title(&"X".repeat(4_000)).chars().count() < 256);
    }

    #[test]
    fn hardware_command_failures_have_visible_error_presentation() {
        assert_eq!(
            get_event_color(event_types::CHATSTRONOMY_COMMAND_FAILED),
            colors::RED
        );
        assert_eq!(
            get_event_title(event_types::CHATSTRONOMY_COMMAND_FAILED),
            "❌ Telescope command failed"
        );
    }

    #[test]
    fn bounded_seen_set_evicts_the_oldest_key() {
        let mut seen = BoundedSeenSet::new(2);
        assert!(!seen.check_and_insert("first".to_string()));
        assert!(!seen.check_and_insert("second".to_string()));
        assert!(seen.check_and_insert("first".to_string()));
        assert!(!seen.check_and_insert("third".to_string()));
        assert_eq!(seen.len(), 2);
        assert!(seen.check_and_insert("first".to_string()));
        assert!(!seen.check_and_insert("second".to_string()));
    }

    #[test]
    fn disabled_plate_solve_does_not_consume_its_delivery_key() {
        let mut state = UpdaterState::new();
        let key = "solve-1".to_string();
        assert!(!claim_plate_solve_output(
            &mut state.plate_solve_outputs_seen,
            false,
            Some(&key),
        ));
        assert!(claim_plate_solve_output(
            &mut state.plate_solve_outputs_seen,
            true,
            Some(&key),
        ));
        assert!(!claim_plate_solve_output(
            &mut state.plate_solve_outputs_seen,
            true,
            Some(&key),
        ));
    }

    #[test]
    fn plate_solve_dedup_identity_does_not_retain_solve_timestamp() {
        let state = UpdaterState::new();
        let private_timestamp = "2026-08-26T06:12:34.567-private";
        let operation = operation(SequenceOperationKind::MountCenter {
            coordinates: None,
            rotation: None,
            output: Some(Box::new(crate::sequence::PlateSolveOutput {
                solve_time: Some(private_timestamp.to_string()),
                success: Some(true),
                coordinates: None,
                position_angle: None,
                pixel_scale: None,
                radius_degrees: None,
                separation_arcseconds: None,
                ra_error: None,
                dec_error: None,
                ra_pixel_error: None,
                dec_pixel_error: None,
                flipped: None,
                thumbnail: None,
                thumbnail_media_type: None,
            })),
        });

        let key = plate_solve_output_key(&state.dedup_hasher, &operation).unwrap();
        assert!(key.starts_with("p:"));
        assert!(!key.contains(private_timestamp));
    }

    #[test]
    fn nina_timestamp_accepts_offset_and_observatory_local_values() {
        let offset = parse_nina_timestamp("2026-08-16T20:00:00-07:00").expect("offset time");
        assert_eq!(offset.offset().local_minus_utc(), -7 * 60 * 60);

        let local = parse_nina_timestamp("2026-08-16T20:00:00.1234567").expect("local time");
        assert_eq!(
            local.naive_local(),
            NaiveDateTime::parse_from_str("2026-08-16T20:00:00.1234567", "%Y-%m-%dT%H:%M:%S%.f")
                .unwrap()
        );
    }

    #[test]
    fn chat_titles_are_bounded_below_discords_limit() {
        let title = truncate_chat_title(&"x".repeat(400));
        assert_eq!(title.chars().count(), 180);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn timed_wait_progress_reaches_notification_milestones() {
        let now = Utc::now();
        let tracked = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::TimeWait {
                target_time: None,
                configured_duration: Some(chrono::Duration::seconds(100)),
            }),
            now,
            None,
        );

        assert_eq!(tracked.progress_percent(now), Some(0));
        assert_eq!(
            tracked.progress_percent(now + chrono::Duration::seconds(51)),
            Some(51)
        );
        assert_eq!(
            tracked.next_milestone(now + chrono::Duration::seconds(51)),
            Some(50)
        );
    }

    #[test]
    fn cooling_progress_uses_live_camera_temperature() {
        let now = Utc::now();
        let initial = CameraInfo {
            connected: true,
            can_set_temperature: true,
            cooler_on: true,
            cooler_power: 80.0,
            temperature: 10.0,
            temperature_set_point: -10.0,
            at_target_temp: false,
            name: "Camera".to_string(),
            display_name: "Camera".to_string(),
        };
        let mut tracked = TrackedSequenceOperation::new(
            operation(SequenceOperationKind::CameraCooling {
                target_temperature: -10.0,
                minimum_duration: Some(chrono::Duration::minutes(10)),
            }),
            now,
            Some(initial.clone()),
        );
        tracked.camera = Some(CameraInfo {
            temperature: 0.0,
            ..initial
        });

        assert_eq!(tracked.progress_percent(now), Some(50));
        assert_eq!(tracked.next_milestone(now), Some(50));
    }

    fn state_test_updater() -> ChatUpdater {
        let event = Event {
            time: "2026-08-26T00:00:00Z".to_string(),
            event: event_types::CAMERA_CONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        };
        ChatUpdater::new(
            Arc::new(FlakyAutofocusSource::unavailable_for(event, 0)),
            "State Test".to_string(),
            ChatTarget::default(),
            Arc::new(ChatServiceManager::new()),
        )
    }

    fn recording_test_updater() -> (ChatUpdater, Arc<RecordingChatState>) {
        let event = Event {
            time: "2026-08-26T00:00:00Z".to_string(),
            event: event_types::CAMERA_CONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        };
        let state = Arc::new(RecordingChatState::default());
        let mut manager = ChatServiceManager::new();
        manager.add_service(Box::new(RecordingChatService {
            state: state.clone(),
        }));
        (
            ChatUpdater::new(
                Arc::new(FlakyAutofocusSource::unavailable_for(event, 0)),
                "State Test".to_string(),
                ChatTarget::default(),
                Arc::new(manager),
            ),
            state,
        )
    }

    fn message_field<'a>(message: &'a ChatMessage, name: &str) -> Option<&'a str> {
        message
            .fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.as_str())
    }

    #[tokio::test]
    async fn motion_edge_messages_use_event_time_positions_and_starts_are_not_durable_state() {
        let (mut updater, chat_state) = recording_test_updater();
        updater.state.last_mount_event = Some(event_types::MOUNT_PARKED.to_string());

        let mount_start: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T01:00:00Z",
            "Event": "MOUNT-SLEW-STARTED",
            "MotionId": 42,
            "From": {
                "RAString": "01:15:00",
                "DecString": "-12:30:00",
                "Epoch": "J2000",
                "Altitude": 31.25,
                "Azimuth": 127.5
            },
            "Target": {
                "RAString": "03:30:00",
                "DecString": "+22:00:00",
                "Epoch": "J2000"
            }
        }))
        .unwrap();
        updater.handle_event(&mount_start).await;
        assert_eq!(
            updater.state.last_mount_event.as_deref(),
            Some(event_types::MOUNT_PARKED),
            "a transient slew start must not replace durable mount state"
        );

        let mount_end: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T01:00:12Z",
            "Event": "MOUNT-SLEWED",
            "MotionId": 42,
            "From": {
                "RAString": "01:15:00",
                "DecString": "-12:30:00",
                "Epoch": "J2000",
                "Altitude": 31.25,
                "Azimuth": 127.5
            },
            "Target": {
                "RAString": "03:30:00",
                "DecString": "+22:00:00",
                "Epoch": "J2000"
            },
            "To": {
                "RAString": "03:29:59",
                "DecString": "+21:59:58",
                "Epoch": "J2000",
                "Altitude": 52.0,
                "Azimuth": 201.75
            },
            "DurationSeconds": 12.25,
            "EndDetection": "motion_state"
        }))
        .unwrap();
        updater.handle_event(&mount_end).await;

        let rotator_start: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T01:01:00Z",
            "Event": "ROTATOR-MOVE-STARTED",
            "MotionId": 43,
            "Position": 12.5,
            "MechanicalPosition": 87.5
        }))
        .unwrap();
        updater.handle_event(&rotator_start).await;

        let rotator_end: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T01:01:02Z",
            "Event": "ROTATOR-MOVED-MECHANICAL",
            "MotionId": 43,
            "From": 87.5,
            "To": 90.0,
            "Position": 15.0,
            "MechanicalFrom": 87.5,
            "MechanicalTo": 90.0,
            "DurationSeconds": 2.0,
            "EndDetection": "nina_moved"
        }))
        .unwrap();
        updater.handle_event(&rotator_end).await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 4);

        let mount_start_message = &deliveries[0].0;
        assert!(mount_start_message.title.contains("Mount Slew Started"));
        let start_position = message_field(mount_start_message, "From position").unwrap();
        assert!(start_position.contains("RA 01:15:00 · Dec -12:30:00 (J2000)"));
        assert!(start_position.contains("Alt 31.25° · Az 127.50°"));
        assert!(message_field(mount_start_message, "Start position").is_none());
        assert!(
            message_field(mount_start_message, "Requested destination")
                .unwrap()
                .contains("RA 03:30:00 · Dec +22:00:00 (J2000)")
        );

        let mount_end_message = &deliveries[1].0;
        assert!(mount_end_message.title.contains("Mount Slew Ended"));
        assert!(!mount_end_message.title.contains("Completed"));
        assert!(message_field(mount_end_message, "From position").is_some());
        let end_position = message_field(mount_end_message, "To position").unwrap();
        assert!(end_position.contains("RA 03:29:59 · Dec +21:59:58 (J2000)"));
        assert!(end_position.contains("Alt 52.00° · Az 201.75°"));
        assert!(message_field(mount_end_message, "End position").is_none());
        assert_eq!(
            message_field(mount_end_message, "Observed interval"),
            Some("12.25 s")
        );
        assert_eq!(
            message_field(mount_end_message, "End detected by"),
            Some("Equipment first reported idle")
        );

        let rotator_start_message = &deliveries[2].0;
        assert!(rotator_start_message.title.contains("Rotator move started"));
        assert_eq!(
            message_field(rotator_start_message, "From position"),
            Some("12.50°")
        );
        assert_eq!(
            message_field(rotator_start_message, "Mechanical from"),
            Some("87.50°")
        );
        assert!(message_field(rotator_start_message, "Start position").is_none());
        assert!(message_field(rotator_start_message, "Start mechanical position").is_none());

        let rotator_end_message = &deliveries[3].0;
        assert!(
            rotator_end_message
                .title
                .contains("Rotator mechanical move ended")
        );
        assert_eq!(
            message_field(rotator_end_message, "To position"),
            Some("15.00°")
        );
        assert_eq!(
            message_field(rotator_end_message, "Mechanical from"),
            Some("87.50°")
        );
        assert_eq!(
            message_field(rotator_end_message, "Mechanical to"),
            Some("90.00°")
        );
        assert!(message_field(rotator_end_message, "End position").is_none());
        assert!(message_field(rotator_end_message, "Mechanical start").is_none());
        assert!(message_field(rotator_end_message, "Mechanical end").is_none());
        assert_eq!(
            message_field(rotator_end_message, "End detected by"),
            Some("N.I.N.A. move event")
        );
    }

    #[tokio::test]
    async fn callback_recovered_motion_starts_have_recovered_titles_without_an_interval() {
        let (mut updater, chat_state) = recording_test_updater();

        let mount_start: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T02:00:00Z",
            "Event": "MOUNT-SLEW-STARTED",
            "MotionId": 51,
            "From": {
                "RAString": "05:00:00",
                "DecString": "+10:00:00",
                "Epoch": "J2000"
            },
            "ObservedInProgress": true
        }))
        .unwrap();
        updater.handle_event(&mount_start).await;

        let rotator_start: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-31T02:00:01Z",
            "Event": "ROTATOR-MOVE-STARTED",
            "MotionId": 52,
            "Position": 21.5,
            "MechanicalPosition": 81.5,
            "ObservedInProgress": true
        }))
        .unwrap();
        updater.handle_event(&rotator_start).await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);

        let mount_message = &deliveries[0].0;
        assert!(mount_message.title.contains("Mount Slew Recovered"));
        assert!(!mount_message.title.contains("Mount Slew Started"));
        assert_eq!(
            message_field(mount_message, "Capture"),
            Some("Recovered after motion began")
        );
        assert!(message_field(mount_message, "Observed interval").is_none());
        assert!(message_field(mount_message, "From position").is_some());
        assert!(message_field(mount_message, "Start position").is_none());

        let rotator_message = &deliveries[1].0;
        assert!(rotator_message.title.contains("Rotator move recovered"));
        assert!(!rotator_message.title.contains("Rotator move started"));
        assert_eq!(
            message_field(rotator_message, "Capture"),
            Some("Recovered after motion began")
        );
        assert!(message_field(rotator_message, "Observed interval").is_none());
        assert_eq!(
            message_field(rotator_message, "From position"),
            Some("21.50°")
        );
        assert_eq!(
            message_field(rotator_message, "Mechanical from"),
            Some("81.50°")
        );
        assert!(message_field(rotator_message, "Start position").is_none());
        assert!(message_field(rotator_message, "Start mechanical position").is_none());
    }

    #[tokio::test]
    async fn astronomical_wait_and_plate_solve_have_lifecycle_and_dedup_delivery() {
        let (mut updater, chat_state) = recording_test_updater();

        let astronomical = operation(SequenceOperationKind::AstronomicalWait {
            target_altitude_degrees: Some(30.0),
            current_altitude_degrees: Some(18.5),
            comparator: Some("GREATER_THAN".to_string()),
            expected_time: Some("2026-08-26T04:30:00-07:00".to_string()),
        });
        updater
            .reconcile_sequence_operations(vec![astronomical.clone()], HashSet::new(), None, true)
            .await;
        assert!(
            updater
                .format_startup_status()
                .contains("🌌 Test operation")
        );
        assert!(
            updater
                .format_startup_status()
                .contains("target GREATER_THAN 30.00°")
        );
        let mut astronomical_finished = astronomical;
        astronomical_finished.status = "FINISHED".to_string();
        updater
            .reconcile_sequence_operations(vec![astronomical_finished], HashSet::new(), None, true)
            .await;

        let output = PlateSolveOutput {
            solve_time: Some("2026-08-26T11:30:00Z".to_string()),
            success: Some(true),
            coordinates: None,
            position_angle: Some(91.45),
            pixel_scale: Some(1.25),
            radius_degrees: None,
            separation_arcseconds: Some(2.4),
            ra_error: None,
            dec_error: None,
            ra_pixel_error: None,
            dec_pixel_error: None,
            flipped: None,
            thumbnail: Some(vec![1, 2, 3]),
            thumbnail_media_type: Some("image/jpeg".to_string()),
        };
        let plate_solve = operation(SequenceOperationKind::PlateSolve {
            coordinates: None,
            rotation: Some(91.5),
            output: Some(Box::new(output)),
        });

        // A local privacy tombstone neither renders nor consumes this solve's
        // output identity, so enabling the operation later still delivers it.
        updater
            .reconcile_sequence_operations(
                Vec::new(),
                HashSet::from([plate_solve.key.clone()]),
                None,
                true,
            )
            .await;
        let before_solve = chat_state.deliveries.lock().unwrap().len();
        updater
            .reconcile_sequence_operations(vec![plate_solve.clone()], HashSet::new(), None, true)
            .await;
        let after_first_solve = chat_state.deliveries.lock().unwrap().len();
        assert_eq!(after_first_solve, before_solve + 2);

        updater
            .reconcile_sequence_operations(vec![plate_solve.clone()], HashSet::new(), None, true)
            .await;
        assert_eq!(
            chat_state.deliveries.lock().unwrap().len(),
            after_first_solve
        );

        let mut plate_solve_finished = plate_solve;
        plate_solve_finished.status = "FINISHED".to_string();
        updater
            .reconcile_sequence_operations(vec![plate_solve_finished], HashSet::new(), None, true)
            .await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert!(
            deliveries
                .iter()
                .any(|(message, _)| message.title.contains("Astronomical wait started"))
        );
        assert!(
            deliveries
                .iter()
                .any(|(message, _)| message.title.contains("Astronomical condition reached"))
        );
        assert!(
            deliveries
                .iter()
                .any(|(message, _)| message.title.contains("Plate solve started"))
        );
        let solve_results = deliveries
            .iter()
            .filter(|(message, _)| message.title.contains("Plate solve result"))
            .collect::<Vec<_>>();
        assert_eq!(solve_results.len(), 1);
        assert_eq!(solve_results[0].1.len(), 1);
        assert!(deliveries.iter().any(|(message, attachments)| {
            message.title.contains("Plate solve finished") && attachments.is_empty()
        }));
        assert!(updater.state.sequence_operations.is_empty());
    }

    #[test]
    fn safety_and_observatory_state_survive_baseline_without_colliding() {
        let mut updater = state_test_updater();
        let events = [
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:00Z", "Event": "SAFETY-CONNECTED"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:01Z", "Event": "SAFETY-CHANGED", "IsSafe": false
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:02Z", "Event": "DOME-CONNECTED"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:03Z", "Event": "DOME-SHUTTER-OPENED"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:04Z", "Event": "DOME-SLEWED",
                "FromAzimuth": 20.0, "ToAzimuth": 40.0
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:05Z", "Event": "DOME-SYNCED"
            }))
            .unwrap(),
        ];

        updater.process_baseline_events(&events);
        assert_eq!(updater.state.safety_state, SafetyState::Unsafe);
        assert_eq!(updater.state.dome_connected, Some(true));
        assert_eq!(updater.state.dome_shutter_open, Some(true));
        assert_eq!(updater.state.dome_azimuth, None);
        let status = updater.format_startup_status();
        assert!(status.contains("Conditions unsafe"));
        assert!(status.contains("shutter open"));
        assert!(!status.contains("azimuth 40.00°"));
    }

    #[test]
    fn weather_change_and_high_wind_state_have_independent_privacy_scopes() {
        let mut updater = state_test_updater();
        let unusable = [
            Event {
                time: "2026-08-25T23:59:58Z".to_string(),
                event: event_types::WEATHER_CHANGED.to_string(),
                chat_enabled: true,
                details: Some(EventDetails::WeatherChanged {
                    changed_fields: "temperature".to_string(),
                    summary: None,
                    conditions: WeatherConditions::default(),
                }),
            },
            Event {
                time: "2026-08-25T23:59:59Z".to_string(),
                event: event_types::WEATHER_HIGH_WIND.to_string(),
                chat_enabled: true,
                details: Some(EventDetails::WeatherHighWind {
                    is_high_wind: true,
                    threshold_meters_per_second: Some(9.0),
                    conditions: WeatherConditions::default(),
                }),
            },
        ];
        updater.process_baseline_events(&unusable);
        assert_eq!(updater.state.weather_conditions, None);
        assert_eq!(updater.state.weather_high_wind, None);

        let events = [
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:00Z",
                "Event": "WEATHER-CONNECTED"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:01Z",
                "Event": "WEATHER-CHANGED",
                "ChangedFields": "temperature, humidity",
                "TemperatureCelsius": 9.4,
                "HumidityPercent": 67.0,
                "CloudCoverPercent": 22.0
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:00:02Z",
                "Event": "WEATHER-HIGH-WIND",
                "IsHighWind": true,
                "WindSpeedMetersPerSecond": 9.5,
                "WindGustMetersPerSecond": 13.2,
                "ThresholdMetersPerSecond": 9.0
            }))
            .unwrap(),
        ];

        updater.process_baseline_events(&events);
        let status = updater.format_startup_status();
        assert!(status.contains("Weather · connected, HIGH WIND (limit 9.0 m/s)"));
        assert!(status.contains("wind 9.5 m/s"));
        assert!(status.contains("gust 13.2 m/s"));
        assert!(status.contains("9.4 °C"));

        updater.revoke_state_for_disabled_event(event_types::WEATHER_CHANGED, None);
        assert_eq!(updater.state.weather_conditions, None);
        assert_eq!(updater.state.weather_high_wind, Some(true));
        let high_wind_only = updater.format_startup_status();
        assert!(high_wind_only.contains("HIGH WIND"));
        assert!(high_wind_only.contains("wind 9.5 m/s"));
        assert!(!high_wind_only.contains("9.4 °C"));

        updater.revoke_state_for_disabled_event(event_types::WEATHER_HIGH_WIND, None);
        assert_eq!(updater.state.weather_connected, Some(true));
        assert_eq!(updater.state.weather_high_wind, None);
        assert_eq!(updater.state.weather_high_wind_conditions, None);
        assert_eq!(
            updater.state.weather_high_wind_threshold_meters_per_second,
            None
        );
        assert_eq!(updater.format_startup_status(), "🌦️ Weather · connected");
    }

    #[tokio::test]
    async fn weather_notifications_are_compact_and_high_wind_refreshes_are_silent() {
        let (mut updater, chat_state) = recording_test_updater();
        let changed: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:01Z",
            "Event": "WEATHER-CHANGED",
            "ChangedFields": "wind, temperature, humidity",
            "Summary": "Wind increased and humidity fell",
            "WindSpeedMetersPerSecond": 5.2,
            "WindGustMetersPerSecond": 7.8,
            "TemperatureCelsius": 10.4,
            "HumidityPercent": 62.0,
            "PressureHectopascals": 1011.8,
            "CloudCoverPercent": 18.0,
            "RainRateMillimetersPerHour": 0.0
        }))
        .unwrap();
        let alert: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:02Z",
            "Event": "WEATHER-HIGH-WIND",
            "IsHighWind": true,
            "WindSpeedMetersPerSecond": 9.5,
            "WindGustMetersPerSecond": 13.2,
            "ThresholdMetersPerSecond": 9.0
        }))
        .unwrap();
        let refresh: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:03Z",
            "Event": "WEATHER-HIGH-WIND",
            "IsHighWind": true,
            "WindSpeedMetersPerSecond": 9.5,
            "WindGustMetersPerSecond": 13.2,
            "ThresholdMetersPerSecond": 8.5
        }))
        .unwrap();
        let recovery: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:04Z",
            "Event": "WEATHER-HIGH-WIND",
            "IsHighWind": false,
            "WindSpeedMetersPerSecond": 4.0,
            "WindGustMetersPerSecond": 6.1,
            "ThresholdMetersPerSecond": 8.5
        }))
        .unwrap();
        let malformed: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:05Z",
            "Event": "WEATHER-HIGH-WIND",
            "WindSpeedMetersPerSecond": 30.0
        }))
        .unwrap();
        let connected = Event {
            time: "2026-08-26T00:00:02.5Z".to_string(),
            event: event_types::WEATHER_CONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        };

        updater.handle_event(&changed).await;
        updater.handle_event(&alert).await;
        updater.apply_event_state(&connected);
        assert_eq!(updater.state.weather_high_wind, Some(true));
        updater.handle_event(&refresh).await;
        assert_eq!(
            updater.state.weather_high_wind_threshold_meters_per_second,
            Some(8.5)
        );
        updater.handle_event(&recovery).await;
        updater.process_live_events(vec![malformed]).await;

        let deliveries = chat_state.deliveries.lock().unwrap();
        assert_eq!(
            deliveries.len(),
            3,
            "same-state refresh and malformed transitions stay silent"
        );
        let changed_message = &deliveries[0].0;
        assert!(changed_message.title.contains("Weather changed"));
        assert!(changed_message.fields.iter().any(|field| {
            field.name == "Changed" && field.value == "wind, temperature, humidity"
        }));
        assert!(
            changed_message
                .fields
                .iter()
                .any(|field| field.name == "Wind" && field.value.contains("5.2 m/s speed"))
        );
        assert!(changed_message.fields.iter().any(|field| {
            field.name == "Atmosphere"
                && field.value.contains("10.4 °C")
                && field.value.contains("62% RH")
                && field.value.contains("1011.8 hPa")
        }));

        assert!(deliveries[1].0.title.contains("High wind reported"));
        assert_eq!(deliveries[1].0.color, Some(colors::RED));
        assert!(deliveries[2].0.title.contains("Wind conditions recovered"));
        assert_eq!(deliveries[2].0.color, Some(colors::GREEN));
        assert_eq!(updater.state.weather_high_wind, Some(false));
        let status = updater.format_startup_status();
        assert!(!status.contains("HIGH WIND"));
        assert!(status.contains("wind 4.0 m/s"));
        assert!(!status.contains("10.4 °C"));
    }

    #[test]
    fn weather_reconnect_clears_measurements_but_preserves_latched_alert() {
        let mut updater = state_test_updater();
        updater.state.weather_connected = Some(true);
        updater.state.weather_conditions = Some(WeatherConditions {
            temperature_celsius: Some(12.0),
            ..WeatherConditions::default()
        });
        updater.state.weather_high_wind = Some(true);
        updater.state.weather_high_wind_conditions = Some(WeatherConditions {
            wind_speed_meters_per_second: Some(10.0),
            ..WeatherConditions::default()
        });
        updater.state.weather_high_wind_threshold_meters_per_second = Some(9.0);

        updater.apply_event_state(&Event {
            time: "2026-08-26T00:01:00Z".to_string(),
            event: event_types::WEATHER_DISCONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        });

        assert_eq!(updater.state.weather_connected, Some(false));
        assert_eq!(updater.state.weather_conditions, None);
        assert_eq!(updater.state.weather_high_wind, Some(true));
        assert_eq!(
            updater
                .state
                .weather_high_wind_conditions
                .as_ref()
                .and_then(|conditions| conditions.wind_speed_meters_per_second),
            Some(10.0)
        );
        assert_eq!(
            updater.state.weather_high_wind_threshold_meters_per_second,
            Some(9.0)
        );
        let disconnected = updater.format_startup_status();
        assert!(disconnected.contains("Weather · disconnected, HIGH WIND"));

        updater.apply_event_state(&Event {
            time: "2026-08-26T00:02:00Z".to_string(),
            event: event_types::WEATHER_CONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert_eq!(updater.state.weather_connected, Some(true));
        assert_eq!(updater.state.weather_high_wind, Some(true));
        assert!(
            updater
                .format_startup_status()
                .contains("Weather · connected, HIGH WIND")
        );

        updater.state.weather_high_wind = Some(false);
        updater.state.weather_high_wind_conditions = Some(WeatherConditions {
            wind_speed_meters_per_second: Some(4.0),
            ..WeatherConditions::default()
        });
        updater.state.weather_high_wind_conditions_at = Some(Utc::now());
        updater.state.weather_high_wind_threshold_meters_per_second = Some(9.0);
        updater.apply_event_state(&Event {
            time: "2026-08-26T00:03:00Z".to_string(),
            event: event_types::WEATHER_DISCONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert_eq!(updater.state.weather_high_wind, None);
        assert_eq!(updater.state.weather_high_wind_conditions, None);
        assert_eq!(
            updater.state.weather_high_wind_threshold_meters_per_second,
            None
        );
    }

    #[test]
    fn weather_status_uses_the_newest_permitted_wind_reading() {
        let mut updater = state_test_updater();
        let recovery: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:04Z",
            "Event": "WEATHER-HIGH-WIND",
            "IsHighWind": false,
            "WindSpeedMetersPerSecond": 4.0,
            "ThresholdMetersPerSecond": 8.5
        }))
        .unwrap();
        updater.apply_event_state(&recovery);
        assert!(updater.format_startup_status().contains("wind 4.0 m/s"));

        let later_weather: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:05Z",
            "Event": "WEATHER-CHANGED",
            "ChangedFields": "wind speed",
            "WindSpeedMetersPerSecond": 3.0,
            "TemperatureCelsius": 10.0
        }))
        .unwrap();
        updater.apply_event_state(&later_weather);
        let status = updater.format_startup_status();
        assert!(status.contains("wind 3.0 m/s"));
        assert!(!status.contains("wind 4.0 m/s"));

        let newest_without_wind: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:06Z",
            "Event": "WEATHER-CHANGED",
            "ChangedFields": "temperature",
            "TemperatureCelsius": 11.0
        }))
        .unwrap();
        updater.apply_event_state(&newest_without_wind);
        let status = updater.format_startup_status();
        assert!(status.contains("wind 4.0 m/s"));
        assert!(status.contains("11.0 °C"));
    }

    #[test]
    fn observatory_lifecycle_status_does_not_require_connection_events() {
        let mut updater = state_test_updater();
        updater.state.dome_shutter_open = Some(true);
        updater.state.dome_parked = Some(true);
        updater.state.flat_cover_state = Some("Closed".to_string());
        updater.state.flat_light_on = Some(true);
        updater.state.flat_brightness = Some(42);

        let status = updater.format_startup_status();
        assert!(status.contains("Dome · shutter open, parked"));
        assert!(status.contains("Flat panel · cover Closed, light on, brightness 42"));
        assert!(!status.contains("connected"));
        assert!(!status.contains("disconnected"));
    }

    #[test]
    fn dome_terminal_positions_clear_stale_azimuth_and_sync_preserves_flags() {
        let mut updater = state_test_updater();
        updater.state.dome_shutter_open = Some(true);
        updater.state.dome_parked = Some(false);
        updater.state.dome_homed = Some(true);
        updater.state.dome_azimuth = Some(123.0);

        updater.apply_event_state(&Event {
            time: "2026-08-26T00:00:00Z".to_string(),
            event: event_types::DOME_SYNCED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert_eq!(updater.state.dome_azimuth, None);
        assert_eq!(updater.state.dome_shutter_open, Some(true));
        assert_eq!(updater.state.dome_homed, Some(true));
        assert_eq!(updater.state.dome_parked, Some(false));

        updater.state.dome_azimuth = Some(90.0);
        updater.apply_event_state(&Event {
            time: "2026-08-26T00:01:00Z".to_string(),
            event: event_types::DOME_PARKED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert_eq!(updater.state.dome_azimuth, None);
        assert_eq!(updater.state.dome_parked, Some(true));
        assert_eq!(updater.state.dome_homed, Some(false));

        updater.state.dome_azimuth = Some(45.0);
        updater.apply_event_state(&Event {
            time: "2026-08-26T00:02:00Z".to_string(),
            event: event_types::DOME_HOMED.to_string(),
            chat_enabled: true,
            details: None,
        });
        assert_eq!(updater.state.dome_azimuth, None);
        assert_eq!(updater.state.dome_homed, Some(true));
        assert_eq!(updater.state.dome_parked, Some(false));
    }

    #[test]
    fn unknown_flat_light_payload_clears_stale_state() {
        let mut updater = state_test_updater();
        updater.state.flat_light_on = Some(true);
        for payload in [
            serde_json::json!({
                "Time": "2026-08-26T00:00:00Z",
                "Event": "FLAT-LIGHT-TOGGLED",
                "On": null
            }),
            serde_json::json!({
                "Time": "2026-08-26T00:01:00Z",
                "Event": "FLAT-LIGHT-TOGGLED"
            }),
        ] {
            updater.state.flat_light_on = Some(true);
            let event: Event = serde_json::from_value(payload).unwrap();
            updater.apply_event_state(&event);
            assert_eq!(updater.state.flat_light_on, None);
        }
    }

    #[test]
    fn observatory_and_connection_tombstones_revoke_only_their_scope() {
        let mut updater = state_test_updater();
        updater.state.dome_connected = Some(true);
        updater.state.dome_shutter_open = Some(true);
        updater.state.dome_azimuth = Some(30.0);

        updater.revoke_state_for_disabled_event(event_types::DOME_SLEWED, None);
        assert_eq!(updater.state.dome_connected, Some(true));
        assert_eq!(updater.state.dome_shutter_open, None);
        assert_eq!(updater.state.dome_azimuth, None);

        updater.state.dome_shutter_open = Some(false);
        updater.revoke_state_for_disabled_event(event_types::DOME_DISCONNECTED, None);
        assert_eq!(updater.state.dome_connected, None);
        assert_eq!(updater.state.dome_shutter_open, Some(false));
    }

    #[tokio::test]
    async fn motion_start_tombstones_are_transient_and_legacy_end_tombstones_keep_old_scopes() {
        let mut updater = state_test_updater();
        updater.state.last_mount_event = Some(event_types::MOUNT_PARKED.to_string());
        updater.state.last_filter = Some(FilterInfo {
            name: "L".to_string(),
            id: 2,
        });

        let mount_start_tombstone = Event {
            time: "2026-08-31T02:00:00Z".to_string(),
            event: event_types::MOUNT_SLEW_STARTED.to_string(),
            chat_enabled: false,
            details: None,
        };
        updater.process_baseline_events(std::slice::from_ref(&mount_start_tombstone));
        assert_eq!(
            updater.state.last_mount_event.as_deref(),
            Some(event_types::MOUNT_PARKED)
        );
        assert_eq!(
            updater
                .state
                .last_filter
                .as_ref()
                .map(|filter| filter.name.as_str()),
            Some("L")
        );

        let rotator_start_tombstone = Event {
            time: "2026-08-31T02:00:01Z".to_string(),
            event: event_types::ROTATOR_MOVE_STARTED.to_string(),
            chat_enabled: false,
            details: None,
        };
        updater
            .process_live_events(vec![rotator_start_tombstone])
            .await;
        assert_eq!(
            updater.state.last_mount_event.as_deref(),
            Some(event_types::MOUNT_PARKED)
        );
        assert_eq!(
            updater
                .state
                .last_filter
                .as_ref()
                .map(|filter| filter.name.as_str()),
            Some("L")
        );

        updater.state.last_mount_event = Some(event_types::MOUNT_SLEWED.to_string());
        updater
            .process_live_events(vec![Event {
                time: "2026-08-31T02:00:02Z".to_string(),
                event: event_types::MOUNT_SLEWED.to_string(),
                chat_enabled: false,
                details: None,
            }])
            .await;
        assert!(updater.state.last_mount_event.is_none());

        updater
            .process_live_events(vec![Event {
                time: "2026-08-31T02:00:03Z".to_string(),
                event: event_types::ROTATOR_MOVED.to_string(),
                chat_enabled: false,
                details: None,
            }])
            .await;
        assert!(updater.state.last_filter.is_none());
    }

    #[test]
    fn dither_and_flip_events_do_not_replace_stable_status_state() {
        let mut updater = state_test_updater();
        updater.state.last_mount_event = Some(event_types::MOUNT_UNPARKED.to_string());
        updater.state.last_guider_event = Some(event_types::GUIDER_START.to_string());
        for event_type in [
            event_types::MOUNT_BEFORE_FLIP,
            event_types::MOUNT_AFTER_FLIP,
            event_types::GUIDER_DITHER,
        ] {
            updater.apply_event_state(&Event {
                time: "2026-08-26T00:00:00Z".to_string(),
                event: event_type.to_string(),
                chat_enabled: true,
                details: None,
            });
        }
        assert_eq!(
            updater.state.last_mount_event.as_deref(),
            Some(event_types::MOUNT_UNPARKED)
        );
        assert_eq!(
            updater.state.last_guider_event.as_deref(),
            Some(event_types::GUIDER_START)
        );
    }

    #[test]
    fn baseline_uses_parsed_target_times_and_retains_scheduled_end() {
        let mut updater = state_test_updater();
        let events = [
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-26T00:30:00+01:00",
                "Event": "TS-TARGETSTART",
                "TargetName": "Earlier instant"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "Time": "2026-08-25T23:45:00Z",
                "Event": "TS-TARGETSTART",
                "TargetName": "Later instant",
                "TargetEndTime": "2026-08-26T02:00:00Z"
            }))
            .unwrap(),
        ];
        updater.process_baseline_events(&events);
        let target = updater.state.current_target.as_ref().unwrap();
        assert_eq!(target.name, "Later instant");
        assert_eq!(target.target_end_time.unwrap().timestamp(), 1_787_709_600);
    }

    #[test]
    fn target_end_uses_event_offset_and_expiry_releases_sequence_target() {
        let mut updater = state_test_updater();
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2099-08-25T23:00:00-07:00",
            "Event": "TS-TARGETSTART",
            "TargetName": "Scheduler target",
            "TargetEndTime": "2099-08-26T04:00:00"
        }))
        .unwrap();
        updater.process_baseline_events(&[event]);
        let target_end = updater
            .state
            .current_target
            .as_ref()
            .and_then(|target| target.target_end_time)
            .expect("scheduled target end");
        assert_eq!(target_end.offset().local_minus_utc(), -7 * 60 * 60);
        assert_eq!(target_end.to_rfc3339(), "2099-08-26T04:00:00-07:00");

        let observatory_offset = FixedOffset::west_opt(7 * 60 * 60).unwrap();
        updater
            .state
            .current_target
            .as_mut()
            .unwrap()
            .target_end_time =
            Some((Utc::now() - chrono::Duration::minutes(1)).with_timezone(&observatory_offset));
        let reconciliation = updater
            .reconcile_sequence_target(Some(("Sequence target".to_string(), true)))
            .expect("expired scheduler target should release sequence reconciliation");
        assert_eq!(reconciliation.1.name, "Sequence target");
        assert_eq!(
            updater
                .state
                .current_target
                .as_ref()
                .map(|target| &target.source),
            Some(&TargetSource::Sequence)
        );
    }

    #[test]
    fn typed_sequence_outcome_does_not_claim_legacy_success() {
        let mut updater = state_test_updater();
        let failed: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T00:00:00Z",
            "Event": "SEQUENCE-FINISHED",
            "Outcome": "completed_with_failures",
            "Status": "FINISHED",
            "HadFailures": true
        }))
        .unwrap();
        updater.apply_event_state(&failed);
        assert!(updater.state.sequence_failure.is_some());
        assert!(updater.format_startup_status().contains("failed"));

        updater.apply_event_state(&Event {
            time: "2026-08-26T00:01:00Z".to_string(),
            event: event_types::SEQUENCE_STARTING.to_string(),
            chat_enabled: true,
            details: None,
        });
        let legacy_finished = Event {
            time: "2026-08-26T00:02:00Z".to_string(),
            event: event_types::SEQUENCE_FINISHED.to_string(),
            chat_enabled: true,
            details: None,
        };
        updater.apply_event_state(&legacy_finished);
        assert!(!updater.format_startup_status().contains("completed"));
    }

    #[test]
    fn legacy_mount_operation_can_be_promoted_to_center() {
        let mut promoted = operation(SequenceOperationKind::MountSlew {
            coordinates: None,
            may_be_center: true,
        });
        assert!(promote_ambiguous_slew_to_center(&mut promoted));
        assert!(matches!(
            promoted.kind,
            SequenceOperationKind::MountCenter {
                coordinates: None,
                rotation: None,
                output: None,
            }
        ));

        let mut direct_slew = operation(SequenceOperationKind::MountSlew {
            coordinates: None,
            may_be_center: false,
        });
        assert!(!promote_ambiguous_slew_to_center(&mut direct_slew));
        assert!(matches!(
            direct_slew.kind,
            SequenceOperationKind::MountSlew { .. }
        ));
    }

    #[test]
    fn backoff_doubles_up_to_max() {
        let initial = Duration::from_secs(60);
        let max = Duration::from_secs(600);
        // 60 -> 120 -> 240 -> 480 -> 600 (capped) -> 600 (stays)
        assert_eq!(
            backoff_delay(initial, initial, max),
            Duration::from_secs(120)
        );
        assert_eq!(
            backoff_delay(Duration::from_secs(120), initial, max),
            Duration::from_secs(240)
        );
        assert_eq!(
            backoff_delay(Duration::from_secs(240), initial, max),
            Duration::from_secs(480)
        );
        assert_eq!(
            backoff_delay(Duration::from_secs(480), initial, max),
            Duration::from_secs(600)
        );
        assert_eq!(backoff_delay(max, initial, max), Duration::from_secs(600));
    }

    #[test]
    fn backoff_honors_max_above_default() {
        // A large configured max is not clamped — it keeps doubling past 600s.
        let initial = Duration::from_secs(60);
        let max = Duration::from_secs(3600);
        assert_eq!(
            backoff_delay(Duration::from_secs(600), initial, max),
            Duration::from_secs(1200)
        );
        assert_eq!(
            backoff_delay(Duration::from_secs(2400), initial, max),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn backoff_never_shrinks_below_initial_when_max_misconfigured() {
        // max < initial must not shrink the wait below the first interval.
        let initial = Duration::from_secs(60);
        let max = Duration::from_secs(10);
        assert_eq!(backoff_delay(initial, initial, max), initial);
        assert_eq!(
            backoff_delay(Duration::from_secs(120), initial, max),
            initial
        );
    }
}
