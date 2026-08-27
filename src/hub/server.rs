//! Axum web server for the hub: health, Discord OAuth login, and sessions.

use super::auth::{
    SESSION_COOKIE, cookie_from_header, pkce_challenge, pkce_verifier, sanitize_next_path,
    signed_cookie_value, verify_signed_cookie_value,
};
use super::config::HubConfig;
use super::db::{Db, DbError};
use super::discord_api::{DiscordOauthClient, parse_snowflake, snowflake_string};
use super::guild_check::{CachedGuildChecker, GuildChecker, SerenityGuildChecker};
use super::store::{GuildSnapshot, SessionRow, UserRow};
use super::tenants::TelescopeRow;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::{Arc, Mutex, Weak};

fn discord_bot_install_permissions() -> poise::serenity_prelude::Permissions {
    use poise::serenity_prelude::Permissions;
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::ATTACH_FILES
}

fn discord_bot_install_url(base_url: &str, client_id: &str, guild_id: i64) -> Option<String> {
    let mut url = url::Url::parse(&format!(
        "{}/oauth2/authorize",
        base_url.trim_end_matches('/')
    ))
    .ok()?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("scope", "bot applications.commands")
        .append_pair("guild_id", &snowflake_string(guild_id))
        .append_pair(
            "permissions",
            &discord_bot_install_permissions().bits().to_string(),
        );
    Some(url.into())
}

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("hub database error: {0}")]
    Db(#[from] DbError),
    #[error("hub I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("hub Discord client error: {0}")]
    Discord(#[from] super::discord_api::DiscordApiError),
    #[error("hub chat error: {0}")]
    Chat(#[from] crate::error::ChatError),
}

/// Shared state handed to every request handler.
#[derive(Clone)]
pub struct HubState {
    pub db: Db,
    pub config: Arc<HubConfig>,
    /// Present once the Discord application credentials are configured.
    pub oauth: Option<Arc<DiscordOauthClient>>,
    /// Present once a bot token is configured; backs live guild checks.
    pub guild_checker: Option<Arc<dyn GuildChecker>>,
    /// Live rig connections from `/v1/direct`.
    pub rig_connections: Arc<super::direct_server::RigConnections>,
    /// Registered when the central chat service is running. Route and trust
    /// removals use the weak handle to cancel stale pending deliveries before
    /// their HTTP mutation returns.
    updater_manager: Arc<Mutex<Option<Weak<super::updaters::UpdaterManager>>>>,
    /// Per-IP rate limits for abuse-prone endpoints.
    pub limits: Arc<HubLimits>,
}

impl HubState {
    /// Build the shared state, wiring the OAuth client when configured.
    /// serve() and the test harness both use this so their wiring cannot
    /// drift apart.
    pub fn build(
        config: HubConfig,
        db: Db,
        guild_checker: Option<Arc<dyn GuildChecker>>,
    ) -> Result<Self, HubError> {
        let oauth = if config.oauth_configured() {
            Some(Arc::new(DiscordOauthClient::new(
                &config.discord.base_url,
                &config.discord.client_id,
                &config.discord.client_secret,
                &config.public_base_url,
            )?))
        } else {
            None
        };
        Ok(Self {
            db,
            config: Arc::new(config),
            oauth,
            guild_checker,
            rig_connections: Arc::new(super::direct_server::RigConnections::default()),
            updater_manager: Arc::new(Mutex::new(None)),
            limits: Arc::new(HubLimits::default()),
        })
    }

    pub(crate) async fn refresh_updater(&self, telescope_id: i64) {
        // Rebuilding an updater loses its in-memory dedup state. Remove the
        // physical socket first so the N.I.N.A. plugin replays its last Direct
        // delta instead of advancing past a response consumed by the retiring
        // task. The logical lifecycle generation deliberately stays intact.
        if let Some(connection) = self.rig_connections.disconnect_for_replay(telescope_id) {
            connection.request_close(
                "Hub delivery configuration changed; reconnecting safely",
                true,
            );
        }
        let updater = self
            .updater_manager
            .lock()
            .ok()
            .and_then(|registered| registered.as_ref().and_then(Weak::upgrade));
        if let Some(updater) = updater {
            updater.refresh_telescope(telescope_id).await;
        }
    }

    pub(crate) async fn adopt_updater_identity(
        &self,
        telescope_id: i64,
        session_id: uuid::Uuid,
        profile_id: uuid::Uuid,
        lifecycle_generation: u64,
    ) {
        let updater = self
            .updater_manager
            .lock()
            .ok()
            .and_then(|registered| registered.as_ref().and_then(Weak::upgrade));
        if let Some(updater) = updater {
            updater
                .adopt_connection_identity(
                    telescope_id,
                    session_id,
                    profile_id,
                    lifecycle_generation,
                )
                .await;
        }
    }
}

/// Rate limits for the endpoints an unauthenticated client can hammer.
pub struct HubLimits {
    /// OAuth state minting (`/login`): each row is a DB insert.
    pub login: super::rate_limit::RateLimiter,
    /// Failed `/v1/direct` authentication attempts: token guessing.
    pub direct_auth: super::rate_limit::RateLimiter,
}

impl Default for HubLimits {
    fn default() -> Self {
        Self {
            login: super::rate_limit::RateLimiter::new(30, std::time::Duration::from_secs(60)),
            direct_auth: super::rate_limit::RateLimiter::new(
                10,
                std::time::Duration::from_secs(60),
            ),
        }
    }
}

/// Client IP for rate limiting. X-Forwarded-For is client-controlled, so it
/// is honored only when the operator explicitly declares a trusted reverse
/// proxy — and then the LAST hop, which is the one that proxy appended.
pub fn client_ip(
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
    trust_x_forwarded_for: bool,
) -> String {
    if trust_x_forwarded_for
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .map(str::trim)
        && !ip.is_empty()
    {
        return ip.to_string();
    }
    peer.ip().to_string()
}

pub fn router(state: HubState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/healthz", get(healthz))
        .route("/login", get(login))
        .route("/oauth/callback", get(oauth_callback))
        .route("/logout", get(logout))
        .route("/api/session", get(api_session))
        .route("/api/guilds", get(api_list_guilds))
        .route("/api/guilds/{guild_id}/register", post(api_register_guild))
        .route(
            "/api/telescopes",
            get(api_my_telescopes).post(api_create_telescope),
        )
        .route(
            "/api/telescopes/{telescope_id}",
            axum::routing::patch(api_update_telescope).delete(api_delete_telescope),
        )
        .route(
            "/api/telescopes/{telescope_id}/pairing-token",
            post(api_issue_pairing_token),
        )
        .route(
            "/api/telescopes/{telescope_id}/pairing-tokens",
            delete(api_revoke_pairing_tokens),
        )
        .route(
            "/api/telescopes/{telescope_id}/credentials",
            delete(api_revoke_credentials),
        )
        .route(
            "/api/telescopes/{telescope_id}/share-code",
            post(api_create_share_code),
        )
        .route(
            "/api/telescopes/{telescope_id}/attach",
            post(api_attach_telescope),
        )
        .route(
            "/api/guilds/{guild_id}/attachments",
            get(api_guild_attachments),
        )
        .route(
            "/api/attachments/{attachment_id}",
            axum::routing::patch(api_update_attachment).delete(api_detach_telescope),
        )
        .route(
            "/api/attachments/{attachment_id}/channels",
            post(api_add_channel_route),
        )
        .route(
            "/api/attachments/{attachment_id}/channels/{route_id}",
            delete(api_delete_channel_route),
        )
        .route(
            "/api/guilds/{guild_id}/subscribe",
            post(api_subscribe_share),
        )
        .route("/api/guilds/{guild_id}/audit", get(api_guild_audit))
        .route("/api/guilds/{guild_id}/options", get(api_guild_options))
        .route(
            crate::direct::protocol::DIRECT_WEBSOCKET_PATH,
            get(super::direct_server::direct_ws),
        )
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(super::web_ui::INDEX_HTML)
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "image/x-icon"),
            (CACHE_CONTROL, "public, max-age=86400"),
        ],
        super::web_ui::FAVICON_ICO,
    )
}

/// Liveness and readiness in one: proves the process is up and the database
/// answers a query.
async fn healthz(State(state): State<HubState>) -> (StatusCode, Json<serde_json::Value>) {
    match state.db.schema_version() {
        Ok(version) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "version": crate::version::VERSION_STRING,
                "schema_version": version,
                "oauth_configured": state.oauth.is_some(),
                "bot_configured": state.guild_checker.is_some(),
            })),
        ),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
            })),
        ),
    }
}

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

