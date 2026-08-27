//! Per-rig chat updaters on the hub.
//!
//! A reconcile loop compares live `/v1/direct` connections against running
//! `ChatUpdater` tasks: a connected telescope with a routed channel gets an
//! updater. That updater follows replacement Direct sockets and survives a
//! disconnect, retaining delivery/dedup state while its source backs off. A
//! route or delivery-configuration change still replaces the task.

use super::db::Db;
use super::direct_server::RigConnections;
use super::direct_source::DirectRigSource;
use crate::chat::{ChatMessage, ChatServiceManager, ChatTarget};
use crate::chat_updater::ChatUpdater;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How often the reconcile loop runs.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Poll interval handed to each ChatUpdater.
const UPDATER_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a scope must stay disconnected before chat hears about it.
/// Absorbs hub deploys, rig reconnects, and plugin flapping.
const PRESENCE_OFFLINE_GRACE: Duration = Duration::from_secs(90);

/// What chat currently believes about one scope, and what we observe.
struct Presence {
    /// The state chat was last told (None until adopted or announced).
    announced: Option<bool>,
    /// The state we currently observe.
    current: bool,
    /// When `current` last changed (or was first observed).
    since: Instant,
}

/// A presence transition worth telling chat about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceEvent {
    pub telescope_id: i64,
    pub telescope_name: String,
    pub online: bool,
}

struct RunningUpdater {
    session_id: Uuid,
    profile_id: Uuid,
    lifecycle_generation: u64,
    /// The config the updater was built with. A change in the database
    /// (destinations added or removed, cooldown adjusted) restarts the
    /// updater, which otherwise freezes its config at construction.
    channels: Vec<i64>,
    image_cooldown_seconds: i64,
    handle: tokio::task::JoinHandle<()>,
}

pub struct UpdaterManager {
    db: Db,
    connections: Arc<RigConnections>,
    chat_manager: Arc<ChatServiceManager>,
    running: Mutex<HashMap<i64, RunningUpdater>>,
    /// Serialize periodic reconciliation with request-driven invalidation.
    /// Route/trust mutation handlers wait for this gate, which makes every
    /// retired updater task a real cancellation barrier before they return.
    reconcile_gate: tokio::sync::Mutex<()>,
    presence: Mutex<HashMap<i64, Presence>>,
    offline_grace: Duration,
}

impl UpdaterManager {
    pub fn new(
        db: Db,
        connections: Arc<RigConnections>,
        chat_manager: Arc<ChatServiceManager>,
    ) -> Self {
        Self {
            db,
            connections,
            chat_manager,
            running: Mutex::new(HashMap::new()),
            reconcile_gate: tokio::sync::Mutex::new(()),
            presence: Mutex::new(HashMap::new()),
            offline_grace: PRESENCE_OFFLINE_GRACE,
        }
    }

    /// Shrink the offline grace for tests.
    #[cfg(test)]
    fn with_offline_grace(mut self, grace: Duration) -> Self {
        self.offline_grace = grace;
        self
    }

    /// Compare observed connection state against what chat believes, and
    /// return the transitions worth announcing.
    ///
    /// The first observation of a scope is adopted silently — after a hub
    /// restart every scope is "first observed", so deploys never generate
    /// chat traffic. A disconnect is announced only after the grace period,
    /// which also absorbs reconnect flapping; a reconnect is announced
    /// immediately once the scope was believed (or adopted as) offline.
    pub fn presence_events(&self) -> Vec<PresenceEvent> {
        let mut events = Vec::new();
        let Ok(mut presence) = self.presence.lock() else {
            return events;
        };
        let connected: std::collections::HashSet<i64> = self
            .connections
            .connected_telescopes()
            .into_iter()
            .collect();
        // Every telescope with destinations participates; others have no
        // audience to tell.
        let mut ids: std::collections::HashSet<i64> = connected.clone();
        ids.extend(presence.keys().copied());
        for id in ids {
            let observed = connected.contains(&id);
            let entry = presence.entry(id).or_insert(Presence {
                announced: None,
                current: observed,
                since: Instant::now(),
            });
            if entry.current != observed {
                entry.current = observed;
                entry.since = Instant::now();
            }
            match (entry.announced, entry.current) {
                // Silent adoption: what chat first learns is the baseline.
                (None, true) => entry.announced = Some(true),
                (None, false) => {
                    if entry.since.elapsed() >= self.offline_grace {
                        entry.announced = Some(false);
                    }
                }
                (Some(true), false) => {
                    if entry.since.elapsed() >= self.offline_grace {
                        entry.announced = Some(false);
                        if let Some(name) = self.telescope_name(id) {
                            events.push(PresenceEvent {
                                telescope_id: id,
                                telescope_name: name,
                                online: false,
                            });
                        }
                    }
                }
                (Some(false), true) => {
                    entry.announced = Some(true);
                    if let Some(name) = self.telescope_name(id) {
                        events.push(PresenceEvent {
                            telescope_id: id,
                            telescope_name: name,
                            online: true,
                        });
                    }
                }
                _ => {}
            }
        }
        events
    }

