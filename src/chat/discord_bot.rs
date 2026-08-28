//! Process-wide Discord bot using Serenity + Poise.
//!
//! One bot identity (one token) serves every telescope. Each telescope can
//! map to a Discord channel via `TelescopeChatOverrides::discord_channel_id`;
//! slash commands invoked in that channel default to that telescope. Outbound
//! posts (events, image notifications, etc.) flow through this service via
//! `ChatService::send_message` and use the bot's `Arc<Http>` to deliver
//! directly without going through the gateway loop.
//!
//! Read-only slash commands (Phase 1):
//!   /status, /sequence, /target, /mount, /filter, /focus, /guider,
//!   /events, /last-image.

use super::rig_resolver::{CommandContext, RigResolver};
use super::status_state::{StatusMessage, StatusState};
use super::{ChatAttachment, ChatMessage, ChatService, ChatTarget, DiscordBotConfig};
use crate::error::ChatError;
use crate::sequence::{SequenceOperation, SequenceOperationKind};
use crate::source::{RigCommand, SharedRigSource};
use async_trait::async_trait;
use poise::serenity_prelude::{self as serenity, CreateAttachment, CreateMessage};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Per-bot state carried by the Poise framework. Each slash command has
/// `ctx.data()` access to this. Telescope lookup and write authorization go
/// through the resolver so one command set serves both the self-hosted bot
/// (static config) and the hub (database-backed tenancy).
pub struct BotData {
    pub resolver: Arc<dyn RigResolver>,
}

pub type BotError = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, BotData, BotError>;

// ---------- Outbound posting (ChatService impl) ----------

/// Chat service that posts via the Discord bot. Holds the bot's `Arc<Http>`
/// after the gateway task is spawned, plus an optional default channel and
/// the persistent live-status state.
pub struct DiscordBotService {
    http: Arc<serenity::Http>,
    default_channel_id: Option<u64>,
    /// Per-telescope (channel_id, message_id) for the pinned live-status
    /// message. Shared across telescope tasks via Mutex; reads are cheap,
    /// writes happen once per poll cycle per telescope.
    status_state: Arc<Mutex<StatusState>>,
    state_file: PathBuf,
    /// Whether live-status upserts are enabled at all (config-driven).
    live_status: bool,
}

impl DiscordBotService {
    pub fn new(
        http: Arc<serenity::Http>,
        default_channel_id: Option<u64>,
        status_state: Arc<Mutex<StatusState>>,
        state_file: PathBuf,
        live_status: bool,
    ) -> Self {
        Self {
            http,
            default_channel_id,
            status_state,
            state_file,
            live_status,
        }
    }

    /// Every channel this target posts to, falling back to the bot's default
    /// channel when the target names none.
    fn resolve_channels(&self, target: &ChatTarget) -> Vec<serenity::ChannelId> {
        let channels = target.all_discord_channels();
        if channels.is_empty() {
            return self
                .default_channel_id
                .into_iter()
                .map(serenity::ChannelId::new)
                .collect();
        }
        channels.into_iter().map(serenity::ChannelId::new).collect()
    }

    /// Send one prepared payload to every destination. A single broken
    /// channel is logged and skipped; the send fails only when every
    /// destination failed.
    async fn fan_out(
        &self,
        channels: Vec<serenity::ChannelId>,
        build: impl Fn() -> CreateMessage,
    ) -> Result<(), ChatError> {
        if channels.is_empty() {
            return Err(ChatError::Discord {
                message: "No Discord channel available (no default and no telescope override)"
                    .to_string(),
            });
        }
        let mut delivered = 0;
        let mut last_error = None;
        for channel in channels {
            match channel.send_message(&self.http, build()).await {
                Ok(_) => delivered += 1,
                Err(e) => {
                    eprintln!("Warning: post to channel {channel} failed: {e}");
                    last_error = Some(e);
                }
            }
        }
        match (delivered, last_error) {
            (0, Some(e)) => Err(ChatError::Discord {
                message: e.to_string(),
            }),
            _ => Ok(()),
        }
    }

    fn build_embed(message: &ChatMessage) -> serenity::CreateEmbed {
        let mut embed = serenity::CreateEmbed::new().title(&message.title);
        if let Some(color) = message.color {
            embed = embed.color(color);
        }
        for field in &message.fields {
            embed = embed.field(&field.name, &field.value, field.inline);
        }
        if let Some(footer) = &message.footer {
            embed = embed.footer(serenity::CreateEmbedFooter::new(footer));
        }
        if let Some(ts) = &message.timestamp
            && let Ok(parsed) = serenity::Timestamp::parse(ts)
        {
            embed = embed.timestamp(parsed);
        }
        embed
    }
}

#[async_trait]
impl ChatService for DiscordBotService {
    async fn send_message(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
    ) -> Result<(), ChatError> {
        self.fan_out(self.resolve_channels(target), || {
            CreateMessage::new().embed(Self::build_embed(message))
        })
        .await
    }

    async fn send_message_with_image(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        image_data: &[u8],
        filename: &str,
    ) -> Result<(), ChatError> {
        self.fan_out(self.resolve_channels(target), || {
            CreateMessage::new()
                .embed(Self::build_embed(message))
                .add_file(CreateAttachment::bytes(image_data.to_vec(), filename))
        })
        .await
    }

    async fn send_message_with_attachments(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        attachments: &[ChatAttachment],
    ) -> Result<(), ChatError> {
        if attachments.is_empty() {
            return self.send_message(message, target).await;
        }
        self.fan_out(self.resolve_channels(target), || {
            let mut payload = CreateMessage::new().embed(Self::build_embed(message));
            for attachment in attachments {
                payload = payload.add_file(CreateAttachment::bytes(
                    attachment.data.clone(),
                    attachment.filename.clone(),
                ));
            }
            payload
        })
        .await
    }

