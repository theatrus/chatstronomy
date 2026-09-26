//! Outbound camera WebSockets: event delivery and locally consented snapshots.
//! No telescope queries, arbitrary URLs, or hardware commands exist here.

use super::{
    db::{Db, DbError, unix_now},
    devices::PROTOCOL_VERSION,
    server::HubState,
};
use axum::{
    extract::{
        ConnectInfo, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

const MAX_JPEG: usize = 512 * 1024;
const MAX_WIRE: usize = 720 * 1024;
const COOLDOWN: i64 = 60;
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
    },
}

#[derive(Default)]
pub struct DeviceConnections(Mutex<HashMap<i64, Connection>>, Mutex<DiscordBackoff>);

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
        use crate::events::event_types;
        if !event.chat_enabled {
            return;
        }
        let Ok(time) = chrono::DateTime::parse_from_rfc3339(&event.time) else {
            return;
        };
        if !(0..=30).contains(&(unix_now() - time.timestamp())) {
            return;
        }
        let name = match event.event.as_str() {
            event_types::MOUNT_SLEW_STARTED => "mount_slew_started",
            event_types::MOUNT_SLEWED => "mount_slewed",
            event_types::SEQUENCE_STARTING => "sequence_started",
            event_types::SEQUENCE_FINISHED => "sequence_finished",
            _ => return,
        };
        let Ok(ids) = db.trigger_devices(telescope_id) else {
            return;
        };
        if let Ok(connections) = self.0.lock() {
            for id in ids {
                if let Some(c) = connections.get(&id).filter(|c| c.telescope_events) {
                    let _ = c.controls.try_send(ControlRequest::Event {
                        event: name,
                        expires_at: unix_now() + 30,
                    });
                }
            }
        }
    }

    pub fn connected(&self, id: i64) -> bool {
        self.0.lock().is_ok_and(|c| c.contains_key(&id))
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
    TriggerCapabilities {
        chat_configuration: bool,
        telescope_events: bool,
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
    if ![PROTOCOL_VERSION, 2].contains(&protocol_version) {
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
            control=controls.recv()=>{
                let Some(control)=control else {return;};
                match control {
                    ControlRequest::Event {event, expires_at} => {
                        if expires_at>unix_now() && !send(socket,json!({"type":"telescope_event","event":event,"expires_at":expires_at})).await {return;}
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
                    ClientMessage::TriggerCapabilities {chat_configuration,telescope_events} => {
                        if protocol_version!=2 {return;}
                        if let Ok(mut connections)=state.device_connections.0.lock() && let Some(c)=connections.get_mut(&id) {
                            c.chat_configuration=chat_configuration; c.telescope_events=telescope_events;
                        }
                    }
                    ClientMessage::TriggerConfigurationResult {request_id,accepted} => {
                        if protocol_version!=2 {return;}
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
                        let authorized=if snapshot {
                            pending.as_ref().is_some_and(|p|Some(p.id)==event.request_id && p.deadline>tokio::time::Instant::now() && !p.reply.is_closed())
                        } else {event.request_id.is_none()};
                        let result=if !state.limits.device_events.check(&id.to_string()) {"rate_limited"}
                            else if authorized {tokio::time::timeout(Duration::from_secs(30),deliver(state,&http,id,&event)).await.unwrap_or("retry")} else {"invalid_request"};
                        if !send(socket,json!({"type":"event_ack","event_id":event.event_id,"status":result,"retry_after_seconds":if result=="retry" || result=="rate_limited" {60}else{0}})).await {return;}
                        if snapshot && authorized && result!="retry" {
                            let _=pending.take().expect("authorized snapshot").reply.send(if result=="delivered" {Ok(())} else {Err("snapshot was not delivered; check routes or cooldown")});
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
    RateLimited,
}
impl Db {
    fn trigger_devices(&self, telescope_id: i64) -> Result<Vec<i64>, DbError> {
        self.with_conn(|c| {
            c.prepare("SELECT DISTINCT d.id FROM devices d JOIN telescopes t ON t.owner_id=d.owner_id JOIN device_channels dc ON dc.device_id=d.id JOIN telescope_channels tc ON tc.telescope_id=t.id AND tc.channel_id=dc.channel_id AND tc.guild_id=dc.guild_id WHERE t.id=?1")?
                .query_map([telescope_id], |r|r.get(0))?.collect()
        })
    }
    fn reserve_device_event(&self, id: i64, event: &CameraEvent) -> Result<Reservation, DbError> {
        let hash =
            super::auth::sha256_b64url(&serde_json::to_string(event).expect("serializable event"));
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            tx.execute("DELETE FROM device_events WHERE received_at<?1",[unix_now()-7*86400])?;
            let old:Option<String>=tx.query_row("SELECT payload_hash FROM device_events WHERE device_id=?1 AND event_id=?2",params![id,event.event_id.to_string()],|r|r.get(0)).optional()?;
            if let Some(old)=old {return Ok(if old==hash {Reservation::Ready} else {Reservation::Conflict});}
            let latest:Option<i64>=tx.query_row("SELECT MAX(received_at) FROM device_events WHERE device_id=?1",[id],|r|r.get(0))?;
            if latest.is_some_and(|t|unix_now()-t<COOLDOWN) {return Ok(Reservation::RateLimited);}
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
    let jpeg = match event.jpeg() {
        Ok(j) => j,
        Err(e) => return e,
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
    match state.db.reserve_device_event(id, event) {
        Ok(Reservation::Ready) => {}
        Ok(Reservation::Conflict) => return "event_conflict",
        Ok(Reservation::RateLimited) => return "rate_limited",
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
    let outcomes=futures_util::future::join_all(targets.into_iter().map(|(route,guild,channel)| {
        let jpeg=&jpeg;
        let name=&device.name;
        async move {
            if !state.device_connections.1.lock().is_ok_and(|mut limits|limits.allows(channel)) {return false;}
            if !checker.bot_in_guild(guild as u64).await || !checker.channel_in_guild(channel as u64,guild as u64).await {return false;}
            let label=match event.kind {EventKind::SceneChange=>"Scene change",EventKind::DayNightTransition=>"Day/night transition",EventKind::Snapshot=>"Requested snapshot",EventKind::Periodic=>"Scheduled image",EventKind::TelescopeEvent=>"Telescope event"};
            let payload=json!({"allowed_mentions":{"parse":[]},"embeds":[{"title":format!("{name} · {label}"),"description":event.summary,"timestamp":event.captured_at.to_rfc3339(),"image":{"url":"attachment://piercam.jpg"},"footer":{"text":"AutoPierCam · automated camera observation"}}],"attachments":[{"id":0,"filename":"piercam.jpg"}]});
            let part=reqwest::multipart::Part::bytes(jpeg.clone()).file_name("piercam.jpg").mime_str("image/jpeg").expect("constant MIME");
            let form=reqwest::multipart::Form::new().text("payload_json",payload.to_string()).part("files[0]",part);
            let result=http.post(format!("{}/api/v10/channels/{}/messages",state.config.discord.base_url.trim_end_matches('/'),channel as u64))
                .header("authorization",format!("Bot {}",state.config.discord.bot_token)).multipart(form).send().await;
            let Ok(response)=result else {return false;};
            if let Ok(mut limits)=state.device_connections.1.lock() {limits.observe(channel,response.status(),response.headers());}
            if !response.status().is_success() {return false;}
            state.db.device_delivered(id,event.event_id,route).is_ok()
        }
    })).await;
    if outcomes.iter().all(|ok| *ok) {
        "delivered"
    } else {
        "retry"
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
        tokio::time::timeout(Duration::from_secs(5), async {
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
        assert_eq!(publish(&mut client, &newer).await["status"], "rate_limited");
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
        assert_eq!(
            publish(&mut client, &event()).await["status"],
            "rate_limited"
        );
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
