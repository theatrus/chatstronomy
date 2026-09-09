//! Bounded, content-free accounting for diagnostics discarded by either guard.

use crate::events::{
    ElidedEventCount, Event, EventDeliveryScope, EventDetails, event_delivery_scope,
    normalize_elision_log_level,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

const MAX_KEYS: usize = 128;
const NOTICE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    event: String,
    level: String,
}

impl Key {
    fn new(event: &str, level: Option<&str>) -> Option<Self> {
        if event.len() > 64
            || event == "CHATSTRONOMY-COMMAND-FAILED"
            || !(event.starts_with("ERROR-")
                || event.ends_with("-FAILED")
                || event.ends_with("-TIMEOUT")
                || matches!(event, "NINA-LOG" | "NINA-NOTIFICATION"))
        {
            return None;
        }
        Some(Self {
            event: event.into(),
            level: if event == "NINA-LOG" {
                normalize_elision_log_level(level)
            } else {
                String::new()
            },
        })
    }
}

#[derive(Default)]
struct Pending {
    hub: u64,
    plugin: u64,
}

struct Watermark {
    epoch: String,
    count: u64,
}

#[derive(Default)]
pub(crate) struct EventElisionReporter {
    pending: HashMap<Key, Pending>,
    source: HashMap<Key, Watermark>,
    has_source_snapshot: bool,
    last_notice: Option<Instant>,
}

impl EventElisionReporter {
    pub(crate) fn dropped_at_hub(&mut self, event: &Event) {
        let level = match &event.details {
            Some(EventDetails::NinaLog { level, .. }) => Some(level.as_str()),
            _ => None,
        };
        if !event.chat_enabled {
            return;
        }
        let Some(key) = Key::new(&event.event, level) else {
            return;
        };
        if self.has_source_snapshot && !self.source.contains_key(&key) {
            // History and metadata may straddle a permission change. An old
            // history record cannot resurrect a newly revoked count.
            return;
        }
        if self.pending.len() < MAX_KEYS || self.pending.contains_key(&key) {
            let pending = self.pending.entry(key).or_default();
            pending.hub = pending.hub.saturating_add(1);
        }
    }

    /// Plugin snapshots are cumulative. Missing metadata means an older
    /// plugin; an explicit empty list revokes all previously reported counts.
    pub(crate) fn observe_source(&mut self, snapshot: Option<&[ElidedEventCount]>, baseline: bool) {
        let Some(snapshot) = snapshot else { return };
        self.has_source_snapshot = true;
        // Supporting plugins enumerate enabled slots even when Count is zero.
        // Their omission revokes Hub-only counts too, not just source counts.
        let present: HashSet<Key> = snapshot
            .iter()
            .take(MAX_KEYS)
            .filter_map(|item| Key::new(&item.event, item.level.as_deref()))
            .collect();
        self.pending.retain(|key, _| present.contains(key));
        self.source.retain(|key, _| present.contains(key));
        for item in snapshot.iter().take(MAX_KEYS) {
            let Some(key) = Key::new(&item.event, item.level.as_deref()) else {
                continue;
            };
            if self.source.len() >= MAX_KEYS && !self.source.contains_key(&key) {
                continue;
            }
            let previous = self.source.get(&key);
            let changed_epoch = previous.is_some_and(|previous| previous.epoch != item.epoch);
            let delta = if baseline {
                0
            } else if let Some(previous) = previous.filter(|previous| previous.epoch == item.epoch)
            {
                item.count.saturating_sub(previous.count)
            } else {
                item.count
            };
            if changed_epoch {
                // Permission changes start a fresh zero-based counter. Old
                // pending details, including their numeric counts, are revoked.
                self.pending.remove(&key);
            }
            let count = previous
                .filter(|previous| previous.epoch == item.epoch)
                .map_or(item.count, |previous| previous.count.max(item.count));
            self.source.insert(
                key.clone(),
                Watermark {
                    epoch: item.epoch.clone(),
                    count,
                },
            );
            if delta > 0 && (self.pending.len() < MAX_KEYS || self.pending.contains_key(&key)) {
                let pending = self.pending.entry(key).or_default();
                pending.plugin = pending.plugin.saturating_add(delta);
            }
        }
    }

    pub(crate) fn revoke_scope(&mut self, scope: EventDeliveryScope) {
        self.pending
            .retain(|key, _| event_delivery_scope(&key.event) != scope);
        // Keep watermarks until an authoritative source reset/removal so a
        // repeated legacy tombstone cannot make old totals look new again.
    }

