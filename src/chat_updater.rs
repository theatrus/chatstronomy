use crate::autofocus::AutofocusResponse;
use crate::camera::CameraInfo;
use crate::chat::{ChatAttachment, ChatField, ChatMessage, ChatServiceManager, ChatTarget};
use crate::discord::colors;
use crate::events::{Event, EventDetails, FilterInfo, TargetCoordinates, event_types};
use crate::images::ImageMetadata;
use crate::sequence::{
    SequenceOperation, SequenceOperationKind, SequenceResponse, extract_current_target,
    extract_current_target_with_delivery, extract_meridian_flip_time, extract_sequence_operations,
    meridian_flip_time_formatted_with_clock,
};
use crate::source::SharedRigSource;
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Information about the current observation target
#[derive(Debug, Clone)]
struct TargetInfo {
    name: String,
    source: TargetSource,
    coordinates: Option<TargetCoordinates>,
    project: Option<String>,
    rotation: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
enum TargetSource {
    Sequence,
    TsTargetStart,
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
            SequenceOperationKind::MountSlew { .. } | SequenceOperationKind::MountCenter { .. } => {
                None
            }
        }
    }

    fn next_milestone(&self, now: DateTime<Utc>) -> Option<u8> {
        let progress = self.progress_percent(now)?;
        [75, 50, 25]
            .into_iter()
            .find(|milestone| progress >= *milestone && self.last_milestone < *milestone)
    }
}

fn plate_solve_output_key(operation: &SequenceOperation) -> Option<String> {
    let SequenceOperationKind::MountCenter {
        output: Some(output),
        ..
    } = &operation.kind
    else {
        return None;
    };
    output.solve_time.clone().or_else(|| {
        Some(format!(
            "{:?}:{:?}:{:?}:{:?}",
            output.success,
            output.position_angle,
            output.separation_arcseconds,
            output.thumbnail.as_ref().map(Vec::len)
        ))
    })
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
    Failed { attach_output: bool },
}

/// Insert-only dedup set with a bounded memory footprint.
///
/// The keys embed payload text — for `NINA-LOG` events, a whole log line — and
/// a local updater lives for the entire N.I.N.A. session, so an unbounded set
/// grows all night. Evicting the oldest key can at worst re-announce something
/// older than the whole retained window, which the source histories have
/// dropped long before.
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