    fn telescope_name(&self, telescope_id: i64) -> Option<String> {
        self.db
            .get_telescope(telescope_id)
            .ok()
            .flatten()
            .map(|t| t.name)
    }

    /// Post presence transitions to the telescope's destination channels.
    pub async fn announce(&self, events: Vec<PresenceEvent>) {
        for event in events {
            let channels = self.route_channels(event.telescope_id);
            if channels.is_empty() {
                continue;
            }
            let target = ChatTarget {
                discord_webhook_url: None,
                matrix_room_id: None,
                discord_channel_id: None,
                discord_channel_ids: channels.iter().map(|c| *c as u64).collect(),
            };
            let message = if event.online {
                ChatMessage::new(&format!(
                    "🔭 [{}] Telescope connected",
                    event.telescope_name
                ))
                .color(0x3fb950)
            } else {
                ChatMessage::new(&format!(
                    "🔌 [{}] Telescope disconnected",
                    event.telescope_name
                ))
                .color(0xd29922)
            };
            self.chat_manager.send_message(&message, &target).await;
        }
    }

    /// This telescope's destination channels, sorted for comparison.
    fn route_channels(&self, telescope_id: i64) -> Vec<i64> {
        let mut channels: Vec<i64> = self
            .db
            .telescope_routes(telescope_id)
            .map(|routes| routes.iter().map(|r| r.channel_id).collect())
            .unwrap_or_default();
        channels.sort_unstable();
        channels
    }

