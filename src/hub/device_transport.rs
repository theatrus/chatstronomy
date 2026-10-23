//! Outbound camera WebSockets: event delivery and locally consented snapshots.
//! No telescope queries, arbitrary URLs, or hardware commands exist here.

use super::{
    db::{Db, DbError, unix_now},
    devices::PROTOCOL_VERSION,
    server::HubState,
};
use axum::{
    Json,
    body::{Bytes, to_bytes},
    extract::{
        ConnectInfo, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

const MAX_JPEG: usize = 512 * 1024;
const MAX_WIRE: usize = 720 * 1024;
const COOLDOWN: i64 = 60;
pub const MAX_TIMELAPSE_BYTES: usize = 20 * 1024 * 1024;
use crate::chat::CameraTriggerRules;

enum ControlRequest {
    Configure {
        id: Uuid,
        rules: CameraTriggerRules,
        reply: oneshot::Sender<Result<(), &'static str>>,
        deadline: tokio::time::Instant,
    },
    Event {
        event: &'static str,
        expires_at: i64,
        deadline: tokio::time::Instant,
        telescope_id: i64,
        context: Option<TelescopeObservation>,
    },
}

/// A display-only projection, never a raw equipment or sequence payload. Only
/// transport-v3 cameras receive it; strict v1/v2 clients keep their old wire.
#[derive(Clone, Serialize)]
struct TelescopeObservation {
    telescope_id: i64,
    telescope_name: String,
    target_name: Option<String>,
    occurred_at: String,
}

fn observation_label(value: &str, limit: usize) -> Option<String> {
    let value: String = value
        .chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(limit)
        .collect();
    let value = value.trim();
    (!value.is_empty() && value != "Sequential Instruction Set").then(|| value.to_owned())
}

fn context_action_allowed(action: &str) -> bool {
    let Some((kind, phase)) = action.rsplit_once('_') else {
        return false;
    };
    matches!(phase, "started" | "completed" | "ended" | "failed")
        && matches!(
            kind,
            "camera_cooling"
                | "camera_warming"
                | "time_wait"
                | "astronomical_wait"
                | "safety_wait"
                | "condition_wait"
                | "manual_wait"
                | "sequence_slew"
                | "mount_center"
                | "plate_solve"
        )
}

#[derive(Default)]
pub struct DeviceConnections(
    Mutex<HashMap<i64, Connection>>,
    Mutex<DiscordBackoff>,
    /// Automatic images swallowed by the per-camera cooldown, reported once
    /// the cooldown window ends.
    Mutex<HashMap<i64, u64>>,
    /// Two bounded binary uploads globally, at most one per camera. HTTP
    /// transfers never occupy the camera WebSocket's heartbeat/control loop.
    Mutex<HashSet<i64>>,
);

#[derive(Default)]
struct DiscordBackoff {
    global: Option<tokio::time::Instant>,
    channels: HashMap<i64, tokio::time::Instant>,
}

impl DiscordBackoff {
    fn allows(&mut self, channel: i64) -> bool {
        let now = tokio::time::Instant::now();
        self.channels.retain(|_, until| *until > now);
        !self.global.is_some_and(|until| until > now) && !self.channels.contains_key(&channel)
    }

    fn observe(&mut self, channel: i64, status: reqwest::StatusCode, headers: &HeaderMap) {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        let denied = matches!(status.as_u16(), 401 | 403 | 404);
        if !limited && !denied && header("x-ratelimit-remaining") != Some("0") {
            return;
        }
        let seconds = if denied {
            300.0
        } else {
            header(if limited {
                "retry-after"
            } else {
                "x-ratelimit-reset-after"
            })
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(60.0)
            .clamp(1.0, 86400.0)
        };
        let until = tokio::time::Instant::now() + Duration::from_secs_f64(seconds);
        if status == reqwest::StatusCode::UNAUTHORIZED
            || header("x-ratelimit-global") == Some("true")
            || header("x-ratelimit-scope") == Some("global")
        {
            self.global = Some(self.global.map_or(until, |old| old.max(until)));
        } else {
            self.channels
                .entry(channel)
                .and_modify(|old| *old = (*old).max(until))
                .or_insert(until);
        }
    }
}

struct Connection {
    generation: Uuid,
    snapshots: bool,
    requests: mpsc::Sender<SnapshotRequest>,
    close: watch::Sender<bool>,
    controls: mpsc::Sender<ControlRequest>,
    chat_configuration: bool,
    telescope_events: bool,
    protocol_version: u32,
    timelapses: bool,
    observation_context: bool,
}

struct SnapshotRequest {
    id: Uuid,
    reply: oneshot::Sender<Result<(), &'static str>>,
    expires_at: i64,
    deadline: tokio::time::Instant,
}

impl DeviceConnections {
    pub async fn configure(&self, id: i64, rules: CameraTriggerRules) -> Result<(), &'static str> {
        if !rules.valid() {
            return Err("Use 0–1440 minutes, 1–3 images, and 60–600 seconds spacing");
        }
        let (reply, received) = oneshot::channel();
        {
            let connections = self.0.lock().map_err(|_| "camera unavailable")?;
            let c = connections.get(&id).ok_or("camera is offline")?;
            if !c.chat_configuration {
                return Err(
                    "Chat configuration is disabled at the camera; enable it locally first",
                );
            }
            c.controls
                .try_send(ControlRequest::Configure {
                    id: Uuid::new_v4(),
                    rules,
                    reply,
                    deadline: tokio::time::Instant::now() + Duration::from_secs(15),
                })
                .map_err(|_| "camera is busy")?;
        }
        tokio::time::timeout(Duration::from_secs(20), received)
            .await
            .map_err(
                |_| "Configuration timed out; check active rules in AutoPierCam before retrying",
            )?
            .map_err(|_| "camera disconnected; check active rules before retrying")?
    }

    /// Only fresh, live, chat-enabled telescope events; shared owner AND route
    /// avoid cross-tenant triggers. Admission/dedup happens in ChatUpdater.
    pub fn telescope_event(&self, db: &Db, telescope_id: i64, event: &crate::events::Event) {
        self.telescope_event_with_target(db, telescope_id, event, None);
    }

    pub fn telescope_event_with_target(
        &self,
        db: &Db,
        telescope_id: i64,
        event: &crate::events::Event,
        target_name: Option<&str>,
    ) {
        use crate::events::event_types;
        if !event.chat_enabled {
            return;
        }
        let Ok(time) = chrono::DateTime::parse_from_rfc3339(&event.time) else {
            return;
        };
        let age = chrono::Utc::now().signed_duration_since(time);
        if age < chrono::Duration::zero() || age >= chrono::Duration::seconds(30) {
            return;
        }
        let (name, legacy) = match event.event.as_str() {
            event_types::MOUNT_SLEW_STARTED => ("mount_slew_started", true),
            event_types::MOUNT_SLEWED => ("mount_slewed", true),
            event_types::SEQUENCE_STARTING => ("sequence_started", true),
            event_types::SEQUENCE_FINISHED => ("sequence_finished", true),
            event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART => {
                ("target_changed", false)
            }
            event_types::AUTOFOCUS_STARTING => ("autofocus_started", false),
            event_types::AUTOFOCUS_FINISHED => ("autofocus_completed", false),
            event_types::ERROR_AF => ("autofocus_failed", false),
            event_types::MOUNT_PARKED => ("mount_parked", false),
            event_types::MOUNT_UNPARKED => ("mount_unparked", false),
            event_types::MOUNT_BEFORE_FLIP => ("meridian_flip_started", false),
            event_types::MOUNT_AFTER_FLIP => ("meridian_flip_completed", false),
            event_types::GUIDER_START => ("guiding_started", false),
            event_types::GUIDER_STOP => ("guiding_stopped", false),
            event_types::GUIDER_DITHER => ("guiding_dithered", false),
            event_types::ROTATOR_MOVE_STARTED => ("rotator_move_started", false),
            event_types::ROTATOR_MOVED => ("rotator_moved", false),
            event_types::FILTERWHEEL_CHANGED => ("filter_changed", false),
            event_types::FOCUSER_MOVED => ("focuser_moved", false),
            event_types::TS_WAITSTART => ("time_wait_started", false),
            _ => return,
        };
        self.notify_telescope_event(db, telescope_id, name, target_name, time, legacy);
    }

    pub fn telescope_target_changed(&self, db: &Db, telescope_id: i64, target_name: &str) {
        if observation_label(target_name, 120).is_none() {
            return;
        }
        self.notify_telescope_event(
            db,
            telescope_id,
            "target_changed",
            Some(target_name),
            chrono::Utc::now().fixed_offset(),
            false,
        );
    }

    pub fn telescope_action(
        &self,
        db: &Db,
        telescope_id: i64,
        action: &'static str,
        target_name: Option<&str>,
    ) {
        if context_action_allowed(action) {
            self.notify_telescope_event(
                db,
                telescope_id,
                action,
                target_name,
                chrono::Utc::now().fixed_offset(),
                false,
            );
        }
    }

    fn notify_telescope_event(
        &self,
        db: &Db,
        telescope_id: i64,
        name: &'static str,
        target_name: Option<&str>,
        time: chrono::DateTime<chrono::FixedOffset>,
        legacy: bool,
    ) {
        let Ok(Some(telescope)) = db.get_telescope(telescope_id) else {
            return;
        };
        let Some(telescope_name) = observation_label(&telescope.name, 80) else {
            return;
        };
        let context = TelescopeObservation {
            telescope_id,
            telescope_name,
            target_name: target_name.and_then(|name| observation_label(name, 120)),
            occurred_at: time.with_timezone(&chrono::Utc).to_rfc3339(),
        };
        if name == "target_changed" && context.target_name.is_none() {
            return;
        }
        let expires_at = time.timestamp() + 30;
        let Some(remaining) = u64::try_from(expires_at - unix_now())
            .ok()
            .filter(|seconds| *seconds > 0)
        else {
            return;
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(remaining);
        let Ok(ids) = db.trigger_devices(telescope_id) else {
            return;
        };
        if let Ok(connections) = self.0.lock() {
            for id in ids {
                if let Some(c) = connections
                    .get(&id)
                    .filter(|c| c.telescope_events && (legacy || c.observation_context))
                {
                    let _ = c.controls.try_send(ControlRequest::Event {
                        event: name,
                        expires_at,
                        deadline,
                        telescope_id,
                        context: c.observation_context.then(|| context.clone()),
                    });
                }
            }
        }
    }

    pub fn connected(&self, id: i64) -> bool {
        self.0.lock().is_ok_and(|c| c.contains_key(&id))
    }

    /// Counts one swallowed image. True for the first in a window, whose
    /// caller schedules the notice.
    fn count_elided(&self, id: i64) -> bool {
        self.2.lock().is_ok_and(|mut counts| {
            let count = counts.entry(id).or_default();
            *count += 1;
            *count == 1
        })
    }

    fn take_elided(&self, id: i64) -> u64 {
        self.2
            .lock()
            .ok()
            .and_then(|mut counts| counts.remove(&id))
            .unwrap_or(0)
    }

    /// Whether the live connection advertised local snapshot consent.
    pub fn snapshots_shared(&self, id: i64) -> bool {
        self.0
            .lock()
            .is_ok_and(|c| c.get(&id).is_some_and(|c| c.snapshots))
    }

    pub fn disconnect(&self, id: i64) {
        if let Ok(c) = self.0.lock()
            && let Some(c) = c.get(&id)
        {
            let _ = c.close.send(true);
        }
    }

    pub async fn snapshot(&self, id: i64) -> Result<(), &'static str> {
        let (reply, received) = oneshot::channel();
        {
            let connections = self.0.lock().map_err(|_| "camera unavailable")?;
            let c = connections.get(&id).ok_or("camera is offline")?;
            if !c.snapshots {
                return Err("snapshot sharing is disabled at the camera");
            }
            c.requests
                .try_send(SnapshotRequest {
                    id: Uuid::new_v4(),
                    reply,
                    expires_at: unix_now() + 90,
                    deadline: tokio::time::Instant::now() + Duration::from_secs(90),
                })
                .map_err(|_| "camera is busy")?;
        }
        tokio::time::timeout(Duration::from_secs(95), received)
            .await
            .map_err(|_| "snapshot timed out")?
            .map_err(|_| "camera disconnected")?
    }
}

struct Lease {
    registry: Arc<DeviceConnections>,
    id: i64,
    generation: Uuid,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut c) = self.registry.0.lock()
            && c.get(&self.id)
                .is_some_and(|c| c.generation == self.generation)
        {
            c.remove(&self.id);
        }
    }
}