/// Start the Discord OAuth dance: mint a single-use state row carrying the
/// PKCE verifier and redirect to Discord.
async fn login(
    State(state): State<HubState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let Some(oauth) = &state.oauth else {
        return service_unavailable("Discord login is not configured on this hub");
    };
    let ip = client_ip(&headers, peer, state.config.trust_x_forwarded_for);
    if !state.limits.login.check(&ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many login attempts; try again in a minute",
        )
            .into_response();
    }

    let next = sanitize_next_path(query.next.as_deref().unwrap_or("/"));
    let verifier = pkce_verifier();
    let nonce = match state.db.begin_oauth_state(&verifier, &next) {
        Ok(nonce) => nonce,
        Err(e) => return internal_error(e),
    };
    Redirect::to(&oauth.authorize_url(&nonce, &pkce_challenge(&verifier))).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Finish the OAuth dance: consume the state row, exchange the code, capture
/// the user's identity (including email) and guild snapshot, and mint a
/// session.
async fn oauth_callback(
    State(state): State<HubState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oauth) = &state.oauth else {
        return service_unavailable("Discord login is not configured on this hub");
    };
    if let Some(error) = query.error {
        return bad_request(&format!("Discord login was denied: {error}"));
    }
    let (Some(code), Some(nonce)) = (query.code, query.state) else {
        return bad_request("Missing code or state parameter");
    };

    let (verifier, next_path) = match state.db.consume_oauth_state(&nonce) {
        Ok(Some(found)) => found,
        Ok(None) => return bad_request("Login state is unknown, expired, or already used"),
        Err(e) => return internal_error(e),
    };

    let token = match oauth.exchange_code(&code, &verifier).await {
        Ok(token) => token,
        Err(e) => return upstream_error("token exchange", e),
    };
    let discord_user = match oauth.fetch_user(&token.access_token).await {
        Ok(user) => user,
        Err(e) => return upstream_error("user lookup", e),
    };
    let discord_guilds = match oauth.fetch_guilds(&token.access_token).await {
        Ok(guilds) => guilds,
        Err(e) => return upstream_error("guild lookup", e),
    };
    // The access token is dropped here — never persisted.

    let user_id = match parse_snowflake(&discord_user.id) {
        Ok(id) => id,
        Err(e) => return upstream_error("user id", e),
    };
    let user_row = UserRow {
        discord_user_id: user_id,
        username: discord_user.display_name().to_string(),
        email: discord_user.email.clone(),
        email_verified: discord_user.verified,
        avatar_url: discord_user.avatar_url(),
    };
    let guild_rows: Vec<GuildSnapshot> = discord_guilds
        .iter()
        .filter_map(|g| {
            Some(GuildSnapshot {
                guild_id: parse_snowflake(&g.id).ok()?,
                guild_name: g.name.clone(),
                permissions: parse_snowflake(&g.permissions).unwrap_or(0),
                is_owner: g.owner,
            })
        })
        .collect();

    let session = match (|| -> Result<SessionRow, DbError> {
        state.db.upsert_user(&user_row)?;
        state.db.replace_user_guilds(user_id, &guild_rows)?;
        state
            .db
            .create_session(user_id, state.config.session.session_hours)
    })() {
        Ok(session) => session,
        Err(e) => return internal_error(e),
    };

    let cookie = build_session_cookie(
        &state.config,
        &signed_cookie_value(&state.config.session.signing_key, &session.session_id),
        (state.config.session.session_hours * 3600) as i64,
    );
    ([(SET_COOKIE, cookie)], Redirect::to(&next_path)).into_response()
}