    /// Reconcile while the caller owns `reconcile_gate`. `force_retire`
    /// invalidates one telescope even when its persisted configuration still
    /// compares equal (for example after a credential rotation).
    async fn reconcile_under_gate(&self, force_retire: Option<i64>) -> (usize, usize) {
        let mut started = 0;
        let mut stopped = 0;
        let mut retired = Vec::new();

        // A transport-only disconnect or replacement must not destroy updater
        // state: an event/image can already have been returned by Direct while
        // its chat post or autofocus graph is still pending. Stop only for a
        // route/config change or an unexpectedly finished task.
        {
            let Ok(mut running) = self.running.lock() else {
                return (0, 0);
            };
            let stale: Vec<i64> = running
                .iter()
                .filter_map(|(telescope_id, updater)| {
                    let config_current = matches!(
                        self.db.get_telescope(*telescope_id),
                        Ok(Some(row)) if row.image_cooldown_seconds == updater.image_cooldown_seconds
                    ) && self.route_channels(*telescope_id) == updater.channels;
                    let session_current = self
                        .connections
                        .get(*telescope_id)
                        .is_none_or(|connection| {
                            connection.session_id == updater.session_id
                                && connection.profile_id == updater.profile_id
                        });
                    let lifecycle_current = self.connections.lifecycle_generation(*telescope_id)
                        == updater.lifecycle_generation;
                    let keep = force_retire != Some(*telescope_id)
                        && config_current
                        && session_current
                        && lifecycle_current
                        && !updater.handle.is_finished();
                    (!keep).then_some(*telescope_id)
                })
                .collect();
            for telescope_id in stale {
                if let Some(updater) = running.remove(&telescope_id) {
                    updater.handle.abort();
                    retired.push((telescope_id, updater.handle));
                    stopped += 1;
                }
            }
        }

        // Do not hold the synchronous map lock while joining. Once this await
        // completes, no retired task can still post a stale message.
        for (telescope_id, handle) in retired {
            let _ = handle.await;
            println!("Stopped chat updater for telescope {telescope_id}");
        }

        // Start updaters for connected telescopes with a routed channel.
        let Ok(mut running) = self.running.lock() else {
            return (started, stopped);
        };
        for telescope_id in self.connections.connected_telescopes() {
            if running.contains_key(&telescope_id) {
                continue;
            }
            let Some(connection) = self.connections.get(telescope_id) else {
                continue;
            };
            let telescope = match self.db.get_telescope(telescope_id) {
                Ok(Some(row)) => row,
                _ => continue,
            };
            let channels = self.route_channels(telescope_id);
            if channels.is_empty() {
                // No destinations yet; nothing to post to.
                continue;
            }

            let source = Arc::new(DirectRigSource::current(
                self.connections.clone(),
                connection.clone(),
            ));
            let target = ChatTarget {
                discord_webhook_url: None,
                matrix_room_id: None,
                discord_channel_id: None,
                discord_channel_ids: channels.iter().map(|c| *c as u64).collect(),
            };
            let mut updater = ChatUpdater::new(
                source,
                telescope.name.clone(),
                target,
                self.chat_manager.clone(),
            )
            .with_image_cooldown(telescope.image_cooldown_seconds.max(0) as u64)
            // Presence is announced from connection state instead. A shorter
            // Hub retry window catches a replacement socket promptly without
            // busy-polling N.I.N.A. while it is genuinely offline.
            .with_lifecycle_announcements(false)
            .with_reconnect_backoff(5, 60);
            let handle = tokio::spawn(async move {
                updater.start_polling(UPDATER_POLL_INTERVAL).await;
            });
            running.insert(
                telescope_id,
                RunningUpdater {
                    session_id: connection.session_id,
                    profile_id: connection.profile_id,
                    lifecycle_generation: self.connections.lifecycle_generation(telescope_id),
                    channels,
                    image_cooldown_seconds: telescope.image_cooldown_seconds,
                    handle,
                },
            );
            started += 1;
            println!(
                "Started chat updater for telescope {telescope_id} ({})",
                telescope.name
            );
        }
        (started, stopped)
    }

    /// One serialized reconcile pass. Returns (started, stopped) counts.
    pub async fn reconcile_once(&self) -> (usize, usize) {
        let _gate = self.reconcile_gate.lock().await;
        self.reconcile_under_gate(None).await
    }