    /// Called even on an empty history poll, so the end of a flood does not
    /// strand its final count. Notices themselves never become source events.
    pub(crate) fn take_notice(&mut self, now: Instant) -> Option<u64> {
        if self
            .last_notice
            .is_some_and(|last| now.saturating_duration_since(last) < NOTICE_INTERVAL)
        {
            return None;
        }
        let count = self.pending.values().fold(0u64, |total, pending| {
            total
                .saturating_add(pending.hub)
                .saturating_add(pending.plugin)
        });
        if count == 0 {
            return None;
        }
        self.pending.clear();
        self.last_notice = Some(now);
        Some(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter(count: u64, epoch: &str) -> ElidedEventCount {
        ElidedEventCount {
            event: "SEQUENCE-ENTITY-FAILED".into(),
            level: None,
            count,
            epoch: epoch.into(),
        }
    }

    fn failure() -> Event {
        Event {
            time: "2026-09-09T00:00:00Z".into(),
            event: "SEQUENCE-ENTITY-FAILED".into(),
            chat_enabled: true,
            details: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn combines_plugin_deltas_and_hub_drops_without_replaying_snapshots() {
        let mut reporter = EventElisionReporter::default();
        reporter.observe_source(Some(&[counter(10, "first")]), true);
        assert_eq!(reporter.take_notice(Instant::now()), None);
        reporter.observe_source(Some(&[counter(30, "first")]), false);
        reporter.dropped_at_hub(&failure());
        assert_eq!(reporter.take_notice(Instant::now()), Some(21));
        reporter.observe_source(Some(&[counter(30, "first")]), false);
        reporter.observe_source(Some(&[counter(20, "first")]), false);
        reporter.observe_source(Some(&[counter(30, "first")]), false);
        tokio::time::advance(NOTICE_INTERVAL).await;
        assert_eq!(reporter.take_notice(Instant::now()), None);
        reporter.observe_source(Some(&[counter(35, "first")]), false);
        assert_eq!(reporter.take_notice(Instant::now()), Some(5));
    }

    #[tokio::test(start_paused = true)]
    async fn bounds_notice_rate_and_flushes_after_the_flood_stops() {
        let mut reporter = EventElisionReporter::default();
        reporter.dropped_at_hub(&failure());
        assert_eq!(reporter.take_notice(Instant::now()), Some(1));
        for _ in 0..10_000 {
            reporter.dropped_at_hub(&failure());
        }
        assert_eq!(reporter.take_notice(Instant::now()), None);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(reporter.take_notice(Instant::now()), None);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(reporter.take_notice(Instant::now()), Some(10_000));
        assert_eq!(reporter.take_notice(Instant::now()), None);
    }

    #[test]
    fn revocations_and_new_epochs_clear_pending_counts_without_losing_new_drops() {
        let mut reporter = EventElisionReporter::default();
        reporter.observe_source(Some(&[counter(100, "old")]), false);
        reporter.observe_source(None, false);
        reporter.observe_source(Some(&[counter(4, "new")]), false);
        assert_eq!(reporter.take_notice(Instant::now()), Some(4));

        let mut reporter = EventElisionReporter::default();
        reporter.observe_source(Some(&[counter(100, "old")]), false);
        reporter.dropped_at_hub(&failure());
        reporter.observe_source(Some(&[]), false);
        reporter.dropped_at_hub(&failure());
        assert_eq!(reporter.take_notice(Instant::now()), None);

        reporter.dropped_at_hub(&failure());
        reporter.revoke_scope(EventDeliveryScope::Sequence);
        assert_eq!(reporter.take_notice(Instant::now()), None);
    }

    #[test]
    fn bounded_keys_and_saturating_counts_accept_no_raw_diagnostic_text() {
        let mut reporter = EventElisionReporter::default();
        for i in 0..1_000 {
            let mut event = failure();
            event.event = format!("ERROR-{i}");
            reporter.dropped_at_hub(&event);
        }
        assert_eq!(reporter.pending.len(), MAX_KEYS);
        let mut reporter = EventElisionReporter::default();
        reporter.observe_source(Some(&[counter(u64::MAX, "first")]), false);
        reporter.dropped_at_hub(&failure());
        assert_eq!(reporter.take_notice(Instant::now()), Some(u64::MAX));
    }

    #[test]
    fn zero_slots_preserve_hub_only_counts_until_permissions_revoke_them() {
        let mut reporter = EventElisionReporter::default();
        reporter.observe_source(Some(&[counter(0, "first")]), true);
        reporter.dropped_at_hub(&failure());
        reporter.observe_source(Some(&[counter(0, "first")]), false);
        assert_eq!(reporter.take_notice(Instant::now()), Some(1));

        let mut reporter = EventElisionReporter::default();
        reporter.dropped_at_hub(&failure());
        reporter.observe_source(Some(&[]), false);
        assert_eq!(reporter.take_notice(Instant::now()), None);

        reporter.observe_source(Some(&[counter(0, "first")]), false);
        reporter.dropped_at_hub(&failure());
        reporter.observe_source(Some(&[counter(0, "new-permissions")]), false);
        assert_eq!(reporter.take_notice(Instant::now()), None);
    }

    #[test]
    fn canonical_log_levels_share_source_permission_slots() {
        for (alias, canonical) in [
            ("INFO", "INFORMATION"),
            ("WARN", "WARNING"),
            ("FATAL", "ERROR"),
            ("CRITICAL", "ERROR"),
            ("VERBOSE", "TRACE"),
        ] {
            let mut reporter = EventElisionReporter::default();
            let source = ElidedEventCount {
                event: "NINA-LOG".into(),
                level: Some(canonical.into()),
                count: 0,
                epoch: "old".into(),
            };
            reporter.observe_source(Some(std::slice::from_ref(&source)), true);
            let log = Event {
                time: "2026-09-09T00:00:00Z".into(),
                event: "NINA-LOG".into(),
                chat_enabled: true,
                details: Some(EventDetails::NinaLog {
                    level: alias.into(),
                    source: "Camera".into(),
                    member: "Read".into(),
                    line: 1,
                    message: "private message".into(),
                }),
            };
            reporter.dropped_at_hub(&log);
            reporter.observe_source(Some(std::slice::from_ref(&source)), false);
            assert_eq!(reporter.pending.len(), 1);
            reporter.observe_source(
                Some(&[ElidedEventCount {
                    epoch: "new".into(),
                    ..source
                }]),
                false,
            );
            assert_eq!(reporter.take_notice(Instant::now()), None);
        }
    }
}
