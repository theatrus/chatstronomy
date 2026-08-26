//! The hub's `/v1/direct` WebSocket endpoint.
//!
//! N.I.N.A. plugins connect outward to this endpoint. The
//! first frame authenticates: a one-time pairing token (first connect) or the
//! durable rig credential minted by that pairing. After the handshake the hub
//! sends [`QueryRequest`] frames and the rig answers with [`QueryResult`];
//! heartbeats keep NATs open. One telescope has one active connection — a
//! newer authenticated connection replaces the older one.

use super::server::HubState;
use crate::direct::protocol::{
    AgentHello, AuthRequest, CURRENT_PAYLOAD_VERSION, ClientHello, DirectMessage,
    LEGACY_PAYLOAD_VERSION, PROTOCOL_VERSION, PairRequest, QueryKind, QueryRequest, QueryResult,
    RigId, negotiate_payload_version,
};
use crate::source::RigCapabilities;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

/// How long the hub waits for the authentication frame.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound every WebSocket write so a peer that stops reading cannot pin a
/// connection task (or a pairing rollback) forever.
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Authenticated plugins send an application heartbeat every 30 seconds.
/// Four intervals provide a full scheduling margin after three missed beats
/// before retiring a stale registry entry.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Bound work awaiting one rig so a stalled socket cannot accumulate an
/// arbitrary number of N.I.N.A. reads or commands.
const MAX_PENDING_QUERIES: usize = 32;

/// Default wait for a rig to answer a query.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(20);

/// One live, authenticated rig connection.
pub struct RigConnection {
    pub telescope_id: i64,
    /// Client-issued identity for this N.I.N.A. plugin process/profile
    /// lifecycle. It survives transport reconnects but changes on a genuine
    /// plugin restart, so long-lived consumers know when retained state is no
    /// longer applicable.
    pub session_id: Uuid,
    /// N.I.N.A. profile identity paired with this credential/session.
    pub profile_id: Uuid,
    /// Server-issued identity for this exact WebSocket generation. The
    /// client's session ID intentionally survives reconnects, so it cannot be
    /// used to decide whether a retiring handler still owns the live slot.
    pub connection_id: Uuid,
    /// Additive response-payload contract selected during the hello exchange.
    /// Version 1 means an older, unmarked Direct client.
    pub payload_version: u16,
    pub capabilities: RigCapabilities,
    pub profile_name: String,
    outgoing: mpsc::Sender<DirectMessage>,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<QueryResult>>>,
    /// Priority control path for replacement/revocation. It is deliberately
    /// separate from ordinary query traffic so no stale command can overtake
    /// a close request.
    close_request: watch::Sender<Option<CloseRequest>>,
}

#[derive(Clone)]
struct CloseRequest {
    reason: String,
    retryable: bool,
}

struct PendingQueryGuard<'a> {
    connection: &'a RigConnection,
    id: Uuid,
    sent: bool,
    completed: bool,
}

impl Drop for PendingQueryGuard<'_> {
    fn drop(&mut self) {
        self.connection.remove_pending(&self.id);
        if self.sent && !self.completed {
            // The plugin advances its private history cursor when it writes a
            // result. If this future is cancelled or times out before the Hub
            // consumes that result, only a physical reconnect can rewind the
            // unacknowledged delta safely.
            self.connection.request_close(
                "a Direct query was interrupted; reconnecting to replay history",
                true,
            );
        }
    }
}