    pub fn running_count(&self) -> usize {
        self.running.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Immediately cancel every pending delivery owned by this telescope's
    /// current route snapshot, then reconcile against the already-committed
    /// database state. Privacy-sensitive removals call this before returning
    /// so a stale channel cannot receive one more event during the periodic
    /// reconcile interval.
    pub async fn refresh_telescope(&self, telescope_id: i64) {
        let _gate = self.reconcile_gate.lock().await;
        self.reconcile_under_gate(Some(telescope_id)).await;
    }

    /// Adopt a newly authenticated logical identity immediately. A missing
    /// predecessor socket is not evidence that its updater has gone away, so
    /// compare against the running updater rather than only `insert`'s return.
    pub async fn adopt_connection_identity(
        &self,
        telescope_id: i64,
        session_id: Uuid,
        profile_id: Uuid,
        lifecycle_generation: u64,
    ) {
        let _gate = self.reconcile_gate.lock().await;
        let running_identity = self.running.lock().ok().and_then(|running| {
            running.get(&telescope_id).map(|updater| {
                (
                    updater.session_id,
                    updater.profile_id,
                    updater.lifecycle_generation,
                )
            })
        });
        match running_identity {
            Some(identity) if identity != (session_id, profile_id, lifecycle_generation) => {
                self.reconcile_under_gate(Some(telescope_id)).await;
            }
            None => {
                self.reconcile_under_gate(None).await;
            }
            Some(_) => {}
        }
    }

    /// Reconcile forever, announcing real presence transitions.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.reconcile_once().await;
            let events = self.presence_events();
            self.announce(events).await;
            tokio::time::sleep(RECONCILE_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::direct_server::RigConnection;
    use crate::hub::store::UserRow;

    fn setup() -> (Db, Arc<RigConnections>, UpdaterManager, i64) {
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
        db.attach_telescope(telescope.id, 100, true, 1).unwrap();
        let connections = Arc::new(RigConnections::default());
        let manager = UpdaterManager::new(
            db.clone(),
            connections.clone(),
            Arc::new(ChatServiceManager::new()),
        );
        (db, connections, manager, telescope.id)
    }

    fn connect(connections: &RigConnections, telescope_id: i64) -> Uuid {
        let session = Uuid::new_v4();
        connect_with_session(connections, telescope_id, session);
        session
    }

    fn connect_with_session(
        connections: &RigConnections,
        telescope_id: i64,
        session_id: Uuid,
    ) -> Uuid {
        connect_with_identity(connections, telescope_id, session_id, Uuid::nil())
    }

    fn connect_with_identity(
        connections: &RigConnections,
        telescope_id: i64,
        session_id: Uuid,
        profile_id: Uuid,
    ) -> Uuid {
        let connection_id = Uuid::new_v4();
        let (connection, rx) =
            RigConnection::stub_with_identity(telescope_id, connection_id, session_id, profile_id);
        std::mem::forget(rx);
        connections.insert(connection);
        connection_id
    }

    #[tokio::test]
    async fn no_updater_without_channel_routing() {
        let (_db, connections, manager, id) = setup();
        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (0, 0));
        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn updater_survives_a_transport_disconnect() {
        let (db, connections, manager, id) = setup();
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();

        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));
        assert_eq!(manager.running_count(), 1);
        // Steady state: nothing changes.
        assert_eq!(manager.reconcile_once().await, (0, 0));

        // Connection drops. The updater remains alive with its dedup and
        // pending-delivery state while the live source waits for a replacement.
        let connection_id = connections.get(id).unwrap().connection_id;
        connections.remove_if_current(id, connection_id);
        assert_eq!(manager.reconcile_once().await, (0, 0));
        assert_eq!(manager.running_count(), 1);
    }

    #[tokio::test]
    async fn explicit_revocation_stops_updater_even_if_identity_is_reused() {
        let (db, connections, manager, id) = setup();
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        let session_id = connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        connections.revoke(id);
        assert_eq!(manager.reconcile_once().await, (0, 1));
        assert_eq!(manager.running_count(), 0);

        connect_with_session(&connections, id, session_id);
        assert_eq!(manager.reconcile_once().await, (1, 0));
        assert_eq!(manager.running_count(), 1);
    }

