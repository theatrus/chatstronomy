//! Drop diagnostic floods before chat delivery; never defer them into a queue.

use crate::events::{Event, EventDetails, event_types};
use std::collections::VecDeque;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;
use tokio::time::Instant;

const REPEAT_INTERVAL: Duration = Duration::from_secs(60);
const MAX_FINGERPRINTS: usize = 128;

#[derive(Clone, Copy)]
enum DiagnosticClass {
    Error,
    Log,
}

fn diagnostic_class(event: &Event) -> Option<DiagnosticClass> {
    let name = event.event.as_str();
    // Accepted command outcomes belong to a user-initiated exchange.
    if !event.chat_enabled || name == event_types::CHATSTRONOMY_COMMAND_FAILED {
        return None;
    }
    if name.starts_with("ERROR-") || name.ends_with("-FAILED") || name.ends_with("-TIMEOUT") {
        return Some(DiagnosticClass::Error);
    }
    if matches!(name, event_types::NINA_LOG | event_types::NINA_NOTIFICATION) {
        let level = match &event.details {
            Some(EventDetails::NinaLog { level, .. })
            | Some(EventDetails::NinaNotification { level, .. }) => level.trim(),
            _ => "",
        };
        return Some(
            if ["WARN", "WARNING", "ERROR", "FATAL", "CRITICAL"]
                .iter()
                .any(|error_level| level.eq_ignore_ascii_case(error_level))
            {
                DiagnosticClass::Error
            } else {
                DiagnosticClass::Log
            },
        );
    }
    None
}

struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_interval: Duration,
    updated_at: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_seconds: u64, now: Instant) -> Self {
        Self {
            capacity: capacity.into(),
            tokens: capacity.into(),
            refill_interval: Duration::from_secs(refill_seconds),
            updated_at: now,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated_at).as_secs_f64()
                / self.refill_interval.as_secs_f64())
        .min(self.capacity);
        self.updated_at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// Owned by one telescope's updater, including across transport reconnects.
/// Only hashes of accepted diagnostics are retained, with a hard size bound.
pub(crate) struct EventRateLimiter {
    errors: TokenBucket,
    logs: TokenBucket,
    fingerprints: VecDeque<(u64, Instant)>,
    hasher: RandomState,
}

impl EventRateLimiter {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            errors: TokenBucket::new(5, 12, now),
            logs: TokenBucket::new(10, 6, now),
            fingerprints: VecDeque::new(),
            hasher: RandomState::new(),
        }
    }

    /// `now` is the receipt time of the whole history response. Slow chat
    /// sends must not refill the budget while traversing that same backlog.
    pub(crate) fn allow(&mut self, event: &Event, now: Instant) -> bool {
        let Some(class) = diagnostic_class(event) else {
            return true;
        };
        while self.fingerprints.front().is_some_and(|(_, accepted)| {
            now.saturating_duration_since(*accepted) >= REPEAT_INTERVAL
        }) {
            self.fingerprints.pop_front();
        }
        // Event.Time is deliberately absent: retry loops timestamp every copy.
        let fingerprint = self
            .hasher
            .hash_one((&event.event, format!("{:?}", event.details)));
        if self.fingerprints.iter().any(|(key, _)| *key == fingerprint) {
            return false;
        }
        let bucket = match class {
            DiagnosticClass::Error => &mut self.errors,
            DiagnosticClass::Log => &mut self.logs,
        };
        if !bucket.allow(now) {
            return false;
        }
        if self.fingerprints.len() == MAX_FINGERPRINTS {
            self.fingerprints.pop_front();
        }
        self.fingerprints.push_back((fingerprint, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failure(index: usize) -> Event {
        serde_json::from_value(json!({
            "Event": "SEQUENCE-ENTITY-FAILED",
            "Time": format!("2026-09-09T04:41:{index}Z"),
            "Entity": if index % 2 == 0 { "TakeExposure" } else { "SetReadoutMode" },
            "EntityType": "Instruction",
            "Error": "Camera not connected"
        }))
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn alternating_failures_ignore_timestamps_and_recover_after_a_minute() {
        let mut limiter = EventRateLimiter::new();
        let now = Instant::now();
        assert_eq!(
            (0..10_000)
                .filter(|i| limiter.allow(&failure(*i), now))
                .count(),
            2
        );
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!limiter.allow(&failure(10_000), Instant::now()));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(limiter.allow(&failure(10_000), Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn varying_errors_are_bounded_and_batches_do_not_refill_during_slow_delivery() {
        let mut limiter = EventRateLimiter::new();
        let batch_time = Instant::now();
        for i in 0..10_000 {
            let mut event = failure(i);
            event.details = Some(EventDetails::SequenceEntityFailed {
                entity: "TakeExposure".into(),
                entity_type: "Instruction".into(),
                error: format!("Camera error {i}"),
            });
            assert_eq!(limiter.allow(&event, batch_time), i < 5);
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(limiter.fingerprints.len() <= MAX_FINGERPRINTS);
        assert!(limiter.allow(&failure(0), Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn log_floods_do_not_starve_errors_other_telescopes_or_critical_events() {
        let mut limiter = EventRateLimiter::new();
        let now = Instant::now();
        for i in 0..1_000 {
            let log = serde_json::from_value(json!({
                "Event": "NINA-LOG", "Time": "now", "Level": "INFO",
                "Source": "Camera", "Member": "Poll", "Line": 1, "Message": format!("log {i}")
            }))
            .unwrap();
            assert_eq!(limiter.allow(&log, now), i < 10);
        }
        assert!(limiter.allow(&failure(0), now));
        assert!(EventRateLimiter::new().allow(&failure(0), now));
        for name in [
            "ERROR-AF",
            "ERROR-PLATESOLVE",
            "CAMERA-DOWNLOAD-TIMEOUT",
            "IMAGE-SAVE-FAILED",
        ] {
            let mut event = failure(0);
            event.event = name.into();
            assert!(limiter.allow(&event, now));
            assert!(!limiter.allow(&event, now));
        }
        for name in [
            "SAFETY-CHANGED",
            "CAMERA-CONNECTED",
            "SEQUENCE-FINISHED",
            "AUTOFOCUS-FINISHED",
            "CHATSTRONOMY-COMMAND-FAILED",
        ] {
            let mut event = failure(0);
            event.event = name.into();
            assert!(limiter.allow(&event, now));
            assert!(limiter.allow(&event, now));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn warnings_logs_and_popups_share_error_budget_but_disabled_records_do_not() {
        let mut limiter = EventRateLimiter::new();
        let now = Instant::now();
        for i in 0..10 {
            let mut event = failure(i);
            event.chat_enabled = false;
            assert!(limiter.allow(&event, now));
        }
        assert!(limiter.fingerprints.is_empty());
        for (i, level) in ["WARN", "warning", "ERROR", "Fatal", " critical ", "ERROR"]
            .iter()
            .enumerate()
        {
            let event = serde_json::from_value(json!({
                "Event": "NINA-NOTIFICATION", "Time": "now", "Level": level,
                "Header": "Camera", "Message": format!("error {i}")
            }))
            .unwrap();
            assert_eq!(limiter.allow(&event, now), i < 5);
        }
        assert!(!limiter.allow(&failure(0), now));
        tokio::time::advance(Duration::from_secs(12)).await;
        assert!(limiter.allow(&failure(0), Instant::now()));
        assert!(!limiter.allow(&failure(1), Instant::now()));
    }
}