impl RigConnection {
    /// Send a query and await its result. Errors are strings so callers can
    /// wrap them in their own error type.
    pub async fn query(&self, kind: QueryKind, timeout: Duration) -> Result<QueryResult, String> {
        if self.is_retiring() {
            return Err("rig connection is reconnecting".to_string());
        }
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| "connection poisoned")?;
            if pending.len() >= MAX_PENDING_QUERIES {
                return Err("rig connection is busy".to_string());
            }
            pending.insert(id, tx);
        }
        let mut pending_guard = PendingQueryGuard {
            connection: self,
            id,
            sent: false,
            completed: false,
        };
        // The rig must not execute this after the hub has stopped waiting.
        let expires_at = Some(crate::hub::db::unix_now() + timeout.as_secs() as i64);
        let sent = self.outgoing.try_send(DirectMessage::Query(QueryRequest {
            id,
            expires_at,
            kind,
        }));
        if let Err(error) = sent {
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => "rig connection is busy".to_string(),
                mpsc::error::TrySendError::Closed(_) => "rig connection is closed".to_string(),
            });
        }
        pending_guard.sent = true;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                pending_guard.completed = true;
                Ok(result)
            }
            Ok(Err(_)) => Err("rig connection closed while waiting".to_string()),
            Err(_) => Err("rig did not answer in time".to_string()),
        }
    }

    fn remove_pending(&self, id: &Uuid) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(id);
        }
    }

    pub(crate) fn resolve(&self, result: QueryResult) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(tx) = pending.remove(&result.id)
        {
            let _ = tx.send(result);
        }
    }

    fn should_send(&self, message: &DirectMessage) -> bool {
        let DirectMessage::Query(query) = message else {
            return true;
        };
        self.pending
            .lock()
            .is_ok_and(|pending| pending.contains_key(&query.id))
    }

    /// Ask the write loop to send a final error frame and close the socket.
    /// `retryable` tells the client whether reconnecting can help.
    pub(crate) fn request_close(&self, reason: &str, retryable: bool) {
        self.close_request.send_if_modified(|request| {
            if request.is_some() {
                return false;
            }
            *request = Some(CloseRequest {
                reason: reason.to_string(),
                retryable,
            });
            true
        });
    }

    fn is_retiring(&self) -> bool {
        self.close_request.borrow().is_some()
    }

    #[cfg(test)]
    fn close_requested(&self) -> bool {
        self.close_request.borrow().is_some()
    }

    /// Test-only connection with no live socket behind it. The receiver end
    /// of the outgoing channel is returned so tests can observe (or ignore)
    /// query traffic.
    #[cfg(test)]
    pub(crate) fn stub(
        telescope_id: i64,
        connection_id: Uuid,
    ) -> (Arc<RigConnection>, mpsc::Receiver<DirectMessage>) {
        Self::stub_with_session(telescope_id, connection_id, connection_id)
    }

    #[cfg(test)]
    pub(crate) fn stub_with_session(
        telescope_id: i64,
        connection_id: Uuid,
        session_id: Uuid,
    ) -> (Arc<RigConnection>, mpsc::Receiver<DirectMessage>) {
        Self::stub_with_identity(telescope_id, connection_id, session_id, Uuid::nil())
    }

    #[cfg(test)]
    pub(crate) fn stub_with_identity(
        telescope_id: i64,
        connection_id: Uuid,
        session_id: Uuid,
        profile_id: Uuid,
    ) -> (Arc<RigConnection>, mpsc::Receiver<DirectMessage>) {
        let (outgoing, rx) = mpsc::channel(MAX_PENDING_QUERIES);
        let (close_request, _close_requests) = watch::channel(None);
        (
            Arc::new(RigConnection {
                telescope_id,
                session_id,
                profile_id,
                connection_id,
                payload_version: CURRENT_PAYLOAD_VERSION,
                capabilities: crate::source::RigCapabilities::all(),
                profile_name: format!("stub-{telescope_id}"),
                outgoing,
                pending: Mutex::new(HashMap::new()),
                close_request,
            }),
            rx,
        )
    }
}

/// Live connections by telescope. Shared between the WebSocket handler and
/// whatever consumes rigs (chat updaters, bot commands, tests).
#[derive(Default)]
pub struct RigConnections {
    inner: Mutex<HashMap<i64, Arc<RigConnection>>>,
    /// Server-side lifecycle boundary. Transport reconnects leave this value
    /// alone; explicit credential rotation/revocation advances it so no later
    /// socket can inherit updater state from the old trust relationship.
    lifecycle_generations: Mutex<HashMap<i64, u64>>,
}

impl RigConnections {
    pub fn get(&self, telescope_id: i64) -> Option<Arc<RigConnection>> {
        self.inner.lock().ok()?.get(&telescope_id).cloned()
    }

    /// Read a connection and its trust epoch as one coherent snapshot. The
    /// lock order matches `insert_if_generation`, so a revoke cannot advance
    /// the epoch between validation and selecting a same-identity socket.
    pub(crate) fn get_if_generation(
        &self,
        telescope_id: i64,
        expected_generation: u64,
    ) -> Option<Arc<RigConnection>> {
        let generations = self.lifecycle_generations.lock().ok()?;
        let current = generations.get(&telescope_id).copied().unwrap_or(0);
        if current != expected_generation {
            return None;
        }
        self.inner.lock().ok()?.get(&telescope_id).cloned()
    }