/// Delete the session row and clear the cookie.
async fn logout(State(state): State<HubState>, headers: HeaderMap) -> Response {
    if let Some(session) = session_from_headers(&state, &headers) {
        let _ = state.db.delete_session(&session.session_id);
    }
    let cookie = build_session_cookie(&state.config, "", 0);
    ([(SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

/// Who am I? Hands the SPA the CSRF token for mutating requests.
async fn api_session(State(state): State<HubState>, headers: HeaderMap) -> Response {
    let Some(session) = session_from_headers(&state, &headers) else {
        return Json(serde_json::json!({ "authenticated": false })).into_response();
    };
    let user = match state.db.get_user(session.discord_user_id) {
        Ok(Some(user)) => user,
        Ok(None) => return Json(serde_json::json!({ "authenticated": false })).into_response(),
        Err(e) => return internal_error(e),
    };
    Json(serde_json::json!({
        "authenticated": true,
        "csrf_token": session.csrf_token,
        "user": {
            "id": snowflake_string(user.discord_user_id),
            "username": user.username,
            "email": user.email,
            "email_verified": user.email_verified,
            "avatar_url": user.avatar_url,
        },
    }))
    .into_response()
}

/// Resolve the current session from request headers: parse the cookie,
/// verify its signature, then load the row (expiry enforced there).
pub fn session_from_headers(state: &HubState, headers: &HeaderMap) -> Option<SessionRow> {
    let header = headers.get(COOKIE)?.to_str().ok()?;
    let value = cookie_from_header(header, SESSION_COOKIE)?;
    let session_id = verify_signed_cookie_value(&state.config.session.signing_key, &value)?;
    state.db.get_session(&session_id).ok().flatten()
}

/// Require a valid session plus a matching `x-csrf-token` header. For the
/// mutating endpoints arriving in later phases.
pub fn require_session_with_csrf(state: &HubState, headers: &HeaderMap) -> Option<SessionRow> {
    let session = session_from_headers(state, headers)?;
    let provided = headers.get("x-csrf-token")?.to_str().ok()?;
    use subtle::ConstantTimeEq;
    bool::from(provided.as_bytes().ct_eq(session.csrf_token.as_bytes())).then_some(session)
}

// ---------------------------------------------------------------------------
// Guild and telescope management API
// ---------------------------------------------------------------------------

/// Authorization outcome for managing a guild.
enum ManageAuth {
    Ok(SessionRow),
    Denied(Response),
}

/// Authorize a management action on a guild: a session (with CSRF for
/// mutations), MANAGE_GUILD/owner in the OAuth snapshot, and — when a bot
/// token is configured — a live membership check so a user who left the
/// guild loses access before their snapshot refreshes.
async fn authorize_manage(
    state: &HubState,
    headers: &HeaderMap,
    guild_id: i64,
    mutating: bool,
) -> ManageAuth {
    let session = if mutating {
        match require_session_with_csrf(state, headers) {
            Some(session) => session,
            None => {
                return ManageAuth::Denied(
                    (StatusCode::UNAUTHORIZED, "login and CSRF token required").into_response(),
                );
            }
        }
    } else {
        match session_from_headers(state, headers) {
            Some(session) => session,
            None => {
                return ManageAuth::Denied(
                    (StatusCode::UNAUTHORIZED, "login required").into_response(),
                );
            }
        }
    };

    let snapshot = match state.db.user_guilds(session.discord_user_id) {
        Ok(guilds) => guilds,
        Err(e) => return ManageAuth::Denied(internal_error(e)),
    };
    let can_manage = snapshot.iter().any(|g| {
        g.guild_id == guild_id && super::auth::can_manage_guild(g.permissions, g.is_owner)
    });
    if !can_manage {
        return ManageAuth::Denied(
            (StatusCode::FORBIDDEN, "not a manager of this guild").into_response(),
        );
    }

    // The snapshot's permission bits go stale until the next login, so a
    // live check confirms the user still holds management rights right now.
    if let Some(checker) = &state.guild_checker
        && !checker
            .user_can_manage(guild_id as u64, session.discord_user_id as u64)
            .await
    {
        return ManageAuth::Denied(
            (
                StatusCode::FORBIDDEN,
                "you no longer hold Manage Server in this guild",
            )
                .into_response(),
        );
    }

    ManageAuth::Ok(session)
}

#[allow(clippy::result_large_err)] // Err is the ready-to-send error Response
fn parse_id_param(raw: &str) -> Result<i64, Response> {
    parse_snowflake(raw).map_err(|_| bad_request("invalid id"))
}

fn telescope_json(t: &TelescopeRow) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "name": t.name,
        "owner_id": snowflake_string(t.owner_id),
        "image_cooldown_seconds": t.image_cooldown_seconds,
    })
}

fn attachment_json(a: &super::tenants::AttachmentRow) -> serde_json::Value {
    serde_json::json!({
        "attachment_id": a.id,
        "telescope_id": a.telescope_id,
        "guild_id": snowflake_string(a.guild_id),
        "can_command": a.can_command,
        "write_policy": a.write_policy,
        "allowed_role_ids": a.allowed_role_ids.iter().copied()
            .map(snowflake_string).collect::<Vec<_>>(),
    })
}

fn route_json(route: &super::tenants::ChannelRoute) -> serde_json::Value {
    serde_json::json!({
        "route_id": route.id,
        "guild_id": snowflake_string(route.guild_id),
        "channel_id": snowflake_string(route.channel_id),
        "channel_name": route.channel_name,
        "guild_name": route.guild_name,
    })
}

/// Resolve a channel's display name from the guild's channel listing.
async fn channel_display_name(state: &HubState, guild_id: i64, channel_id: i64) -> String {
    match &state.guild_checker {
        Some(checker) => checker
            .guild_channels(guild_id as u64)
            .await
            .into_iter()
            .find(|c| c.id == channel_id as u64)
            .map(|c| c.name)
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// The guilds this user can manage, with registration and bot state.
async fn api_list_guilds(State(state): State<HubState>, headers: HeaderMap) -> Response {
    let Some(session) = session_from_headers(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    let snapshot = match state.db.user_guilds(session.discord_user_id) {
        Ok(guilds) => guilds,
        Err(e) => return internal_error(e),
    };
    let manageable: Vec<_> = snapshot
        .iter()
        .filter(|g| super::auth::can_manage_guild(g.permissions, g.is_owner))
        .collect();

    // One query for registration state, and the (REST-backed) bot checks
    // run concurrently — a manager of many guilds must not pay one Discord
    // round trip per guild in series.
    let guild_ids: Vec<i64> = manageable.iter().map(|g| g.guild_id).collect();
    let registered = match state.db.registered_guild_ids(&guild_ids) {
        Ok(registered) => registered,
        Err(e) => return internal_error(e),
    };
    let installed: Vec<serde_json::Value> = match &state.guild_checker {
        Some(checker) => futures_util::future::join_all(
            manageable
                .iter()
                .map(|g| checker.bot_in_guild(g.guild_id as u64)),
        )
        .await
        .into_iter()
        .map(serde_json::Value::from)
        .collect(),
        None => vec![serde_json::Value::Null; manageable.len()],
    };

    let mut out = Vec::new();
    for (g, bot_installed) in manageable.iter().zip(installed) {
        // The Discord app-install link for this guild. The client ID is
        // public, so exposing it here is fine.
        let install_url = (!state.config.discord.client_id.is_empty())
            .then(|| {
                discord_bot_install_url(
                    &state.config.discord.base_url,
                    &state.config.discord.client_id,
                    g.guild_id,
                )
            })
            .flatten();
        out.push(serde_json::json!({
            "id": snowflake_string(g.guild_id),
            "name": g.guild_name,
            "registered": registered.contains(&g.guild_id),
            "bot_installed": bot_installed,
            "install_url": install_url,
        }));
    }
    Json(serde_json::json!({
        "guilds": out,
        // False means the hub runs without a bot token: pickers, install
        // badges, notifications, and commands are all inert. The UI shows
        // an operator-facing banner instead of quietly looking broken.
        "bot_configured": state.guild_checker.is_some(),
    }))
    .into_response()
}

/// Register a guild as a tenant. Requires the bot to be installed when a
/// live checker is available.
async fn api_register_guild(
    State(state): State<HubState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let guild_id = match parse_id_param(&guild_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let session = match authorize_manage(&state, &headers, guild_id, true).await {
        ManageAuth::Ok(session) => session,
        ManageAuth::Denied(response) => return response,
    };
    if let Some(checker) = &state.guild_checker
        && !checker.bot_in_guild(guild_id as u64).await
    {
        return bad_request("install the Chatstronomy Discord app in this server first");
    }
    // Name comes from the user's snapshot, refreshed at each login.
    let name = state
        .db
        .user_guilds(session.discord_user_id)
        .ok()
        .and_then(|guilds| {
            guilds
                .into_iter()
                .find(|g| g.guild_id == guild_id)
                .map(|g| g.guild_name)
        })
        .unwrap_or_else(|| snowflake_string(guild_id));
    if let Err(e) = state
        .db
        .register_guild(guild_id, &name, session.discord_user_id)
    {
        return internal_error(e);
    }
    state
        .db
        .audit(session.discord_user_id, guild_id, "guild_registered", &name);
    Json(serde_json::json!({ "registered": true })).into_response()
}

// ---------------------------------------------------------------------------
// Telescopes (user-owned)
// ---------------------------------------------------------------------------

/// Require a session (with CSRF for mutations) and that it owns the
/// telescope.
async fn owner_telescope(
    state: &HubState,
    headers: &HeaderMap,
    telescope_id: &str,
    mutating: bool,
) -> Result<(TelescopeRow, SessionRow), Response> {
    let session = if mutating {
        require_session_with_csrf(state, headers).ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, "login and CSRF token required").into_response()
        })?
    } else {
        session_from_headers(state, headers)
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "login required").into_response())?
    };
    let id: i64 = telescope_id
        .parse()
        .map_err(|_| bad_request("invalid telescope id"))?;
    let telescope = match state.db.get_telescope(id) {
        Ok(Some(telescope)) => telescope,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such telescope").into_response()),
        Err(e) => return Err(internal_error(e)),
    };
    if telescope.owner_id != session.discord_user_id {
        return Err((StatusCode::FORBIDDEN, "not your telescope").into_response());
    }
    Ok((telescope, session))
}

/// The session user's telescopes, with connection state, attachments, and
/// destinations — everything the "My telescopes" section needs.
async fn api_my_telescopes(State(state): State<HubState>, headers: HeaderMap) -> Response {
    let Some(session) = session_from_headers(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    let telescopes = match state.db.user_telescopes(session.discord_user_id) {
        Ok(telescopes) => telescopes,
        Err(e) => return internal_error(e),
    };
    let mut out = Vec::new();
    for t in &telescopes {
        let mut value = telescope_json(t);
        let connection = state.rig_connections.get(t.id);
        value["connected"] = serde_json::Value::from(connection.is_some());
        value["commands_enabled"] = serde_json::Value::from(
            connection
                .as_ref()
                .is_some_and(|connection| connection.capabilities.commands),
        );
        let attachments = state.db.telescope_attachments(t.id).unwrap_or_default();
        value["attachments"] = serde_json::Value::from(
            attachments
                .iter()
                .map(|a| {
                    let mut aj = attachment_json(a);
                    aj["guild_name"] = serde_json::Value::from(
                        state
                            .db
                            .get_guild(a.guild_id)
                            .ok()
                            .flatten()
                            .map(|g| g.name)
                            .unwrap_or_default(),
                    );
                    aj["channels"] = serde_json::Value::from(
                        state
                            .db
                            .attachment_routes(t.id, a.guild_id)
                            .unwrap_or_default()
                            .iter()
                            .map(route_json)
                            .collect::<Vec<_>>(),
                    );
                    aj
                })
                .collect::<Vec<_>>(),
        );
        out.push(value);
    }
    Json(serde_json::json!({ "telescopes": out })).into_response()
}

#[derive(Deserialize)]
struct CreateTelescopeBody {
    name: String,
}

async fn api_create_telescope(
    State(state): State<HubState>,
    headers: HeaderMap,
    Json(body): Json<CreateTelescopeBody>,
) -> Response {
    let Some(session) = require_session_with_csrf(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "login and CSRF token required").into_response();
    };
    let name = body.name.trim();
    if name.is_empty() || name.len() > 64 {
        return bad_request("telescope name must be 1-64 characters");
    }
    match state.db.create_telescope(session.discord_user_id, name) {
        Ok(telescope) => {
            state
                .db
                .audit(session.discord_user_id, 0, "telescope_created", name);
            Json(telescope_json(&telescope)).into_response()
        }
        Err(_) => bad_request("you already have a telescope with this name"),
    }
}

#[derive(Deserialize)]
struct UpdateTelescopeBody {
    image_cooldown_seconds: Option<i64>,
}