// Secret-bearing messages deliberately do not implement Debug.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMessage {
    ObservationCapabilities {
        telemetry: bool,
    },
    MediaCapabilities {
        timelapses: bool,
    },
    TriggerCapabilities {
        chat_configuration: bool,
        telescope_events: bool,
        #[serde(default)]
        observation_context: Option<bool>,
    },
    TriggerConfigurationResult {
        request_id: Uuid,
        accepted: bool,
    },
    Authenticate {
        protocol_version: u32,
        installation_id: Uuid,
        credential: String,
        snapshots: bool,
    },
    Event {
        event: CameraEvent,
    },
    SnapshotUnavailable {
        request_id: Uuid,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SceneChange,
    DayNightTransition,
    Snapshot,
    Periodic,
    TelescopeEvent,
    Timelapse,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEvent {
    pub event_id: Uuid,
    pub kind: EventKind,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub summary: String,
    pub jpeg_base64: String,
    pub request_id: Option<Uuid>,
}

impl CameraEvent {
    fn jpeg(&self) -> Result<Vec<u8>, &'static str> {
        if self.kind == EventKind::Timelapse {
            return Err("invalid_event"); // Binary HTTP upload only, never base64 over WS.
        }
        let age = unix_now() - self.captured_at.timestamp();
        if self.event_id.is_nil()
            || self.summary.chars().count() > 300
            || self.summary.chars().any(char::is_control)
            || !(-30..=300).contains(&age)
            || (self.kind == EventKind::Snapshot && age > 120)
            || self.jpeg_base64.len() > MAX_JPEG.div_ceil(3) * 4
        {
            return Err("invalid_event");
        }
        let bytes = STANDARD
            .decode(&self.jpeg_base64)
            .map_err(|_| "invalid_image")?;
        // Bound the envelope before forwarding; the Hub never decodes pixels or
        // fetches client-provided URLs. Discord validates the actual attachment.
        if bytes.len() > MAX_JPEG
            || bytes.len() < 4
            || !bytes.starts_with(&[0xff, 0xd8])
            || !bytes.ends_with(&[0xff, 0xd9])
        {
            return Err("invalid_image");
        }
        Ok(bytes)
    }
}

pub async fn upgrade(
    State(state): State<HubState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = super::server::client_ip(&headers, peer, state.config.trust_x_forwarded_for);
    ws.max_message_size(MAX_WIRE)
        .max_frame_size(MAX_WIRE)
        .on_upgrade(move |socket| session(state, socket, ip))
}

async fn send(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(10),
            socket.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}

struct TimelapseLease {
    connections: Arc<DeviceConnections>,
    id: i64,
    generation: Uuid,
    installation: Uuid,
    credential: String,
    closed: watch::Receiver<bool>,
}

impl TimelapseLease {
    fn authorized(&self, state: &HubState) -> bool {
        !*self.closed.borrow()
            && state
                .db
                .authenticate_device(&self.credential, self.installation)
                .ok()
                .flatten()
                == Some(self.id)
            && self.connections.0.lock().is_ok_and(|connections| {
                connections.get(&self.id).is_some_and(|c| {
                    c.generation == self.generation && c.protocol_version >= 3 && c.timelapses
                })
            })
    }
}

impl Drop for TimelapseLease {
    fn drop(&mut self) {
        if let Ok(mut active) = self.connections.3.lock() {
            active.remove(&self.id);
        }
    }
}

/// Validate only the bounded ISO-BMFF envelope. No untrusted video is decoded
/// or transcoded on the Hub. Discord checks whether the attachment is playable.
fn valid_mp4(bytes: &[u8]) -> bool {
    let mut offset = 0usize;
    let (mut ftyp, mut moov, mut mdat) = (false, false, false);
    while offset < bytes.len() {
        let Some(header) = bytes.get(offset..offset.saturating_add(8)) else {
            return false;
        };
        let mut size = u32::from_be_bytes(header[..4].try_into().expect("four bytes")) as u64;
        let kind = &header[4..8];
        let header_len = if size == 1 {
            let Some(large) = bytes.get(offset + 8..offset + 16) else {
                return false;
            };
            size = u64::from_be_bytes(large.try_into().expect("eight bytes"));
            16
        } else {
            8
        };
        if size == 0 {
            size = (bytes.len() - offset) as u64;
        }
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        let Some(end) = offset.checked_add(size).filter(|end| *end <= bytes.len()) else {
            return false;
        };
        if size < header_len {
            return false;
        }
        if offset == 0 && kind != b"ftyp" {
            return false;
        }
        match kind {
            b"ftyp" => {
                if ftyp || size < header_len + 8 {
                    return false;
                }
                let payload = &bytes[offset + header_len..end];
                if !payload.chunks_exact(4).enumerate().any(|(index, brand)| {
                    index != 1
                        && matches!(
                            brand,
                            b"isom"
                                | b"iso2"
                                | b"iso4"
                                | b"iso5"
                                | b"iso6"
                                | b"mp41"
                                | b"mp42"
                                | b"avc1"
                        )
                }) {
                    return false;
                }
                ftyp = true;
            }
            b"moov" => moov = size > header_len,
            b"mdat" => mdat = size > header_len,
            _ => {}
        }
        offset = end;
    }
    ftyp && moov && mdat
}

/// Raw MP4 HTTP body, authenticated before buffering. No URLs, file paths,
/// filenames, destinations, cookies or user-authored captions are accepted.
pub async fn timelapse_upload(State(state): State<HubState>, request: Request) -> Response {
    let headers = request.headers();
    let value = |key: &str| headers.get(key).and_then(|v| v.to_str().ok());
    let Some(credential) = value("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() <= 256)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(installation) = value("x-installation-id")
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|id| !id.is_nil())
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Some(id)) = state.db.authenticate_device(credential, installation) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(event_id) = value("x-event-id")
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|id| !id.is_nil())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(captured_at) = value("x-captured-at")
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|v| v.with_timezone(&chrono::Utc))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let age = chrono::Utc::now().signed_duration_since(captured_at);
    if age < chrono::Duration::seconds(-30) || age > chrono::Duration::hours(24) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    if value("content-type") != Some("video/mp4") {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Some(length) = value("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= MAX_TIMELAPSE_BYTES)
    else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    if !state.limits.device_events.check(&id.to_string()) {
        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")]).into_response();
    }
    let lease = {
        let Ok(connections) = state.device_connections.0.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let Some(c) = connections
            .get(&id)
            .filter(|c| c.protocol_version >= 3 && c.timelapses && !*c.close.borrow())
        else {
            return StatusCode::FORBIDDEN.into_response();
        };
        let Ok(mut active) = state.device_connections.3.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if active.len() >= 2 || !active.insert(id) {
            return (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "60")]).into_response();
        }
        TimelapseLease {
            connections: state.device_connections.clone(),
            id,
            generation: c.generation,
            installation,
            credential: credential.to_owned(),
            closed: c.close.subscribe(),
        }
    };
    let mut closed = lease.closed.clone();
    tokio::select! {
        biased;
        _ = closed.changed() => StatusCode::FORBIDDEN.into_response(),
        result = tokio::time::timeout(Duration::from_secs(180), async {
            let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_secs(120), to_bytes(request.into_body(), MAX_TIMELAPSE_BYTES)).await else {return StatusCode::PAYLOAD_TOO_LARGE.into_response();};
            if bytes.len() != length || !valid_mp4(&bytes) {return StatusCode::UNPROCESSABLE_ENTITY.into_response();}
            if !lease.authorized(&state) {return StatusCode::FORBIDDEN.into_response();}
            let Ok(http) = reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never()).build() else {return StatusCode::SERVICE_UNAVAILABLE.into_response();};
            let event = CameraEvent {event_id, kind: EventKind::Timelapse, captured_at,
                summary: "Daily pier-camera timelapse".into(), jpeg_base64: String::new(), request_id: None};
            let status = deliver_media(&state, &http, id, &event, Some((bytes, &lease))).await;
            Json(json!({"status":status,"retry_after_seconds":if status=="retry" {60} else {0}})).into_response()
        }) => result.unwrap_or_else(|_| StatusCode::GATEWAY_TIMEOUT.into_response()),
    }
}

