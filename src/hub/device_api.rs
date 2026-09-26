//! Session-authorized management of feed-only devices and one-shot pairing.

use super::devices::{Device, DeviceKind, PAIRING_TTL, PROTOCOL_VERSION};
use super::server::{
    HubState, ManageAuth, authorize_manage, internal_error, require_session_with_csrf,
    session_from_headers,
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/api/devices", get(list).post(create))
        .route("/api/devices/{id}", delete(remove))
        .route("/api/devices/{id}/pairing-token", post(issue_token))
        .route("/api/devices/{id}/attach", post(attach))
        .route("/api/devices/{id}/attachments/{guild}", delete(detach))
        .route("/api/devices/{id}/credentials", delete(revoke))
        .route("/api/devices/{id}/channels", post(add_channel))
        .route("/api/devices/{id}/channels/{route}", delete(remove_channel))
        .route("/api/guilds/{guild}/devices", get(guild_devices))
        .route("/v1/devices/pair", post(pair))
        .route("/v1/devices", get(super::device_transport::upgrade))
        .route("/api/devices/{id}/snapshot", post(snapshot))
        .layer(DefaultBodyLimit::max(4096))
}

#[allow(clippy::result_large_err)]
fn owner(state: &HubState, headers: &HeaderMap, id: i64) -> Result<Device, Response> {
    let session = require_session_with_csrf(state, headers)
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let device = state
        .db
        .get_device(id)
        .map_err(internal_error)?
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if device.owner_id != session.discord_user_id {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    Ok(device)
}

async fn list(State(state): State<HubState>, headers: HeaderMap) -> Response {
    let Some(session) = session_from_headers(&state, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let result = (|| {
        let mut devices = Vec::new();
        for device in state.db.user_devices(session.discord_user_id)? {
            let channels = state.db.device_channels(device.id)?;
            let mut value = device_json(&state, &device);
            value["attachments"] = state
                .db
                .device_attachments(device.id)?
                .into_iter()
                .map(|a| {
                    let mut attachment = json!(a);
                    attachment["channels"] = json!(
                        channels
                            .iter()
                            .filter(|c| c.guild_id == a.guild_id)
                            .collect::<Vec<_>>()
                    );
                    attachment
                })
                .collect();
            devices.push(value);
        }
        Ok::<_, super::db::DbError>(devices)
    })();
    match result {
        Ok(devices) => Json(json!({"devices":devices})).into_response(),
        Err(e) => internal_error(e),
    }
}

fn device_json(state: &HubState, device: &Device) -> serde_json::Value {
    let mut value = json!(device);
    value["connected"] = json!(state.device_connections.connected(device.id));
    value["snapshots"] = json!(state.device_connections.snapshots_shared(device.id));
    value
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    kind: DeviceKind,
}

async fn create(
    State(state): State<HubState>,
    headers: HeaderMap,
    Json(body): Json<Create>,
) -> Response {
    let Some(session) = require_session_with_csrf(&state, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be 1–64 printable characters",
        )
            .into_response();
    }
    match state
        .db
        .create_device(session.discord_user_id, name, body.kind)
    {
        Ok(d) => {
            state
                .db
                .audit(session.discord_user_id, 0, "device_created", &d.name);
            Json(d).into_response()
        }
        Err(super::db::DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            (StatusCode::CONFLICT, "device name already exists").into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn remove(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let device = match owner(&state, &headers, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    state.device_connections.disconnect(id);
    match state.db.delete_device(id) {
        Ok(()) => {
            state
                .db
                .audit(device.owner_id, 0, "device_deleted", &device.name);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn issue_token(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let device = match owner(&state, &headers, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match state.db.issue_device_token(id) {
        Ok(token) => {
            state.db.audit(
                device.owner_id,
                0,
                "device_pairing_token_issued",
                &device.name,
            );
            (
                [("cache-control", "no-store")],
                Json(json!({"pairing_token":token,"expires_in_seconds":PAIRING_TTL})),
            )
                .into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn revoke(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let device = match owner(&state, &headers, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    state.device_connections.disconnect(id);
    match state.db.revoke_device(id) {
        Ok(()) => {
            state.db.audit(
                device.owner_id,
                0,
                "device_credentials_revoked",
                &device.name,
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    guild_id: String,
}

/// Same consent as attaching a telescope: the device owner who also
/// manages the target server.
async fn attach(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<Attach>,
) -> Response {
    let device = match owner(&state, &headers, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Ok(guild) = super::discord_api::parse_snowflake(&body.guild_id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let ManageAuth::Denied(r) = authorize_manage(&state, &headers, guild, true).await {
        return r;
    }
    match state.db.get_guild(guild) {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::BAD_REQUEST, "register the server first").into_response(),
        Err(e) => return internal_error(e),
    }
    match state.db.attach_device(id, guild, device.owner_id) {
        Ok(true) => {
            state
                .db
                .audit(device.owner_id, guild, "device_attached", &device.name);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            "this camera is already attached to that server",
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

/// Either side may sever, as with telescopes: the owner, or a manager of
/// the attached server. Removes that server's channel links too.
async fn detach(
    State(state): State<HubState>,
    Path((id, guild)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    let Ok(guild) = super::discord_api::parse_snowflake(&guild) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(session) = require_session_with_csrf(&state, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let device = match state.db.get_device(id) {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return internal_error(e),
    };
    if device.owner_id != session.discord_user_id
        && let ManageAuth::Denied(r) = authorize_manage(&state, &headers, guild, true).await
    {
        return r;
    }
    match state.db.detach_device(id, guild) {
        Ok(true) => {
            state.device_connections.disconnect(id);
            state.db.audit(
                session.discord_user_id,
                guild,
                "device_detached",
                &device.name,
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channel {
    guild_id: String,
    channel_id: String,
}

async fn add_channel(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<Channel>,
) -> Response {
    let device = match owner(&state, &headers, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match state.db.device_channels(id) {
        Ok(channels) if channels.len() >= 8 => {
            return (
                StatusCode::BAD_REQUEST,
                "at most eight destinations per device",
            )
                .into_response();
        }
        Err(e) => return internal_error(e),
        _ => {}
    }
    let (Ok(guild), Ok(channel)) = (
        super::discord_api::parse_snowflake(&body.guild_id),
        super::discord_api::parse_snowflake(&body.channel_id),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let ManageAuth::Denied(r) = authorize_manage(&state, &headers, guild, true).await {
        return r;
    }
    let Some(checker) = &state.guild_checker else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !checker.bot_in_guild(guild as u64).await
        || !checker.channel_in_guild(channel as u64, guild as u64).await
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let channels = checker.guild_channels(guild as u64).await;
    let Some(channel_info) = channels.iter().find(|c| c.id == channel as u64) else {
        return (StatusCode::BAD_REQUEST, "choose a text channel").into_response();
    };
    match state.db.get_guild(guild) {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::BAD_REQUEST, "register the server first").into_response(),
        Err(e) => return internal_error(e),
    }
    match state
        .db
        .add_device_channel(id, guild, channel, &channel_info.name)
    {
        Ok(false) => (
            StatusCode::BAD_REQUEST,
            "attach the camera to that server first",
        )
            .into_response(),
        Ok(true) => {
            state.db.audit(
                device.owner_id,
                guild,
                "device_destination_added",
                &format!("device {id}, channel {channel}"),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal_error(e),
    }
}

// Owners can stop sharing; server managers can remove an unwanted feed even
// when they do not own the device. Neither can alter the other's credentials.
async fn remove_channel(
    State(state): State<HubState>,
    Path((id, route)): Path<(i64, i64)>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = require_session_with_csrf(&state, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let device = match state.db.get_device(id) {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return internal_error(e),
    };
    let channels = match state.db.device_channels(id) {
        Ok(c) => c,
        Err(e) => return internal_error(e),
    };
    let Some(channel) = channels.iter().find(|c| c.id == route) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let guild = super::discord_api::parse_snowflake(&channel.guild_id).expect("stored snowflake");
    if device.owner_id != session.discord_user_id
        && let ManageAuth::Denied(r) = authorize_manage(&state, &headers, guild, true).await
    {
        return r;
    }
    state.device_connections.disconnect(id);
    match state.db.delete_device_channel(id, route) {
        Ok(()) => {
            state.db.audit(
                session.discord_user_id,
                guild,
                "device_destination_removed",
                &format!("device {id}, route {route}"),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn guild_devices(
    State(state): State<HubState>,
    Path(guild): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(guild) = super::discord_api::parse_snowflake(&guild) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let ManageAuth::Denied(r) = authorize_manage(&state, &headers, guild, false).await {
        return r;
    }
    let viewer = session_from_headers(&state, &headers)
        .map(|s| s.discord_user_id)
        .unwrap_or(0);
    let result = (|| {
        let mut devices = Vec::new();
        for (device, owner_name) in state.db.guild_devices(guild)? {
            let mut value = device_json(&state, &device);
            value["owner_name"] = json!(owner_name);
            value["owned_by_me"] = json!(device.owner_id == viewer);
            let guild_id = super::discord_api::snowflake_string(guild);
            value["channels"] = json!(
                state
                    .db
                    .device_channels(device.id)?
                    .into_iter()
                    .filter(|c| c.guild_id == guild_id)
                    .collect::<Vec<_>>()
            );
            devices.push(value);
        }
        Ok::<_, super::db::DbError>(devices)
    })();
    match result {
        Ok(devices) => Json(json!({"devices":devices})).into_response(),
        Err(e) => internal_error(e),
    }
}

// Never derive Debug on a wire type containing a secret.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    protocol_version: u32,
    kind: DeviceKind,
    installation_id: Uuid,
    pairing_token: String,
}

async fn pair(
    State(state): State<HubState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Pair>,
) -> Response {
    let ip = super::server::client_ip(&headers, peer, state.config.trust_x_forwarded_for);
    if !state.limits.direct_auth.check(&ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if body.protocol_version != PROTOCOL_VERSION
        || body.installation_id.is_nil()
        || body.pairing_token.len() > 256
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let DeviceKind::PierCamera = body.kind;
    match state
        .db
        .pair_device(&body.pairing_token, body.installation_id)
    {
        Ok(Some((id, credential))) => {
            state.device_connections.disconnect(id);
            (
            [("cache-control", "no-store")],
            Json(
                json!({"protocol_version":PROTOCOL_VERSION,"device_id":id,"credential":credential}),
            ),
        )
            .into_response()
        }
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => internal_error(e),
    }
}

async fn snapshot(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = owner(&state, &headers, id) {
        return r;
    }
    match state.db.device_channels(id) {
        Ok(channels) if channels.is_empty() => {
            return (StatusCode::CONFLICT, "choose a destination channel first").into_response();
        }
        Err(e) => return internal_error(e),
        _ => {}
    }
    match state.device_connections.snapshot(id).await {
        Ok(()) => Json(json!({"status":"delivered"})).into_response(),
        Err(reason) => (StatusCode::CONFLICT, reason).into_response(),
    }
}