async fn api_update_telescope(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateTelescopeBody>,
) -> Response {
    let (telescope, _session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if let Some(cooldown) = body.image_cooldown_seconds {
        if !(0..=86400).contains(&cooldown) {
            return bad_request("image_cooldown_seconds must be 0-86400");
        }
        if let Err(e) = state.db.set_telescope_cooldown(telescope.id, cooldown) {
            return internal_error(e);
        }
        state.refresh_updater(telescope.id).await;
    }
    match state.db.get_telescope(telescope.id) {
        Ok(Some(updated)) => Json(telescope_json(&updated)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such telescope").into_response(),
        Err(e) => internal_error(e),
    }
}

async fn api_delete_telescope(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    match state.db.delete_telescope(telescope.id) {
        Ok(()) => {
            // Attachments, routes, credentials, and tokens cascade; the
            // live connection and updater need explicit teardown.
            if let Some(connection) = state.rig_connections.revoke(telescope.id) {
                connection.request_close("telescope deleted by its owner", false);
            }
            state.refresh_updater(telescope.id).await;
            state.db.audit(
                session.discord_user_id,
                0,
                "telescope_deleted",
                &telescope.name,
            );
            Json(serde_json::json!({ "deleted": true })).into_response()
        }
        Err(e) => internal_error(e),
    }
}

/// Issue a fresh pairing token, revoking any live unconsumed ones so only
/// one token is outstanding per telescope.
async fn api_issue_pairing_token(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if let Err(e) = state.db.revoke_pairing_tokens(telescope.id) {
        return internal_error(e);
    }
    match state
        .db
        .issue_pairing_token(telescope.id, session.discord_user_id)
    {
        Ok(token) => {
            state.db.audit(
                session.discord_user_id,
                0,
                "pairing_token_issued",
                &telescope.name,
            );
            Json(serde_json::json!({
                "token": token,
                "expires_in_seconds": super::tenants::PAIRING_TOKEN_TTL_SECONDS,
            }))
            .into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn api_revoke_pairing_tokens(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    match state.db.revoke_pairing_tokens(telescope.id) {
        Ok(revoked) => {
            state.db.audit(
                session.discord_user_id,
                0,
                "pairing_tokens_revoked",
                &telescope.name,
            );
            Json(serde_json::json!({ "revoked": revoked })).into_response()
        }
        Err(e) => internal_error(e),
    }
}

/// Revoke every rig credential for a telescope and drop its live
/// connection. The rig must re-pair with a fresh token.
async fn api_revoke_credentials(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let revoked = match state.db.revoke_rig_credentials(telescope.id) {
        Ok(revoked) => revoked,
        Err(e) => return internal_error(e),
    };
    let disconnected = match state.rig_connections.revoke(telescope.id) {
        Some(connection) => {
            connection.request_close("credentials revoked by the telescope owner", false);
            true
        }
        None => false,
    };
    state.refresh_updater(telescope.id).await;
    state.db.audit(
        session.discord_user_id,
        0,
        "credentials_revoked",
        &telescope.name,
    );
    Json(serde_json::json!({ "revoked": revoked, "disconnected": disconnected })).into_response()
}

/// Mint a share code so another server's manager can subscribe to this
/// telescope's feed.
async fn api_create_share_code(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    match state
        .db
        .create_share_code(telescope.id, session.discord_user_id)
    {
        Ok(code) => {
            state.db.audit(
                session.discord_user_id,
                0,
                "share_code_issued",
                &telescope.name,
            );
            Json(serde_json::json!({
                "code": code,
                "expires_in_seconds": super::tenants::SHARE_CODE_TTL_SECONDS,
            }))
            .into_response()
        }
        Err(e) => internal_error(e),
    }
}

#[derive(Deserialize)]
struct AttachBody {
    guild_id: String,
}

/// Attach your telescope to a server you manage: the owner path, no share
/// code. Both consents come from one authenticated person — telescope
/// ownership and Manage Server in the target guild.
async fn api_attach_telescope(
    State(state): State<HubState>,
    Path(telescope_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AttachBody>,
) -> Response {
    let (telescope, session) = match owner_telescope(&state, &headers, &telescope_id, true).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let guild_id = match parse_snowflake(&body.guild_id) {
        Ok(id) => id,
        Err(_) => return bad_request("invalid guild_id"),
    };
    if let ManageAuth::Denied(response) = authorize_manage(&state, &headers, guild_id, true).await {
        return response;
    }
    match state.db.get_guild(guild_id) {
        Ok(Some(_)) => {}
        Ok(None) => return bad_request("register that server first"),
        Err(e) => return internal_error(e),
    }
    match state
        .db
        .attach_telescope(telescope.id, guild_id, true, session.discord_user_id)
    {
        Ok(attachment) => {
            state.db.audit(
                session.discord_user_id,
                guild_id,
                "telescope_attached",
                &telescope.name,
            );
            Json(attachment_json(&attachment)).into_response()
        }
        Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            bad_request("this telescope is already attached to that server")
        }
        Err(e) => internal_error(e),
    }
}

// ---------------------------------------------------------------------------
// Attachments (telescope x guild)
// ---------------------------------------------------------------------------

/// Load an attachment and authorize management of its guild.
async fn attachment_for_manage(
    state: &HubState,
    headers: &HeaderMap,
    attachment_id: &str,
    mutating: bool,
) -> Result<(super::tenants::AttachmentRow, SessionRow), Response> {
    let id: i64 = attachment_id
        .parse()
        .map_err(|_| bad_request("invalid attachment id"))?;
    let attachment = match state.db.get_attachment(id) {
        Ok(Some(attachment)) => attachment,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such attachment").into_response()),
        Err(e) => return Err(internal_error(e)),
    };
    match authorize_manage(state, headers, attachment.guild_id, mutating).await {
        ManageAuth::Ok(session) => Ok((attachment, session)),
        ManageAuth::Denied(response) => Err(response),
    }
}

/// Everything attached to this guild, with telescopes, owners, connection
/// state, and destinations.
async fn api_guild_attachments(
    State(state): State<HubState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let guild_id = match parse_id_param(&guild_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    if let ManageAuth::Denied(response) = authorize_manage(&state, &headers, guild_id, false).await
    {
        return response;
    }
    let session_user = session_from_headers(&state, &headers)
        .map(|s| s.discord_user_id)
        .unwrap_or(0);
    match state.db.guild_attachments(guild_id) {
        Ok(attachments) => Json(serde_json::json!({
            "attachments": attachments
                .iter()
                .map(|entry| {
                    let mut value = attachment_json(&entry.attachment);
                    value["telescope_name"] = serde_json::Value::from(entry.telescope.name.clone());
                    value["owner_name"] = serde_json::Value::from(entry.owner_name.clone());
                    value["owned_by_me"] =
                        serde_json::Value::from(entry.telescope.owner_id == session_user);
                    let connection = state.rig_connections.get(entry.telescope.id);
                    value["connected"] = serde_json::Value::from(connection.is_some());
                    value["commands_enabled"] = serde_json::Value::from(
                        connection
                            .as_ref()
                            .is_some_and(|connection| connection.capabilities.commands),
                    );
                    value["channels"] = serde_json::Value::from(
                        state
                            .db
                            .attachment_routes(entry.telescope.id, guild_id)
                            .unwrap_or_default()
                            .iter()
                            .map(route_json)
                            .collect::<Vec<_>>(),
                    );
                    value
                })
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal_error(e),
    }
}

#[derive(Deserialize)]
struct UpdateAttachmentBody {
    write_policy: Option<String>,
    allowed_role_ids: Option<Vec<String>>,
}

/// The attachment's guild managers set THEIR server's command policy.
async fn api_update_attachment(
    State(state): State<HubState>,
    Path(attachment_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateAttachmentBody>,
) -> Response {
    let (attachment, session) =
        match attachment_for_manage(&state, &headers, &attachment_id, true).await {
            Ok(found) => found,
            Err(response) => return response,
        };
    if let Some(policy) = &body.write_policy
        && !["disabled", "admins", "roles"].contains(&policy.as_str())
    {
        return bad_request("write_policy must be 'disabled', 'admins', or 'roles'");
    }
    let roles = match &body.allowed_role_ids {
        None => None,
        Some(raw_roles) => {
            let mut parsed = Vec::new();
            for raw in raw_roles {
                match parse_snowflake(raw) {
                    Ok(id) => parsed.push(id),
                    Err(_) => return bad_request("invalid role id"),
                }
            }
            Some(parsed)
        }
    };
    let update = super::tenants::AttachmentUpdate {
        write_policy: body.write_policy.clone(),
        allowed_role_ids: roles,
    };
    if let Err(e) = state.db.update_attachment(attachment.id, &update) {
        return internal_error(e);
    }
    state.db.audit(
        session.discord_user_id,
        attachment.guild_id,
        "attachment_updated",
        &attachment.telescope_id.to_string(),
    );
    match state.db.get_attachment(attachment.id) {
        Ok(Some(updated)) => Json(attachment_json(&updated)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such attachment").into_response(),
        Err(e) => internal_error(e),
    }
}

/// Remove an attachment (and its routes). Either side may sever: a manager
/// of the attachment's guild, or the telescope's owner.
async fn api_detach_telescope(
    State(state): State<HubState>,
    Path(attachment_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id: i64 = match attachment_id.parse() {
        Ok(id) => id,
        Err(_) => return bad_request("invalid attachment id"),
    };
    let attachment = match state.db.get_attachment(id) {
        Ok(Some(attachment)) => attachment,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such attachment").into_response(),
        Err(e) => return internal_error(e),
    };
    let telescope = match state.db.get_telescope(attachment.telescope_id) {
        Ok(Some(telescope)) => telescope,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such telescope").into_response(),
        Err(e) => return internal_error(e),
    };
    // Owner path first (session + CSRF, no guild management needed).
    let session = match require_session_with_csrf(&state, &headers) {
        Some(session) if session.discord_user_id == telescope.owner_id => Some(session),
        _ => match authorize_manage(&state, &headers, attachment.guild_id, true).await {
            ManageAuth::Ok(session) => Some(session),
            ManageAuth::Denied(response) => return response,
        },
    };
    if let Err(e) = state.db.detach_telescope(attachment.id) {
        return internal_error(e);
    }
    state.refresh_updater(attachment.telescope_id).await;
    if let Some(session) = session {
        state.db.audit(
            session.discord_user_id,
            attachment.guild_id,
            "telescope_detached",
            &telescope.name,
        );
    }
    Json(serde_json::json!({ "deleted": true })).into_response()
}

#[derive(Deserialize)]
struct AddChannelBody {
    channel_id: String,
}

/// Add a destination channel to an attachment. The channel must be in the
/// attachment's guild, and only that guild's managers may do it — channel
/// ownership stays with the server the channel lives in.
async fn api_add_channel_route(
    State(state): State<HubState>,
    Path(attachment_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AddChannelBody>,
) -> Response {
    let (attachment, session) =
        match attachment_for_manage(&state, &headers, &attachment_id, true).await {
            Ok(found) => found,
            Err(response) => return response,
        };
    let channel_id = match parse_snowflake(&body.channel_id) {
        Ok(id) => id,
        Err(_) => return bad_request("invalid channel_id"),
    };
    if let Some(checker) = &state.guild_checker
        && !checker
            .channel_in_guild(channel_id as u64, attachment.guild_id as u64)
            .await
    {
        return bad_request("that channel is not in this server");
    }
    let channel_name = channel_display_name(&state, attachment.guild_id, channel_id).await;
    let guild_name = state
        .db
        .get_guild(attachment.guild_id)
        .ok()
        .flatten()
        .map(|g| g.name)
        .unwrap_or_default();
    match state.db.add_channel_route(
        attachment.telescope_id,
        attachment.guild_id,
        channel_id,
        &channel_name,
        &guild_name,
        session.discord_user_id,
    ) {
        Ok(route) => {
            state.refresh_updater(attachment.telescope_id).await;
            state.db.audit(
                session.discord_user_id,
                attachment.guild_id,
                "destination_added",
                &format!("#{}", route.channel_name),
            );
            Json(route_json(&route)).into_response()
        }
        Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            bad_request("that channel is already routed to a telescope")
        }
        Err(e) => internal_error(e),
    }
}

/// Remove a destination. Managers of the attachment's guild, or the
/// telescope's owner.
async fn api_delete_channel_route(
    State(state): State<HubState>,
    Path((attachment_id, route_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let attachment_id: i64 = match attachment_id.parse() {
        Ok(id) => id,
        Err(_) => return bad_request("invalid attachment id"),
    };
    let route_id: i64 = match route_id.parse() {
        Ok(id) => id,
        Err(_) => return bad_request("invalid route id"),
    };
    let attachment = match state.db.get_attachment(attachment_id) {
        Ok(Some(attachment)) => attachment,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such attachment").into_response(),
        Err(e) => return internal_error(e),
    };
    let route = match state.db.get_route(route_id) {
        Ok(Some(route))
            if route.telescope_id == attachment.telescope_id
                && route.guild_id == attachment.guild_id =>
        {
            route
        }
        Ok(_) => return (StatusCode::NOT_FOUND, "no such destination").into_response(),
        Err(e) => return internal_error(e),
    };
    let owner_id = state
        .db
        .get_telescope(attachment.telescope_id)
        .ok()
        .flatten()
        .map(|t| t.owner_id);
    let session = match require_session_with_csrf(&state, &headers) {
        Some(session) if Some(session.discord_user_id) == owner_id => Some(session),
        _ => match authorize_manage(&state, &headers, attachment.guild_id, true).await {
            ManageAuth::Ok(session) => Some(session),
            ManageAuth::Denied(response) => return response,
        },
    };
    if let Err(e) = state.db.delete_route(route.id) {
        return internal_error(e);
    }
    state.refresh_updater(attachment.telescope_id).await;
    if let Some(session) = session {
        state.db.audit(
            session.discord_user_id,
            attachment.guild_id,
            "destination_removed",
            &format!("#{}", route.channel_name),
        );
    }
    Json(serde_json::json!({ "deleted": true })).into_response()
}

#[derive(Deserialize)]
struct SubscribeBody {
    code: String,
    channel_id: String,
}

/// Redeem a share code: creates a feed-only attachment in THIS guild plus
/// its first destination. Only this guild's manager can do it, so a feed
/// can never be pushed into a server whose managers didn't act.
async fn api_subscribe_share(
    State(state): State<HubState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Response {
    let guild_id = match parse_id_param(&guild_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let session = match authorize_manage(&state, &headers, guild_id, true).await {
        ManageAuth::Ok(session) => session,
        ManageAuth::Denied(response) => return response,
    };
    match state.db.get_guild(guild_id) {
        Ok(Some(_)) => {}
        Ok(None) => return bad_request("register this server first"),
        Err(e) => return internal_error(e),
    }
    let channel_id = match parse_snowflake(&body.channel_id) {
        Ok(id) => id,
        Err(_) => return bad_request("invalid channel_id"),
    };
    if let Some(checker) = &state.guild_checker
        && !checker
            .channel_in_guild(channel_id as u64, guild_id as u64)
            .await
    {
        return bad_request("that channel is not in this server");
    }
    // Check the channel is free BEFORE consuming the single-use code, so a
    // conflict doesn't burn it.
    match state.db.telescope_by_channel(channel_id) {
        Ok(Some(_)) => return bad_request("that channel is already routed to a telescope"),
        Ok(None) => {}
        Err(e) => return internal_error(e),
    }
    let telescope_id = match state.db.consume_share_code(body.code.trim()) {
        Ok(Some(id)) => id,
        Ok(None) => return bad_request("share code is unknown, expired, or already used"),
        Err(e) => return internal_error(e),
    };
    let telescope = match state.db.get_telescope(telescope_id) {
        Ok(Some(telescope)) => telescope,
        Ok(None) => return bad_request("the shared telescope no longer exists"),
        Err(e) => return internal_error(e),
    };
    // A feed-only attachment, unless this guild already has one.
    let attachment = match state.db.attachment_for(telescope.id, guild_id) {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            match state
                .db
                .attach_telescope(telescope.id, guild_id, false, session.discord_user_id)
            {
                Ok(attachment) => attachment,
                Err(e) => return internal_error(e),
            }
        }
        Err(e) => return internal_error(e),
    };
    let channel_name = channel_display_name(&state, guild_id, channel_id).await;
    let guild_name = state
        .db
        .get_guild(guild_id)
        .ok()
        .flatten()
        .map(|g| g.name)
        .unwrap_or_default();
    match state.db.add_channel_route(
        telescope.id,
        guild_id,
        channel_id,
        &channel_name,
        &guild_name,
        session.discord_user_id,
    ) {
        Ok(route) => {
            state.refresh_updater(telescope.id).await;
            state.db.audit(
                session.discord_user_id,
                guild_id,
                "shared_telescope_subscribed",
                &format!("{} -> #{}", telescope.name, route.channel_name),
            );
            Json(serde_json::json!({
                "telescope_name": telescope.name,
                "attachment": attachment_json(&attachment),
                "route": route_json(&route),
            }))
            .into_response()
        }
        Err(e) => internal_error(e),
    }
}

/// Picker options for a guild: its text channels and assignable roles,
/// straight from Discord — nobody should ever type an ID.
async fn api_guild_options(
    State(state): State<HubState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let guild_id = match parse_id_param(&guild_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    if let ManageAuth::Denied(response) = authorize_manage(&state, &headers, guild_id, false).await
    {
        return response;
    }
    let (channels, roles) = match &state.guild_checker {
        Some(checker) => {
            let (channels, roles) = tokio::join!(
                checker.guild_channels(guild_id as u64),
                checker.guild_roles(guild_id as u64),
            );
            (channels, roles)
        }
        None => (Vec::new(), Vec::new()),
    };
    let to_json = |items: Vec<super::guild_check::NamedId>| {
        items
            .into_iter()
            .map(|item| {
                serde_json::json!({
                    "id": item.id.to_string(),
                    "name": item.name,
                })
            })
            .collect::<Vec<_>>()
    };
    Json(serde_json::json!({
        "channels": to_json(channels),
        "roles": to_json(roles),
        "bot_configured": state.guild_checker.is_some(),
    }))
    .into_response()
}

/// Newest-first management audit entries for a guild.
async fn api_guild_audit(
    State(state): State<HubState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let guild_id = match parse_id_param(&guild_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    if let ManageAuth::Denied(response) = authorize_manage(&state, &headers, guild_id, false).await
    {
        return response;
    }
    match state.db.guild_audit(guild_id, 100) {
        Ok(entries) => Json(serde_json::json!({
            "entries": entries.iter().map(|entry| serde_json::json!({
                "at": entry.at,
                "user_id": snowflake_string(entry.discord_user_id),
                "action": entry.action,
                "detail": entry.detail,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal_error(e),
    }
}

fn build_session_cookie(config: &HubConfig, value: &str, max_age: i64) -> String {
    let secure = if config.public_base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

fn service_unavailable(message: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, message.to_string()).into_response()
}

fn internal_error(e: impl std::fmt::Display) -> Response {
    eprintln!("Hub internal error: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error".to_string(),
    )
        .into_response()
}

fn upstream_error(stage: &str, e: impl std::fmt::Display) -> Response {
    eprintln!("Hub Discord {stage} failed: {e}");
    (StatusCode::BAD_GATEWAY, format!("Discord {stage} failed")).into_response()
}

/// Open the database, bind, and serve until ctrl-c.
pub async fn run(config: HubConfig) -> Result<(), HubError> {
    let db = Db::open(&config.database_path)?;
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    println!(
        "Hub listening on http://{} (database: {})",
        listener.local_addr()?,
        config.database_path
    );
    serve(listener, config, db).await
}

/// Serve on an already-bound listener. Split from `run` so tests can bind
/// port 0 and use an in-memory database.
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: HubConfig,
    db: Db,
) -> Result<(), HubError> {
    let guild_checker: Option<Arc<dyn GuildChecker>> = if config.discord.bot_token.is_empty() {
        if config.oauth_configured() {
            // Half-configured is the worst state: login works, so the hub
            // *looks* alive while pickers, badges, notifications, and
            // commands are all inert. Be impossible to miss.
            eprintln!("==========================================================");
            eprintln!("WARNING: discord.bot_token is EMPTY in the hub config.");
            eprintln!("Web login works, but everything the bot powers is OFF:");
            eprintln!("  - channel/role pickers show nothing");
            eprintln!("  - bot-installed badges and install checks are skipped");
            eprintln!("  - no notifications are posted, no slash commands work");
            eprintln!("Set discord.bot_token (Developer Portal -> Bot) and restart.");
            eprintln!("==========================================================");
        } else {
            println!("No bot token configured; live guild checks are disabled");
        }
        None
    } else {
        Some(Arc::new(CachedGuildChecker::new(
            SerenityGuildChecker::new(&config.discord.bot_token),
        )))
    };
    let state = HubState::build(config, db, guild_checker)?;
    if state.oauth.is_none() {
        println!("Discord login not configured; web login is disabled");
    }

    // With a bot token, run the central Discord bot and the per-rig chat
    // updater manager alongside the web server.
    if !state.config.discord.bot_token.is_empty() {
        let bot_config = crate::chat::DiscordBotConfig {
            enabled: true,
            token: state.config.discord.bot_token.clone(),
            application_id: None,
            public_key: None,
            default_channel_id: None,
            live_status: false,
            state_file: "chatstronomy-hub-state.json".to_string(),
            write_acl: Vec::new(),
        };
        let resolver = Arc::new(super::rig_resolver::HubRigResolver::new(
            state.db.clone(),
            state.rig_connections.clone(),
        ));
        let (service, _gateway) = crate::chat::run_bot(&bot_config, resolver).await?;
        let mut manager = crate::chat::ChatServiceManager::new();
        manager.add_service(Box::new(service));
        let updaters = Arc::new(super::updaters::UpdaterManager::new(
            state.db.clone(),
            state.rig_connections.clone(),
            Arc::new(manager),
        ));
        if let Ok(mut registered) = state.updater_manager.lock() {
            *registered = Some(Arc::downgrade(&updaters));
        }
        tokio::spawn(updaters.run());
        println!("Central Discord bot and chat updater manager started");
    }

    // Hourly sweep of expired sessions and stale OAuth states.
    {
        let db = state.db.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                if let Err(e) = db.cleanup_auth_rows() {
                    eprintln!("Warning: auth-row cleanup failed: {e}");
                }
            }
        });
    }

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("Hub shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Form;

    async fn spawn_hub_with(
        config: HubConfig,
        checker: Option<Arc<dyn GuildChecker>>,
    ) -> (String, Db) {
        let db = Db::open_in_memory().unwrap();
        // The production constructor, so test wiring cannot drift.
        let state = HubState::build(config, db.clone(), checker).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (format!("http://{addr}"), db)
    }

    async fn spawn_hub(config: HubConfig) -> (String, Db) {
        spawn_hub_with(config, None).await
    }

    /// Guild checker with fixed answers for tests.
    struct StubChecker {
        bot: bool,
        member: bool,
        channel: bool,
    }

    #[async_trait::async_trait]
    impl GuildChecker for StubChecker {
        async fn bot_in_guild(&self, _guild_id: u64) -> bool {
            self.bot
        }
        async fn user_can_manage(&self, _guild_id: u64, _user_id: u64) -> bool {
            self.member
        }
        async fn channel_in_guild(&self, _channel_id: u64, _guild_id: u64) -> bool {
            self.channel
        }
        async fn guild_channels(&self, _guild_id: u64) -> Vec<super::super::guild_check::NamedId> {
            vec![super::super::guild_check::NamedId {
                id: 555,
                name: "observatory".to_string(),
            }]
        }
        async fn guild_roles(&self, _guild_id: u64) -> Vec<super::super::guild_check::NamedId> {
            vec![super::super::guild_check::NamedId {
                id: 1111,
                name: "astronomers".to_string(),
            }]
        }
    }

    async fn spawn_test_hub() -> String {
        spawn_hub(HubConfig::default()).await.0
    }

    #[tokio::test]
    async fn healthz_reports_ok_and_schema_version() {
        let base = spawn_test_hub().await;
        let response = reqwest::get(format!("{base}/healthz")).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body["schema_version"].as_u64().unwrap() >= 2);
        assert_eq!(body["oauth_configured"], false);
        assert_eq!(body["bot_configured"], false);
    }

    #[test]
    fn bot_install_url_requests_attachment_permissions() {
        let url = discord_bot_install_url("https://discord.com", "123", 456).unwrap();
        let url = url::Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(query.get("client_id").map(String::as_str), Some("123"));
        assert_eq!(query.get("guild_id").map(String::as_str), Some("456"));
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some("bot applications.commands")
        );
        let permissions = query["permissions"].parse::<u64>().unwrap();
        assert_eq!(permissions, 52_224);
        assert!(
            poise::serenity_prelude::Permissions::from_bits_truncate(permissions)
                .contains(poise::serenity_prelude::Permissions::ATTACH_FILES)
        );
    }

    #[tokio::test]
    async fn index_serves_html() {
        let base = spawn_test_hub().await;
        let response = reqwest::get(&base).await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.text().await.unwrap().contains("Chatstronomy hub"));
    }

    #[tokio::test]
    async fn favicon_serves_the_embedded_branding() {
        let base = spawn_test_hub().await;
        let response = reqwest::get(format!("{base}/favicon.ico")).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "image/x-icon"
        );
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "public, max-age=86400"
        );
        let body = response.bytes().await.unwrap();
        assert_eq!(body.as_ref(), super::super::web_ui::FAVICON_ICO);
        assert!(body.starts_with(&[0, 0, 1, 0]));
    }

    #[tokio::test]
    async fn login_unconfigured_returns_503() {
        let base = spawn_test_hub().await;
        let response = reqwest::get(format!("{base}/login")).await.unwrap();
        assert_eq!(response.status(), 503);
    }

    #[tokio::test]
    async fn session_endpoint_unauthenticated() {
        let base = spawn_test_hub().await;
        let body: serde_json::Value = reqwest::get(format!("{base}/api/session"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["authenticated"], false);
    }

    /// A stub of the three Discord endpoints the login flow calls.
    async fn spawn_stub_discord() -> String {
        #[derive(Deserialize)]
        struct TokenForm {
            code: String,
            code_verifier: String,
            grant_type: String,
        }
        async fn token(Form(form): Form<TokenForm>) -> Response {
            if form.grant_type != "authorization_code"
                || form.code != "test-code"
                || form.code_verifier.is_empty()
            {
                return (StatusCode::BAD_REQUEST, "bad token request").into_response();
            }
            Json(serde_json::json!({
                "access_token": "stub-access-token",
                "token_type": "Bearer"
            }))
            .into_response()
        }
        fn authed(headers: &HeaderMap) -> bool {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "Bearer stub-access-token")
        }
        async fn me(headers: HeaderMap) -> Response {
            if !authed(&headers) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(serde_json::json!({
                "id": "80351110224678912",
                "username": "nelly",
                "global_name": "Nelly",
                "avatar": "abc123",
                "email": "nelly@example.com",
                "verified": true
            }))
            .into_response()
        }
        async fn guilds(headers: HeaderMap) -> Response {
            if !authed(&headers) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(serde_json::json!([
                {"id": "197038439483310086", "name": "Observatory", "owner": true,
                 "permissions": "2147483647"},
                {"id": "300", "name": "Other", "owner": false, "permissions": "0"},
                // MANAGE_GUILD only: a second manageable guild for the
                // cross-server share tests.
                {"id": "400", "name": "Partner", "owner": false, "permissions": "32"}
            ]))
            .into_response()
        }
        let app = Router::new()
            .route("/api/v10/oauth2/token", axum::routing::post(token))
            .route("/api/v10/users/@me", get(me))
            .route("/api/v10/users/@me/guilds", get(guilds));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn oauth_config(stub_base: &str) -> HubConfig {
        let mut config = HubConfig::default();
        config.discord.base_url = stub_base.to_string();
        config.discord.client_id = "client-1".to_string();
        config.discord.client_secret = "secret-1".to_string();
        config.public_base_url = "http://hub.test".to_string();
        config.session.signing_key = "0123456789abcdef0123456789abcdef".to_string();
        config
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .build()
            .unwrap()
    }

    /// Drive /login and return the state nonce Discord would echo back.
    async fn login_state(client: &reqwest::Client, base: &str) -> String {
        let response = client
            .get(format!("{base}/login?next=/after-login"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        let location = response.headers()["location"].to_str().unwrap();
        assert!(location.contains("/oauth2/authorize"));
        assert!(location.contains("code_challenge_method=S256"));
        let url = url::Url::parse(location).unwrap();
        url.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string())
            .unwrap()
    }

    #[tokio::test]
    async fn full_login_flow_captures_email_and_guilds() {
        let stub = spawn_stub_discord().await;
        let (base, db) = spawn_hub(oauth_config(&stub)).await;
        let client = client();

        let state = login_state(&client, &base).await;
        let response = client
            .get(format!(
                "{base}/oauth/callback?code=test-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/after-login");
        assert!(response.headers().contains_key("set-cookie"));

        // Session endpoint sees the user, with email captured.
        let body: serde_json::Value = client
            .get(format!("{base}/api/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["authenticated"], true);
        assert_eq!(body["user"]["email"], "nelly@example.com");
        assert_eq!(body["user"]["username"], "Nelly");
        assert_eq!(body["user"]["id"], "80351110224678912");
        assert!(body["csrf_token"].is_string());

        // Guild snapshot with permissions landed in the database.
        let guilds = db.user_guilds(80351110224678912).unwrap();
        assert_eq!(guilds.len(), 3);
        let observatory = guilds
            .iter()
            .find(|g| g.guild_name == "Observatory")
            .unwrap();
        assert!(observatory.is_owner);
        assert!(crate::hub::auth::can_manage_guild(
            observatory.permissions,
            observatory.is_owner
        ));
    }

    #[tokio::test]
    async fn oauth_state_cannot_be_replayed() {
        let stub = spawn_stub_discord().await;
        let (base, _db) = spawn_hub(oauth_config(&stub)).await;
        let client = client();

        let state = login_state(&client, &base).await;
        let first = client
            .get(format!(
                "{base}/oauth/callback?code=test-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 303);
        let replay = client
            .get(format!(
                "{base}/oauth/callback?code=test-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), 400);
    }

    #[tokio::test]
    async fn callback_with_unknown_state_rejected() {
        let stub = spawn_stub_discord().await;
        let (base, _db) = spawn_hub(oauth_config(&stub)).await;
        let response = reqwest::get(format!(
            "{base}/oauth/callback?code=test-code&state=made-up"
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn logout_ends_session() {
        let stub = spawn_stub_discord().await;
        let (base, _db) = spawn_hub(oauth_config(&stub)).await;
        let client = client();

        let state = login_state(&client, &base).await;
        client
            .get(format!(
                "{base}/oauth/callback?code=test-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        let response = client.get(format!("{base}/logout")).send().await.unwrap();
        assert_eq!(response.status(), 303);

        let body: serde_json::Value = client
            .get(format!("{base}/api/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["authenticated"], false);
    }

    #[tokio::test]
    async fn forged_cookie_rejected() {
        let stub = spawn_stub_discord().await;
        let (base, db) = spawn_hub(oauth_config(&stub)).await;
        // A session row exists, but the cookie is signed with the wrong key.
        db.upsert_user(&UserRow {
            discord_user_id: 1,
            username: "u".to_string(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        let session = db.create_session(1, 1).unwrap();
        let forged = signed_cookie_value("wrong-key-wrong-key-wrong-key-00", &session.session_id);

        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/api/session"))
            .header("cookie", format!("{SESSION_COOKIE}={forged}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["authenticated"], false);
    }

    // The stub Discord user owns guild 197038439483310086 ("Observatory")
    // and is a plain member of guild 300 ("Other").
    const OWNED_GUILD: &str = "197038439483310086";
    const MEMBER_GUILD: &str = "300";
    const PARTNER_GUILD: &str = "400";

    /// Log in through the stub Discord and return the CSRF token.
    async fn login(client: &reqwest::Client, base: &str) -> String {
        let state = login_state(client, base).await;
        client
            .get(format!(
                "{base}/oauth/callback?code=test-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = client
            .get(format!("{base}/api/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["csrf_token"].as_str().unwrap().to_string()
    }

    async fn managed_hub(
        checker: Option<Arc<dyn GuildChecker>>,
    ) -> (String, Db, reqwest::Client, String) {
        let stub = spawn_stub_discord().await;
        let (base, db) = spawn_hub_with(oauth_config(&stub), checker).await;
        let client = client();
        let csrf = login(&client, &base).await;
        (base, db, client, csrf)
    }

    /// Helper: register the owned guild, create a telescope owned by the
    /// session user, and attach it there. Returns (telescope_id,
    /// attachment_id).
    async fn create_and_attach(client: &reqwest::Client, base: &str, csrf: &str) -> (i64, i64) {
        client
            .post(format!("{base}/api/guilds/{OWNED_GUILD}/register"))
            .header("x-csrf-token", csrf)
            .send()
            .await
            .unwrap();
        let telescope: serde_json::Value = client
            .post(format!("{base}/api/telescopes"))
            .header("x-csrf-token", csrf)
            .json(&serde_json::json!({ "name": "c925" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = telescope["id"].as_i64().unwrap();
        let attachment: serde_json::Value = client
            .post(format!("{base}/api/telescopes/{id}/attach"))
            .header("x-csrf-token", csrf)
            .json(&serde_json::json!({ "guild_id": OWNED_GUILD }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (id, attachment["attachment_id"].as_i64().unwrap())
    }

    #[tokio::test]
    async fn owner_flow_create_attach_route_pair() {
        let (base, db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let (id, attachment_id) = create_and_attach(&client, &base, &csrf).await;

        // The attachment made by the owner can command, defaults to the
        // managers policy, and is visible on the guild page as mine.
        let listed: serde_json::Value = client
            .get(format!("{base}/api/guilds/{OWNED_GUILD}/attachments"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let entry = &listed["attachments"][0];
        assert_eq!(entry["telescope_name"], "c925");
        assert_eq!(entry["can_command"], true);
        assert_eq!(entry["write_policy"], "admins");
        assert_eq!(entry["owned_by_me"], true);
        assert_eq!(entry["connected"], false);
        assert_eq!(entry["commands_enabled"], false);

        let mine: serde_json::Value = client
            .get(format!("{base}/api/telescopes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mine["telescopes"][0]["connected"], false);
        assert_eq!(mine["telescopes"][0]["commands_enabled"], false);

        // Route a channel, set a role policy on the attachment.
        let route: serde_json::Value = client
            .post(format!("{base}/api/attachments/{attachment_id}/channels"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "channel_id": "555" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(route["channel_name"], "observatory");
        let updated: serde_json::Value = client
            .patch(format!("{base}/api/attachments/{attachment_id}"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({
                "write_policy": "roles",
                "allowed_role_ids": ["1111"],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(updated["write_policy"], "roles");

        // The owner's view rolls everything up.
        let mine: serde_json::Value = client
            .get(format!("{base}/api/telescopes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let t = &mine["telescopes"][0];
        assert_eq!(t["name"], "c925");
        assert_eq!(t["attachments"][0]["channels"][0]["channel_id"], "555");

        // Owner mints a pairing token; single use.
        let issued: serde_json::Value = client
            .post(format!("{base}/api/telescopes/{id}/pairing-token"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = issued["token"].as_str().unwrap();
        assert_eq!(db.consume_pairing_token(token).unwrap(), Some(id));
        assert_eq!(db.consume_pairing_token(token).unwrap(), None);

        // Guild audit recorded the attach and the destination.
        let audit: serde_json::Value = client
            .get(format!("{base}/api/guilds/{OWNED_GUILD}/audit"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let actions: Vec<&str> = audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["action"].as_str().unwrap())
            .collect();
        assert!(actions.contains(&"telescope_attached"));
        assert!(actions.contains(&"destination_added"));
    }

    #[tokio::test]
    async fn attach_requires_registered_guild_and_ownership() {
        let (base, db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        // Attaching to an unregistered guild fails.
        let telescope: serde_json::Value = client
            .post(format!("{base}/api/telescopes"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "name": "c925" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = telescope["id"].as_i64().unwrap();
        let response = client
            .post(format!("{base}/api/telescopes/{id}/attach"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "guild_id": OWNED_GUILD }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);

        // Someone else's telescope is untouchable.
        db.upsert_user(&crate::hub::store::UserRow {
            discord_user_id: 999,
            username: "stranger".to_string(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        let other = db.create_telescope(999, "not-yours").unwrap();
        let response = client
            .patch(format!("{base}/api/telescopes/{}", other.id))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "image_cooldown_seconds": 5 }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn credential_revocation_is_owner_only() {
        let (base, db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let (id, _attachment_id) = create_and_attach(&client, &base, &csrf).await;
        let credential = db.create_rig_credential(id, "node-1", "profile-1").unwrap();
        assert!(db.lookup_rig_credential(&credential).unwrap().is_some());

        let body: serde_json::Value = client
            .delete(format!("{base}/api/telescopes/{id}/credentials"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["revoked"], 1);
        assert!(db.lookup_rig_credential(&credential).unwrap().is_none());
    }

    #[tokio::test]
    async fn channel_outside_guild_rejected() {
        // The live check says the channel is not in this guild.
        let (base, _db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: false,
        })))
        .await;
        let (_id, attachment_id) = create_and_attach(&client, &base, &csrf).await;
        let response = client
            .post(format!("{base}/api/attachments/{attachment_id}/channels"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "channel_id": "999888777" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("not in this server")
        );
    }

    #[tokio::test]
    async fn duplicate_channel_claim_rejected() {
        let (base, _db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let (_first, first_attachment) = create_and_attach(&client, &base, &csrf).await;
        let second: serde_json::Value = client
            .post(format!("{base}/api/telescopes"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "name": "esprit" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let second_id = second["id"].as_i64().unwrap();
        let second_attachment: serde_json::Value = client
            .post(format!("{base}/api/telescopes/{second_id}/attach"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "guild_id": OWNED_GUILD }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let second_attachment = second_attachment["attachment_id"].as_i64().unwrap();

        let ok = client
            .post(format!(
                "{base}/api/attachments/{first_attachment}/channels"
            ))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "channel_id": "555" }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let clash = client
            .post(format!(
                "{base}/api/attachments/{second_attachment}/channels"
            ))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "channel_id": "555" }))
            .send()
            .await
            .unwrap();
        assert_eq!(clash.status(), 400);
    }

    #[tokio::test]
    async fn share_code_subscribes_another_guild_feed_only() {
        let (base, db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let (id, _attachment_id) = create_and_attach(&client, &base, &csrf).await;
        let issued: serde_json::Value = client
            .post(format!("{base}/api/telescopes/{id}/share-code"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let code = issued["code"].as_str().unwrap().to_string();

        // The partner guild registers and redeems the code against one of
        // ITS channels: a feed-only attachment.
        client
            .post(format!("{base}/api/guilds/{PARTNER_GUILD}/register"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap();
        let subscribed: serde_json::Value = client
            .post(format!("{base}/api/guilds/{PARTNER_GUILD}/subscribe"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "code": code, "channel_id": "777" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(subscribed["telescope_name"], "c925");
        assert_eq!(subscribed["attachment"]["can_command"], false);
        let partner_attachment = subscribed["attachment"]["attachment_id"].as_i64().unwrap();

        // Single use.
        let replay = client
            .post(format!("{base}/api/guilds/{PARTNER_GUILD}/subscribe"))
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({ "code": code, "channel_id": "778" }))
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), 400);

        // The owner sees both attachments; the partner page shows the feed.
        let mine: serde_json::Value = client
            .get(format!("{base}/api/telescopes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            mine["telescopes"][0]["attachments"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let partner: serde_json::Value = client
            .get(format!("{base}/api/guilds/{PARTNER_GUILD}/attachments"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(partner["attachments"][0]["can_command"], false);

        // Detaching from the partner side removes the feed and its routes.
        let removed = client
            .delete(format!("{base}/api/attachments/{partner_attachment}"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), 200);
        assert!(db.attachment_for(id, 400).unwrap().is_none());
        assert!(db.telescope_by_channel(777).unwrap().is_none());
    }

    #[tokio::test]
    async fn non_manager_cannot_register() {
        let (base, _db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let response = client
            .post(format!("{base}/api/guilds/{MEMBER_GUILD}/register"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn mutation_requires_csrf() {
        let (base, _db, client, _csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let response = client
            .post(format!("{base}/api/guilds/{OWNED_GUILD}/register"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn departed_member_loses_access() {
        // Snapshot says owner, but the live check says they left.
        let (base, _db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: false,
            channel: true,
        })))
        .await;
        let response = client
            .post(format!("{base}/api/guilds/{OWNED_GUILD}/register"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn register_requires_bot_installed() {
        let (base, _db, client, csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: false,
            member: true,
            channel: true,
        })))
        .await;
        let response = client
            .post(format!("{base}/api/guilds/{OWNED_GUILD}/register"))
            .header("x-csrf-token", &csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }

    #[test]
    fn client_ip_trust_semantics() {
        let peer: std::net::SocketAddr = "10.0.0.9:4444".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        // Untrusted: the client-controlled header is ignored.
        assert_eq!(client_ip(&headers, peer, false), "10.0.0.9");
        // Trusted: the LAST hop (appended by the trusted proxy) wins.
        assert_eq!(client_ip(&headers, peer, true), "5.6.7.8");
        // Trusted but absent header: peer.
        assert_eq!(client_ip(&HeaderMap::new(), peer, true), "10.0.0.9");
    }

    #[tokio::test]
    async fn options_endpoint_serves_channels_and_roles() {
        let (base, _db, client, _csrf) = managed_hub(Some(Arc::new(StubChecker {
            bot: true,
            member: true,
            channel: true,
        })))
        .await;
        let body: serde_json::Value = client
            .get(format!("{base}/api/guilds/{OWNED_GUILD}/options"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["channels"][0]["id"], "555");
        assert_eq!(body["channels"][0]["name"], "observatory");
        assert_eq!(body["roles"][0]["id"], "1111");
        assert_eq!(body["roles"][0]["name"], "astronomers");
        assert_eq!(body["bot_configured"], true);

        // Options are manage-gated like everything else.
        let anonymous = reqwest::get(format!("{base}/api/guilds/{OWNED_GUILD}/options"))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
    }

    #[tokio::test]
    async fn guild_api_requires_login() {
        let stub = spawn_stub_discord().await;
        let (base, _db) = spawn_hub_with(
            oauth_config(&stub),
            Some(Arc::new(StubChecker {
                bot: true,
                member: true,
                channel: true,
            })),
        )
        .await;
        let response = reqwest::get(format!("{base}/api/guilds")).await.unwrap();
        assert_eq!(response.status(), 401);
    }
}
