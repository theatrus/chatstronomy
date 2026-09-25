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
        .route("/api/devices/{id}/credentials", delete(revoke))
        .route("/api/devices/{id}/channels", post(add_channel))
        .route("/api/devices/{id}/channels/{route}", delete(remove_channel))
        .route("/api/guilds/{guild}/devices", get(guild_devices))
        .route("/v1/devices/pair", post(pair))
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
            let mut value = json!(device);
            value["channels"] = json!(channels);
            devices.push(value);
        }
        Ok::<_, super::db::DbError>(devices)
    })();
    match result {
        Ok(devices) => Json(json!({"devices":devices})).into_response(),
        Err(e) => internal_error(e),
    }
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
        Ok(d) => Json(d).into_response(),
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
    if let Err(r) = owner(&state, &headers, id) {
        return r;
    }
    match state.db.delete_device(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(e),
    }
}

async fn issue_token(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = owner(&state, &headers, id) {
        return r;
    }
    match state.db.issue_device_token(id) {
        Ok(token) => (
            [("cache-control", "no-store")],
            Json(json!({"pairing_token":token,"expires_in_seconds":PAIRING_TTL})),
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

async fn revoke(
    State(state): State<HubState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = owner(&state, &headers, id) {
        return r;
    }
    match state.db.revoke_device(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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
        Ok(()) => {
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
    let result=state.db.with_conn(|c|c.prepare("SELECT dc.id, dc.device_id, d.name, dc.channel_name FROM device_channels dc JOIN devices d ON d.id=dc.device_id WHERE dc.guild_id=?1 ORDER BY dc.id")?.query_map([guild],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"device_id":r.get::<_,i64>(1)?,"name":r.get::<_,String>(2)?,"channel_name":r.get::<_,String>(3)?})))?.collect::<Result<Vec<_>,_>>());
    match result {
        Ok(routes) => Json(json!({"routes":routes})).into_response(),
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
        Ok(Some((id, credential))) => (
            [("cache-control", "no-store")],
            Json(
                json!({"protocol_version":PROTOCOL_VERSION,"device_id":id,"credential":credential}),
            ),
        )
            .into_response(),
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => internal_error(e),
    }
}