    pub fn connected_telescopes(&self) -> Vec<i64> {
        self.inner
            .lock()
            .map(|map| map.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Insert a connection, returning the one it replaced (if any).
    #[cfg(test)]
    pub(crate) fn insert(&self, connection: Arc<RigConnection>) -> Option<Arc<RigConnection>> {
        self.inner
            .lock()
            .ok()?
            .insert(connection.telescope_id, connection)
    }

    /// Insert only while the server-side trust generation observed during
    /// authentication is still current. The generation lock is held through
    /// the connection-map write, using the same lock order as `revoke`, so an
    /// owner revoke/delete cannot race a completed handshake back into place.
    pub(crate) fn insert_if_generation(
        &self,
        connection: Arc<RigConnection>,
        expected_generation: u64,
    ) -> Result<Option<Arc<RigConnection>>, Arc<RigConnection>> {
        let Ok(generations) = self.lifecycle_generations.lock() else {
            return Err(connection);
        };
        let current = generations
            .get(&connection.telescope_id)
            .copied()
            .unwrap_or(0);
        if current != expected_generation {
            return Err(connection);
        }
        let Ok(mut connections) = self.inner.lock() else {
            return Err(connection);
        };
        Ok(connections.insert(connection.telescope_id, connection))
    }

    pub(crate) fn lifecycle_generation(&self, telescope_id: i64) -> u64 {
        self.lifecycle_generations
            .lock()
            .ok()
            .and_then(|generations| generations.get(&telescope_id).copied())
            .unwrap_or(0)
    }

    /// Advance the logical trust lifecycle and force-remove the connection.
    /// The caller receives the socket so it can send a final close reason.
    pub(crate) fn revoke(&self, telescope_id: i64) -> Option<Arc<RigConnection>> {
        if let Ok(mut generations) = self.lifecycle_generations.lock() {
            let generation = generations.entry(telescope_id).or_default();
            *generation = generation.saturating_add(1);
        }
        self.inner.lock().ok()?.remove(&telescope_id)
    }

    /// Return whether this exact WebSocket generation still owns the slot.
    pub(crate) fn is_current(&self, telescope_id: i64, connection_id: Uuid) -> bool {
        self.inner.lock().is_ok_and(|map| {
            map.get(&telescope_id)
                .is_some_and(|connection| connection.connection_id == connection_id)
        })
    }

    /// Remove a connection only if this exact WebSocket generation still owns
    /// the slot; a retiring handler must not evict its replacement.
    pub(crate) fn remove_if_current(&self, telescope_id: i64, connection_id: Uuid) {
        if let Ok(mut map) = self.inner.lock()
            && map
                .get(&telescope_id)
                .is_some_and(|connection| connection.connection_id == connection_id)
        {
            map.remove(&telescope_id);
        }
    }

    /// Remove a socket without advancing the logical trust generation. This
    /// forces the plugin's physical transport replay for an updater rebuild
    /// while preserving the authenticated session/profile identity.
    pub(crate) fn disconnect_for_replay(&self, telescope_id: i64) -> Option<Arc<RigConnection>> {
        self.inner.lock().ok()?.remove(&telescope_id)
    }
}

pub async fn direct_ws(
    State(state): State<HubState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let ip = crate::hub::server::client_ip(&headers, peer, state.config.trust_x_forwarded_for);
    upgrade.on_upgrade(move |socket| handle_socket(state, socket, ip))
}

async fn recv_message(socket: &mut WebSocket) -> Option<DirectMessage> {
    while let Some(frame) = socket.recv().await {
        match frame {
            Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                Ok(message) => return Some(message),
                Err(_) => return None,
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            _ => return None,
        }
    }
    None
}

async fn send_message(socket: &mut WebSocket, message: &DirectMessage) -> bool {
    let Ok(json) = serde_json::to_string(message) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            SOCKET_WRITE_TIMEOUT,
            socket.send(Message::Text(json.into())),
        )
        .await,
        Ok(Ok(()))
    )
}

async fn send_close(socket: &mut WebSocket) {
    let _ = tokio::time::timeout(SOCKET_WRITE_TIMEOUT, socket.send(Message::Close(None))).await;
}

async fn reject(mut socket: WebSocket, message: &str, retryable: bool) {
    let _ = send_message(
        &mut socket,
        &DirectMessage::Error {
            message: message.to_string(),
            retryable,
        },
    )
    .await;
    send_close(&mut socket).await;
}

/// What to undo if the PairResult carrying the freshly minted credential
/// never reaches the client: restore the token, drop the orphan credential.
struct PairRollback {
    token: String,
    credential: String,
}

/// Why a handshake was refused.
///
/// The distinction matters: a client fault is the client's to fix and counts
/// against the per-IP guessing budget, while a hub fault must not tell a
/// legitimate rig to stop retrying, and must not spend its budget either.
enum AuthFailure {
    Client(String),
    Internal {
        message: String,
        /// Pairing may have committed revocation of every prior credential
        /// before a later hub-side failure. The live socket/updater must then
        /// be retired even though authentication never produced a response.
        retire_telescope: Option<i64>,
    },
}

impl AuthFailure {
    fn client(message: impl Into<String>) -> Self {
        Self::Client(message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            retire_telescope: None,
        }
    }

    fn internal_and_retire(message: impl Into<String>, telescope_id: i64) -> Self {
        Self::Internal {
            message: message.into(),
            retire_telescope: Some(telescope_id),
        }
    }
}

