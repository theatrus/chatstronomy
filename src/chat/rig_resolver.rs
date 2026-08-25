//! Telescope resolution and write authorization for bot commands.
//!
//! The Discord bot resolves "which telescope does this command mean" and
//! "may this user run write commands" through this trait, so the same
//! command set serves both a self-hosted bot (static config maps) and the
//! hub (database-backed, per-guild tenancy, live rig connections).

use crate::source::SharedRigSource;
use std::collections::{HashMap, HashSet};

/// Facts about a slash-command invocation that resolution and authorization
/// may use.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub user_id: u64,
    /// The invoking member's role IDs (empty in DMs).
    pub role_ids: Vec<u64>,
    /// True when the invoker manages this guild right now: its owner, or a
    /// member whose interaction permissions carry ADMINISTRATOR or
    /// MANAGE_GUILD. Computed from Discord's own data at command time.
    pub manages_guild: bool,
}

pub trait RigResolver: Send + Sync {
    /// Resolve a telescope from an explicit override or the invocation's
    /// channel. The error is a user-facing message.
    fn resolve(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<(String, SharedRigSource), String>;

    /// May this user run write commands against this telescope? The error is
    /// a user-facing message.
    fn write_allowed(&self, invocation: &CommandContext, telescope: &str) -> Result<(), String>;

    /// Resolve a telescope and authorize a write against it in one step.
    /// Implementations must guarantee the authorization decision applies to
    /// the exact rig the command will actuate — resolving by channel and
    /// authorizing by name against a different row is how cross-tenant
    /// writes happen.
    fn resolve_for_write(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<(String, SharedRigSource), String> {
        let resolved = self.resolve(invocation, override_name)?;
        self.write_allowed(invocation, &resolved.0)?;
        Ok(resolved)
    }
}

/// Config-file-backed resolver used by the local bot: fixed telescope maps
/// plus either Discord server managers or an explicit user-ID allowlist.
pub struct StaticRigResolver {
    /// One source-neutral rig connection per telescope, keyed by name.
    pub rig_sources: HashMap<String, SharedRigSource>,
    /// Discord channel ID -> telescope name.
    pub channel_to_telescope: HashMap<u64, String>,
    /// Discord user IDs allowed to invoke write commands. When empty, only
    /// authenticated managers of the invoking Discord guild are allowed.
    pub write_acl: HashSet<u64>,
}

impl StaticRigResolver {
    fn known_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.rig_sources.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Discord channel IDs are globally unique. Requiring this exact mapping
    /// is what prevents a manager from another server where the bot is also
    /// installed from naming and operating this server's telescope.
    fn ensure_channel_route(
        &self,
        invocation: &CommandContext,
        telescope: &str,
    ) -> Result<(), String> {
        match self.channel_to_telescope.get(&invocation.channel_id) {
            Some(mapped) if mapped == telescope => Ok(()),
            Some(_) => Err(format!(
                "Telescope '{telescope}' is not routed to this Discord channel."
            )),
            None => Err("No telescope is routed to this Discord channel.".to_string()),
        }
    }
}

impl RigResolver for StaticRigResolver {
    fn resolve(
        &self,
        invocation: &CommandContext,
        override_name: Option<&str>,
    ) -> Result<(String, SharedRigSource), String> {
        if let Some(name) = override_name {
            self.ensure_channel_route(invocation, name)?;
            return self
                .rig_sources
                .get(name)
                .cloned()
                .map(|source| (name.to_string(), source))
                .ok_or_else(|| {
                    format!(
                        "Unknown telescope '{name}'. Known: {:?}",
                        self.known_names()
                    )
                });
        }
        if let Some(name) = self.channel_to_telescope.get(&invocation.channel_id) {
            let source = self
                .rig_sources
                .get(name)
                .cloned()
                .expect("channel_to_telescope -> rig_sources invariant");
            return Ok((name.clone(), source));
        }
        Err(format!(
            "No telescope mapped to this channel. Pass `telescope:<name>`. Known: {:?}",
            self.known_names()
        ))
    }

    fn write_allowed(&self, invocation: &CommandContext, telescope: &str) -> Result<(), String> {
        if invocation.guild_id.is_none() {
            return Err(
                "Write commands only work in a Discord server, not direct messages.".into(),
            );
        }

        self.ensure_channel_route(invocation, telescope)?;
        let Some(source) = self.rig_sources.get(telescope) else {
            return Err(format!("Unknown telescope '{telescope}'."));
        };
        if !source.capabilities().commands {
            return Err("Telescope control is disabled in N.I.N.A. Its owner must enable remote control and approve at least one individual command in the Chatstronomy plugin.".into());
        }

        if self.write_acl.is_empty() {
            return if invocation.manages_guild {
                Ok(())
            } else {
                Err("Write commands are limited to Discord server managers unless an explicit user allowlist is configured.".into())
            };
        }

        if self.write_acl.contains(&invocation.user_id) {
            return Ok(());
        }
        Err(format!(
            "You are not authorized to run write commands. \
             Your user ID `{}` is not in `chat.discord_bot.write_acl`.",
            invocation.user_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::CommandResponse;
    use crate::source::{RigCapabilities, RigCommand, RigSource, RigSourceError, RigSourceKind};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct TestDirectSource {
        commands: bool,
    }

    fn unused<T>() -> Result<T, RigSourceError> {
        Err(RigSourceError::Unavailable {
            kind: RigSourceKind::NinaDirect,
            reason: "test source".to_string(),
        })
    }

    #[async_trait]
    impl RigSource for TestDirectSource {
        fn kind(&self) -> RigSourceKind {
            RigSourceKind::NinaDirect
        }
        fn capabilities(&self) -> RigCapabilities {
            RigCapabilities {
                commands: self.commands,
                ..RigCapabilities::none()
            }
        }
        async fn get_event_history(
            &self,
        ) -> crate::source::RigSourceResult<crate::events::EventHistoryResponse> {
            unused()
        }
        async fn get_all_image_history(
            &self,
        ) -> crate::source::RigSourceResult<crate::images::ImageHistoryResponse> {
            unused()
        }
        async fn get_sequence(
            &self,
        ) -> crate::source::RigSourceResult<crate::sequence::SequenceResponse> {
            unused()
        }
        async fn get_thumbnail(
            &self,
            _: u32,
        ) -> crate::source::RigSourceResult<crate::images::ThumbnailResponse> {
            unused()
        }
        async fn get_last_autofocus(
            &self,
        ) -> crate::source::RigSourceResult<crate::autofocus::AutofocusResponse> {
            unused()
        }
        async fn get_mount_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::mount::MountInfoResponse> {
            unused()
        }
        async fn get_camera_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::camera::CameraInfoResponse> {
            unused()
        }
        async fn get_filterwheel_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::filterwheel::FilterWheelInfoResponse> {
            unused()
        }
        async fn get_guider_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::guider::GuiderInfoResponse> {
            unused()
        }
        async fn get_guider_graph(
            &self,
        ) -> crate::source::RigSourceResult<crate::guider::GuiderGraphResponse> {
            unused()
        }
        async fn get_rotator_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::rotator::RotatorInfoResponse> {
            unused()
        }
        async fn get_focuser_info(
            &self,
        ) -> crate::source::RigSourceResult<crate::focuser::FocuserInfoResponse> {
            unused()
        }
        async fn execute_command(
            &self,
            _: RigCommand,
        ) -> crate::source::RigSourceResult<CommandResponse> {
            unused()
        }
    }

    fn resolver() -> StaticRigResolver {
        let source: SharedRigSource = Arc::new(TestDirectSource { commands: true });
        StaticRigResolver {
            rig_sources: HashMap::from([("c925".to_string(), source)]),
            channel_to_telescope: HashMap::from([(42, "c925".to_string())]),
            write_acl: HashSet::from([7]),
        }
    }

    fn invocation(channel_id: u64, user_id: u64) -> CommandContext {
        CommandContext {
            guild_id: Some(1),
            channel_id,
            user_id,
            role_ids: Vec::new(),
            manages_guild: false,
        }
    }

    #[test]
    fn resolves_by_name_and_channel() {
        let r = resolver();
        assert_eq!(r.resolve(&invocation(42, 7), None).unwrap().0, "c925");
        assert_eq!(
            r.resolve(&invocation(42, 7), Some("c925")).unwrap().0,
            "c925"
        );
    }

    #[test]
    fn unknown_name_and_unmapped_channel_error() {
        let r = resolver();
        assert!(
            r.resolve(&invocation(42, 7), Some("nope"))
                .err()
                .unwrap()
                .contains("not routed")
        );
        assert!(
            r.resolve(&invocation(0, 7), None)
                .err()
                .unwrap()
                .contains("No telescope mapped")
        );
    }

    #[test]
    fn write_acl_gates_by_user_id() {
        let r = resolver();
        assert!(r.write_allowed(&invocation(42, 7), "c925").is_ok());
        assert!(r.write_allowed(&invocation(42, 8), "c925").is_err());
    }

    #[test]
    fn empty_allowlist_grants_only_discord_server_managers() {
        let mut resolver = resolver();
        resolver.write_acl.clear();
        let manager = CommandContext {
            manages_guild: true,
            ..invocation(42, 8)
        };

        assert!(resolver.write_allowed(&manager, "c925").is_ok());
        assert!(resolver.resolve_for_write(&manager, None).is_ok());

        let error = resolver
            .write_allowed(&invocation(42, 8), "c925")
            .expect_err("ordinary guild members must not gain hardware control");
        assert!(error.contains("server managers"), "got: {error}");
    }

    #[test]
    fn local_commands_never_run_from_direct_messages() {
        let mut resolver = resolver();
        resolver.write_acl.clear();
        let direct_message = CommandContext {
            guild_id: None,
            manages_guild: true,
            ..invocation(42, 7)
        };

        let error = resolver
            .write_allowed(&direct_message, "c925")
            .expect_err("a Discord guild is required even for an apparent manager");
        assert!(error.contains("not direct messages"), "got: {error}");
    }

    #[test]
    fn explicit_allowlist_does_not_implicitly_grant_guild_managers() {
        let resolver = resolver();
        let manager = CommandContext {
            manages_guild: true,
            ..invocation(42, 8)
        };

        assert!(resolver.write_allowed(&invocation(42, 7), "c925").is_ok());
        let error = resolver
            .write_allowed(&manager, "c925")
            .expect_err("an explicit allowlist is authoritative");
        assert!(error.contains("write_acl"), "got: {error}");
    }

    #[test]
    fn local_plugin_control_lock_overrides_every_discord_permission() {
        let mut resolver = resolver();
        resolver.rig_sources.insert(
            "c925".to_string(),
            Arc::new(TestDirectSource { commands: false }),
        );

        // Reads stay available to the regular channel mapping.
        assert!(resolver.resolve(&invocation(42, 7), None).is_ok());

        let error = resolver
            .write_allowed(&invocation(42, 7), "c925")
            .expect_err("even an explicitly allowlisted user needs local consent");
        assert!(error.contains("disabled in N.I.N.A."), "got: {error}");

        resolver.write_acl.clear();
        let manager = CommandContext {
            manages_guild: true,
            ..invocation(42, 7)
        };
        assert!(resolver.resolve_for_write(&manager, None).is_err());
    }

    #[test]
    fn another_servers_manager_cannot_name_or_control_a_local_telescope() {
        let mut resolver = resolver();
        resolver.write_acl.clear();
        let foreign_manager = CommandContext {
            guild_id: Some(999),
            manages_guild: true,
            ..invocation(999, 8)
        };

        let error = resolver
            .resolve(&foreign_manager, Some("c925"))
            .err()
            .expect("explicit telescope names must not bypass channel routing");
        assert!(error.contains("No telescope is routed"), "got: {error}");
        let error = resolver
            .write_allowed(&foreign_manager, "c925")
            .expect_err("a manager in another server has no hardware authority");
        assert!(error.contains("No telescope is routed"), "got: {error}");
    }

    #[test]
    fn telescope_names_cannot_cross_mapped_discord_channels() {
        let mut resolver = resolver();
        resolver.write_acl.clear();
        resolver.rig_sources.insert(
            "esprit100".to_string(),
            Arc::new(TestDirectSource { commands: true }),
        );
        resolver
            .channel_to_telescope
            .insert(43, "esprit100".to_string());
        let manager = CommandContext {
            manages_guild: true,
            ..invocation(42, 8)
        };

        let error = resolver
            .resolve(&manager, Some("esprit100"))
            .err()
            .expect("an explicit read cannot cross telescope channel routes");
        assert!(error.contains("not routed"), "got: {error}");
        let error = resolver
            .write_allowed(&manager, "esprit100")
            .expect_err("a manager cannot control another channel's telescope");
        assert!(error.contains("not routed"), "got: {error}");
    }
}