/// State management for the chat updater
struct UpdaterState {
    events_seen: BoundedSeenSet,
    images_seen: BoundedSeenSet,
    current_target: Option<TargetInfo>,
    meridian_flip_time: Option<f64>,
    sequence: Option<SequenceResponse>,
    last_image_time: Option<Instant>,
    skipped_images_count: u32,
    last_filter: Option<FilterInfo>,
    /// Latest mount-state event we've observed (PARKED, UNPARKED, HOMED, etc.).
    last_mount_event: Option<String>,
    /// Latest guider-state event we've observed (START, STOP, DITHER).
    last_guider_event: Option<String>,
    /// True if the last sequence event was STARTING (not FINISHED).
    sequence_running: bool,
    /// Active TS-WAITSTART wait-end time, if NINA is currently waiting.
    wait_until: Option<DateTime<FixedOffset>>,
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
            events_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            images_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            current_target: None,
            meridian_flip_time: None,
            sequence: None,
            last_image_time: None,
            skipped_images_count: 0,
            last_filter: None,
            last_mount_event: None,
            last_guider_event: None,
            sequence_running: false,
            wait_until: None,
            center_event_seen_at: None,
            sequence_operations: HashMap::new(),
            plate_solve_outputs_seen: BoundedSeenSet::new(SEEN_SET_CAPACITY),
            last_status_fingerprint: None,
            connected: false,
            consecutive_failures: 0,
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
        let wait_minutes = self
            .wait_until
            .map(|end| {
                end.with_timezone(&Utc)
                    .signed_duration_since(Utc::now())
                    .num_minutes()
            })
            .unwrap_or(-1);
        let mut operations = self
            .sequence_operations
            .iter()
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
        format!(
            "t={target}|f={filter}|m={mount}|g={guider}|w={wait_minutes}|sr={}|flip={flip_minutes}|ops={}",
            self.sequence_running,
            operations.join(",")
        )
    }

    fn event_key(event: &Event) -> String {
        format!("{}|{}|{:?}", event.time, event.event, event.details)
    }

    fn image_key(image: &ImageMetadata) -> String {
        format!("{}|{}", image.date, image.camera_name)
    }

    fn has_seen_event(&mut self, event: &Event) -> bool {
        self.events_seen.check_and_insert(Self::event_key(event))
    }

    fn has_seen_image(&mut self, image: &ImageMetadata) -> bool {
        self.images_seen.check_and_insert(Self::image_key(image))
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
                sleep(poll_interval).await;
            } else {
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
                && matches!(operation.kind, SequenceOperationKind::CameraCooling { .. })
        }) || self.state.sequence_operations.values().any(|tracked| {
            matches!(
                tracked.operation.kind,
                SequenceOperationKind::CameraCooling { .. }
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
        let event_wait_end = self.state.wait_until.map(|end| end.with_timezone(&Utc));
        let mut notifications = Vec::new();
        let mut sequence_wait_ended = false;

        let existing_keys = self
            .state
            .sequence_operations
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for key in existing_keys {
            let Some(next) = incoming.get(&key) else {
                if let Some(mut previous) = self.state.sequence_operations.remove(&key) {
                    if matches!(
                        previous.operation.kind,
                        SequenceOperationKind::CameraCooling { .. }
                    ) {
                        previous.camera = camera.clone();
                    }
                    sequence_wait_ended |= matches!(
                        previous.operation.kind,
                        SequenceOperationKind::TimeWait { .. }
                    );
                    notifications.push((
                        previous,
                        OperationUpdate::Finished {
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
                    ) {
                        previous.camera = camera.clone();
                    }
                    sequence_wait_ended |= matches!(
                        previous.operation.kind,
                        SequenceOperationKind::TimeWait { .. }
                    );
                    notifications.push((
                        previous,
                        OperationUpdate::Finished {
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
                    ) {
                        previous.camera = camera.clone();
                    }
                    sequence_wait_ended |= matches!(
                        previous.operation.kind,
                        SequenceOperationKind::TimeWait { .. }
                    );
                    let output_key = plate_solve_output_key(next);
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
                        } else {
                            OperationUpdate::Finished { attach_output }
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
                ) && event_wait_end.is_some()
                {
                    tracked.estimated_end = event_wait_end;
                }

                if let Some(milestone) = tracked.next_milestone(now) {
                    tracked.last_milestone = milestone;
                    notifications.push((tracked.clone(), OperationUpdate::Progress(milestone)));
                }
                let output_key = plate_solve_output_key(&tracked.operation);
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
            let operation_camera =
                matches!(operation.kind, SequenceOperationKind::CameraCooling { .. })
                    .then(|| camera.clone())
                    .flatten();
            let mut tracked = TrackedSequenceOperation::new(operation, now, operation_camera);
            if matches!(
                tracked.operation.kind,
                SequenceOperationKind::TimeWait { .. }
            ) && event_wait_end.is_some()
            {
                tracked.estimated_end = event_wait_end;
            }
            if !announce {
                tracked.last_milestone = tracked
                    .progress_percent(now)
                    .map(completed_milestone)
                    .unwrap_or(0);
            }
            let output_key = plate_solve_output_key(&tracked.operation);
            let output_is_new = claim_plate_solve_output(
                &mut self.state.plate_solve_outputs_seen,
                tracked.operation.chat_enabled,
                output_key.as_ref(),
            );
            let suppress_duplicate_wait = matches!(
                tracked.operation.kind,
                SequenceOperationKind::TimeWait { .. }
            ) && self.state.wait_until.is_some();
            if announce && !suppress_duplicate_wait && !suppress_duplicate_center {
                notifications.push((tracked.clone(), OperationUpdate::Started));
            }
            if announce && output_is_new {
                notifications.push((tracked.clone(), OperationUpdate::Output));
            }
            tracked.last_output_key = output_key;
            self.state.sequence_operations.insert(key, tracked);
        }

        if sequence_wait_ended
            && !self.state.sequence_operations.values().any(|tracked| {
                matches!(
                    tracked.operation.kind,
                    SequenceOperationKind::TimeWait { .. }
                )
            })
        {
            self.state.wait_until = None;
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
            (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Failed { .. }) => {
                ("Camera cooling", "❌ Camera cooling failed")
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
            (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Failed { .. }) => {
                ("Timed wait", "❌ Timed wait failed")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Started) => {
                ("Mount slew", "🔭 Mount slew started")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Finished { .. }) => {
                ("Mount slew", "✅ Mount slew finished")
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
            (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Failed { .. }) => {
                ("Center", "❌ Centering failed")
            }
            (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::MountSlew { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::MountCenter { .. }, OperationUpdate::Progress(_))
            | (SequenceOperationKind::CameraCooling { .. }, OperationUpdate::Output)
            | (SequenceOperationKind::TimeWait { .. }, OperationUpdate::Output) => {
                ("Sequence operation", "Sequence operation update")
            }
        };
        let color = match update {
            OperationUpdate::Finished { .. } => colors::GREEN,
            OperationUpdate::Failed { .. } => colors::RED,
            OperationUpdate::Output => match &tracked.operation.kind {
                SequenceOperationKind::MountCenter {
                    output: Some(output),
                    ..
                } if output.success == Some(false) => colors::RED,
                _ => colors::CYAN,
            },
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
            SequenceOperationKind::TimeWait { .. } => {
                if let Some(end) = tracked.estimated_end {
                    let remaining = end
                        .signed_duration_since(Utc::now())
                        .max(chrono::Duration::zero());
                    message = message
                        .field(
                            "Until",
                            &end.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                            false,
                        )
                        .field("Remaining", &format_duration(remaining), true);
                }
            }
            SequenceOperationKind::MountSlew { coordinates, .. } => {
                if let Some(coordinates) = coordinates {
                    message = message.field("Destination", &coordinates.display(), false);
                }
            }
            SequenceOperationKind::MountCenter {
                coordinates,
                rotation,
                output,
            } => {
                if let Some(coordinates) = coordinates {
                    message = message.field("Target", &coordinates.display(), false);
                }
                if let Some(rotation) = rotation {
                    message = message.field("Target rotation", &format!("{rotation:.1}°"), true);
                }
                if let Some(output) = output {
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
                        message =
                            message.field("Image scale", &format!("{scale:.2} arcsec/px"), true);
                    }
                    if let Some(radius) = output.radius_degrees {
                        message = message.field("Solve radius", &format!("{radius:.2}°"), true);
                    }
                    if let Some(separation) = output.separation_arcseconds {
                        message = message.field(
                            "Pointing error",
                            &format!("{separation:.1} arcsec"),
                            true,
                        );
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
                }
            }
        }
        let attach_output = matches!(update, OperationUpdate::Output)
            || matches!(
                update,
                OperationUpdate::Finished {
                    attach_output: true
                } | OperationUpdate::Failed {
                    attach_output: true
                }
            );
        let attachments = if attach_output {
            match &tracked.operation.kind {
                SequenceOperationKind::MountCenter {
                    output: Some(output),
                    ..
                } => output
                    .thumbnail
                    .as_ref()
                    .map(|thumbnail| {
                        vec![ChatAttachment {
                            data: thumbnail.clone(),
                            filename: if output.thumbnail_media_type.as_deref() == Some("image/png")
                            {
                                "plate_solve.png".to_string()
                            } else {
                                "plate_solve.jpg".to_string()
                            },
                        }]
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
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
            self.process_baseline_events(&events.response);
        }

        // Load sequence to get meridian flip time and potential sequence target
        if capabilities.sequence {
            match self.source.get_sequence().await {
                Ok(sequence) => {
                    self.state.meridian_flip_time = extract_meridian_flip_time(&sequence);
                    let operations = extract_sequence_operations(&sequence);
                    let camera = self.camera_snapshot_for(&operations).await;
                    self.reconcile_sequence_operations(operations, camera, false)
                        .await;

                    // Only use sequence target if no TS-TARGETSTART target was found
                    if self.state.current_target.is_none()
                        && let Some(target_name) = extract_current_target(&sequence)
                    {
                        self.state.current_target = Some(TargetInfo {
                            name: target_name,
                            source: TargetSource::Sequence,
                            coordinates: None,
                            project: None,
                            rotation: None,
                        });
                    }

                    self.state.sequence = Some(sequence);
                }
                Err(e) => {
                    println!("[{n}] Could not load sequence during initialization: {e}");
                }
            }
        }

        // Load images
        if capabilities.image_history {
            let images = self.source.get_all_image_history().await?;
            for image in &images.response {
                self.state
                    .images_seen
                    .insert(UpdaterState::image_key(image));
            }
        }

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

    fn process_baseline_events(&mut self, events: &[Event]) {
        let mut latest_ts_target: Option<(String, TargetInfo)> = None;

        for event in events {
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
            if event.event == event_types::TS_TARGETSTART
                && let Some(EventDetails::TargetStart {
                    target_name,
                    coordinates,
                    project_name,
                    rotation,
                    ..
                }) = &event.details
                && target_name != "Sequential Instruction Set"
            {
                let target_info = TargetInfo {
                    name: target_name.clone(),
                    source: TargetSource::TsTargetStart,
                    coordinates: coordinates.clone(),
                    project: project_name.clone(),
                    rotation: *rotation,
                };

                if latest_ts_target.is_none()
                    || latest_ts_target
                        .as_ref()
                        .map(|(time, _)| time < &event.time)
                        .unwrap_or(false)
                {
                    latest_ts_target = Some((event.time.clone(), target_info));
                }
            }

            // Track latest mount-state event (events are in chronological order).
            match event.event.as_str() {
                event_types::MOUNT_PARKED
                | event_types::MOUNT_UNPARKED
                | event_types::MOUNT_HOMED
                | event_types::MOUNT_BEFORE_FLIP
                | event_types::MOUNT_AFTER_FLIP
                | event_types::MOUNT_CENTER => {
                    self.state.last_mount_event = Some(event.event.clone());
                }
                event_types::GUIDER_START
                | event_types::GUIDER_STOP
                | event_types::GUIDER_DITHER => {
                    self.state.last_guider_event = Some(event.event.clone());
                }
                event_types::SEQUENCE_STARTING => self.state.sequence_running = true,
                event_types::SEQUENCE_FINISHED => self.state.sequence_running = false,
                event_types::TS_WAITSTART => {
                    if let Some(EventDetails::WaitStart { wait_end_time }) = &event.details
                        && let Ok(parsed) = DateTime::parse_from_rfc3339(wait_end_time)
                    {
                        self.state.wait_until = Some(parsed);
                    }
                }
                _ => {}
            }

            self.state
                .events_seen
                .insert(UpdaterState::event_key(event));
        }

        // If the recorded wait has already elapsed, clear it.
        if let Some(end) = self.state.wait_until
            && Utc::now() >= end
        {
            self.state.wait_until = None;
        }

        // Set the latest TS target if found
        if let Some((_, target)) = latest_ts_target {
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
                for event in events.response {
                    if !self.should_process_event(&event) {
                        continue;
                    }

                    if !self.state.has_seen_event(&event) {
                        self.print_new_event(&event);
                        self.handle_event(&event).await;
                    }
                }
                if self.state.wait_until.is_some_and(|end| Utc::now() >= end) {
                    self.state.wait_until = None;
                }
                true
            }
            Err(e) => {
                eprintln!("Error fetching events: {e}");
                false
            }
        }
    }

    fn should_process_event(&self, event: &Event) -> bool {
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
        match event.event.as_str() {
            event_types::MOUNT_PARKED
            | event_types::MOUNT_UNPARKED
            | event_types::MOUNT_HOMED
            | event_types::MOUNT_BEFORE_FLIP
            | event_types::MOUNT_AFTER_FLIP
            | event_types::MOUNT_CENTER => {
                self.state.last_mount_event = Some(event.event.clone());
                if event.event == event_types::MOUNT_CENTER {
                    self.state.center_event_seen_at = Some(Utc::now());
                }
            }
            event_types::GUIDER_START | event_types::GUIDER_STOP | event_types::GUIDER_DITHER => {
                self.state.last_guider_event = Some(event.event.clone());
            }
            event_types::SEQUENCE_STARTING => self.state.sequence_running = true,
            event_types::SEQUENCE_FINISHED => self.state.sequence_running = false,
            event_types::TS_WAITSTART => {
                if let Some(EventDetails::WaitStart { wait_end_time }) = &event.details
                    && let Some(parsed) = parse_nina_timestamp(wait_end_time)
                {
                    self.state.wait_until = Some(parsed);
                }
            }
            _ => {}
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

        if !event.chat_enabled {
            return;
        }

        match event.event.as_str() {
            event_types::AUTOFOCUS_FINISHED => self.handle_autofocus_finished(event).await,
            event_types::MOUNT_BEFORE_FLIP
            | event_types::MOUNT_AFTER_FLIP
            | event_types::MOUNT_PARKED
            | event_types::MOUNT_UNPARKED
            | event_types::MOUNT_HOMED
            | event_types::MOUNT_CENTER => self.handle_mount_event(event).await,
            event_types::GUIDER_START | event_types::GUIDER_DITHER => {
                self.handle_guider_event(event).await
            }
            event_types::SEQUENCE_STARTING | event_types::SEQUENCE_FINISHED => {
                self.handle_sequence_event(event).await
            }
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
        if let Some(EventDetails::TargetStart {
            target_name,
            coordinates,
            project_name,
            rotation,
            ..
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
            };

            let old_target = self.state.current_target.clone();
            let target_changed = old_target
                .as_ref()
                .map(|t| t.name != new_target.name)
                .unwrap_or(true);

            if target_changed {
                self.state.current_target = Some(new_target.clone());
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

    async fn handle_autofocus_finished(&self, event: &Event) {
        println!("[AUTOFOCUS FINISHED] {}", event.time);
        println!("Fetching autofocus results...");

        match self.source.get_last_autofocus().await {
            Ok(autofocus_data) => {
                self.display_autofocus_results(&autofocus_data);

                if self.chat_manager.service_count() > 0 {
                    self.send_autofocus_notification(&autofocus_data).await;
                }
            }
            Err(e) => eprintln!("Failed to fetch autofocus data: {e}"),
        }
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
                let camera = self.camera_snapshot_for(&operations).await;
                self.reconcile_sequence_operations(operations, camera, true)
                    .await;

                self.state.meridian_flip_time = new_meridian_flip_time;
                self.state.sequence = Some(sequence);

                // Only update target if we don't have a TS-TARGETSTART override
                if self
                    .state
                    .current_target
                    .as_ref()
                    .map(|t| t.source != TargetSource::TsTargetStart)
                    .unwrap_or(true)
                    && let Some((target_name, chat_enabled)) = new_sequence_target
                {
                    let new_target = TargetInfo {
                        name: target_name.clone(),
                        source: TargetSource::Sequence,
                        coordinates: None,
                        project: None,
                        rotation: None,
                    };

                    let old_target = self.state.current_target.clone();
                    let target_changed = old_target
                        .as_ref()
                        .map(|t| t.name != new_target.name)
                        .unwrap_or(true);

                    if target_changed {
                        self.state.current_target = Some(new_target.clone());
                        println!("[SEQUENCE TARGET] {}", target_name);

                        if chat_enabled && self.chat_manager.service_count() > 0 {
                            if let Some(old) = old_target {
                                self.send_target_change_notification(&old, &new_target)
                                    .await;
                            } else {
                                self.send_target_start_notification(&new_target).await;
                            }
                        }
                    }
                }
                true
            }
            Err(e) => {
                if self.state.sequence.is_none() {
                    eprintln!("Error fetching sequence (will retry silently): {e}");
                }
                false
            }
        }
    }

    /// Returns whether the Direct source responded (see [`Self::poll_events`]).
    pub async fn poll_images(&mut self) -> bool {
        if !self.source.capabilities().image_history {
            return false;
        }
        match self.source.get_all_image_history().await {
            Ok(images) => {
                for (index, image) in images.response.iter().enumerate() {
                    if !self.state.has_seen_image(image) {
                        self.print_new_image(image);

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

    fn display_autofocus_results(&self, af: &AutofocusResponse) {
        if !af.success {
            println!("❌ Autofocus failed: {}", af.error);
            return;
        }

        let af_data = &af.response;
        let success_indicator = if af.is_successful() { "✅" } else { "⚠️" };

        println!("{success_indicator} Autofocus Summary");
        println!("  Filter: {}", af_data.filter);
        println!("  Method: {}", af_data.method);
        println!("  Temperature: {:.1}°C", af_data.temperature);
        println!("  Duration: {}", af_data.duration);
        println!(
            "  Position Change: {}",
            af_data.calculated_focus_point.position - af_data.initial_focus_point.position
        );
        println!("  Best R-squared: {:.4}", af.get_best_r_squared());
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
        let mut parts: Vec<String> = Vec::new();

        let mut operations = self.state.sequence_operations.values().collect::<Vec<_>>();
        operations.sort_by(|left, right| left.operation.key.cmp(&right.operation.key));
        let has_sequence_wait = operations.iter().any(|tracked| {
            matches!(
                tracked.operation.kind,
                SequenceOperationKind::TimeWait { .. }
            )
        });
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
                SequenceOperationKind::TimeWait { .. } => {
                    if let Some(end) = tracked.estimated_end {
                        let remaining = end
                            .signed_duration_since(Utc::now())
                            .max(chrono::Duration::zero());
                        parts.push(format!(
                            "⏳ Waiting until {} ({} remaining)",
                            end.format("%H:%M UTC"),
                            format_duration(remaining)
                        ));
                    } else {
                        parts.push("⏳ Timed wait in progress".to_string());
                    }
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
            }
        }

        if !has_sequence_wait && let Some(end) = self.state.wait_until {
            let now = Utc::now();
            let minutes = end
                .with_timezone(&Utc)
                .signed_duration_since(now)
                .num_minutes();
            if minutes > 0 {
                parts.push(format!(
                    "⏳ Waiting until {} ({} min remaining)",
                    end.format("%H:%M %Z"),
                    minutes
                ));
            }
        }

        if self.state.sequence_running {
            parts.push("▶️ Sequence running".to_string());
        }

        if let Some(ev) = &self.state.last_mount_event {
            let label = match ev.as_str() {
                event_types::MOUNT_PARKED => "🅿️ Mount parked",
                event_types::MOUNT_UNPARKED => "🔭 Mount unparked",
                event_types::MOUNT_HOMED => "🏠 Mount homed",
                event_types::MOUNT_BEFORE_FLIP => "🔄 Mount pre-flip",
                event_types::MOUNT_AFTER_FLIP => "✅ Mount post-flip",
                event_types::MOUNT_CENTER => "🎯 Centering started",
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

        self.add_meridian_flip_info(&mut message);
        self.add_mount_info(&mut message).await;
        self.chat_manager
            .send_message(&message, &self.chat_target)
            .await;
    }

    async fn send_autofocus_notification(&self, af: &AutofocusResponse) {
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
        let position_change_text = if position_change > 0 {
            format!("+{position_change}")
        } else {
            position_change.to_string()
        };

        let message =
            ChatMessage::new(&self.titled(format!("{success_indicator} Autofocus Completed")))
                .color(color)
                .field("Filter", &af_data.filter, true)
                .field("Method", &af_data.method, true)
                .field("Duration", &af_data.duration, true)
                .field(
                    "Temperature",
                    &format!("{:.1}°C", af_data.temperature),
                    true,
                )
                .field(
                    "Focus Position",
                    &af_data.calculated_focus_point.position.to_string(),
                    true,
                )
                .field("Position Change", &position_change_text, true)
                .field(
                    "HFR Before",
                    &af_data
                        .initial_hfr()
                        .map(|v| format!("{v:.3}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    true,
                )
                .field(
                    "HFR After",
                    &af_data
                        .final_hfr()
                        .map(|v| format!("{v:.3}"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    true,
                )
                .field(
                    "R-squared",
                    &format!("{:.4}", af.get_best_r_squared()),
                    true,
                )
                .field(
                    "Measurements",
                    &af_data.measure_points.len().to_string(),
                    true,
                )
                .footer(&format!("Focuser: {}", af_data.auto_focuser_name));

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
        self.chat_manager
            .send_message_with_attachments(&message, &self.chat_target, &attachments)
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
            _ => ("🔭 Mount Event", colors::GRAY),
        };

        let mut message = ChatMessage::new(&self.titled(title))
            .color(color)
            .field("Event", &event.event, true)
            .field("Time", &event.time, true);

        if let Some(target) = &self.state.current_target {
            message = message.field("Current Target", &target.name, true);
        }

        self.add_mount_info(&mut message).await;
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
            event_types::SEQUENCE_FINISHED => ("🏁 Sequence Finished", colors::GREEN),
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

        if let Some(seq) = &self.state.sequence {
            let containers = seq.get_containers();
            if !containers.is_empty() {
                let running = containers
                    .iter()
                    .filter(|c| c.status.eq_ignore_ascii_case("RUNNING"))
                    .count();
                message = message.field(
                    "Containers",
                    &format!("{} total / {} running", containers.len(), running),
                    true,
                );
            }
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
        let (color, title) = match &event.details {
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
            _ => (get_event_color(&event.event), get_event_title(&event.event)),
        };

        let mut message =
            ChatMessage::new(&self.titled(title))
                .color(color)
                .field("Time", &event.time, false);

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
                    message = message.field("Wait Until", wait_end_time, false);
                }
                EventDetails::AutofocusPointAdded { position, hfr } => {
                    message = message
                        .field("Position", &position.to_string(), true)
                        .field("HFR", &format!("{hfr:.3}"), true);
                }
                EventDetails::RotatorMoved { from, to } => {
                    message = message
                        .field("From", &format!("{from:.2}°"), true)
                        .field("To", &format!("{to:.2}°"), true)
                        .field("Δ", &format!("{:+.2}°", to - from), true);
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
                inline: true,
            });
            if !alt.is_empty() && !az.is_empty() {
                message.fields.push(ChatField {
                    name: "Alt/Az".to_string(),
                    value: format!("Alt: {alt}\nAz: {az}"),
                    inline: true,
                });
            }
            message.fields.push(ChatField {
                name: "Pier Side".to_string(),
                value: mount_info.get_side_of_pier().to_string(),
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
                inline: true,
            });
        }
    }
}

fn get_event_color(event: &str) -> u32 {
    match event {
        // Camera events
        event_types::CAMERA_CONNECTED => colors::GREEN,
        event_types::CAMERA_DISCONNECTED => colors::RED,

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
        event_types::SEQUENCE_FINISHED => colors::GREEN,
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
        | event_types::SAFETY_CONNECTED => colors::GREEN,
        event_types::SAFETY_CHANGED => colors::ORANGE,
        event_types::CAMERA_DOWNLOAD_TIMEOUT => colors::RED,
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
/// dropped silently — leaving the sequence "waiting until" state unset. Treat
/// an offset-less stamp as observatory-local, which is what it is.
fn parse_nina_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed);
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|local| local.fixed_offset())
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
        event_types::TS_WAITSTART => "⏳ Sequence Waiting".to_string(),
        event_types::AUTOFOCUS_POINT_ADDED => "📈 Autofocus Point".to_string(),
        event_types::ROTATOR_MOVED => "🧭 Rotator Moved".to_string(),
        event_types::ROTATOR_MOVED_MECHANICAL => "🧭 Rotator Moved (Mech.)".to_string(),
        event_types::NINA_NOTIFICATION => "🔔 N.I.N.A. notification".to_string(),
        event_types::NINA_LOG => "📝 N.I.N.A. log".to_string(),
        event_types::CHATSTRONOMY_COMMAND_FAILED => "❌ Telescope command failed".to_string(),
        // The event name comes from the plugin, so it is not length-bounded.
        _ => format!("📡 {}", truncate_chat_title(event)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(kind: SequenceOperationKind) -> SequenceOperation {
        SequenceOperation {
            key: "1/0".to_string(),
            name: "Test operation".to_string(),
            status: "RUNNING".to_string(),
            chat_enabled: true,
            kind,
        }
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