/// Authenticate the first frame: either a pairing exchange or a credential
/// presentation. Returns the telescope, the hello, the response to send,
/// and — for pairing — the rollback data for a failed delivery.
fn authenticate(
    state: &HubState,
    first: &DirectMessage,
) -> Result<(i64, ClientHello, DirectMessage, Option<PairRollback>), AuthFailure> {
    match first {
        DirectMessage::Pair(PairRequest {
            pairing_token,
            hello,
        }) => {
            check_hello(hello).map_err(AuthFailure::Client)?;
            let telescope_id = state
                .db
                .consume_pairing_token(pairing_token)
                .map_err(|e| AuthFailure::internal(format!("database error: {e}")))?
                .ok_or_else(|| {
                    AuthFailure::client("pairing token is unknown, expired, or already used")
                })?;

            // The token is spent now, and it was committed in its own
            // transaction. Every later failure has to hand it back, or a hub
            // fault burns the user's one-time code for good.
            let restore_token = |context: String, retire_live_connection: bool| {
                if let Err(error) = state.db.restore_pairing_token(pairing_token) {
                    eprintln!(
                        "Could not restore the pairing token for telescope {telescope_id} after {context}: {error}"
                    );
                }
                if retire_live_connection {
                    AuthFailure::internal_and_retire(context, telescope_id)
                } else {
                    AuthFailure::internal(context)
                }
            };

            // Pairing rotates: earlier credentials die so a retired install can
            // never reconnect and displace the new rig. That is a security
            // invariant, so a failure here fails the pairing instead of
            // quietly leaving the old credentials live.
            match state.db.revoke_rig_credentials(telescope_id) {
                Ok(revoked) if revoked > 0 => println!(
                    "Pairing for telescope {telescope_id} revoked {revoked} earlier credential(s)"
                ),
                Ok(_) => {}
                Err(error) => {
                    return Err(restore_token(
                        format!("could not revoke earlier credentials: {error}"),
                        false,
                    ));
                }
            }

            let credential = match state.db.create_rig_credential(
                telescope_id,
                &hello.node_id.to_string(),
                &hello.profile_id.to_string(),
            ) {
                Ok(credential) => credential,
                Err(error) => {
                    return Err(restore_token(
                        format!("could not create the rig credential: {error}"),
                        true,
                    ));
                }
            };
            let response = DirectMessage::PairResult(crate::direct::protocol::PairResult {
                credential: credential.clone(),
                agent_hello: agent_hello(hello),
            });
            Ok((
                telescope_id,
                hello.clone(),
                response,
                Some(PairRollback {
                    token: pairing_token.clone(),
                    credential,
                }),
            ))
        }
        DirectMessage::Auth(AuthRequest { credential, hello }) => {
            check_hello(hello).map_err(AuthFailure::Client)?;
            let row = state
                .db
                .lookup_rig_credential(credential)
                .map_err(|e| AuthFailure::internal(format!("database error: {e}")))?
                .ok_or_else(|| AuthFailure::client("credential is unknown or revoked"))?;
            // The credential is bound to the installation that paired it.
            if row.node_id != hello.node_id.to_string() {
                return Err(AuthFailure::client(
                    "credential is bound to a different node",
                ));
            }
            if row.profile_id != hello.profile_id.to_string() {
                return Err(AuthFailure::client(
                    "credential is bound to a different N.I.N.A. profile",
                ));
            }
            let response = DirectMessage::AgentHello(agent_hello(hello));
            Ok((row.telescope_id, hello.clone(), response, None))
        }
        _ => Err(AuthFailure::client("first frame must be pair or auth")),
    }
}

fn check_hello(hello: &ClientHello) -> Result<(), String> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported protocol version {} (hub speaks {PROTOCOL_VERSION})",
            hello.protocol_version
        ));
    }
    // A client newer than this hub is clamped in `agent_hello` rather than
    // refused. Its additive fields are ignored by the permissive serde
    // defaults, so refusing would break a rig that would otherwise work.
    if hello.payload_version < LEGACY_PAYLOAD_VERSION {
        return Err(format!(
            "unsupported payload version {} (hub supports {LEGACY_PAYLOAD_VERSION} and newer)",
            hello.payload_version
        ));
    }
    if hello.session_id.is_nil() {
        return Err("Direct session ID must not be nil".to_string());
    }
    Ok(())
}

fn agent_hello(hello: &ClientHello) -> AgentHello {
    AgentHello {
        protocol_version: PROTOCOL_VERSION,
        payload_version: negotiate_payload_version(hello.payload_version),
        connection_id: Uuid::new_v4(),
        rig_id: RigId {
            node_id: hello.node_id,
            profile_id: hello.profile_id,
        },
    }
}