async fn session(state: HubState, mut socket: WebSocket, ip: String) {
    if state.limits.direct_auth.blocked(&ip) {
        let _ = send(&mut socket, json!({"type":"error","code":"rate_limited"})).await;
        return;
    }
    let Ok(Some(Ok(Message::Text(text)))) =
        tokio::time::timeout(Duration::from_secs(10), socket.recv()).await
    else {
        return;
    };
    if text.len() > 4096 {
        state.limits.direct_auth.check(&ip);
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"invalid_message"}),
        )
        .await;
        return;
    }
    let Ok(ClientMessage::Authenticate {
        protocol_version,
        installation_id,
        credential,
        snapshots,
    }) = serde_json::from_str(&text)
    else {
        state.limits.direct_auth.check(&ip);
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"invalid_message"}),
        )
        .await;
        return;
    };
    if ![PROTOCOL_VERSION, 2, 3, 4].contains(&protocol_version) {
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"unsupported_version"}),
        )
        .await;
        return;
    }
    if credential.len() > 256 {
        state.limits.direct_auth.check(&ip);
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"authentication_failed"}),
        )
        .await;
        return;
    }
    let Ok(Some(id)) = state.db.authenticate_device(&credential, installation_id) else {
        state.limits.direct_auth.check(&ip);
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"authentication_failed"}),
        )
        .await;
        return;
    };
    let generation = Uuid::new_v4();
    let (requests, receiver) = mpsc::channel(1);
    let (controls, control_receiver) = mpsc::channel(4);
    let (close, mut closed) = watch::channel(false);
    let registered = if let Ok(mut c) = state.device_connections.0.lock() {
        if let std::collections::hash_map::Entry::Vacant(e) = c.entry(id) {
            e.insert(Connection {
                generation,
                snapshots,
                requests,
                close,
                controls,
                chat_configuration: false,
                telescope_events: false,
                protocol_version,
                timelapses: false,
                observation_context: false,
            });
            true
        } else {
            false
        }
    } else {
        false
    };
    if !registered {
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"already_connected"}),
        )
        .await;
        return;
    }
    let _lease = Lease {
        registry: state.device_connections.clone(),
        id,
        generation,
    };
    // Cancellation drops any in-flight delivery future. An already accepted
    // Discord message cannot be recalled; receipts provide at-least-once delivery.
    tokio::select! {
        biased;
        _=closed.changed()=>{},
        _=run(&state,&mut socket,id,installation_id,&credential,receiver,(protocol_version,control_receiver))=>{},
    }
}

async fn run(
    state: &HubState,
    socket: &mut WebSocket,
    id: i64,
    installation: Uuid,
    credential: &str,
    mut requests: mpsc::Receiver<SnapshotRequest>,
    (protocol_version, mut controls): (u32, mpsc::Receiver<ControlRequest>),
) {
    if !send(
        socket,
        json!({"type":"ready","protocol_version":protocol_version,"device_id":id}),
    )
    .await
    {
        return;
    }
    let Ok(http) = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return;
    };
    let mut pending: Option<SnapshotRequest> = None;
    let mut pending_configuration: Option<ControlRequest> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    let mut last_received = tokio::time::Instant::now();
    let mut telemetry = false;
    let mut observation_ticks = tokio::time::interval(Duration::from_secs(5));
    observation_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed_telescopes = HashSet::new();
    let mut observation_query: Option<super::observation::ObservationQuery<'_>> = None;
    loop {
        // Recheck durable authority, not just the original handshake.
        if state
            .db
            .authenticate_device(credential, installation)
            .ok()
            .flatten()
            != Some(id)
        {
            return;
        }
        tokio::select! {
            observations=async {observation_query.as_mut().expect("guarded observation query").await}, if observation_query.is_some()=>{
                observation_query=None;
                if !telemetry {continue;}
                let routed = state.db.observation_telescopes(id).unwrap_or_default();
                let current: HashSet<_> = routed.iter().map(|(id,_)|*id).collect();
                for (telescope,mut message,connection_id) in observations {
                    if !current.contains(&telescope) || !state.db.observation_telescopes(id)
                        .is_ok_and(|routes|routes.iter().any(|(target,_)|*target==telescope)) {continue;}
                    // Revalidate the exact source after async work. Profile/consent
                    // rotations cannot republish a retiring connection's snapshot.
                    let live = connection_id.is_some_and(|expected|state.rig_connections.get(telescope)
                        .is_some_and(|c|c.connection_id==expected && !c.is_retiring()));
                    if !live {message=super::observation::unavailable(telescope);}
                    if !send(socket,message).await {return;}
                }
                for removed in observed_telescopes.difference(&current) {
                    if !send(socket,super::observation::unavailable(*removed)).await {return;}
                }
                observed_telescopes=current;
            },
            _=observation_ticks.tick(), if telemetry && observation_query.is_none()=>{
                observation_query=Some(Box::pin(super::observation::collect(state,id)));
            },
            control=controls.recv()=>{
                let Some(control)=control else {return;};
                match control {
                    ControlRequest::Event {event, expires_at, deadline, telescope_id, context} => {
                        // Authority may have changed after queue admission. Never
                        // drain queued labels across a local consent/route boundary.
                        let allowed = state.device_connections.0.lock().is_ok_and(|connections|
                             connections.get(&id).is_some_and(|c| c.telescope_events && (context.is_none() || c.observation_context)));
                        let routed = state.db.trigger_devices(telescope_id).is_ok_and(|ids| ids.contains(&id));
                        if !allowed || !routed || expires_at<=unix_now() || deadline<=tokio::time::Instant::now() {continue;}
                        let mut message = json!({"type":"telescope_event","event":event,"expires_at":expires_at});
                        if let Some(context) = context {message["context"] = json!(context);}
                        if !send(socket,message).await {return;}
                    }
                    ControlRequest::Configure {id, ref rules, ref reply, deadline} => {
                        if pending_configuration.as_ref().is_some_and(|p| matches!(p, ControlRequest::Configure{reply,deadline,..} if reply.is_closed() || *deadline<=tokio::time::Instant::now())) {pending_configuration=None;}
                        if pending_configuration.is_some() {
                            if let ControlRequest::Configure {reply,..}=control {let _=reply.send(Err("camera configuration is busy"));}
                            continue;
                        }
                        if reply.is_closed() || deadline<=tokio::time::Instant::now() {continue;}
                        if !send(socket,json!({"type":"configure_triggers","request_id":id,"rules":rules})).await {return;}
                        pending_configuration=Some(control);
                    }
                }
            },
            _=heartbeat.tick()=>{
                if last_received.elapsed()>Duration::from_secs(120) {return;}
                if pending.as_ref().is_some_and(|p|p.deadline<=tokio::time::Instant::now() || p.reply.is_closed()) {
                    let p=pending.take().expect("checked pending");let _=p.reply.send(Err("snapshot timed out"));
                }
                if !matches!(tokio::time::timeout(Duration::from_secs(10),socket.send(Message::Ping(Vec::new().into()))).await,Ok(Ok(()))) {return;}
            },
            request=requests.recv()=>{
                let Some(request)=request else {return;};
                if pending.is_some() {let _=request.reply.send(Err("snapshot already in progress"));continue;}
                if request.reply.is_closed() || request.deadline<=tokio::time::Instant::now() {continue;}
                if !send(socket,json!({"type":"snapshot_request","request_id":request.id,"expires_at":request.expires_at,"max_jpeg_bytes":MAX_JPEG,"max_frame_age_seconds":120})).await {return;}
                pending=Some(request);
            },
            message=socket.recv()=>{
                let Some(Ok(message))=message else {return;};
                last_received=tokio::time::Instant::now();
                let text=match message {
                    Message::Text(text)=>text,
                    Message::Ping(bytes)=>{if !matches!(tokio::time::timeout(Duration::from_secs(10),socket.send(Message::Pong(bytes))).await,Ok(Ok(()))) {return;} continue;},
                    Message::Pong(_)=>continue,
                    _=>return,
                };
                let Ok(message)=serde_json::from_str::<ClientMessage>(&text) else {return;};
                match message {
                    ClientMessage::ObservationCapabilities {telemetry: enabled} => {
                        if protocol_version != 4 {return;}
                        observation_query=None;
                        telemetry=enabled;
                        observed_telescopes.clear();
                        if !send(socket,json!({"type":"observation_ready","telemetry":enabled,"interval_seconds":5,"max_age_seconds":30})).await {return;}
                    }
                    ClientMessage::MediaCapabilities {timelapses} => {
                        if ![3,4].contains(&protocol_version) {return;}
                        if let Ok(mut connections)=state.device_connections.0.lock() && let Some(c)=connections.get_mut(&id) {
                            if c.timelapses && !timelapses {return;}
                            c.timelapses=timelapses;
                        }
                        if timelapses && !send(socket,json!({"type":"media_ready","max_timelapse_bytes":MAX_TIMELAPSE_BYTES})).await {return;}
                    }
                    ClientMessage::TriggerCapabilities {chat_configuration,telescope_events,observation_context} => {
                        if ![2,3,4].contains(&protocol_version) {return;}
                        if protocol_version == 2 && observation_context.is_some() {return;}
                        if let Ok(mut connections)=state.device_connections.0.lock() && let Some(c)=connections.get_mut(&id) {
                            c.chat_configuration=chat_configuration; c.telescope_events=telescope_events;
                            c.observation_context=protocol_version>=3 && observation_context.unwrap_or(true);
                        }
                    }
                    ClientMessage::TriggerConfigurationResult {request_id,accepted} => {
                        if ![2,3,4].contains(&protocol_version) {return;}
                        if pending_configuration.as_ref().is_some_and(|p|matches!(p,ControlRequest::Configure{id,..} if *id==request_id))
                            && let Some(ControlRequest::Configure{reply,deadline,..})=pending_configuration.take() {
                            let _=reply.send(if deadline<=tokio::time::Instant::now() {Err("Configuration reply expired; check AutoPierCam")}
                                else if accepted {Ok(())} else {Err("Camera declined these rules; they exceed locally enabled permissions or could not be saved")});
                        }
                    }
                    ClientMessage::Authenticate{..}=>return,
                    ClientMessage::SnapshotUnavailable{request_id}=>{
                        if pending.as_ref().is_some_and(|p|p.id==request_id) {
                            let _=pending.take().expect("checked pending").reply.send(Err("camera declined or has no recent frame"));
                        }
                    },
                    ClientMessage::Event{event}=>{
                        let snapshot=event.kind==EventKind::Snapshot;
                        let authorized=if event.kind==EventKind::Timelapse {false} else if snapshot {
                            pending.as_ref().is_some_and(|p|Some(p.id)==event.request_id && p.deadline>tokio::time::Instant::now() && !p.reply.is_closed())
                        } else {event.request_id.is_none()};
                        let result=if !state.limits.device_events.check(&id.to_string()) {"rate_limited"}
                            else if authorized {tokio::time::timeout(Duration::from_secs(30),deliver(state,&http,id,&event)).await.unwrap_or("retry")} else {"invalid_request"};
                        if !send(socket,json!({"type":"event_ack","event_id":event.event_id,"status":result,"retry_after_seconds":if result=="retry" || result=="rate_limited" {60}else{0}})).await {return;}
                        if snapshot && authorized && result!="retry" {
                            let _=pending.take().expect("authorized snapshot").reply.send(match result {
                                "delivered"=>Ok(()),
                                "elided"=>Err("the camera posted less than a minute ago; try again shortly"),
                                _=>Err("snapshot was not delivered; check routes"),
                            });
                        }
                    }
                }
            }
        }
    }
}