    #[tokio::test]
    async fn route_removal_refresh_is_an_updater_cancellation_barrier() {
        let (db, connections, manager, id) = setup();
        let route = db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        db.delete_route(route.id).unwrap();
        manager.refresh_telescope(id).await;
        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn periodic_reconcile_winning_the_race_is_still_awaited_by_refresh() {
        let (db, connections, manager, id) = setup();
        let route = db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        connect(&connections, id);
        let manager = Arc::new(manager);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        // Queue the periodic pass first. Tokio's mutex is FIFO, so it observes
        // the committed removal and owns retirement before the request-driven
        // refresh gets the gate. The refresh must wait behind that join rather
        // than return merely because the map entry is already gone.
        let held = manager.reconcile_gate.lock().await;
        db.delete_route(route.id).unwrap();
        let periodic_manager = manager.clone();
        let periodic = tokio::spawn(async move { periodic_manager.reconcile_once().await });
        tokio::task::yield_now().await;
        let refresh_manager = manager.clone();
        let refresh = tokio::spawn(async move { refresh_manager.refresh_telescope(id).await });
        tokio::task::yield_now().await;
        assert!(!periodic.is_finished());
        assert!(!refresh.is_finished());

        drop(held);
        assert_eq!(periodic.await.unwrap(), (0, 1));
        refresh.await.unwrap();
        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn destination_changes_restart_updater() {
        let (db, connections, manager, id) = setup();
        let first = db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        // Adding a second destination restarts the updater with both.
        let second = db.add_channel_route(id, 100, 43, "alerts", "g", 1).unwrap();
        assert_eq!(manager.reconcile_once().await, (1, 1));

        // Removing one destination restarts again.
        db.delete_route(second.id).unwrap();
        assert_eq!(manager.reconcile_once().await, (1, 1));

        // Removing the last destination stops it without a replacement.
        db.delete_route(first.id).unwrap();
        assert_eq!(manager.reconcile_once().await, (0, 1));
        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn presence_first_observation_is_silent_then_transitions_announce() {
        let (db, connections, manager, id) = setup();
        let manager = manager.with_offline_grace(Duration::ZERO);
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();

        // First observation (e.g. right after a hub deploy): silent adoption.
        connect(&connections, id);
        assert!(manager.presence_events().is_empty());
        assert!(manager.presence_events().is_empty());

        // A real disconnect (grace elapsed) announces once.
        let connection_id = connections.get(id).unwrap().connection_id;
        connections.remove_if_current(id, connection_id);
        let events = manager.presence_events();
        assert_eq!(events.len(), 1);
        assert!(!events[0].online);
        assert_eq!(events[0].telescope_name, "c925");
        assert!(manager.presence_events().is_empty());

        // Reconnect announces once.
        connect(&connections, id);
        let events = manager.presence_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].online);
        assert!(manager.presence_events().is_empty());
    }

    #[tokio::test]
    async fn presence_flap_within_grace_is_silent() {
        let (db, connections, manager, id) = setup();
        // Default 90s grace: a quick drop and reconnect never reaches chat.
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        connect(&connections, id);
        assert!(manager.presence_events().is_empty());
        let connection_id = connections.get(id).unwrap().connection_id;
        connections.remove_if_current(id, connection_id);
        assert!(manager.presence_events().is_empty());
        connect(&connections, id);
        assert!(manager.presence_events().is_empty());
    }

    #[tokio::test]
    async fn presence_scope_offline_at_startup_announces_when_it_connects() {
        let (db, connections, manager, id) = setup();
        let manager = manager.with_offline_grace(Duration::ZERO);
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();

        // Seed presence with a disconnected observation (scope was down
        // when the hub started): silent adoption as offline...
        {
            let mut presence = manager.presence.lock().unwrap();
            presence.insert(
                id,
                Presence {
                    announced: None,
                    current: false,
                    since: Instant::now(),
                },
            );
        }
        assert!(manager.presence_events().is_empty());

        // ...so its eventual arrival is news.
        connect(&connections, id);
        let events = manager.presence_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].online);
    }

    #[tokio::test]
    async fn reconnect_keeps_updater_and_pending_delivery_state() {
        let (db, connections, manager, id) = setup();
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();

        let session_id = connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        // A new WebSocket generation takes the slot (rig reconnected).
        connect_with_session(&connections, id, session_id);
        assert_eq!(manager.reconcile_once().await, (0, 0));
        assert_eq!(manager.running_count(), 1);
    }

    #[tokio::test]
    async fn new_plugin_session_replaces_updater_state() {
        let (db, connections, manager, id) = setup();
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();

        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 0));

        // A genuine plugin/profile lifecycle gets a new client session and
        // must not inherit targets, dedup keys, or pending chart delivery.
        connect(&connections, id);
        assert_eq!(manager.reconcile_once().await, (1, 1));
        assert_eq!(manager.running_count(), 1);
    }

    #[tokio::test]
    async fn different_profile_replaces_updater_even_with_same_session_value() {
        let (db, connections, manager, id) = setup();
        db.add_channel_route(id, 100, 42, "obs", "g", 1).unwrap();
        let session_id = Uuid::new_v4();
        connect_with_identity(&connections, id, session_id, Uuid::new_v4());
        assert_eq!(manager.reconcile_once().await, (1, 0));

        connect_with_identity(&connections, id, session_id, Uuid::new_v4());
        assert_eq!(manager.reconcile_once().await, (1, 1));
        assert_eq!(manager.running_count(), 1);
    }
}