async fn handle_socket(state: HubState, mut socket: WebSocket, client_ip: String) {
    // Token/credential guessing protection: an IP with too many recent
    // failures is refused before any database work.
    if state.limits.direct_auth.blocked(&client_ip) {
        reject(
            socket,
            "too many failed attempts; try again in a minute",
            true,
        )
        .await;
        return;
    }

    let first = match tokio::time::timeout(HANDSHAKE_TIMEOUT, recv_message(&mut socket)).await {
        Ok(Some(message)) => message,
        _ => {
            reject(socket, "expected an authentication frame", true).await;
            return;
        }
    };

    let (telescope_id, hello, response, pair_rollback) = match authenticate(&state, &first) {
        Ok(ok) => ok,
        Err(AuthFailure::Client(message)) => {
            state.limits.direct_auth.check(&client_ip);
            reject(socket, &message, false).await;
            return;
        }
        Err(AuthFailure::Internal {
            message,
            retire_telescope,
        }) => {
            if let Some(telescope_id) = retire_telescope {
                if let Some(connection) = state.rig_connections.revoke(telescope_id) {
                    connection.request_close(
                        "pairing failed after earlier credentials were revoked",
                        false,
                    );
                }
                state.refresh_updater(telescope_id).await;
            }
            // A hub-side fault is not the rig's doing: tell it to come back,
            // and do not spend its per-IP budget on our outage.
            eprintln!("Direct handshake failed for {client_ip}: {message}");
            reject(
                socket,
                "the hub is temporarily unavailable; retry shortly",
                true,
            )
            .await;
            return;
        }
    };
    let connection_id = match &response {
        DirectMessage::PairResult(result) => result.agent_hello.connection_id,
        DirectMessage::AgentHello(hello) => hello.connection_id,
        _ => unreachable!("authentication only returns hello responses"),
    };
    let is_pairing = pair_rollback.is_some();
    if !send_message(&mut socket, &response).await {
        // A pairing reply that never arrived means the client still has no
        // credential: give the token back and drop the orphan credential so
        // the client's retry with the same token works.
        if let Some(rollback) = pair_rollback.as_ref() {
            let mut failures = Vec::new();
            if let Err(error) = state.db.delete_rig_credential(&rollback.credential) {
                failures.push(format!("credential not deleted: {error}"));
            }
            if let Err(error) = state.db.restore_pairing_token(&rollback.token) {
                failures.push(format!("pairing token not restored: {error}"));
            }
            if failures.is_empty() {
                println!(
                    "Pairing reply for telescope {telescope_id} was not delivered; rolled back"
                );
            } else {
                // Claiming a clean rollback when it failed sends whoever is
                // debugging a stuck pairing in exactly the wrong direction.
                eprintln!(
                    "Pairing reply for telescope {telescope_id} was not delivered and the rollback did not complete ({})",
                    failures.join("; ")
                );
            }
        }
        // `authenticate` already revoked every older credential before it
        // minted the replacement. A failed PairResult cannot restore those
        // credentials, so the old live socket and updater must not outlive
        // the now-revoked database trust relationship.
        if is_pairing {
            if let Some(replaced) = state.rig_connections.revoke(telescope_id) {
                replaced.request_close("pairing failed after credentials rotated", false);
            }
            state.refresh_updater(telescope_id).await;
        }
        return;
    }

    // A successful pairing is a new trust lifecycle even when the same
    // running plugin reuses its profile/session IDs. Retire the old updater
    // generation before adopting the freshly credentialed socket.
    let authenticated_generation = if is_pairing {
        if let Some(replaced) = state.rig_connections.revoke(telescope_id) {
            replaced.request_close("replaced after a new pairing", false);
        }
        // Capture the epoch created by this pairing before awaiting updater
        // retirement. Any owner revoke or competing pairing during that wait
        // then makes the later conditional insert fail closed.
        let generation = state.rig_connections.lifecycle_generation(telescope_id);
        state.refresh_updater(telescope_id).await;
        generation
    } else {
        // Sample before the final database check. A revoke after that check is
        // still caught by `insert_if_generation`; a revoke before it removes
        // the credential that is checked below.
        state.rig_connections.lifecycle_generation(telescope_id)
    };

    let authenticated_credential = match (&first, pair_rollback.as_ref()) {
        (DirectMessage::Auth(AuthRequest { credential, .. }), _) => credential.as_str(),
        (DirectMessage::Pair(_), Some(rollback)) => rollback.credential.as_str(),
        _ => unreachable!("authenticated pairing must retain its credential"),
    };
    let still_valid = state
        .db
        .lookup_rig_credential(authenticated_credential)
        .ok()
        .flatten()
        .is_some_and(|row| {
            row.telescope_id == telescope_id
                && row.node_id == hello.node_id.to_string()
                && row.profile_id == hello.profile_id.to_string()
        });
    if !still_valid {
        reject(
            socket,
            "credential was revoked while this connection authenticated",
            false,
        )
        .await;
        return;
    }

    // Everything downstream must reason about what both peers agreed to, not
    // what the client asked for.
    let payload_version = negotiate_payload_version(hello.payload_version);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(MAX_PENDING_QUERIES);
    let (close_request, mut close_requests) = watch::channel(None);
    let connection = Arc::new(RigConnection {
        telescope_id,
        session_id: hello.session_id,
        profile_id: hello.profile_id,
        connection_id,
        payload_version,
        capabilities: hello.capabilities,
        profile_name: hello.profile_name.clone(),
        outgoing: outgoing_tx,
        pending: Mutex::new(HashMap::new()),
        close_request,
    });
    // A newer connection for the same telescope replaces the older one. The
    // old connection is told to close — its own task holds an Arc of its
    // connection, so only an explicit close ends its write loop.
    let compatibility = match payload_version {
        LEGACY_PAYLOAD_VERSION => "legacy",
        CURRENT_PAYLOAD_VERSION => "current",
        _ => "downgraded",
    };
    if hello.payload_version > CURRENT_PAYLOAD_VERSION {
        println!(
            "Rig for telescope {telescope_id} advertised payload v{}; this hub speaks v{CURRENT_PAYLOAD_VERSION}, so newer fields will be ignored",
            hello.payload_version
        );
    }
    let replaced = match state
        .rig_connections
        .insert_if_generation(connection.clone(), authenticated_generation)
    {
        Ok(replaced) => replaced,
        Err(_) => {
            reject(
                socket,
                "telescope access changed while this connection authenticated",
                false,
            )
            .await;
            return;
        }
    };
    if let Some(replaced) = replaced {
        replaced.request_close("replaced by a newer connection for this telescope", true);
        println!(
            "Rig for telescope {telescope_id} reconnected with payload v{payload_version} {compatibility}; replacing previous connection"
        );
    } else {
        println!(
            "Rig connected for telescope {telescope_id} ({}, payload v{payload_version} {compatibility})",
            hello.profile_name
        );
    }
    state
        .adopt_updater_identity(
            telescope_id,
            connection.session_id,
            connection.profile_id,
            authenticated_generation,
        )
        .await;

    let idle = tokio::time::sleep(CLIENT_IDLE_TIMEOUT);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            biased;
            changed = close_requests.changed() => {
                if changed.is_err() {
                    break;
                }
                let request = close_requests.borrow_and_update().clone();
                if let Some(request) = request {
                    let _ = send_message(
                        &mut socket,
                        &DirectMessage::Error {
                            message: request.reason,
                            retryable: request.retryable,
                        },
                    )
                    .await;
                    send_close(&mut socket).await;
                    break;
                }
            }
            _ = &mut idle => {
                println!(
                    "Rig connection {connection_id} for telescope {telescope_id} timed out after {} seconds without an inbound frame",
                    CLIENT_IDLE_TIMEOUT.as_secs()
                );
                break;
            }
            inbound = socket.recv() => {
                let Some(Ok(frame)) = inbound else { break };
                idle.as_mut().reset(tokio::time::Instant::now() + CLIENT_IDLE_TIMEOUT);
                match frame {
                    Message::Text(text) => {
                        match serde_json::from_str::<DirectMessage>(&text) {
                            Ok(DirectMessage::QueryResult(result)) => connection.resolve(result),
                            Ok(DirectMessage::Heartbeat { seq }) => {
                                if state
                                    .rig_connections
                                    .is_current(telescope_id, connection_id)
                                {
                                    if !send_message(
                                        &mut socket,
                                        &DirectMessage::HeartbeatAck { seq },
                                    )
                                    .await
                                    {
                                        break;
                                    }
                                } else {
                                    connection.request_close(
                                        "this connection no longer owns the telescope slot",
                                        true,
                                    );
                                }
                            }
                            Ok(_) | Err(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            outbound = outgoing_rx.recv() => {
                match outbound {
                    Some(message) => {
                        if !connection.should_send(&message) {
                            continue;
                        }
                        if !send_message(&mut socket, &message).await {
                            break;
                        }
                    }
                    // Sender dropped: this connection was replaced.
                    None => break,
                }
            }
        }
    }

    // Close the queue before clearing waiters. Any query racing teardown will
    // then fail its send and remove its own pending entry instead of waiting
    // for the full query timeout.
    drop(outgoing_rx);
    state
        .rig_connections
        .remove_if_current(telescope_id, connection_id);
    if let Ok(mut pending) = connection.pending.lock() {
        pending.clear();
    }
    println!("Rig connection {connection_id} disconnected for telescope {telescope_id}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::config::HubConfig;
    use crate::hub::db::Db;
    use crate::hub::store::UserRow;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// Hub with a telescope, an issued pairing token, and the direct route.
    async fn spawn_hub() -> (String, Db, HubState, i64, String) {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&UserRow {
            discord_user_id: 1,
            username: "admin".to_string(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        db.register_guild(100, "g", 1).unwrap();
        let telescope = db.create_telescope(1, "c925").unwrap();
        let pairing_token = db.issue_pairing_token(telescope.id, 1).unwrap();

        let state = HubState::build(HubConfig::default(), db.clone(), None).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = crate::hub::server::router(state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (
            format!("http://{addr}"),
            db,
            state,
            telescope.id,
            pairing_token,
        )
    }

    #[test]
    fn unmarked_websocket_hello_is_accepted_and_echoed_as_legacy() {
        let message: DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/client-hello-legacy.json"
        ))
        .unwrap();
        let DirectMessage::ClientHello(hello) = message else {
            panic!("expected client hello");
        };

        check_hello(&hello).unwrap();
        assert_eq!(hello.payload_version, LEGACY_PAYLOAD_VERSION);
        assert_eq!(agent_hello(&hello).payload_version, LEGACY_PAYLOAD_VERSION);
    }

    fn hello_fixture() -> ClientHello {
        let message: DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/client-hello.json"
        ))
        .unwrap();
        let DirectMessage::ClientHello(hello) = message else {
            panic!("expected client hello");
        };
        hello
    }

    #[test]
    fn websocket_hello_clamps_newer_payload_versions_instead_of_refusing() {
        let mut hello = hello_fixture();
        hello.payload_version = CURRENT_PAYLOAD_VERSION + 1;

        // A plugin from a future release still connects; the hub simply tells
        // it which contract was selected so it can withhold newer payloads.
        check_hello(&hello).unwrap();
        assert_eq!(agent_hello(&hello).payload_version, CURRENT_PAYLOAD_VERSION);
    }

    #[test]
    fn websocket_hello_rejects_payload_versions_below_the_legacy_floor() {
        let mut hello = hello_fixture();
        hello.payload_version = LEGACY_PAYLOAD_VERSION - 1;

        let error = check_hello(&hello).unwrap_err();
        assert!(error.contains("unsupported payload version"));
    }

    #[test]
    fn websocket_hello_rejects_nil_session_identity() {
        let mut hello = hello_fixture();
        hello.session_id = Uuid::nil();

        let error = check_hello(&hello).unwrap_err();
        assert!(error.contains("session ID"));
    }

    #[tokio::test]
    async fn websocket_pairs_unmarked_legacy_payload_client() {
        let (hub_base, _db, state, telescope_id, pairing_token) = spawn_hub().await;
        let url = format!("{}/v1/direct", hub_base.replacen("http://", "ws://", 1));
        let (mut socket, _) = connect_async(&url).await.unwrap();
        let fixture: DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/client-hello-legacy.json"
        ))
        .unwrap();
        let DirectMessage::ClientHello(hello) = fixture else {
            panic!("expected client hello");
        };
        let request = DirectMessage::Pair(PairRequest {
            pairing_token,
            hello,
        });
        socket
            .send(WsMessage::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();

        let frame = socket.next().await.unwrap().unwrap();
        let response: DirectMessage = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        let DirectMessage::PairResult(result) = response else {
            panic!("expected pair result");
        };
        assert_eq!(result.agent_hello.payload_version, LEGACY_PAYLOAD_VERSION);

        let connected = state
            .rig_connections
            .get(telescope_id)
            .expect("legacy rig connected");
        assert_eq!(connected.payload_version, LEGACY_PAYLOAD_VERSION);
        socket.close(None).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn authenticated_idle_connection_is_retired() {
        let (hub_base, _db, state, telescope_id, pairing_token) = spawn_hub().await;
        let url = format!("{}/v1/direct", hub_base.replacen("http://", "ws://", 1));
        let (mut socket, _) = connect_async(&url).await.unwrap();
        socket
            .send(WsMessage::Text(
                serde_json::to_string(&DirectMessage::Pair(PairRequest {
                    pairing_token,
                    hello: hello_fixture(),
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let _ = socket.next().await.expect("pair response").unwrap();
        tokio::task::yield_now().await;
        assert!(state.rig_connections.get(telescope_id).is_some());

        tokio::time::advance(CLIENT_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(state.rig_connections.get(telescope_id).is_none());
    }

    #[test]
    fn request_close_uses_priority_control_path() {
        let connections = RigConnections::default();
        let (connection, rx) = RigConnection::stub(7, Uuid::new_v4());
        std::mem::forget(rx);
        connections.insert(connection);

        let removed = connections.revoke(7).expect("connection present");
        removed.request_close("credentials revoked", false);
        assert!(connections.get(7).is_none());
        assert!(removed.close_requested());

        let request = removed
            .close_request
            .borrow()
            .clone()
            .expect("close request");
        assert!(request.reason.contains("revoked"));
        assert!(!request.retryable);
    }

    #[tokio::test]
    async fn pending_queries_are_bounded_and_cancel_safe() {
        let (connection, mut rx) = RigConnection::stub(7, Uuid::new_v4());
        let mut queries = Vec::new();
        for _ in 0..MAX_PENDING_QUERIES {
            let connection = connection.clone();
            queries.push(tokio::spawn(async move {
                connection
                    .query(QueryKind::CameraInfo, Duration::from_secs(60))
                    .await
            }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if connection.pending.lock().unwrap().len() == MAX_PENDING_QUERIES {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending queries fill the bounded queue");

        let error = connection
            .query(QueryKind::CameraInfo, Duration::from_secs(1))
            .await
            .expect_err("pending query limit is enforced");
        assert!(error.contains("busy"));

        for query in queries {
            query.abort();
            let _ = query.await;
        }
        assert!(connection.pending.lock().unwrap().is_empty());

        // A sent query whose waiter was cancelled makes this physical socket
        // retire. The plugin may already have advanced its history cursor, so
        // accepting another query here could silently skip that delta rather
        // than forcing a reconnect/replay.
        let queue_error = connection
            .query(QueryKind::CameraInfo, Duration::from_secs(1))
            .await
            .expect_err("cancelled sent queries retire the transport");
        assert!(queue_error.contains("reconnecting"));
        assert!(connection.close_requested());
        assert!(connection.pending.lock().unwrap().is_empty());

        let mut queued = 0;
        while let Ok(message) = rx.try_recv() {
            assert!(!connection.should_send(&message));
            queued += 1;
        }
        assert_eq!(queued, MAX_PENDING_QUERIES);
    }

    #[tokio::test]
    async fn updater_refresh_forces_a_physical_transport_replay() {
        let db = Db::open_in_memory().unwrap();
        let state = HubState::build(HubConfig::default(), db, None).unwrap();
        let (connection, outgoing) = RigConnection::stub(7, Uuid::new_v4());
        std::mem::forget(outgoing);
        state.rig_connections.insert(connection.clone());

        // Even if a history query already returned and is blocked later in
        // chart/chat delivery, rebuilding its updater closes this transport.
        // The plugin's next BeginDirectTransport call rewinds that confirmed
        // delta instead of baselining it away.
        state.refresh_updater(7).await;

        assert!(state.rig_connections.get(7).is_none());
        assert!(connection.close_requested());
        let request = connection.close_request.borrow().clone().unwrap();
        assert!(request.retryable);
        assert!(request.reason.contains("reconnecting"));
    }

    #[test]
    fn retiring_generation_cannot_remove_its_replacement() {
        let connections = RigConnections::default();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();
        let (old, old_rx) = RigConnection::stub(7, old_id);
        let (replacement, replacement_rx) = RigConnection::stub(7, new_id);
        std::mem::forget(old_rx);
        std::mem::forget(replacement_rx);

        connections.insert(old);
        connections.insert(replacement.clone());
        connections.remove_if_current(7, old_id);

        let current = connections.get(7).expect("replacement remains current");
        assert!(Arc::ptr_eq(&current, &replacement));
        assert_eq!(current.connection_id, new_id);

        connections.remove_if_current(7, new_id);
        assert!(connections.get(7).is_none());
    }

    #[tokio::test]
    async fn reconnect_with_same_client_session_keeps_new_socket_registered() {
        let (hub_base, _db, state, telescope_id, pairing_token) = spawn_hub().await;
        let url = format!("{}/v1/direct", hub_base.replacen("http://", "ws://", 1));
        let hello = hello_fixture();

        let (mut first, _) = connect_async(&url).await.unwrap();
        first
            .send(WsMessage::Text(
                serde_json::to_string(&DirectMessage::Pair(PairRequest {
                    pairing_token,
                    hello: hello.clone(),
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let first_frame = first.next().await.unwrap().unwrap();
        let first_response: DirectMessage =
            serde_json::from_str(first_frame.to_text().unwrap()).unwrap();
        let DirectMessage::PairResult(first_result) = first_response else {
            panic!("expected pair result");
        };

        let (mut replacement, _) = connect_async(&url).await.unwrap();
        replacement
            .send(WsMessage::Text(
                serde_json::to_string(&DirectMessage::Auth(AuthRequest {
                    credential: first_result.credential,
                    // A plugin load intentionally keeps one client session ID
                    // over all of its reconnect attempts.
                    hello,
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let replacement_frame = replacement.next().await.unwrap().unwrap();
        let replacement_response: DirectMessage =
            serde_json::from_str(replacement_frame.to_text().unwrap()).unwrap();
        let DirectMessage::AgentHello(replacement_hello) = replacement_response else {
            panic!("expected agent hello");
        };

        // Let the retired handler send its replacement error and close frame,
        // then finish its cleanup. That cleanup must not unregister the new
        // socket even though both auth frames carried the same client session.
        let retired_error = tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("retired connection receives an error")
            .expect("retired connection error frame")
            .unwrap();
        let error: DirectMessage = serde_json::from_str(retired_error.to_text().unwrap()).unwrap();
        assert!(matches!(
            error,
            DirectMessage::Error {
                retryable: true,
                ..
            }
        ));
        let retired_close = tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("retired connection receives a close frame");
        assert!(matches!(
            retired_close,
            Some(Ok(WsMessage::Close(_))) | None
        ));
        tokio::task::yield_now().await;

        let current = state
            .rig_connections
            .get(telescope_id)
            .expect("replacement remains registered");
        assert_eq!(current.connection_id, replacement_hello.connection_id);

        replacement
            .send(WsMessage::Text(
                serde_json::to_string(&DirectMessage::Heartbeat { seq: 41 })
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
        let heartbeat_frame = replacement.next().await.unwrap().unwrap();
        let heartbeat: DirectMessage =
            serde_json::from_str(heartbeat_frame.to_text().unwrap()).unwrap();
        assert!(matches!(heartbeat, DirectMessage::HeartbeatAck { seq: 41 }));

        replacement.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn bad_pairing_token_rejected() {
        let (hub_base, _db, _state, _telescope_id, _token) = spawn_hub().await;
        let url = format!("{}/v1/direct", hub_base.replacen("http://", "ws://", 1));
        let (mut socket, _) = connect_async(&url).await.unwrap();
        let fixture: DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/client-hello.json"
        ))
        .unwrap();
        let DirectMessage::ClientHello(hello) = fixture else {
            panic!("expected client hello");
        };
        let request = DirectMessage::Pair(PairRequest {
            pairing_token: "cspt_bogus".to_string(),
            hello,
        });
        socket
            .send(WsMessage::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        let response: DirectMessage = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert!(matches!(
            response,
            DirectMessage::Error {
                retryable: false,
                ..
            }
        ));
    }
}