enum Reservation {
    Ready,
    Conflict,
    /// Inside the cooldown; the window ends at this Unix time.
    Elided(i64),
}
impl Db {
    /// Same owner AND exact shared guild/channel; cap fan-out per camera.
    pub(crate) fn observation_telescopes(
        &self,
        camera: i64,
    ) -> Result<Vec<(i64, String)>, DbError> {
        self.with_conn(|c|c.prepare("SELECT DISTINCT t.id,t.name FROM telescopes t JOIN devices d ON d.owner_id=t.owner_id JOIN device_channels dc ON dc.device_id=d.id JOIN telescope_channels tc ON tc.telescope_id=t.id AND tc.channel_id=dc.channel_id AND tc.guild_id=dc.guild_id WHERE d.id=?1 ORDER BY t.id LIMIT 16")?
            .query_map([camera],|r|Ok((r.get(0)?,r.get(1)?)))?.collect())
    }
    fn trigger_devices(&self, telescope_id: i64) -> Result<Vec<i64>, DbError> {
        self.with_conn(|c| {
            c.prepare("SELECT DISTINCT d.id FROM devices d JOIN telescopes t ON t.owner_id=d.owner_id JOIN device_channels dc ON dc.device_id=d.id JOIN telescope_channels tc ON tc.telescope_id=t.id AND tc.channel_id=dc.channel_id AND tc.guild_id=dc.guild_id WHERE t.id=?1")?
                .query_map([telescope_id], |r|r.get(0))?.collect()
        })
    }
    fn reserve_device_event(&self, id: i64, event: &CameraEvent) -> Result<Reservation, DbError> {
        let hash =
            super::auth::sha256_b64url(&serde_json::to_string(event).expect("serializable event"));
        self.reserve_device_media(id, event, &hash)
    }

    fn reserve_device_media(
        &self,
        id: i64,
        event: &CameraEvent,
        hash: &str,
    ) -> Result<Reservation, DbError> {
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            tx.execute("DELETE FROM device_events WHERE received_at<?1",[unix_now()-7*86400])?;
            let old:Option<String>=tx.query_row("SELECT payload_hash FROM device_events WHERE device_id=?1 AND event_id=?2",params![id,event.event_id.to_string()],|r|r.get(0)).optional()?;
            if let Some(old)=old {return Ok(if old==hash {Reservation::Ready} else {Reservation::Conflict});}
            let latest:Option<i64>=tx.query_row("SELECT MAX(received_at) FROM device_events WHERE device_id=?1",[id],|r|r.get(0))?;
            if let Some(t)=latest.filter(|t|unix_now()-t<COOLDOWN) {return Ok(Reservation::Elided(t+COOLDOWN));}
            tx.execute("INSERT INTO device_events(device_id,event_id,payload_hash,received_at) VALUES(?1,?2,?3,?4)",params![id,event.event_id.to_string(),hash,unix_now()])?;
            tx.execute("INSERT INTO device_event_targets(device_id,event_id,route_id) SELECT device_id,?2,id FROM device_channels WHERE device_id=?1",params![id,event.event_id.to_string()])?;
            tx.commit()?;
            Ok(Reservation::Ready)
        })
    }

    fn pending_device_targets(
        &self,
        id: i64,
        event: Uuid,
    ) -> Result<Vec<(i64, i64, i64)>, DbError> {
        self.with_conn(|c|c.prepare("SELECT dc.id,dc.guild_id,dc.channel_id FROM device_event_targets t JOIN device_channels dc ON dc.id=t.route_id WHERE t.device_id=?1 AND t.event_id=?2 AND t.delivered_at IS NULL")?
            .query_map(params![id,event.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect())
    }

    fn device_delivered(&self, id: i64, event: Uuid, route: i64) -> Result<(), DbError> {
        self.with_conn(|c|c.execute("UPDATE device_event_targets SET delivered_at=?4 WHERE device_id=?1 AND event_id=?2 AND route_id=?3",params![id,event.to_string(),route,unix_now()]).map(|_|()))
    }
}

async fn deliver(
    state: &HubState,
    http: &reqwest::Client,
    id: i64,
    event: &CameraEvent,
) -> &'static str {
    deliver_media(state, http, id, event, None).await
}

async fn deliver_media(
    state: &HubState,
    http: &reqwest::Client,
    id: i64,
    event: &CameraEvent,
    video: Option<(Bytes, &TimelapseLease)>,
) -> &'static str {
    let is_video = video.is_some();
    let media = match &video {
        Some((bytes, _)) => bytes.clone(),
        None => match event.jpeg() {
            Ok(j) => j,
            Err(e) => return e,
        }
        .into(),
    };
    let Some(checker) = &state.guild_checker else {
        return "retry";
    };
    if state.config.discord.bot_token.is_empty() {
        return "retry";
    }
    let Ok(Some(device)) = state.db.get_device(id) else {
        return "retry";
    };
    let reservation = if is_video {
        use sha2::{Digest, Sha256};
        let digest = STANDARD.encode(Sha256::digest(&media));
        let hash = super::auth::sha256_b64url(&format!(
            "{}:{digest}",
            serde_json::to_string(event).expect("serializable event")
        ));
        state.db.reserve_device_media(id, event, &hash)
    } else {
        state.db.reserve_device_event(id, event)
    };
    match reservation {
        Ok(Reservation::Ready) => {}
        Ok(Reservation::Conflict) => return "event_conflict",
        Ok(Reservation::Elided(until)) => {
            // A finalized daily movie is not a disposable fast preview. Let the
            // bounded client retry the same ID after the shared cooldown.
            if is_video {
                return "retry";
            }
            // A requested snapshot is refused to its requester instead.
            if !is_video
                && event.kind != EventKind::Snapshot
                && state.device_connections.count_elided(id)
            {
                let (state, http) = (state.clone(), http.clone());
                tokio::spawn(async move {
                    let wait = (until - unix_now()).clamp(0, COOLDOWN) as u64;
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    let count = state.device_connections.take_elided(id);
                    if count > 0 {
                        post_elision_notice(&state, &http, id, count).await;
                    }
                });
            }
            return "elided";
        }
        Err(_) => return "retry",
    }
    let Ok(targets) = state.db.pending_device_targets(id, event.event_id) else {
        return "retry";
    };
    let total = state.db.with_conn(|c| {
        c.query_row(
            "SELECT count(*) FROM device_event_targets WHERE device_id=?1 AND event_id=?2",
            params![id, event.event_id.to_string()],
            |r| r.get::<_, i64>(0),
        )
    });
    match total {
        Ok(0) => return "no_destinations",
        Err(_) => return "retry",
        _ => {}
    }
    // All destinations are owner-authorized DB routes, never client-supplied IDs.
    let permanent_rejection = std::sync::atomic::AtomicBool::new(false);
    let outcomes=futures_util::future::join_all(targets.into_iter().map(|(route,guild,channel)| {
        let media=&media;
        let authority = video.as_ref().map(|(_, lease)| *lease);
        let permanent_rejection = &permanent_rejection;
        let name=&device.name;
        async move {
            if !state.device_connections.1.lock().is_ok_and(|mut limits|limits.allows(channel)) {return false;}
            if !checker.bot_in_guild(guild as u64).await || !checker.channel_in_guild(channel as u64,guild as u64).await {return false;}
            if authority.is_some_and(|lease| !lease.authorized(state)) {return false;}
            let label=match event.kind {EventKind::SceneChange=>"Scene change",EventKind::DayNightTransition=>"Day/night transition",EventKind::Snapshot=>"Requested snapshot",EventKind::Periodic=>"Scheduled image",EventKind::TelescopeEvent=>"Telescope event",EventKind::Timelapse=>"Daily timelapse"};
            let (filename, mime) = if is_video {("timelapse.mp4", "video/mp4")} else {("piercam.jpg", "image/jpeg")};
            let payload=if is_video {json!({"allowed_mentions":{"parse":[]},"content":format!("{name} · {label}\nCompleted: {}",event.captured_at.to_rfc3339()),"attachments":[{"id":0,"filename":filename}]})}
                else {json!({"allowed_mentions":{"parse":[]},"embeds":[{"title":format!("{name} · {label}"),"description":event.summary,"timestamp":event.captured_at.to_rfc3339(),"image":{"url":"attachment://piercam.jpg"},"footer":{"text":"AutoPierCam · automated camera observation"}}],"attachments":[{"id":0,"filename":filename}]})};
            let part=reqwest::multipart::Part::stream_with_length(reqwest::Body::from(media.clone()),media.len() as u64).file_name(filename).mime_str(mime).expect("constant MIME");
            let form=reqwest::multipart::Form::new().text("payload_json",payload.to_string()).part("files[0]",part);
            let result=http.post(format!("{}/api/v10/channels/{}/messages",state.config.discord.base_url.trim_end_matches('/'),channel as u64))
                .header("authorization",format!("Bot {}",state.config.discord.bot_token)).multipart(form).send().await;
            let Ok(response)=result else {return false;};
            if let Ok(mut limits)=state.device_connections.1.lock() {limits.observe(channel,response.status(),response.headers());}
            if is_video && matches!(response.status(),reqwest::StatusCode::PAYLOAD_TOO_LARGE | reqwest::StatusCode::BAD_REQUEST) {permanent_rejection.store(true,std::sync::atomic::Ordering::Relaxed);return false;}
            if !response.status().is_success() {return false;}
            state.db.device_delivered(id,event.event_id,route).is_ok()
        }
    })).await;
    if permanent_rejection.load(std::sync::atomic::Ordering::Relaxed) {
        "media_rejected"
    } else if outcomes.iter().all(|ok| *ok) {
        "delivered"
    } else {
        "retry"
    }
}