    fn service_name(&self) -> &'static str {
        "Discord (bot)"
    }

    fn can_route(&self, target: &ChatTarget) -> bool {
        !target.all_discord_channels().is_empty() || self.default_channel_id.is_some()
    }

    fn supports_status_upsert(&self) -> bool {
        self.live_status
    }

    /// Edit-or-post the live status message for this telescope.
    ///
    /// On first call (or when state has no record), posts a fresh message
    /// and remembers `(channel_id, message_id)`. On subsequent calls, edits
    /// the existing message in place. If the previous message was deleted
    /// (404 from Discord), reposts and updates state.
    async fn upsert_status(
        &self,
        telescope: &str,
        target: &ChatTarget,
        message: &ChatMessage,
    ) -> Result<(), ChatError> {
        let channels = self.resolve_channels(target);
        if channels.is_empty() {
            return Err(ChatError::Discord {
                message: "No Discord channel available for status upsert".to_string(),
            });
        }
        let embed = Self::build_embed(message);
        let mut delivered = 0;
        let mut last_error = None;
        for channel in channels {
            match self
                .upsert_status_in(telescope, channel, embed.clone())
                .await
            {
                Ok(()) => delivered += 1,
                Err(e) => {
                    eprintln!("Warning: status upsert in channel {channel} failed: {e}");
                    last_error = Some(e);
                }
            }
        }
        match (delivered, last_error) {
            (0, Some(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

impl DiscordBotService {
    /// Edit-or-post the status message for one telescope in one channel.
    /// State is keyed per (telescope, channel) so every destination keeps
    /// its own pinned message; the legacy telescope-only key is honored
    /// when it points at this channel.
    async fn upsert_status_in(
        &self,
        telescope: &str,
        channel: serenity::ChannelId,
        embed: serenity::CreateEmbed,
    ) -> Result<(), ChatError> {
        let key = format!("{telescope}@{}", channel.get());
        let existing = {
            let state = self.status_state.lock().await;
            state.get(&key).or_else(|| {
                state
                    .get(telescope)
                    .filter(|m| m.channel_id == channel.get())
            })
        };

        // Try to edit if we have a known message in the same channel.
        if let Some(known) = existing
            && known.channel_id == channel.get()
        {
            let edit = serenity::EditMessage::new()
                .content("")
                .embed(embed.clone());
            match channel
                .edit_message(&self.http, serenity::MessageId::new(known.message_id), edit)
                .await
            {
                Ok(_) => return Ok(()),
                Err(serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(err)))
                    if err.status_code == reqwest::StatusCode::NOT_FOUND =>
                {
                    // Message was deleted; fall through and repost.
                    eprintln!(
                        "[{telescope}] status message {} not found — reposting",
                        known.message_id
                    );
                }
                Err(e) => {
                    return Err(ChatError::Discord {
                        message: format!("edit_message failed: {e}"),
                    });
                }
            }
        }

        // Post new message and record its ID.
        let payload = CreateMessage::new().embed(embed);
        let posted = channel
            .send_message(&self.http, payload)
            .await
            .map_err(|e| ChatError::Discord {
                message: format!("status post failed: {e}"),
            })?;

        let mut state = self.status_state.lock().await;
        state.set(
            &key,
            StatusMessage {
                channel_id: channel.get(),
                message_id: posted.id.get(),
            },
        );
        if let Err(e) = state.save(&self.state_file) {
            eprintln!(
                "Warning: failed to persist status state to {}: {e}",
                self.state_file.display()
            );
        }
        Ok(())
    }
}

// ---------- Bot startup ----------

/// Start the Discord bot. Builds a Serenity client wired with the Poise
/// framework + Phase 1 commands, spawns the gateway loop, and returns the
/// service handle (which holds `Arc<Http>`) plus the join handle for the
/// gateway task. The join handle is kept by the caller so the service stays
/// alive for the life of the process.
pub async fn run_bot(
    bot_config: &DiscordBotConfig,
    resolver: Arc<dyn RigResolver>,
) -> Result<(DiscordBotService, tokio::task::JoinHandle<()>), ChatError> {
    let token = bot_config.token.clone();
    let default_channel_id = bot_config.default_channel_id;
    let state_file = PathBuf::from(&bot_config.state_file);
    let status_state = StatusState::load(&state_file).unwrap_or_else(|e| {
        eprintln!(
            "Warning: could not load status state from {}: {e} — starting fresh",
            state_file.display()
        );
        StatusState::default()
    });
    let status_state = Arc::new(Mutex::new(status_state));

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: phase1_commands(),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                println!(
                    "Discord bot connected as {} (id {}), guilds: {}",
                    ready.user.name,
                    ready.user.id,
                    ready.guilds.len()
                );
                poise::builtins::register_globally(ctx, &framework.options().commands)
                    .await
                    .map_err(|e| -> BotError { Box::new(e) })?;
                Ok(BotData { resolver })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILDS;
    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .await
        .map_err(|e| ChatError::Initialization {
            service_name: "Discord bot".to_string(),
            reason: e.to_string(),
        })?;

    let http = client.http.clone();

    let join = tokio::spawn(async move {
        if let Err(e) = client.start().await {
            eprintln!("Discord bot gateway error: {e}");
        }
    });

    Ok((
        DiscordBotService::new(
            http,
            default_channel_id,
            status_state,
            state_file,
            bot_config.live_status,
        ),
        join,
    ))
}

// ---------- Slash commands (Phase 1, read-only) ----------

/// Chatstronomy telescope monitoring commands.
#[poise::command(
    slash_command,
    subcommands(
        // Read-only (anyone)
        "status",
        "sequence",
        "target",
        "mount",
        "filter",
        "focus",
        "guider",
        "events",
        "last_image",
        // Write (ACL-gated; destructive ones require confirmation)
        "park",
        "unpark",
        "home",
        "change_filter",
        "guider_start",
        "guider_stop",
        "cool",
        "warm",
        "autofocus",
        "abort_capture",
        "stop_sequence",
        "start_sequence",
    )
)]
async fn chatstronomy(_ctx: Context<'_>) -> Result<(), BotError> {
    // Parent never runs directly when subcommands are defined.
    Ok(())
}

fn phase1_commands() -> Vec<poise::Command<BotData, BotError>> {
    vec![chatstronomy()]
}

/// Facts about this invocation for the resolver: where it happened and who
/// invoked it, including the member's roles when in a guild.
async fn command_context(ctx: Context<'_>) -> CommandContext {
    let (role_ids, member_manages) = match ctx.author_member().await {
        Some(member) => {
            // Slash-command interactions carry the member's computed
            // permissions from Discord itself.
            let manages = member.permissions.is_some_and(|permissions| {
                permissions.administrator() || permissions.manage_guild()
            });
            (member.roles.iter().map(|r| r.get()).collect(), manages)
        }
        None => (Vec::new(), false),
    };
    // Guild owners manage regardless of roles. The cache holds the guild
    // thanks to the GUILDS intent; the ref must drop before any await.
    let is_owner = ctx
        .guild()
        .map(|guild| guild.owner_id == ctx.author().id)
        .unwrap_or(false);
    CommandContext {
        guild_id: ctx.guild_id().map(|g| g.get()),
        channel_id: ctx.channel_id().get(),
        user_id: ctx.author().id.get(),
        role_ids,
        manages_guild: member_manages || is_owner,
    }
}

/// Shorthand for "resolve telescope, send an ephemeral error to the user if
/// it fails."
async fn resolve_or_reply<'a>(
    ctx: Context<'a>,
    telescope: Option<String>,
) -> Result<(String, SharedRigSource), BotError> {
    let invocation = command_context(ctx).await;
    match ctx
        .data()
        .resolver
        .resolve(&invocation, telescope.as_deref())
    {
        Ok(v) => Ok(v),
        Err(msg) => {
            ctx.send(poise::CreateReply::default().content(msg).ephemeral(true))
                .await?;
            Err("telescope resolution failed".into())
        }
    }
}

