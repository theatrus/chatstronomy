//! Database-backed telescope resolution for the hub's Discord bot.
//!
//! Telescopes are user-owned; a guild reaches one through its attachment.
//! Channel routing is global (channel IDs are unique), name lookup is
//! scoped to the invoking guild's attachments. Write authorization comes
//! from the ATTACHMENT of the invoking guild: `can_command` plus that
//! guild's own policy — a feed-only subscription can never drive the rig.

use super::db::Db;
use super::direct_server::RigConnections;
use super::direct_source::DirectRigSource;
use super::tenants::{AttachmentRow, TelescopeRow};
use crate::chat::{CommandContext, RigResolver};
use crate::source::SharedRigSource;
use std::sync::Arc;

pub struct HubRigResolver {
    db: Db,
    connections: Arc<RigConnections>,
    devices: Option<Arc<super::device_transport::DeviceConnections>>,
}

impl HubRigResolver {
    pub fn new(db: Db, connections: Arc<RigConnections>) -> Self {
        Self {
            db,
            connections,
            devices: None,
        }
    }

    pub fn with_devices(
        mut self,
        devices: Arc<super::device_transport::DeviceConnections>,
    ) -> Self {
        self.devices = Some(devices);
        self
    }

    /// Without a name, picks the invoker's only camera routed to this
    /// channel, as telescope commands default to the channel's telescope.
    fn camera_id(&self, invocation: &CommandContext, name: Option<&str>) -> Result<i64, String> {
        let guild = invocation
            .guild_id
            .ok_or("Camera commands require a server channel")?;
        // User ownership AND exact guild/channel route. A server manager is
        // not implicitly allowed to change another user's camera sharing.
        let cameras: Vec<(i64, String)> = self
            .db
            .with_conn(|c| {
                c.prepare("SELECT d.id, d.name FROM devices d JOIN device_channels r ON r.device_id=d.id WHERE d.owner_id=?1 AND r.guild_id=?2 AND r.channel_id=?3 ORDER BY d.name")?
                    .query_map(
                        rusqlite::params![invocation.user_id as i64, guild as i64, invocation.channel_id as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?
                    .collect()
            })
            .map_err(|_| "Camera lookup failed".to_owned())?;
        match name {
            Some(name) => cameras
                .iter()
                .find(|(_, n)| n == name)
                .map(|(id, _)| *id)
                .ok_or_else(|| {
                    "No camera with that name is owned by you and routed to this channel.".into()
                }),
            None => match cameras.as_slice() {
                [(id, _)] => Ok(*id),
                [] => Err("No camera owned by you is routed to this channel.".into()),
                several => Err(format!(
                    "Several of your cameras are routed to this channel ({}). Choose one with `camera`.",
                    several
                        .iter()
                        .map(|(_, n)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
        }
    }

    fn guild_names(&self, guild_id: i64) -> Vec<String> {
        self.db.guild_telescope_names(guild_id).unwrap_or_default()
    }

    fn find_telescope(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<TelescopeRow, String> {
        if let Some(name) = override_name {
            let Some(guild_id) = invocation.guild_id else {
                return Err("Commands with a telescope name only work in a server".to_string());
            };
            return match self.db.telescope_by_guild_and_name(guild_id as i64, name) {
                Ok(Some(row)) => Ok(row),
                Ok(None) => Err(format!(
                    "No telescope named '{name}' is attached to this server. Known: {:?}",
                    self.guild_names(guild_id as i64)
                )),
                Err(e) => Err(format!("Lookup failed: {e}")),
            };
        }
        match self.db.telescope_by_channel(invocation.channel_id as i64) {
            Ok(Some(row)) => Ok(row),
            Ok(None) => {
                let known = invocation
                    .guild_id
                    .map(|g| self.guild_names(g as i64))
                    .unwrap_or_default();
                Err(format!(
                    "No telescope routed to this channel. Pass `telescope:<name>`. Known: {known:?}"
                ))
            }
            Err(e) => Err(format!("Lookup failed: {e}")),
        }
    }

    /// The invoking guild's attachment to this telescope. Its absence means
    /// the command came from outside every attached guild.
    fn invoking_attachment(
        &self,
        telescope: &TelescopeRow,
        invocation: &CommandContext,
    ) -> Result<AttachmentRow, String> {
        let Some(guild_id) = invocation.guild_id else {
            return Err("Write commands only work in a server".to_string());
        };
        match self.db.attachment_for(telescope.id, guild_id as i64) {
            Ok(Some(attachment)) => Ok(attachment),
            Ok(None) => Err("This telescope is not attached to this server.".to_string()),
            Err(e) => Err(format!("Lookup failed: {e}")),
        }
    }

    fn source_for(&self, row: &TelescopeRow) -> Result<SharedRigSource, String> {
        let Some(connection) = self.connections.get(row.id) else {
            return Err(format!(
                "Telescope '{}' is not connected to the hub right now.",
                row.name
            ));
        };
        Ok(Arc::new(DirectRigSource::new(connection)))
    }
}

#[async_trait::async_trait]
impl RigResolver for HubRigResolver {
    async fn camera_command(
        &self,
        invocation: &CommandContext,
        camera: Option<&str>,
        rules: Option<crate::chat::CameraTriggerRules>,
    ) -> Result<(), String> {
        let id = self.camera_id(invocation, camera)?;
        let devices = self
            .devices
            .as_ref()
            .ok_or("Camera transport is unavailable")?;
        match rules {
            Some(rules) => devices.configure(id, rules).await,
            None => devices.snapshot(id).await,
        }
        .map_err(str::to_owned)
    }
    fn resolve(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<(String, SharedRigSource), String> {
        let row = self.find_telescope(invocation, override_name)?;
        let source = self.source_for(&row)?;
        Ok((row.name, source))
    }

    /// One lookup chain: the attachment that authorizes belongs to the
    /// invoking guild and the telescope the command actuates.
    fn resolve_for_write(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<(String, SharedRigSource), String> {
        let row = self.find_telescope(invocation, override_name)?;
        let attachment = self.invoking_attachment(&row, invocation)?;
        check_write_policy(&attachment, invocation)?;
        let source = self.source_for(&row)?;
        ensure_locally_enabled(&source)?;
        Ok((row.name, source))
    }

    fn write_allowed(&self, invocation: &CommandContext, telescope: &str) -> Result<(), String> {
        let row = self.find_telescope(invocation, Some(telescope))?;
        let attachment = self.invoking_attachment(&row, invocation)?;
        check_write_policy(&attachment, invocation)?;
        let source = self.source_for(&row)?;
        ensure_locally_enabled(&source)
    }
}

/// Guild permissions can narrow access, but only the owner sitting at the
/// N.I.N.A. profile can grant the plugin permission to actuate hardware.
fn ensure_locally_enabled(source: &SharedRigSource) -> Result<(), String> {
    if source.capabilities().commands {
        Ok(())
    } else {
        Err("Telescope control is disabled in N.I.N.A. Its owner must enable remote control and approve at least one individual command in the Chatstronomy plugin.".to_string())
    }
}

/// Write policy of one guild's attachment.
///
/// - `can_command == false`: a feed-only subscription; no one here drives
///   the rig, whatever the policy says.
/// - `disabled`: nobody, not even admins — a deliberate off switch.
/// - `admins` (the default): whoever manages the guild on Discord — its
///   owner or members holding ADMINISTRATOR/MANAGE_GUILD.
/// - `roles`: guild managers plus members holding an allowlisted role.
fn check_write_policy(
    attachment: &AttachmentRow,
    invocation: &CommandContext,
) -> Result<(), String> {
    if !attachment.can_command {
        return Err(
            "This server receives this telescope's feed but cannot send it commands.".to_string(),
        );
    }
    match attachment.write_policy.as_str() {
        "admins" if invocation.manages_guild => Ok(()),
        "admins" => Err(
            "Write commands on this telescope are limited to server managers. \
             Ask an admin to add your role in the hub settings."
                .to_string(),
        ),
        "roles" => {
            let allowed = invocation.manages_guild
                || attachment
                    .allowed_role_ids
                    .iter()
                    .any(|role| invocation.role_ids.contains(&(*role as u64)));
            if allowed {
                Ok(())
            } else {
                Err(
                    "You are not authorized to run write commands for this telescope. \
                     Ask a server admin to grant your role in the hub settings."
                        .to_string(),
                )
            }
        }
        _ => Err(
            "Write commands are disabled for this telescope. A server admin can \
             enable them in the hub settings."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::execute_authorized_command;
    use crate::direct::protocol::{DirectMessage, QueryResult};
    use crate::hub::direct_server::RigConnection;
    use crate::hub::store::UserRow;
    use crate::hub::tenants::AttachmentUpdate;
    use crate::source::{RigCommand, RigSourceError};
    use tokio::sync::mpsc::error::TryRecvError;
    use uuid::Uuid;

    fn setup() -> (Db, Arc<RigConnections>, HubRigResolver, i64) {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&UserRow {
            discord_user_id: 1,
            username: "alice".to_string(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        db.register_guild(100, "home", 1).unwrap();
        let telescope = db.create_telescope(1, "c925").unwrap();
        db.attach_telescope(telescope.id, 100, true, 1).unwrap();
        db.add_channel_route(telescope.id, 100, 42, "obs", "home", 1)
            .unwrap();
        let connections = Arc::new(RigConnections::default());
        let resolver = HubRigResolver::new(db.clone(), connections.clone());
        (db, connections, resolver, telescope.id)
    }

    #[test]
    fn camera_commands_require_owner_and_exact_guild_channel_not_manager_privilege() {
        let (db, _, resolver, _) = setup();
        let camera = db
            .create_device(1, "Pier", super::super::devices::DeviceKind::PierCamera)
            .unwrap();
        db.attach_device(camera.id, 100, 1).unwrap();
        db.add_device_channel(camera.id, 100, 42, "obs").unwrap();
        let owner = CommandContext {
            guild_id: Some(100),
            channel_id: 42,
            user_id: 1,
            ..Default::default()
        };
        assert_eq!(resolver.camera_id(&owner, Some("Pier")).unwrap(), camera.id);
        for other in [
            CommandContext {
                user_id: 7,
                manages_guild: true,
                ..owner.clone()
            },
            CommandContext {
                guild_id: None,
                ..owner.clone()
            },
            CommandContext {
                guild_id: Some(999),
                ..owner.clone()
            },
            CommandContext {
                channel_id: 43,
                ..owner.clone()
            },
        ] {
            assert!(resolver.camera_id(&other, Some("Pier")).is_err());
        }
        assert!(resolver.camera_id(&owner, Some("Unknown")).is_err());
        assert_eq!(resolver.camera_id(&owner, None).unwrap(), camera.id);
        let second = db
            .create_device(1, "Roof", super::super::devices::DeviceKind::PierCamera)
            .unwrap();
        db.attach_device(second.id, 100, 1).unwrap();
        db.add_device_channel(second.id, 100, 42, "obs").unwrap();
        let ambiguous = resolver.camera_id(&owner, None).unwrap_err();
        assert!(ambiguous.contains("Pier, Roof"), "{ambiguous}");
        assert_eq!(resolver.camera_id(&owner, Some("Roof")).unwrap(), second.id);
    }

    fn invocation(guild_id: u64, channel_id: u64, roles: Vec<u64>) -> CommandContext {
        CommandContext {
            guild_id: Some(guild_id),
            channel_id,
            user_id: 7,
            role_ids: roles,
            manages_guild: false,
        }
    }

    fn manager_invocation(guild_id: u64, channel_id: u64) -> CommandContext {
        CommandContext {
            manages_guild: true,
            ..invocation(guild_id, channel_id, Vec::new())
        }
    }

    fn connect(connections: &RigConnections, telescope_id: i64) {
        let (connection, rx) =
            crate::hub::direct_server::RigConnection::stub(telescope_id, Uuid::new_v4());
        std::mem::forget(rx);
        connections.insert(connection);
    }

    fn connect_read_only(connections: &RigConnections, telescope_id: i64) {
        let (mut connection, rx) =
            crate::hub::direct_server::RigConnection::stub(telescope_id, Uuid::new_v4());
        Arc::get_mut(&mut connection)
            .expect("the new test connection has a single owner")
            .capabilities
            .commands = false;
        std::mem::forget(rx);
        connections.insert(connection);
    }

    #[test]
    fn resolves_by_channel_and_attached_name() {
        let (_db, connections, resolver, id) = setup();
        connect(&connections, id);

        assert_eq!(
            resolver
                .resolve(&invocation(100, 42, vec![]), None)
                .unwrap()
                .0,
            "c925"
        );
        assert_eq!(
            resolver
                .resolve(&invocation(100, 0, vec![]), Some("c925"))
                .unwrap()
                .0,
            "c925"
        );
        // A guild the telescope is not attached to cannot name it.
        let err = resolver
            .resolve(&invocation(999, 0, vec![]), Some("c925"))
            .err()
            .unwrap();
        assert!(err.contains("No telescope named"), "got: {err}");
    }

    #[test]
    fn offline_rig_reports_clearly() {
        let (_db, _connections, resolver, _id) = setup();
        let err = resolver
            .resolve(&invocation(100, 42, vec![]), None)
            .err()
            .unwrap();
        assert!(err.contains("not connected"));
    }

    #[test]
    fn default_policy_lets_managers_and_only_managers_write() {
        let (_db, connections, resolver, id) = setup();
        connect(&connections, id);
        assert!(
            resolver
                .resolve_for_write(&manager_invocation(100, 42), None)
                .is_ok()
        );
        let err = resolver
            .resolve_for_write(&invocation(100, 42, vec![1111]), None)
            .err()
            .unwrap();
        assert!(err.contains("server managers"), "got: {err}");
    }

    #[test]
    fn local_plugin_lock_overrides_every_guild_write_permission() {
        let (_db, connections, resolver, id) = setup();
        connect_read_only(&connections, id);
        let manager = manager_invocation(100, 42);

        // Monitoring remains available while the physical control boundary is
        // closed, including to an authorized server manager.
        assert!(resolver.resolve(&manager, None).is_ok());
        let error = resolver
            .resolve_for_write(&manager, None)
            .err()
            .expect("local consent is mandatory");
        assert!(error.contains("disabled in N.I.N.A."), "got: {error}");
        let error = resolver
            .write_allowed(&manager, "c925")
            .expect_err("the alternate authorization path also respects local consent");
        assert!(error.contains("Chatstronomy plugin"), "got: {error}");
    }

    #[test]
    fn feed_only_attachment_reads_but_never_writes() {
        // The club subscribes to alice's scope: reads resolve from the
        // club's routed channel, writes are refused even for its managers.
        let (db, connections, resolver, id) = setup();
        connect(&connections, id);
        db.upsert_user(&UserRow {
            discord_user_id: 2,
            username: "bob".to_string(),
            email: None,
            email_verified: false,
            avatar_url: None,
        })
        .unwrap();
        db.register_guild(200, "club", 2).unwrap();
        db.attach_telescope(id, 200, false, 2).unwrap();
        db.add_channel_route(id, 200, 900, "feed", "club", 2)
            .unwrap();

        assert_eq!(
            resolver
                .resolve(&invocation(200, 900, vec![]), None)
                .unwrap()
                .0,
            "c925"
        );
        let err = resolver
            .resolve_for_write(&manager_invocation(200, 900), None)
            .err()
            .unwrap();
        assert!(err.contains("cannot send it commands"), "got: {err}");
    }

    #[test]
    fn attachment_policies_are_per_guild() {
        let (db, connections, resolver, id) = setup();
        connect(&connections, id);
        let attachment = db.attachment_for(id, 100).unwrap().unwrap();
        db.update_attachment(
            attachment.id,
            &AttachmentUpdate {
                write_policy: Some("roles".to_string()),
                allowed_role_ids: Some(vec![1111]),
            },
        )
        .unwrap();

        assert!(
            resolver
                .resolve_for_write(&invocation(100, 42, vec![1111]), None)
                .is_ok()
        );
        assert!(
            resolver
                .resolve_for_write(&invocation(100, 42, vec![3333]), None)
                .is_err()
        );
        // Managers pass without the role.
        assert!(
            resolver
                .resolve_for_write(&manager_invocation(100, 42), None)
                .is_ok()
        );

        // Disabled blocks even managers.
        db.update_attachment(
            attachment.id,
            &AttachmentUpdate {
                write_policy: Some("disabled".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = resolver
            .resolve_for_write(&manager_invocation(100, 42), None)
            .err()
            .unwrap();
        assert!(err.contains("disabled"), "got: {err}");
    }

    #[tokio::test]
    async fn confirmation_cannot_outlive_policy_attachment_or_member_permission() {
        for change in ["policy", "attachment", "member"] {
            let (db, connections, resolver, id) = setup();
            let (connection, mut outgoing) = RigConnection::stub(id, Uuid::new_v4());
            connections.insert(connection);
            let mut invocation = manager_invocation(100, 42);
            let (name, expected) = resolver.resolve_for_write(&invocation, None).unwrap();
            let attachment = db.attachment_for(id, 100).unwrap().unwrap();
            match change {
                "policy" => db
                    .update_attachment(
                        attachment.id,
                        &AttachmentUpdate {
                            write_policy: Some("disabled".to_string()),
                            ..Default::default()
                        },
                    )
                    .unwrap(),
                "attachment" => db.detach_telescope(attachment.id).unwrap(),
                "member" => invocation.manages_guild = false,
                _ => unreachable!(),
            }
            let result = execute_authorized_command(
                &resolver,
                &invocation,
                &name,
                &expected,
                RigCommand::ParkMount,
            )
            .await;
            assert!(
                matches!(result, Err(RigSourceError::Rejected { .. })),
                "{change} did not revoke dispatch"
            );
            assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[tokio::test]
    async fn confirmation_cannot_redirect_to_another_rig_with_the_same_name() {
        let (db, connections, resolver, id) = setup();
        let (connection, mut outgoing) = RigConnection::stub(id, Uuid::new_v4());
        connections.insert(connection);
        let invocation = manager_invocation(100, 42);
        let (name, expected) = resolver.resolve_for_write(&invocation, None).unwrap();
        db.delete_telescope(id).unwrap();
        let replacement = db.create_telescope(1, &name).unwrap();
        db.attach_telescope(replacement.id, 100, true, 1).unwrap();
        let (replacement, mut replacement_outgoing) =
            RigConnection::stub(replacement.id, Uuid::new_v4());
        connections.insert(replacement);
        let result = execute_authorized_command(
            &resolver,
            &invocation,
            &name,
            &expected,
            RigCommand::CenterTarget,
        )
        .await;
        assert!(
            matches!(result, Err(RigSourceError::Rejected { reason, .. }) if reason.contains("connection changed"))
        );
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(
            replacement_outgoing.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn confirmation_cannot_cross_a_transport_reconnection() {
        let (_db, connections, resolver, id) = setup();
        let (connection, mut outgoing) = RigConnection::stub(id, Uuid::new_v4());
        let session_id = connection.session_id;
        let profile_id = connection.profile_id;
        connections.insert(connection);
        let invocation = manager_invocation(100, 42);
        let (name, expected) = resolver.resolve_for_write(&invocation, None).unwrap();
        let (replacement, mut replacement_outgoing) =
            RigConnection::stub_with_identity(id, Uuid::new_v4(), session_id, profile_id);
        connections.insert(replacement);
        assert!(
            matches!(execute_authorized_command(&resolver, &invocation, &name, &expected, RigCommand::StartAutofocus).await, Err(RigSourceError::Rejected { reason, .. }) if reason.contains("connection changed"))
        );
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(
            replacement_outgoing.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn dispatch_revalidation_accepts_the_same_authorized_connection() {
        let (_db, connections, resolver, id) = setup();
        let (connection, mut outgoing) = RigConnection::stub(id, Uuid::new_v4());
        connections.insert(connection.clone());
        let invocation = manager_invocation(100, 42);
        let (name, expected) = resolver.resolve_for_write(&invocation, None).unwrap();
        let dispatch = tokio::spawn(async move {
            execute_authorized_command(
                &resolver,
                &invocation,
                &name,
                &expected,
                RigCommand::CenterTarget,
            )
            .await
        });
        let DirectMessage::Query(query) = outgoing.recv().await.unwrap() else {
            panic!("expected command");
        };
        connection.resolve(QueryResult {
            id: query.id,
            ok: true,
            payload: serde_json::json!({ "Response": "Centering queued", "Error": "", "StatusCode": 202, "Success": true, "Type": "API" }),
            error: None,
            error_code: None,
        });
        assert!(dispatch.await.unwrap().unwrap().is_pending());
    }
}