/// One text post per route saying how many images the cooldown swallowed.
/// Best effort: a failed notice is not retried.
async fn post_elision_notice(state: &HubState, http: &reqwest::Client, id: i64, count: u64) {
    let (Some(checker), Ok(Some(device)), Ok(routes)) = (
        &state.guild_checker,
        state.db.get_device(id),
        state.db.device_channels(id),
    ) else {
        return;
    };
    if state.config.discord.bot_token.is_empty() {
        return;
    }
    let images = if count == 1 { "image" } else { "images" };
    let payload = json!({"allowed_mentions":{"parse":[]},"embeds":[{
        "title":format!("{} · {count} {images} skipped",device.name),
        "description":format!("Pier cameras post at most one image per minute. {count} {images} arrived too soon after the last post and were not sent."),
        "footer":{"text":"AutoPierCam · automated camera observation"},
    }]});
    for route in routes {
        let (Ok(guild), Ok(channel)) = (
            super::discord_api::parse_snowflake(&route.guild_id),
            super::discord_api::parse_snowflake(&route.channel_id),
        ) else {
            continue;
        };
        if !state
            .device_connections
            .1
            .lock()
            .is_ok_and(|mut limits| limits.allows(channel))
            || !checker.bot_in_guild(guild as u64).await
            || !checker.channel_in_guild(channel as u64, guild as u64).await
        {
            continue;
        }
        let result = http
            .post(format!(
                "{}/api/v10/channels/{}/messages",
                state.config.discord.base_url.trim_end_matches('/'),
                channel as u64
            ))
            .header(
                "authorization",
                format!("Bot {}", state.config.discord.bot_token),
            )
            .json(&payload)
            .send()
            .await;
        if let Ok(response) = result
            && let Ok(mut limits) = state.device_connections.1.lock()
        {
            limits.observe(channel, response.status(), response.headers());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        config::HubConfig,
        devices::DeviceKind,
        guild_check::{GuildChecker, NamedId},
        store::UserRow,
    };
    use super::*;
    use axum::{Router, body::Bytes, extract::Path, http::StatusCode, routing::post};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as ClientFrame};

    #[derive(Default)]
    struct Posts {
        attempts: HashMap<i64, usize>,
        fail: Option<i64>,
        bodies: Vec<String>,
    }
    struct Checker;
    #[async_trait::async_trait]
    impl GuildChecker for Checker {
        async fn bot_in_guild(&self, _: u64) -> bool {
            true
        }
        async fn user_can_manage(&self, _: u64, _: u64) -> bool {
            true
        }
        async fn channel_in_guild(&self, _: u64, _: u64) -> bool {
            true
        }
        async fn guild_channels(&self, _: u64) -> Vec<NamedId> {
            vec![]
        }
        async fn guild_roles(&self, _: u64) -> Vec<NamedId> {
            vec![]
        }
    }
    struct Harness {
        state: HubState,
        base: String,
        id: i64,
        installation: Uuid,
        credential: String,
        posts: Arc<Mutex<Posts>>,
        cookie: String,
        csrf: String,
    }
    async fn harness() -> Harness {
        let posts = Arc::new(Mutex::new(Posts::default()));
        async fn post_image(
            State(posts): State<Arc<Mutex<Posts>>>,
            Path(channel): Path<i64>,
            body: Bytes,
        ) -> StatusCode {
            let mut posts = posts.lock().unwrap();
            *posts.attempts.entry(channel).or_default() += 1;
            posts
                .bodies
                .push(String::from_utf8_lossy(&body).into_owned());
            if posts.fail == Some(channel) {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let discord = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/v10/channels/{channel}/messages", post(post_image))
            .with_state(posts.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&UserRow {
            discord_user_id: 1,
            username: "Owner".into(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        db.register_guild(10, "Observatory", 1).unwrap();
        let device = db.create_device(1, "Pier", DeviceKind::PierCamera).unwrap();
        db.attach_device(device.id, 10, 1).unwrap();
        db.add_device_channel(device.id, 10, 100, "pier").unwrap();
        let installation = Uuid::new_v4();
        let token = db.issue_device_token(device.id).unwrap();
        let (_, credential) = db.pair_device(&token, installation).unwrap().unwrap();
        let mut config = HubConfig::default();
        config.discord.base_url = discord;
        config.discord.bot_token = "mock-token".into();
        config.session.signing_key = "test-signing-key-at-least-thirty-two-characters".into();
        let session = db.create_session(1, 1).unwrap();
        let cookie = format!(
            "{}={}",
            super::super::auth::SESSION_COOKIE,
            super::super::auth::signed_cookie_value(
                &config.session.signing_key,
                &session.session_id
            )
        );
        let state = HubState::build(config, db, Some(Arc::new(Checker))).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = super::super::server::router(state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Harness {
            state,
            base,
            id: device.id,
            installation,
            credential,
            posts,
            cookie,
            csrf: session.csrf_token,
        }
    }
    type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
    async fn next(client: &mut Client) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match client.next().await.unwrap().unwrap() {
                    ClientFrame::Text(t) => return serde_json::from_str(&t).unwrap(),
                    ClientFrame::Ping(bytes) => {
                        client.send(ClientFrame::Pong(bytes)).await.unwrap()
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }
    async fn connect(h: &Harness, snapshots: bool) -> Client {
        let (mut client, _) = tokio_tungstenite::connect_async(format!(
            "{}/v1/devices",
            h.base.replace("http:", "ws:")
        ))
        .await
        .unwrap();
        client.send(ClientFrame::Text(json!({"type":"authenticate","protocol_version":1,"installation_id":h.installation,"credential":h.credential,"snapshots":snapshots}).to_string().into())).await.unwrap();
        assert_eq!(next(&mut client).await["type"], "ready");
        client
    }
    fn event() -> CameraEvent {
        CameraEvent {
            event_id: Uuid::new_v4(),
            kind: EventKind::SceneChange,
            captured_at: chrono::Utc::now(),
            summary: "A scene change, not an object classification".into(),
            jpeg_base64: STANDARD.encode([0xff, 0xd8, 0xff, 0xd9]),
            request_id: None,
        }
    }

    fn trigger_rules() -> CameraTriggerRules {
        CameraTriggerRules {
            interval_minutes: 5,
            scene_changes: false,
            day_night: false,
            telescope_events: true,
            burst_count: 2,
            spacing_seconds: 60,
        }
    }

    #[tokio::test]
    async fn v2_configuration_round_trip_requires_capability_and_matching_ack() {
        let h = harness().await;
        let (mut client, _) = tokio_tungstenite::connect_async(format!(
            "{}/v1/devices",
            h.base.replace("http:", "ws:")
        ))
        .await
        .unwrap();
        client.send(ClientFrame::Text(json!({"type":"authenticate","protocol_version":2,"installation_id":h.installation,"credential":h.credential,"snapshots":false}).to_string().into())).await.unwrap();
        assert_eq!(next(&mut client).await["protocol_version"], 2);
        assert!(
            h.state
                .device_connections
                .configure(h.id, trigger_rules())
                .await
                .is_err()
        );
        client.send(ClientFrame::Text(json!({"type":"trigger_capabilities","chat_configuration":true,"telescope_events":true}).to_string().into())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if h.state.device_connections.0.lock().unwrap()[&h.id].chat_configuration {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let registry = h.state.device_connections.clone();
        let id = h.id;
        let result = tokio::spawn(async move { registry.configure(id, trigger_rules()).await });
        let command = next(&mut client).await;
        assert_eq!(command["type"], "configure_triggers");
        assert_eq!(
            command["rules"],
            serde_json::to_value(trigger_rules()).unwrap()
        );
        client.send(ClientFrame::Text(json!({"type":"trigger_configuration_result","request_id":Uuid::new_v4(),"accepted":true}).to_string().into())).await.unwrap();
        client.send(ClientFrame::Text(json!({"type":"trigger_configuration_result","request_id":command["request_id"],"accepted":false}).to_string().into())).await.unwrap();
        assert!(result.await.unwrap().unwrap_err().contains("declined"));
        assert!(h.posts.lock().unwrap().bodies.is_empty());
        let mut e = event();
        e.kind = EventKind::Periodic;
        assert_eq!(publish(&mut client, &e).await["status"], "delivered");
        let mut newer = event();
        newer.kind = EventKind::TelescopeEvent;
        assert_eq!(publish(&mut client, &newer).await["status"], "elided");
    }

    #[tokio::test]
    async fn telescope_triggers_require_live_consent_owner_and_shared_route() {
        let h = harness().await;
        let db = &h.state.db;
        let telescope = db.create_telescope(1, "Scope").unwrap();
        db.attach_telescope(telescope.id, 10, true, 1).unwrap();
        db.add_channel_route(telescope.id, 10, 100, "pier", "Observatory", 1)
            .unwrap();
        assert_eq!(db.trigger_devices(telescope.id).unwrap(), vec![h.id]);
        db.upsert_user(&UserRow {
            discord_user_id: 2,
            username: "Other".into(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        let other = db.create_telescope(2, "Other scope").unwrap();
        db.attach_telescope(other.id, 10, false, 2).unwrap();
        // Same channel is enough to read, not enough to trigger somebody else's camera.
        db.add_device_channel(h.id, 10, 102, "shared-other")
            .unwrap();
        db.add_channel_route(other.id, 10, 102, "shared-other", "Observatory", 2)
            .unwrap();
        assert!(db.trigger_devices(other.id).unwrap().is_empty());
        let unrelated = db.create_telescope(1, "Other channel").unwrap();
        db.attach_telescope(unrelated.id, 10, true, 1).unwrap();
        db.add_channel_route(unrelated.id, 10, 101, "other", "Observatory", 1)
            .unwrap();
        assert!(db.trigger_devices(unrelated.id).unwrap().is_empty());

        let (controls, mut received) = mpsc::channel(4);
        let registry = &h.state.device_connections;
        registry.0.lock().unwrap().insert(
            h.id,
            Connection {
                generation: Uuid::new_v4(),
                snapshots: false,
                requests: mpsc::channel(1).0,
                close: watch::channel(false).0,
                controls,
                chat_configuration: false,
                telescope_events: true,
                protocol_version: 2,
                timelapses: false,
                observation_context: false,
            },
        );
        let mut event = crate::events::Event {
            time: chrono::Utc::now().to_rfc3339(),
            event: crate::events::event_types::MOUNT_SLEW_STARTED.into(),
            chat_enabled: true,
            details: None,
        };
        registry.telescope_event(db, telescope.id, &event);
        assert!(matches!(
            received.try_recv(),
            Ok(ControlRequest::Event {
                event: "mount_slew_started",
                ..
            })
        ));
        event.chat_enabled = false;
        registry.telescope_event(db, telescope.id, &event);
        event.chat_enabled = true;
        event.time = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
        registry.telescope_event(db, telescope.id, &event);
        event.time = chrono::Utc::now().to_rfc3339();
        registry
            .0
            .lock()
            .unwrap()
            .get_mut(&h.id)
            .unwrap()
            .telescope_events = false;
        registry.telescope_event(db, telescope.id, &event);
        assert!(received.try_recv().is_err());
        assert!(h.posts.lock().unwrap().bodies.is_empty());
    }

    async fn connect_triggers(h: &Harness, version: u32) -> Client {
        let (mut client, _) = tokio_tungstenite::connect_async(format!(
            "{}/v1/devices",
            h.base.replace("http:", "ws:")
        ))
        .await
        .unwrap();
        client
            .send(ClientFrame::Text(
                json!({"type":"authenticate","protocol_version":version,
            "installation_id":h.installation,"credential":h.credential,"snapshots":false})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(next(&mut client).await["protocol_version"], version);
        client
            .send(ClientFrame::Text(
                json!({"type":"trigger_capabilities",
            "chat_configuration":false,"telescope_events":true})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if h.state.device_connections.0.lock().unwrap()[&h.id].telescope_events {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client
    }

    #[tokio::test]
    async fn v4_state_is_opt_in_live_routed_and_does_not_trigger_image_posts() {
        use crate::direct::protocol::{DirectMessage, QueryKind, QueryResult};
        let h = harness().await;
        let telescope = routed_telescope(&h);
        let (rig, mut queries) =
            super::super::direct_server::RigConnection::stub(telescope, Uuid::new_v4());
        h.state.rig_connections.insert(rig.clone());
        tokio::spawn(async move {
            while let Some(DirectMessage::Query(request)) = queries.recv().await {
                assert_eq!(request.kind, QueryKind::ObservationState);
                let now = chrono::Utc::now();
                rig.resolve(QueryResult {id:request.id,ok:true,error:None,error_code:None,payload:json!({
                    "state_epoch":"6a9a7146-60eb-47ea-abec-43ad43af71e1","revision":42,
                    "observed_at":now,"available":true,
                    "target":{"run_id":"2d3768d4-3bf2-4a6e-a568-776aa6a22a39","name":"NGC 7023","started_at":now-chrono::Duration::hours(1)},
                    "temperatures":{"focuser":{"celsius":21.3,"observed_at":now,"valid_until":now+chrono::Duration::minutes(2)},"external":null},
                    "completed_exposures":{"count":505,"image_type":"LIGHT","basis":"successfully_saved","as_of":now}
                })});
            }
        });
        let mut client = connect_triggers(&h, 4).await;
        // v4 alone never grants overlay telemetry consent.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), next(&mut client))
                .await
                .is_err()
        );
        client
            .send(ClientFrame::Text(
                json!({"type":"observation_capabilities","telemetry":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert_eq!(next(&mut client).await["type"], "observation_ready");
        let state = next(&mut client).await;
        assert_eq!(state["type"], "observation_state");
        assert_eq!(state["target"]["name"], "NGC 7023");
        assert_eq!(state["completed_exposures"]["count"], 505);
        assert_eq!(state["temperatures"]["focuser"]["celsius"], 21.3);
        assert!(state["temperatures"]["external"].is_null());
        assert!(h.posts.lock().unwrap().bodies.is_empty());
        client.close(None).await.unwrap();
        // A reconnect gets state, never a telescope_event or image trigger replay.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut client = connect_triggers(&h, 4).await;
        client
            .send(ClientFrame::Text(
                json!({"type":"observation_capabilities","telemetry":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert_eq!(next(&mut client).await["type"], "observation_ready");
        let fresh = next(&mut client).await;
        assert_eq!(fresh["state_epoch"], state["state_epoch"]);
        assert_eq!(fresh["completed_exposures"]["count"], 505);
        // The exact guild/channel relationship is a second trust boundary.
        h.state
            .db
            .with_conn(|c| {
                c.execute(
                    "DELETE FROM telescope_channels WHERE telescope_id=?1",
                    [telescope],
                )
                .map(|_| ())
            })
            .unwrap();
        let clear = tokio::time::timeout(Duration::from_secs(7), next(&mut client))
            .await
            .unwrap();
        assert_eq!(clear["available"], false);
        assert!(clear["target"].is_null() && clear["completed_exposures"].is_null());
        assert!(h.posts.lock().unwrap().bodies.is_empty());
    }

    fn routed_telescope(h: &Harness) -> i64 {
        let telescope = h.state.db.create_telescope(1, "Scope").unwrap();
        h.state
            .db
            .attach_telescope(telescope.id, 10, true, 1)
            .unwrap();
        h.state
            .db
            .add_channel_route(telescope.id, 10, 100, "pier", "Observatory", 1)
            .unwrap();
        telescope.id
    }

    #[tokio::test]
    async fn v2_telescope_events_keep_the_exact_legacy_wire_shape() {
        let h = harness().await;
        let telescope = routed_telescope(&h);
        let mut client = connect_triggers(&h, 2).await;
        let event = crate::events::Event {
            time: chrono::Utc::now().to_rfc3339(),
            event: crate::events::event_types::MOUNT_SLEW_STARTED.into(),
            chat_enabled: true,
            details: None,
        };
        h.state.device_connections.telescope_event_with_target(
            &h.state.db,
            telescope,
            &event,
            Some("NGC 7023"),
        );
        let wire = next(&mut client).await;
        assert_eq!(wire.as_object().unwrap().len(), 3);
        assert_eq!(wire["type"], "telescope_event");
        assert_eq!(wire["event"], "mount_slew_started");
        assert!(wire.get("context").is_none());
        // New actions and target-only observations never enter a v2 queue.
        h.state.device_connections.telescope_target_changed(
            &h.state.db,
            telescope,
            "Private to v3",
        );
        h.state.device_connections.telescope_action(
            &h.state.db,
            telescope,
            "safety_wait_started",
            Some("Private to v3"),
        );
        let mut changed = event;
        changed.event = crate::events::event_types::AUTOFOCUS_STARTING.into();
        h.state.device_connections.telescope_event_with_target(
            &h.state.db,
            telescope,
            &changed,
            Some("Private to v3"),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), next(&mut client))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn v3_telescope_observations_are_bounded_display_only_snapshots() {
        let h = harness().await;
        let telescope = routed_telescope(&h);
        let mut client = connect_triggers(&h, 3).await;
        let mut event: crate::events::Event = serde_json::from_value(json!({
            "Time":chrono::Utc::now().to_rfc3339(), "Event":"TS-TARGETSTART",
            "TargetName":"NGC 7023", "ProjectName":"private project",
            "Coordinates":{"RA":1.0,"Dec":2.0}, "Rotation":42.0
        }))
        .unwrap();
        h.state.device_connections.telescope_event_with_target(
            &h.state.db,
            telescope,
            &event,
            Some("NGC 7023"),
        );
        let wire = next(&mut client).await;
        assert_eq!(wire["event"], "target_changed");
        let context = &wire["context"];
        assert_eq!(context.as_object().unwrap().len(), 4);
        assert_eq!(context["telescope_id"], telescope);
        assert_eq!(context["telescope_name"], "Scope");
        assert_eq!(context["target_name"], "NGC 7023");
        assert!(context["occurred_at"].as_str().unwrap().ends_with("+00:00"));
        assert!(!wire.to_string().contains("private project"));
        assert!(!wire.to_string().contains("Coordinates"));

        event.event = crate::events::event_types::AUTOFOCUS_STARTING.into();
        h.state.device_connections.telescope_event_with_target(
            &h.state.db,
            telescope,
            &event,
            Some("Sequential Instruction Set"),
        );
        let wire = next(&mut client).await;
        assert_eq!(wire["event"], "autofocus_started");
        assert!(wire["context"]["target_name"].is_null());

        h.state.device_connections.telescope_action(
            &h.state.db,
            telescope,
            "safety_wait_ended",
            Some(&format!("\n{}\u{202e}", "星".repeat(200))),
        );
        let wire = next(&mut client).await;
        assert_eq!(wire["event"], "safety_wait_ended");
        assert_eq!(
            wire["context"]["target_name"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            120
        );
        assert!(
            !wire["context"]["target_name"]
                .as_str()
                .unwrap()
                .chars()
                .any(char::is_control)
        );

        for time in [
            chrono::Utc::now() - chrono::Duration::seconds(31),
            chrono::Utc::now() + chrono::Duration::seconds(1),
        ] {
            event.time = time.to_rfc3339();
            h.state.device_connections.telescope_event_with_target(
                &h.state.db,
                telescope,
                &event,
                Some("Do not replay"),
            );
        }
        h.state.device_connections.telescope_action(
            &h.state.db,
            telescope,
            "arbitrary_private_operation_started",
            Some("Do not send"),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), next(&mut client))
                .await
                .is_err()
        );
    }

    #[test]
    fn observation_actions_have_fixed_kinds_and_truthful_phases() {
        assert!(context_action_allowed("camera_cooling_started"));
        assert!(context_action_allowed("mount_center_completed"));
        assert!(context_action_allowed("safety_wait_ended"));
        assert!(context_action_allowed("plate_solve_failed"));
        assert!(!context_action_allowed("safety_wait_success"));
        assert!(!context_action_allowed("log_message_started"));
        assert!(!context_action_allowed("camera_cooling_progress"));
        assert_eq!(
            observation_label(" \nNGC 7023\u{202e} ", 120).as_deref(),
            Some("NGC 7023")
        );
    }

    fn mp4_fixture() -> Vec<u8> {
        b"\0\0\0\x18ftypmp42\0\0\0\0isommp42\0\0\0\x09moovx\0\0\0\x09mdatx".to_vec()
    }

    #[test]
    fn mp4_envelope_checks_are_bounded_and_do_not_trust_extensions() {
        assert!(valid_mp4(&mp4_fixture()));
        for invalid in [
            Vec::new(),
            b"not an mp4".to_vec(),
            b"\xff\xff\xff\xffftyp".to_vec(),
            b"\0\0\0\x08ftyp".to_vec(),
            b"\0\0\0\x01ftyp".to_vec(),
        ] {
            assert!(!valid_mp4(&invalid));
        }
        let mut truncated = mp4_fixture();
        truncated.pop();
        assert!(!valid_mp4(&truncated));
    }

    fn clip_request(h: &Harness, event_id: Uuid, body: axum::body::Body) -> Request {
        Request::builder()
            .method("POST")
            .uri("/v1/devices/timelapse")
            .header("authorization", format!("Bearer {}", h.credential))
            .header("x-installation-id", h.installation.to_string())
            .header("x-event-id", event_id.to_string())
            .header("x-captured-at", chrono::Utc::now().to_rfc3339())
            .header("content-type", "video/mp4")
            .header("content-length", mp4_fixture().len())
            .body(body)
            .unwrap()
    }

    async fn enable_timelapses(h: &Harness) -> Client {
        let mut client = connect_triggers(h, 3).await;
        client
            .send(ClientFrame::Text(
                json!({"type":"media_capabilities","timelapses":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let ready = next(&mut client).await;
        assert_eq!(ready["type"], "media_ready");
        assert_eq!(ready["max_timelapse_bytes"], MAX_TIMELAPSE_BYTES);
        client
    }

    #[tokio::test]
    async fn timelapses_require_live_opt_in_and_post_mp4_with_immutable_receipts() {
        let h = harness().await;
        let event_id = Uuid::new_v4();
        let body = || axum::body::Body::from(mp4_fixture());
        let denied =
            timelapse_upload(State(h.state.clone()), clip_request(&h, event_id, body())).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let mut client = enable_timelapses(&h).await;
        let timestamp = chrono::Utc::now().to_rfc3339();
        for _ in 0..2 {
            let mut request = clip_request(&h, event_id, body());
            request
                .headers_mut()
                .insert("x-captured-at", timestamp.parse().unwrap());
            let response = timelapse_upload(State(h.state.clone()), request).await;
            assert_eq!(response.status(), StatusCode::OK);
            let response: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(response["status"], "delivered");
        }
        let mut altered = mp4_fixture();
        *altered.last_mut().unwrap() = b'y';
        let mut request = clip_request(&h, event_id, axum::body::Body::from(altered));
        request
            .headers_mut()
            .insert("x-captured-at", timestamp.parse().unwrap());
        let response = timelapse_upload(State(h.state.clone()), request).await;
        let response: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(response["status"], "event_conflict");
        {
            let posts = h.posts.lock().unwrap();
            assert_eq!(posts.bodies.len(), 1);
            assert!(posts.bodies[0].contains("filename=\"timelapse.mp4\""));
            assert!(posts.bodies[0].contains("video/mp4"));
            assert!(!posts.bodies[0].contains("piercam.jpg"));
        }
        client
            .send(ClientFrame::Ping(vec![1, 2, 3].into()))
            .await
            .unwrap();
        // Unknown/oversized media is refused without changing successful receipts.
        let invalid = timelapse_upload(
            State(h.state.clone()),
            clip_request(
                &h,
                Uuid::new_v4(),
                axum::body::Body::from(vec![0; mp4_fixture().len()]),
            ),
        )
        .await;
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let mut oversized = clip_request(&h, Uuid::new_v4(), axum::body::Body::empty());
        oversized.headers_mut().insert(
            "content-length",
            (MAX_TIMELAPSE_BYTES + 1).to_string().parse().unwrap(),
        );
        assert_eq!(
            timelapse_upload(State(h.state.clone()), oversized)
                .await
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(h.state.device_connections.3.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn revoking_camera_cancels_a_stalled_binary_upload_and_releases_capacity() {
        let h = harness().await;
        let _client = enable_timelapses(&h).await;
        let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let request = clip_request(&h, Uuid::new_v4(), axum::body::Body::from_stream(pending));
        let state = h.state.clone();
        let upload = tokio::spawn(timelapse_upload(State(state), request));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !h.state.device_connections.3.lock().unwrap().contains(&h.id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        h.state.device_connections.disconnect(h.id);
        let response = tokio::time::timeout(Duration::from_secs(2), upload)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(h.state.device_connections.3.lock().unwrap().is_empty());
        assert!(h.posts.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn timelapse_http_route_accepts_binary_bodies_above_management_json_limit() {
        let h = harness().await;
        let _client = enable_timelapses(&h).await;
        let mut movie = mp4_fixture();
        movie[33..37].copy_from_slice(&5009_u32.to_be_bytes());
        movie.extend_from_slice(&vec![0; 5000]);
        assert!(valid_mp4(&movie));
        let response = reqwest::Client::new()
            .post(format!("{}/v1/devices/timelapse", h.base))
            .bearer_auth(&h.credential)
            .header("x-installation-id", h.installation.to_string())
            .header("x-event-id", Uuid::new_v4().to_string())
            .header("x-captured-at", chrono::Utc::now().to_rfc3339())
            .header("content-type", "video/mp4")
            .body(movie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["status"],
            "delivered"
        );
        assert_eq!(h.posts.lock().unwrap().bodies.len(), 1);
    }

    async fn publish(client: &mut Client, event: &CameraEvent) -> serde_json::Value {
        client
            .send(ClientFrame::Text(
                json!({"type":"event","event":event}).to_string().into(),
            ))
            .await
            .unwrap();
        next(client).await
    }

    #[tokio::test]
    async fn images_inside_the_cooldown_are_swallowed_and_reported_once() {
        let h = harness().await;
        let mut client = connect(&h, true).await;
        assert_eq!(publish(&mut client, &event()).await["status"], "delivered");
        for _ in 0..2 {
            let ack = publish(&mut client, &event()).await;
            // Terminal: the camera must not retry a swallowed image.
            assert_eq!(
                (ack["status"].as_str(), ack["retry_after_seconds"].as_i64()),
                (Some("elided"), Some(0))
            );
        }
        assert_eq!(h.posts.lock().unwrap().attempts[&100], 1);
        let count = h.state.device_connections.take_elided(h.id);
        assert_eq!(count, 2);
        assert_eq!(h.state.device_connections.take_elided(h.id), 0);
        post_elision_notice(&h.state, &reqwest::Client::new(), h.id, count).await;
        let posts = h.posts.lock().unwrap();
        assert_eq!(posts.attempts[&100], 2);
        let notice = posts.bodies.last().unwrap();
        assert!(notice.contains("Pier · 2 images skipped"), "{notice}");
        assert!(!notice.contains("piercam.jpg"));
    }

    #[tokio::test]
    async fn events_deliver_dedupe_and_retry_only_failed_destinations() {
        let h = harness().await;
        h.state
            .db
            .add_device_channel(h.id, 10, 200, "second")
            .unwrap();
        h.posts.lock().unwrap().fail = Some(200);
        let mut client = connect(&h, true).await;
        let e = event();
        assert_eq!(publish(&mut client, &e).await["status"], "retry");
        h.posts.lock().unwrap().fail = None;
        assert_eq!(publish(&mut client, &e).await["status"], "delivered");
        assert_eq!(publish(&mut client, &e).await["status"], "delivered");
        let posts = h.posts.lock().unwrap();
        assert_eq!(posts.attempts[&100], 1);
        assert_eq!(posts.attempts[&200], 2);
        assert!(posts.bodies[0].contains("\"allowed_mentions\":{\"parse\":[]}"));
        assert!(posts.bodies[0].contains("filename=\"piercam.jpg\""));
        assert!(!posts.bodies[0].contains(&h.credential));
    }

    #[tokio::test]
    async fn snapshot_round_trip_is_owner_authorized_and_locally_consented() {
        let h = harness().await;
        let mut client = connect(&h, true).await;
        let url = format!("{}/api/devices/{}/snapshot", h.base, h.id);
        assert_eq!(
            reqwest::Client::new()
                .post(&url)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let request = reqwest::Client::new()
            .post(url)
            .header("cookie", &h.cookie)
            .header("x-csrf-token", &h.csrf);
        let result = tokio::spawn(async move { request.send().await.unwrap() });
        let command = next(&mut client).await;
        assert_eq!(command["type"], "snapshot_request");
        let mut e = event();
        e.kind = EventKind::Snapshot;
        e.request_id = Some(serde_json::from_value(command["request_id"].clone()).unwrap());
        assert_eq!(publish(&mut client, &e).await["status"], "delivered");
        assert_eq!(result.await.unwrap().status(), 200);
        assert_eq!(publish(&mut client, &e).await["status"], "invalid_request");
        assert_eq!(h.posts.lock().unwrap().attempts[&100], 1);
    }

    #[tokio::test]
    async fn snapshot_disabled_and_camera_denial_do_not_post() {
        let h = harness().await;
        let mut client = connect(&h, false).await;
        assert_eq!(
            h.state.device_connections.snapshot(h.id).await,
            Err("snapshot sharing is disabled at the camera")
        );
        let mut e = event();
        e.kind = EventKind::Snapshot;
        e.request_id = Some(Uuid::new_v4());
        assert_eq!(publish(&mut client, &e).await["status"], "invalid_request");
        assert!(h.posts.lock().unwrap().attempts.is_empty());
        let h = harness().await;
        let mut client = connect(&h, true).await;
        let registry = h.state.device_connections.clone();
        let id = h.id;
        let pending = tokio::spawn(async move { registry.snapshot(id).await });
        let request = next(&mut client).await;
        client
            .send(ClientFrame::Text(
                json!({"type":"snapshot_unavailable","request_id":request["request_id"]})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(pending.await.unwrap().is_err());
        assert!(h.posts.lock().unwrap().attempts.is_empty());
    }

    #[tokio::test]
    async fn invalid_payloads_conflicts_cooldowns_and_revocation() {
        let h = harness().await;
        let mut client = connect(&h, true).await;
        let mut e = event();
        e.jpeg_base64 = "https://private.example/image.jpg".into();
        assert_eq!(publish(&mut client, &e).await["status"], "invalid_image");
        let mut e = event();
        e.captured_at -= chrono::Duration::minutes(6);
        assert_eq!(publish(&mut client, &e).await["status"], "invalid_event");
        let mut e = event();
        assert_eq!(publish(&mut client, &e).await["status"], "delivered");
        e.summary = "Different image identity".into();
        assert_eq!(publish(&mut client, &e).await["status"], "event_conflict");
        assert_eq!(publish(&mut client, &event()).await["status"], "elided");
        h.state.db.revoke_device(h.id).unwrap();
        h.state.device_connections.disconnect(h.id);
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(Ok(frame)) = client.next().await {
                if matches!(frame, ClientFrame::Close(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(h.posts.lock().unwrap().attempts[&100], 1);
    }

    #[tokio::test]
    async fn retry_does_not_backfill_new_or_recreated_routes() {
        let h = harness().await;
        h.posts.lock().unwrap().fail = Some(100);
        let http = reqwest::Client::new();
        let e = event();
        assert_eq!(deliver(&h.state, &http, h.id, &e).await, "retry");
        let route = h.state.db.device_channels(h.id).unwrap()[0].id;
        h.state.db.delete_device_channel(h.id, route).unwrap();
        h.state
            .db
            .add_device_channel(h.id, 10, 100, "recreated")
            .unwrap();
        h.state.db.add_device_channel(h.id, 10, 200, "new").unwrap();
        h.posts.lock().unwrap().fail = None;
        assert_eq!(deliver(&h.state, &http, h.id, &e).await, "no_destinations");
        assert_eq!(h.posts.lock().unwrap().attempts[&100], 1);
        assert!(!h.posts.lock().unwrap().attempts.contains_key(&200));
    }

    #[test]
    fn jpeg_size_and_unknown_protocol_commands_are_rejected() {
        let mut e = event();
        e.jpeg_base64 = STANDARD.encode(vec![0; MAX_JPEG + 1]);
        assert!(e.jpeg().is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"slew","ra":1}"#).is_err());
        let mut json = serde_json::to_value(event()).unwrap();
        json["channel_id"] = json!("123");
        assert!(serde_json::from_value::<CameraEvent>(json).is_err());
    }

    #[tokio::test]
    async fn receipts_survive_reopen_and_do_not_store_images() {
        let h = harness().await;
        let path = std::env::temp_dir().join(format!("chatstronomy-device-{}.db", Uuid::new_v4()));
        // Copy the fixture through SQLite's VACUUM INTO, then use a disk-backed DB.
        h.state
            .db
            .with_conn(|c| {
                c.execute("VACUUM INTO ?1", [path.to_str().unwrap()])
                    .map(|_| ())
            })
            .unwrap();
        let e = event();
        {
            let db = Db::open(&path).unwrap();
            assert!(matches!(
                db.reserve_device_event(h.id, &e).unwrap(),
                Reservation::Ready
            ));
            let route = db.pending_device_targets(h.id, e.event_id).unwrap()[0].0;
            db.device_delivered(h.id, e.event_id, route).unwrap();
        }
        {
            let db = Db::open(&path).unwrap();
            assert!(matches!(
                db.reserve_device_event(h.id, &e).unwrap(),
                Reservation::Ready
            ));
            assert!(
                db.pending_device_targets(h.id, e.event_id)
                    .unwrap()
                    .is_empty()
            );
            let hash = db
                .with_conn(|c| {
                    c.query_row("SELECT payload_hash FROM device_events", [], |r| {
                        r.get::<_, String>(0)
                    })
                })
                .unwrap();
            assert_ne!(hash, e.jpeg_base64);
            assert_eq!(hash.len(), 43);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn credentials_cannot_cross_installations_and_only_one_session_is_active() {
        let h = harness().await;
        let mut active = connect(&h, true).await;
        let url = format!("{}/v1/devices", h.base.replace("http:", "ws:"));
        let (mut duplicate, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        duplicate.send(ClientFrame::Text(json!({"type":"authenticate","protocol_version":1,"installation_id":h.installation,"credential":h.credential,"snapshots":true}).to_string().into())).await.unwrap();
        assert_eq!(next(&mut duplicate).await["code"], "already_connected");
        let (mut wrong, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        wrong.send(ClientFrame::Text(json!({"type":"authenticate","protocol_version":1,"installation_id":Uuid::new_v4(),"credential":h.credential,"snapshots":true}).to_string().into())).await.unwrap();
        assert_eq!(next(&mut wrong).await["code"], "authentication_failed");
        assert_eq!(publish(&mut active, &event()).await["status"], "delivered");
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_timeout_is_bounded() {
        let registry = DeviceConnections::default();
        let (requests, _receiver) = mpsc::channel(1);
        let (close, _closed) = watch::channel(false);
        registry.0.lock().unwrap().insert(
            1,
            Connection {
                generation: Uuid::new_v4(),
                snapshots: true,
                requests,
                close,
                controls: mpsc::channel(1).0,
                chat_configuration: false,
                telescope_events: false,
                protocol_version: 1,
                timelapses: false,
                observation_context: false,
            },
        );
        assert_eq!(registry.snapshot(1).await, Err("snapshot timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn discord_backoff_honors_retry_after_and_global_scope() {
        let mut limits = DiscordBackoff::default();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "65.5".parse().unwrap());
        limits.observe(100, reqwest::StatusCode::TOO_MANY_REQUESTS, &headers);
        assert!(!limits.allows(100));
        assert!(limits.allows(200));
        tokio::time::advance(Duration::from_secs(65)).await;
        assert!(!limits.allows(100));
        tokio::time::advance(Duration::from_millis(501)).await;
        assert!(limits.allows(100));
        headers.insert("x-ratelimit-global", "true".parse().unwrap());
        limits.observe(100, reqwest::StatusCode::TOO_MANY_REQUESTS, &headers);
        assert!(!limits.allows(200));
        tokio::time::advance(Duration::from_secs(66)).await;
        assert!(limits.allows(200));
        headers.clear();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "2.5".parse().unwrap());
        limits.observe(100, reqwest::StatusCode::OK, &headers);
        assert!(!limits.allows(100));
        assert!(limits.allows(200));
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(limits.allows(100));
        headers.clear();
        headers.insert("retry-after", "NaN".parse().unwrap());
        limits.observe(100, reqwest::StatusCode::TOO_MANY_REQUESTS, &headers);
        assert!(!limits.allows(100));
    }
}