/// Resolve the telescope, then check write authorization for it. Sends an
/// ephemeral error on either failure. Write commands call this instead of
/// `resolve_or_reply`.
async fn resolve_write_or_reply<'a>(
    ctx: Context<'a>,
    telescope: Option<String>,
) -> Result<(String, SharedRigSource), BotError> {
    let invocation = command_context(ctx).await;
    // One resolver call so the authorization decision provably applies to
    // the rig the command will actuate.
    match ctx
        .data()
        .resolver
        .resolve_for_write(&invocation, telescope.as_deref())
    {
        Ok(v) => Ok(v),
        Err(msg) => {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!("❌ {msg}"))
                    .ephemeral(true),
            )
            .await?;
            Err("write resolution or authorization failed".into())
        }
    }
}

/// Display a one-page status summary including target, mount, sequence, and filter.
#[poise::command(slash_command)]
async fn status(
    ctx: Context<'_>,
    #[description = "Telescope name (defaults to this channel's telescope)"] telescope: Option<
        String,
    >,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;

    let mut embed = serenity::CreateEmbed::new().title(format!("[{name}] Status"));

    if let Ok(mount) = client.get_mount_info().await {
        let (ra, dec) = mount.get_coordinates();
        let (alt, az) = mount.get_alt_az();
        let mut status = format!(
            "Connected: {}\nTracking: {}\nParked: {}\nRA: {} Dec: {}",
            mount.is_connected(),
            mount.response.tracking_enabled,
            mount.response.at_park,
            ra,
            dec
        );
        if !alt.is_empty() && !az.is_empty() {
            status.push_str(&format!("\nAlt: {alt} Az: {az}"));
        }
        embed = embed.field("Mount", status, false);
    }

    if let Ok(seq) = client.get_sequence().await {
        let containers = seq.get_containers();
        let running = containers
            .iter()
            .filter(|c| c.status.eq_ignore_ascii_case("RUNNING"))
            .count();
        let active_target =
            crate::sequence::extract_current_target(&seq).unwrap_or_else(|| "(none)".to_string());
        embed = embed.field(
            "Sequence",
            format!(
                "Target: {active_target}\nContainers: {} total, {running} running",
                containers.len()
            ),
            false,
        );
        let operations = active_chat_operations(&seq);
        if !operations.is_empty() {
            let camera = if operations.iter().any(|operation| {
                matches!(
                    operation.kind,
                    SequenceOperationKind::CameraCooling { .. }
                        | SequenceOperationKind::CameraWarming { .. }
                )
            }) {
                client
                    .get_camera_info()
                    .await
                    .ok()
                    .filter(|response| response.success && response.response.connected)
                    .map(|response| response.response)
            } else {
                None
            };
            embed = embed.field(
                "Active operations",
                operations
                    .iter()
                    .map(|operation| sequence_operation_summary(operation, camera.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                false,
            );
        }
    }

    if let Ok(fw) = client.get_filterwheel_info().await
        && fw.response.connected
        && let Some(sel) = &fw.response.selected_filter
    {
        embed = embed.field("Filter", format!("{} (ID: {})", sel.name, sel.id), true);
    }

    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Display the current imaging sequence details and the estimated meridian flip time.
#[poise::command(slash_command)]
async fn sequence(
    ctx: Context<'_>,
    #[description = "Telescope name (defaults to this channel's telescope)"] telescope: Option<
        String,
    >,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let seq = client.get_sequence().await?;
    let containers = seq.get_containers();
    let lines: Vec<String> = containers
        .iter()
        .map(|c| format!("• {} — {} ({} items)", c.name, c.status, c.items.len()))
        .collect();
    let active_target =
        crate::sequence::extract_current_target(&seq).unwrap_or_else(|| "(none)".to_string());
    let flip = crate::sequence::extract_meridian_flip_time(&seq)
        .map(crate::sequence::meridian_flip_time_formatted_with_clock)
        .unwrap_or_else(|| "(n/a)".to_string());
    let operations = active_chat_operations(&seq);
    let camera = if operations.iter().any(|operation| {
        matches!(
            operation.kind,
            SequenceOperationKind::CameraCooling { .. }
                | SequenceOperationKind::CameraWarming { .. }
        )
    }) {
        client
            .get_camera_info()
            .await
            .ok()
            .filter(|response| response.success && response.response.connected)
            .map(|response| response.response)
    } else {
        None
    };

    let mut embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Sequence"))
        .field("Active target", active_target, true)
        .field("Meridian flip in", flip, true)
        .field("Containers", lines.join("\n"), false);
    if !operations.is_empty() {
        embed = embed.field(
            "Active operations",
            operations
                .iter()
                .map(|operation| sequence_operation_summary(operation, camera.as_ref()))
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        );
    }
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

fn active_chat_operations(sequence: &crate::sequence::SequenceResponse) -> Vec<SequenceOperation> {
    crate::sequence::extract_sequence_operations(sequence)
        .into_iter()
        .filter(|operation| operation.chat_enabled && operation.is_active())
        .collect()
}

fn sequence_operation_summary(
    operation: &SequenceOperation,
    camera: Option<&crate::camera::CameraInfo>,
) -> String {
    match &operation.kind {
        SequenceOperationKind::CameraCooling {
            target_temperature,
            minimum_duration,
        } => {
            let minimum = minimum_duration
                .map(|duration| format!(", minimum {}", short_duration(duration)))
                .unwrap_or_default();
            camera
                .filter(|camera| camera.temperature.is_finite())
                .map_or_else(
                    || format!("❄️ Cooling to {target_temperature:.1} °C{minimum}"),
                    |camera| {
                        let power = if camera.cooler_power.is_finite() {
                            format!(" at {:.0}% power", camera.cooler_power)
                        } else {
                            String::new()
                        };
                        format!(
                            "❄️ Cooling {:.1} → {target_temperature:.1} °C{power}{minimum}",
                            camera.temperature
                        )
                    },
                )
        }
        SequenceOperationKind::CameraWarming { minimum_duration } => {
            let minimum = minimum_duration
                .map(|duration| format!(", minimum {}", short_duration(duration)))
                .unwrap_or_default();
            camera
                .filter(|camera| camera.temperature.is_finite())
                .map_or_else(
                    || format!("🌡️ Camera warming{minimum}"),
                    |camera| format!("🌡️ Camera warming at {:.1} °C{minimum}", camera.temperature),
                )
        }
        SequenceOperationKind::TimeWait {
            target_time,
            configured_duration,
        } => {
            if let Some(target) = target_time {
                let remaining = target
                    .with_timezone(&chrono::Utc)
                    .signed_duration_since(chrono::Utc::now())
                    .max(chrono::Duration::zero());
                format!(
                    "⏳ Waiting until {} ({} remaining)",
                    target.format("%H:%M %Z"),
                    short_duration(remaining)
                )
            } else if let Some(duration) = configured_duration {
                format!("⏳ Timed wait ({} configured)", short_duration(*duration))
            } else {
                "⏳ Timed wait".to_string()
            }
        }
        SequenceOperationKind::AstronomicalWait {
            target_altitude_degrees,
            current_altitude_degrees,
            comparator,
            expected_time,
        } => {
            let current = current_altitude_degrees.map(|value| format!("current {value:.2}°"));
            let target = target_altitude_degrees.map(|value| {
                format!(
                    "target {}{value:.2}°",
                    comparator
                        .as_deref()
                        .map(|comparison| format!("{comparison} "))
                        .unwrap_or_default()
                )
            });
            let expected = expected_time
                .as_deref()
                .map(|value| format!("expected {value}"));
            let details = [current, target, expected]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
            if details.is_empty() {
                format!("🌌 {}", operation.name)
            } else {
                format!("🌌 {} ({details})", operation.name)
            }
        }
        SequenceOperationKind::SafetyWait {
            is_safe,
            wait_interval,
        } => {
            let state = match is_safe {
                Some(true) => "safe; resolving",
                Some(false) => "unsafe",
                None => "monitor disconnected",
            };
            let interval = wait_interval
                .map(|duration| format!("; checking every {}", short_duration(duration)))
                .unwrap_or_default();
            format!("🛡️ Waiting for safe conditions ({state}{interval})")
        }
        SequenceOperationKind::ConditionWait { wait_interval } => wait_interval.map_or_else(
            || "⏳ Waiting for a sequence condition".to_string(),
            |duration| {
                format!(
                    "⏳ Waiting for a sequence condition (checking every {})",
                    short_duration(duration)
                )
            },
        ),
        SequenceOperationKind::ManualWait => "⏸️ Waiting for manual sequence resume".to_string(),
        SequenceOperationKind::MountSlew { coordinates, .. } => coordinates.as_ref().map_or_else(
            || "🔭 Mount slew in progress".to_string(),
            |coordinates| format!("🔭 Slewing to {}", coordinates.display()),
        ),
        SequenceOperationKind::MountCenter {
            coordinates,
            rotation,
            output,
        } => {
            let target = coordinates
                .as_ref()
                .map(|coordinates| format!(" on {}", coordinates.display()))
                .unwrap_or_default();
            let rotation = rotation
                .map(|rotation| format!(" at {rotation:.1}°"))
                .unwrap_or_default();
            let solve = output
                .as_ref()
                .and_then(|output| output.success)
                .map(|success| {
                    if success {
                        "; solve succeeded"
                    } else {
                        "; solve failed"
                    }
                })
                .unwrap_or_default();
            format!("🎯 Centering{target}{rotation}{solve}")
        }
        SequenceOperationKind::PlateSolve {
            coordinates,
            rotation,
            output,
        } => {
            let target = coordinates
                .as_ref()
                .map(|coordinates| format!(" near {}", coordinates.display()))
                .unwrap_or_default();
            let rotation = rotation
                .map(|rotation| format!(" at {rotation:.1}°"))
                .unwrap_or_default();
            let result = output
                .as_ref()
                .and_then(|output| output.success)
                .map(|success| {
                    if success {
                        "; result succeeded"
                    } else {
                        "; result failed"
                    }
                })
                .unwrap_or_default();
            format!("🔎 Plate solving{target}{rotation}{result}")
        }
    }
}

fn short_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Events the local N.I.N.A. profile permitted for chat delivery. Keeping this
/// filter shared prevents read-only slash commands from exposing details sent
/// by an older payload-v3 peer with `ChatEnabled: false`.
fn chat_enabled_events(
    history: &crate::events::EventHistoryResponse,
) -> std::vec::IntoIter<&crate::events::Event> {
    let mut visible = Vec::new();
    for event in &history.response {
        let scope = crate::events::event_delivery_scope(&event.event);
        if event.chat_enabled {
            visible.push(event);
        } else {
            // A disabled payload-v3 record is a category tombstone. Remove
            // older values in the same local privacy scope so commands cannot
            // walk backwards to data shared before that switch was disabled.
            visible
                .retain(|candidate| crate::events::event_delivery_scope(&candidate.event) != scope);
        }
    }
    visible.into_iter()
}

type TargetEventProjection = (
    String,
    Option<crate::events::TargetCoordinates>,
    Option<String>,
    Option<f64>,
);

fn latest_target_event(
    history: &crate::events::EventHistoryResponse,
    history_allowed: bool,
) -> Option<TargetEventProjection> {
    if !history_allowed {
        return None;
    }
    chat_enabled_events(history).rev().find_map(|event| {
        if !matches!(
            event.event.as_str(),
            crate::events::event_types::TS_TARGETSTART
                | crate::events::event_types::TS_NEWTARGETSTART
        ) {
            return None;
        }
        if let Some(crate::events::EventDetails::TargetStart {
            target_name,
            coordinates,
            project_name,
            rotation,
            ..
        }) = &event.details
            && target_name != "Sequential Instruction Set"
        {
            Some((
                target_name.clone(),
                coordinates.clone(),
                project_name.clone(),
                *rotation,
            ))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod sequence_operation_summary_tests {
    use super::*;

    fn operation(kind: SequenceOperationKind) -> SequenceOperation {
        SequenceOperation {
            key: "0/1".to_string(),
            name: "Sequencer+ wait".to_string(),
            status: "RUNNING".to_string(),
            chat_enabled: true,
            kind,
        }
    }

    #[test]
    fn sequencer_plus_wait_summaries_are_useful_without_condition_details() {
        let condition = sequence_operation_summary(
            &operation(SequenceOperationKind::ConditionWait {
                wait_interval: Some(chrono::Duration::seconds(13)),
            }),
            None,
        );
        let manual =
            sequence_operation_summary(&operation(SequenceOperationKind::ManualWait), None);

        assert_eq!(
            condition,
            "⏳ Waiting for a sequence condition (checking every 13s)"
        );
        assert_eq!(manual, "⏸️ Waiting for manual sequence resume");
    }

    #[test]
    fn astronomical_wait_and_plate_solve_have_slash_command_summaries() {
        let astronomical = sequence_operation_summary(
            &operation(SequenceOperationKind::AstronomicalWait {
                target_altitude_degrees: Some(-12.0),
                current_altitude_degrees: Some(-8.5),
                comparator: Some("LESS_THAN_OR_EQUAL".to_string()),
                expected_time: Some("2026-08-26T21:30:00-07:00".to_string()),
            }),
            None,
        );
        assert!(astronomical.contains("current -8.50°"));
        assert!(astronomical.contains("target LESS_THAN_OR_EQUAL -12.00°"));

        let solve = sequence_operation_summary(
            &operation(SequenceOperationKind::PlateSolve {
                coordinates: None,
                rotation: Some(31.5),
                output: Some(Box::new(crate::sequence::PlateSolveOutput {
                    solve_time: Some("2026-08-26T21:30:00-07:00".to_string()),
                    success: Some(true),
                    coordinates: None,
                    position_angle: Some(31.5),
                    pixel_scale: None,
                    radius_degrees: None,
                    separation_arcseconds: None,
                    ra_error: None,
                    dec_error: None,
                    ra_pixel_error: None,
                    dec_pixel_error: None,
                    flipped: None,
                    thumbnail: None,
                    thumbnail_media_type: None,
                })),
            }),
            None,
        );
        assert_eq!(solve, "🔎 Plate solving at 31.5°; result succeeded");
    }

    #[test]
    fn disabled_operations_are_absent_from_discord_status_and_sequence_views() {
        let sequence: crate::sequence::SequenceResponse =
            serde_json::from_value(serde_json::json!({
                "Response": [{
                    "Name": "Waits",
                    "Status": "RUNNING",
                    "Items": [{
                        "Name": "Private safety wait",
                        "Status": "RUNNING",
                        "OperationKind": "safety_wait",
                        "IsSafe": false,
                        "WaitInterval": "00:00:05",
                        "ChatEnabled": false
                    }, {
                        "Name": "Visible condition wait",
                        "Status": "RUNNING",
                        "OperationKind": "condition_wait",
                        "WaitInterval": "00:00:13",
                        "ChatEnabled": true
                    }]
                }],
                "Error": "",
                "StatusCode": 200,
                "Success": true,
                "Type": "API"
            }))
            .unwrap();

        let operations = active_chat_operations(&sequence);
        assert_eq!(operations.len(), 1);
        assert!(matches!(
            operations[0].kind,
            SequenceOperationKind::ConditionWait { .. }
        ));
    }

    #[test]
    fn disabled_events_revoke_older_values_before_recent_limits_and_target_lookup() {
        let history: crate::events::EventHistoryResponse =
            serde_json::from_value(serde_json::json!({
                "Response": [{
                    "Time": "2026-08-26T06:00:00Z",
                    "Event": "TS-TARGETSTART",
                    "ChatEnabled": true,
                    "TargetName": "Visible target"
                }, {
                    "Time": "2026-08-26T06:00:01Z",
                    "Event": "TS-TARGETSTART",
                    "ChatEnabled": false,
                    "TargetName": "Private target"
                }],
                "Error": "",
                "StatusCode": 200,
                "Success": true,
                "Type": "API"
            }))
            .unwrap();

        let recent: Vec<_> = chat_enabled_events(&history).rev().take(1).collect();
        assert!(recent.is_empty());
    }

    #[test]
    fn disabled_sequence_target_blocks_older_target_event_fallback() {
        let sequence: crate::sequence::SequenceResponse =
            serde_json::from_value(serde_json::json!({
                "Response": [{
                    "Name": "_Container",
                    "Status": "SUPPRESSED",
                    "IsTargetContainer": true,
                    "ChatEnabled": false,
                    "Items": []
                }],
                "Error": "",
                "StatusCode": 200,
                "Success": true,
                "Type": "API"
            }))
            .unwrap();
        let history: crate::events::EventHistoryResponse =
            serde_json::from_value(serde_json::json!({
                "Response": [{
                    "Time": "2026-08-26T06:00:00Z",
                    "Event": "TS-TARGETSTART",
                    "ChatEnabled": true,
                    "TargetName": "Previously shared target"
                }],
                "Error": "",
                "StatusCode": 200,
                "Success": true,
                "Type": "API"
            }))
            .unwrap();

        let sequence_target = crate::sequence::extract_current_target_with_delivery(&sequence);
        let history_allowed = !matches!(sequence_target, Some((_, false)));
        assert!(latest_target_event(&history, history_allowed).is_none());
    }

    #[test]
    fn target_history_ignores_wrapper_and_accepts_new_target_events() {
        let history: crate::events::EventHistoryResponse =
            serde_json::from_value(serde_json::json!({
                "Response": [
                    {
                        "Time": "2026-08-26T06:00:00Z",
                        "Event": "TS-NEWTARGETSTART",
                        "ChatEnabled": true,
                        "TargetName": "M31"
                    },
                    {
                        "Time": "2026-08-26T06:00:01Z",
                        "Event": "TS-TARGETSTART",
                        "ChatEnabled": true,
                        "TargetName": "Sequential Instruction Set"
                    }
                ],
                "Error": "",
                "StatusCode": 200,
                "Success": true,
                "Type": "API"
            }))
            .unwrap();

        assert_eq!(
            latest_target_event(&history, true).map(|target| target.0),
            Some("M31".to_string())
        );
    }
}

/// Display target coordinates and sequence details for the active target.
#[poise::command(slash_command)]
async fn target(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let seq = client.get_sequence().await?;
    let sequence_target = crate::sequence::extract_current_target_with_delivery(&seq);
    let history_allowed = !matches!(sequence_target, Some((_, false)));
    let active_target = sequence_target
        .filter(|(_, chat_enabled)| *chat_enabled)
        .map(|(name, _)| name)
        .unwrap_or_else(|| "(none)".to_string());

    // Look for the latest TS-TARGETSTART event for richer coords.
    let events = client.get_event_history().await.ok();
    let ts_target = events
        .as_ref()
        .and_then(|events| latest_target_event(events, history_allowed));

    let mut embed = serenity::CreateEmbed::new().title(format!("[{name}] Target"));
    if let Some((tname, coords, project, rot)) = ts_target {
        // Target Scheduler only supplies project, rotation, and coordinates
        // when the target carries those broker headers; omit the rows rather
        // than showing empty ones.
        embed = embed.field("Name", tname, true);
        if let Some(project) = project {
            embed = embed.field("Project", project, true);
        }
        if let Some(rot) = rot {
            embed = embed.field("Rotation", format!("{rot}°"), true);
        }
        if let Some(s) = coords.as_ref().and_then(|coords| coords.display()) {
            embed = embed.field("Coordinates", s, false);
        }
    } else {
        embed = embed.field("Sequence target", active_target, false);
    }
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Display mount connection status, tracking state, and current coordinates.
#[poise::command(slash_command)]
async fn mount(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let mount = client.get_mount_info().await?;
    let m = &mount.response;
    let (ra, dec) = mount.get_coordinates();
    let (alt, az) = mount.get_alt_az();
    let flip = mount.get_time_to_meridian_flip_string();

    let mut embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Mount"))
        .field(
            "Status",
            format!(
                "Connected: {}\nTracking: {}\nParked: {}\nSlewing: {}\nAt home: {}",
                m.connected, m.tracking_enabled, m.at_park, m.slewing, m.at_home
            ),
            false,
        )
        .field("RA / Dec", format!("RA: {ra}\nDec: {dec}"), true);
    if !alt.is_empty() && !az.is_empty() {
        embed = embed.field("Alt / Az", format!("Alt: {alt}\nAz: {az}"), true);
    }
    embed = embed.field("Pier side", mount.get_side_of_pier().to_string(), true);
    if !m.location_redacted && !m.sidereal_time_string.is_empty() {
        embed = embed.field("Sidereal time", &m.sidereal_time_string, true);
    }
    if !flip.is_empty() {
        embed = embed.field("Time to flip", flip, true);
    }
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Display the active optical filter and the list of available filters.
#[poise::command(slash_command)]
async fn filter(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let fw = client.get_filterwheel_info().await?;
    let mut embed = serenity::CreateEmbed::new().title(format!("[{name}] Filter wheel"));
    if let Some(sel) = &fw.response.selected_filter {
        embed = embed.field("Selected", format!("{} (ID: {})", sel.name, sel.id), false);
    } else {
        embed = embed.field("Selected", "(none)", false);
    }
    if !fw.response.available_filters.is_empty() {
        let avail = fw
            .response
            .available_filters
            .iter()
            .map(|f| format!("{} ({})", f.name, f.id))
            .collect::<Vec<_>>()
            .join(", ");
        embed = embed.field("Available", avail, false);
    }
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Display current focuser position, temperature, and last autofocus run stats.
#[poise::command(slash_command)]
async fn focus(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let af = client.get_last_autofocus().await?;
    let d = &af.response;
    let position = format!("{:.0}", d.calculated_focus_point.position);
    let position_text = if d.previous_focus_point.position.is_finite() {
        let position_change = d.calculated_focus_point.position - d.previous_focus_point.position;
        format!("{position} (Δ {position_change:+.0})")
    } else {
        position
    };
    let temperature = if d.temperature.is_finite() {
        format!("{:.1}°C", d.temperature)
    } else {
        "n/a".to_string()
    };
    let final_measurement = d
        .final_hfr()
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "n/a".to_string());
    let fit_quality = d.fit_quality_summary().unwrap_or_else(|| "n/a".to_string());
    let mut embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Last autofocus"))
        .field("Filter", d.filter_name(), true)
        .field("Mode", d.method_summary(), true)
        .field("Duration", &d.duration, true)
        .field("Temperature", temperature, true)
        .field("Position", position_text, true)
        .field(d.final_measurement_label(), final_measurement, true)
        .field("Fit R²", fit_quality, true)
        .field("Timestamp", &d.timestamp, true);
    if let Some(stars) = d.accepted_star_count_summary() {
        embed = embed.field("Accepted stars / point", stars, true);
    }
    if let Some(uncertainty) = d.hyperbolic_minimum_std_error {
        embed = embed.field("Focus precision", format!("±{uncertainty:.2} steps"), true);
    }
    if let Some(stability) = d.hyperbolic_leave_one_out_std_error {
        embed = embed.field("LOO stability", format!("{stability:.2} steps"), true);
    }
    if let Some(chi_squared) = d.hyperbolic_reduced_chi_squared {
        embed = embed.field("Reduced χ²", format!("{chi_squared:.4}"), true);
    }
    if let Some(region) = d.region_summary() {
        embed = embed.field("Detection region", region, true);
    }
    if let Some(acceptance) = d.fit_acceptance_summary() {
        embed = embed.field("Acceptance", acceptance, true);
    }
    if let Some(detection) = d.detection_summary() {
        embed = embed.field("Star detection", detection, false);
    }
    let mut reply = poise::CreateReply::default();
    if let Ok(png) = crate::charts::render_autofocus_graph_png(d) {
        let filename = "autofocus.png";
        embed = embed.image(format!("attachment://{filename}"));
        reply = reply.attachment(CreateAttachment::bytes(png, filename));
    }
    ctx.send(reply.embed(embed)).await?;
    Ok(())
}

/// Display guider connection status, tracking state, and real-time RMS error.
#[poise::command(slash_command)]
async fn guider(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let info = client.get_guider_info().await?;
    let g = &info.response;
    let mut embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Guider"))
        .field("Connected", g.connected.to_string(), true)
        .field("State", &g.state, true);
    if g.pixel_scale > 0.0 {
        embed = embed.field(
            "Pixel scale",
            format!("{:.3} arcsec/px", g.pixel_scale),
            true,
        );
    }
    if let Some(rms) = &g.rms_error {
        embed = embed.field(
            "RMS error",
            format!(
                "Total: {:.2}\"\nRA: {:.2}\"  Dec: {:.2}\"",
                rms.total.arcseconds, rms.ra.arcseconds, rms.dec.arcseconds
            ),
            false,
        );
    }
    let mut reply = poise::CreateReply::default();
    if client.capabilities().guider_graph
        && let Ok(graph) = client.get_guider_graph().await
        && graph.success
        && graph.response.has_graph_data()
        && let Ok(png) = crate::charts::render_guider_graph_png(&graph.response)
    {
        let filename = "guiding.png";
        embed = embed.image(format!("attachment://{filename}"));
        reply = reply.attachment(CreateAttachment::bytes(png, filename));
    }
    ctx.send(reply.embed(embed)).await?;
    Ok(())
}

/// Display the recent event log history for the telescope.
#[poise::command(slash_command)]
async fn events(
    ctx: Context<'_>,
    #[description = "Number of events to show (default 10)"] count: Option<u32>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let history = client.get_event_history().await?;
    let count = count.unwrap_or(10).min(25) as usize;
    let events: Vec<&crate::events::Event> =
        chat_enabled_events(&history).rev().take(count).collect();
    let lines: Vec<String> = events
        .iter()
        .rev()
        .map(|e| format!("`{}` {}", e.time, e.event))
        .collect();
    let embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Last {count} events"))
        .description(if lines.is_empty() {
            "(no events)".to_string()
        } else {
            lines.join("\n")
        });
    ctx.send(poise::CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Display the most recently captured image with exposure metadata.
#[poise::command(slash_command, rename = "last-image")]
async fn last_image(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    let images = client.get_all_image_history().await?;
    // Image delivery is one local privacy category. A trailing disabled
    // compatibility record revokes the older history rather than allowing
    // `/last-image` to walk backwards across that boundary.
    let Some((idx, img)) = images
        .response
        .iter()
        .enumerate()
        .next_back()
        .filter(|(_, image)| image.chat_enabled)
    else {
        ctx.send(
            poise::CreateReply::default()
                .content("No images in history.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    let thumb_bytes = client.get_thumbnail(idx as u32).await.ok().map(|t| t.data);

    let mut embed = serenity::CreateEmbed::new()
        .title(format!("[{name}] Last image"))
        .field("Date", &img.date, true)
        .field("Type", &img.image_type, true)
        .field("Filter", &img.filter, true)
        .field("Exposure", format!("{:.1}s", img.exposure_time), true)
        .field("Temperature", format!("{:.1}°C", img.temperature), true)
        .field("Stars", img.stars.to_string(), true)
        .field("HFR", format!("{:.2}", img.hfr), true)
        .field("RMS", &img.rms_text, true);

    let mut reply = poise::CreateReply::default();
    if let Some(bytes) = thumb_bytes {
        let filename = format!("thumbnail_{idx}.jpg");
        embed = embed.image(format!("attachment://{filename}"));
        reply = reply.attachment(CreateAttachment::bytes(bytes, filename));
    }
    if client.capabilities().guider_graph
        && let Ok(graph) = client.get_guider_graph().await
        && graph.success
        && graph.response.has_graph_data()
        && let Ok(png) = crate::charts::render_guider_graph_png(&graph.response)
    {
        reply = reply.attachment(CreateAttachment::bytes(png, format!("guiding_{idx}.png")));
    }
    reply = reply.embed(embed);
    ctx.send(reply).await?;
    Ok(())
}

// ---------- Phase 3: write commands (ACL-gated) ----------

/// Post a Confirm / Cancel button pair and wait for the invoker to click.
/// Returns `Ok(true)` on Confirm, `Ok(false)` on Cancel or 30s timeout.
async fn confirm_destructive(ctx: Context<'_>, action: &str) -> Result<bool, BotError> {
    let prompt =
        format!("⚠️ Confirm **{action}**?\nThis is a destructive operation — you have 30 seconds.");
    let row = serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new("chatstronomy-confirm")
            .label("Confirm")
            .style(serenity::ButtonStyle::Danger),
        serenity::CreateButton::new("chatstronomy-cancel")
            .label("Cancel")
            .style(serenity::ButtonStyle::Secondary),
    ]);
    let handle = ctx
        .send(
            poise::CreateReply::default()
                .content(prompt)
                .components(vec![row])
                .ephemeral(true),
        )
        .await?;
    let message = handle.message().await?;

    let interaction = message
        .await_component_interaction(ctx.serenity_context().shard.clone())
        .author_id(ctx.author().id)
        .timeout(std::time::Duration::from_secs(30))
        .await;

    let (confirmed, response_text) = match interaction.as_ref().map(|i| i.data.custom_id.as_str()) {
        Some("chatstronomy-confirm") => (true, "✅ Confirmed, running command…"),
        Some("chatstronomy-cancel") => (false, "❎ Cancelled."),
        _ => (false, "⏱️ Timed out — no action taken."),
    };

    // Acknowledge the interaction (or just edit the original message if
    // the user didn't click anything).
    if let Some(i) = interaction {
        let _ = i
            .create_response(
                &ctx.serenity_context().http,
                serenity::CreateInteractionResponse::UpdateMessage(
                    serenity::CreateInteractionResponseMessage::new()
                        .content(response_text)
                        .components(vec![]),
                ),
            )
            .await;
    } else {
        let _ = handle
            .edit(
                ctx,
                poise::CreateReply::default()
                    .content(response_text)
                    .components(vec![]),
            )
            .await;
    }
    Ok(confirmed)
}

/// Issue a typed rig command and reply with a status line. Used by all
/// write commands once authorization (and any confirmation) has passed.
async fn run_command(
    ctx: Context<'_>,
    telescope: &str,
    client: &SharedRigSource,
    label: &str,
    command: RigCommand,
) -> Result<(), BotError> {
    if !client.capabilities().commands {
        ctx.send(
            poise::CreateReply::default()
                .ephemeral(true)
                .content(format!(
                    "❌ [{telescope}] {label} is not supported by this rig connection"
                )),
        )
        .await?;
        return Ok(());
    }
    let result = client.execute_command(command).await;
    let reply = match result {
        Ok(resp) if resp.is_pending() => poise::CreateReply::default().content(format!(
            "⏳ [{telescope}] {label}: {} (accepted; completion not yet confirmed)",
            resp.summary()
        )),
        Ok(resp) if resp.success => poise::CreateReply::default()
            .content(format!("✅ [{telescope}] {label}: {}", resp.summary())),
        Ok(resp) => poise::CreateReply::default()
            .ephemeral(true)
            .content(format!("❌ [{telescope}] {label}: {}", resp.summary())),
        Err(e) => poise::CreateReply::default()
            .ephemeral(true)
            .content(format!("❌ [{telescope}] {label} failed: {e}")),
    };
    ctx.send(reply).await?;
    Ok(())
}

// --- Non-destructive (ACL only) ---

/// Unpark the mount to enable tracking and slewing.
#[poise::command(slash_command)]
async fn unpark(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(ctx, &name, &client, "Unpark mount", RigCommand::UnparkMount).await
}

/// Send the mount to its designated home position.
#[poise::command(slash_command)]
async fn home(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(ctx, &name, &client, "Home mount", RigCommand::HomeMount).await
}

/// Change the optical filter by name.
#[poise::command(slash_command, rename = "change-filter")]
async fn change_filter(
    ctx: Context<'_>,
    #[description = "Filter name (e.g. L, R, G, B, HA)"] filter: String,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;

    let info = match client.get_filterwheel_info().await {
        Ok(i) => i,
        Err(e) => {
            ctx.send(
                poise::CreateReply::default()
                    .ephemeral(true)
                    .content(format!("❌ [{name}] couldn't fetch filterwheel info: {e}")),
            )
            .await?;
            return Ok(());
        }
    };

    let target = info
        .response
        .available_filters
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(&filter));
    let Some(target) = target else {
        let known: Vec<&str> = info
            .response
            .available_filters
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        ctx.send(
            poise::CreateReply::default()
                .ephemeral(true)
                .content(format!(
                    "❌ [{name}] no filter '{filter}'. Known: {known:?}"
                )),
        )
        .await?;
        return Ok(());
    };

    run_command(
        ctx,
        &name,
        &client,
        &format!("Change filter → {} (ID {})", target.name, target.id),
        RigCommand::ChangeFilter {
            filter_id: target.id,
        },
    )
    .await
}

/// Start guider tracking.
#[poise::command(slash_command, rename = "guider-start")]
async fn guider_start(
    ctx: Context<'_>,
    #[description = "Run calibration first"] calibrate: Option<bool>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(
        ctx,
        &name,
        &client,
        "Start guiding",
        RigCommand::StartGuiding {
            calibrate: calibrate.unwrap_or(false),
        },
    )
    .await
}

/// Stop guider tracking.
#[poise::command(slash_command, rename = "guider-stop")]
async fn guider_stop(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(ctx, &name, &client, "Stop guiding", RigCommand::StopGuiding).await
}

/// Cool the camera sensor to a specified target temperature.
#[poise::command(slash_command)]
async fn cool(
    ctx: Context<'_>,
    #[description = "Target temperature in °C"] temperature: f64,
    #[description = "Minutes to ramp down (default 10)"] minutes: Option<f64>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(
        ctx,
        &name,
        &client,
        &format!(
            "Cool to {temperature:.1}°C over {} min",
            minutes.unwrap_or(10.0)
        ),
        RigCommand::CoolCamera {
            temperature,
            minutes: minutes.unwrap_or(10.0),
        },
    )
    .await
}

/// Warm the camera sensor over a specified duration.
#[poise::command(slash_command)]
async fn warm(
    ctx: Context<'_>,
    #[description = "Minutes to warm (default 10)"] minutes: Option<f64>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    ctx.defer().await?;
    run_command(
        ctx,
        &name,
        &client,
        &format!("Warm camera over {} min", minutes.unwrap_or(10.0)),
        RigCommand::WarmCamera {
            minutes: minutes.unwrap_or(10.0),
        },
    )
    .await
}

// --- Destructive (ACL + button confirm) ---

/// Trigger or cancel a NINA autofocus run.
#[poise::command(slash_command)]
async fn autofocus(
    ctx: Context<'_>,
    #[description = "Cancel a running autofocus"] cancel: Option<bool>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let cancel = cancel.unwrap_or(false);
    if !cancel && !confirm_destructive(ctx, &format!("autofocus run on {name}")).await? {
        return Ok(());
    }
    let label = if cancel {
        "Cancel autofocus"
    } else {
        "Start autofocus"
    };
    run_command(
        ctx,
        &name,
        &client,
        label,
        if cancel {
            RigCommand::CancelAutofocus
        } else {
            RigCommand::StartAutofocus
        },
    )
    .await
}

/// Park the mount safely (requires confirmation).
#[poise::command(slash_command)]
async fn park(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if !confirm_destructive(ctx, &format!("park {name}")).await? {
        return Ok(());
    }
    run_command(ctx, &name, &client, "Park mount", RigCommand::ParkMount).await
}

/// Abort the current camera exposure (requires confirmation).
#[poise::command(slash_command, rename = "abort-capture")]
async fn abort_capture(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if !confirm_destructive(ctx, &format!("abort capture on {name}")).await? {
        return Ok(());
    }
    run_command(
        ctx,
        &name,
        &client,
        "Abort capture",
        RigCommand::AbortExposure,
    )
    .await
}

/// Stop the currently active imaging sequence (requires confirmation).
#[poise::command(slash_command, rename = "stop-sequence")]
async fn stop_sequence(
    ctx: Context<'_>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if !confirm_destructive(ctx, &format!("stop sequence on {name}")).await? {
        return Ok(());
    }
    run_command(
        ctx,
        &name,
        &client,
        "Stop sequence",
        RigCommand::StopSequence,
    )
    .await
}

/// Start the loaded imaging sequence (requires confirmation).
#[poise::command(slash_command, rename = "start-sequence")]
async fn start_sequence(
    ctx: Context<'_>,
    #[description = "Skip pre-run validation"] skip_validation: Option<bool>,
    #[description = "Telescope name"] telescope: Option<String>,
) -> Result<(), BotError> {
    let (name, client) = match resolve_write_or_reply(ctx, telescope).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if !confirm_destructive(ctx, &format!("start sequence on {name}")).await? {
        return Ok(());
    }
    run_command(
        ctx,
        &name,
        &client,
        "Start sequence",
        RigCommand::StartSequence {
            skip_validation: skip_validation.unwrap_or(false),
        },
    )
    .await
}
