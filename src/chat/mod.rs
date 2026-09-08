mod discord_bot;
mod discord_service;
mod matrix_service;
mod rig_resolver;
mod status_state;

pub use discord_bot::{DiscordBotService, run_bot};
pub use discord_service::DiscordChatService;
pub use matrix_service::MatrixChatService;
pub use rig_resolver::{CommandContext, RigResolver, StaticRigResolver};
pub use status_state::{StatusMessage, StatusState};

use crate::error::ChatError;
use crate::source::{RigSourceError, SharedRigSource};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;

// N.I.N.A. publishes image metadata before its background JPEG encoder has
// necessarily populated the thumbnail. Retry only that explicit readiness
// response: transport and protocol failures can each consume their own longer
// timeout and should degrade immediately instead of holding up chat delivery.
const THUMBNAIL_READY_MAX_ATTEMPTS: usize = 6;
const THUMBNAIL_READY_RETRY_DELAY: Duration = Duration::from_millis(200);

pub(crate) async fn retry_resource_not_ready<T, Operation, OperationFuture>(
    mut operation: Operation,
    max_attempts: usize,
    retry_delay: Duration,
) -> Result<T, RigSourceError>
where
    Operation: FnMut() -> OperationFuture,
    OperationFuture: Future<Output = Result<T, RigSourceError>>,
{
    let max_attempts = max_attempts.max(1);
    for attempt in 1..=max_attempts {
        match operation().await {
            Err(RigSourceError::NotReady { .. }) if attempt < max_attempts => {
                tokio::time::sleep(retry_delay).await;
            }
            result => return result,
        }
    }
    unreachable!("the bounded retry loop always returns on its final attempt")
}

fn thumbnail_is_still_preparing(error: &RigSourceError) -> bool {
    match error {
        RigSourceError::NotReady { .. } => true,
        RigSourceError::Rejected { reason, .. } => {
            let reason = reason.to_ascii_lowercase();
            reason.contains("thumbnail")
                && reason.contains("still")
                && (reason.contains("prepar") || reason.contains("encod"))
        }
        _ => false,
    }
}

/// Represents a field in a chat message
#[derive(Debug, Clone)]
pub struct ChatField {
    pub name: String,
    pub value: String,
    /// Optional Discord-specific rendering for values such as native
    /// timestamps. Matrix always receives `value`, so Discord markup never
    /// leaks into Matrix rooms.
    pub discord_value: Option<String>,
    pub inline: bool,
}

/// Represents a chat message to be sent
#[derive(Debug, Clone, Default)]
pub struct ChatMessage {
    pub title: String,
    pub color: Option<u32>,
    pub fields: Vec<ChatField>,
    pub footer: Option<String>,
    pub timestamp: Option<String>,
    /// Matrix does not have Discord's embed timestamp. When set, render the
    /// timestamp as a canonical UTC field under this label for Matrix only.
    pub matrix_timestamp_label: Option<String>,
}

impl ChatMessage {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            color: None,
            fields: Vec::new(),
            footer: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            matrix_timestamp_label: None,
        }
    }

    pub fn color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn field(mut self, name: &str, value: &str, inline: bool) -> Self {
        self.fields.push(ChatField {
            name: name.to_string(),
            value: value.to_string(),
            discord_value: None,
            inline,
        });
        self
    }

    /// Add a field whose Discord representation differs from the portable
    /// Matrix/plain-text value.
    pub fn field_with_discord_value(
        mut self,
        name: &str,
        value: &str,
        discord_value: &str,
        inline: bool,
    ) -> Self {
        self.fields.push(ChatField {
            name: name.to_string(),
            value: value.to_string(),
            discord_value: Some(discord_value.to_string()),
            inline,
        });
        self
    }

    /// Attribute a notification to the source event time. Discord renders it
    /// as the embed timestamp; Matrix renders a labeled canonical UTC field.
    pub fn occurred_at(mut self, label: &str, timestamp: chrono::DateTime<chrono::Utc>) -> Self {
        self.timestamp = Some(timestamp.to_rfc3339());
        self.matrix_timestamp_label = Some(label.to_string());
        self
    }

    pub fn footer(mut self, text: &str) -> Self {
        self.footer = Some(text.to_string());
        self
    }
}

/// Per-telescope routing overrides. Each field, when `Some`, redirects this
/// telescope's posts away from the shared default destination configured on
/// the corresponding `ChatService`.
///
/// When `discord_channel_id` is set, the Discord bot service takes precedence
/// over webhook posting for this telescope — the webhook service defers via
/// `can_route`, and the bot routes the message to the channel.
#[derive(Clone, Default)]
pub struct ChatTarget {
    pub discord_webhook_url: Option<String>,
    pub matrix_room_id: Option<String>,
    pub discord_channel_id: Option<u64>,
    /// Additional Discord channels this telescope posts to (multi-channel
    /// and cross-server destinations on the hub). The bot fans out to
    /// `discord_channel_id` plus all of these, deduplicated.
    pub discord_channel_ids: Vec<u64>,
}

impl std::fmt::Debug for ChatTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatTarget")
            .field(
                "discord_webhook_url",
                &self
                    .discord_webhook_url
                    .as_deref()
                    .map(crate::security::redact_sensitive),
            )
            .field("matrix_room_id", &self.matrix_room_id)
            .field("discord_channel_id", &self.discord_channel_id)
            .field("discord_channel_ids", &self.discord_channel_ids)
            .finish()
    }
}

#[cfg(test)]
mod chat_target_tests {
    use super::*;

    #[test]
    fn all_discord_channels_dedupes_and_merges() {
        let target = ChatTarget {
            discord_webhook_url: None,
            matrix_room_id: None,
            discord_channel_id: Some(1),
            discord_channel_ids: vec![2, 1, 3],
        };
        assert_eq!(target.all_discord_channels(), vec![1, 2, 3]);
        // The hub's targets carry only the list; it must still count as a
        // routable Discord destination everywhere can_route is consulted.
        let list_only = ChatTarget {
            discord_channel_ids: vec![7],
            ..ChatTarget::default()
        };
        assert_eq!(list_only.all_discord_channels(), vec![7]);
    }

    #[test]
    fn config_and_routing_debug_never_reveal_chat_credentials() {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "chat": {
                "discord": {
                    "default_webhook_url": "https://discord.com/api/webhooks/42/shared-hook-secret"
                },
                "matrix": {
                    "homeserver_url": "https://matrix.example/?access_token=matrix-url-secret",
                    "username": "@chat:matrix.example",
                    "password": "matrix-password-secret"
                },
                "discord_bot": {
                    "token": "discord-bot-secret"
                }
            },
            "telescopes": [{
                "name": "North Rig",
                "chat": {
                    "discord_webhook_url": "https://discord.com/api/webhooks/43/override-hook-secret"
                }
            }]
        }))
        .unwrap();

        let rendered = format!("{config:?}");
        let target = format!("{:?}", config.telescopes[0].chat.to_chat_target());
        for secret in [
            "shared-hook-secret",
            "matrix-url-secret",
            "matrix-password-secret",
            "discord-bot-secret",
            "override-hook-secret",
        ] {
            assert!(
                !rendered.contains(secret),
                "config leaked {secret}: {rendered}"
            );
            assert!(!target.contains(secret), "target leaked {secret}: {target}");
        }
        assert!(rendered.contains("North Rig"));
        assert!(rendered.contains("matrix.example"));
        assert!(rendered.contains("[redacted]"));
    }
}

impl ChatTarget {
    /// Every Discord channel this target posts to, deduplicated, in order.
    pub fn all_discord_channels(&self) -> Vec<u64> {
        let mut seen = std::collections::HashSet::new();
        self.discord_channel_id
            .iter()
            .chain(self.discord_channel_ids.iter())
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    }
}

/// Shared Discord configuration. The webhook here is the fallback destination
/// used when a telescope doesn't supply its own override.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SharedDiscordConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Default webhook used by telescopes that don't override it. Accepts
    /// either `default_webhook_url` (new) or `webhook_url` (legacy) on the
    /// wire — see the manual Deserialize impl in serde.
    #[serde(default, alias = "webhook_url")]
    pub default_webhook_url: Option<String>,
}

impl std::fmt::Debug for SharedDiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDiscordConfig")
            .field("enabled", &self.enabled)
            .field(
                "default_webhook_url",
                &self
                    .default_webhook_url
                    .as_deref()
                    .map(crate::security::redact_sensitive),
            )
            .finish()
    }
}

/// Shared Matrix configuration. The login is held once per process and reused
/// across every telescope (each telescope can post to a different room via
/// `ChatTarget::matrix_room_id`).
#[derive(Clone, Serialize, Deserialize)]
pub struct SharedMatrixConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub homeserver_url: String,
    pub username: String,
    pub password: String,
    /// Default room used by telescopes that don't override it. Accepts either
    /// `default_room_id` (new) or `room_id` (legacy).
    #[serde(default, alias = "room_id")]
    pub default_room_id: Option<String>,
}

impl std::fmt::Debug for SharedMatrixConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedMatrixConfig")
            .field("enabled", &self.enabled)
            .field(
                "homeserver_url",
                &crate::security::redact_sensitive(&self.homeserver_url),
            )
            .field("username", &self.username)
            .field("password", &crate::security::secret_marker(&self.password))
            .field("default_room_id", &self.default_room_id)
            .finish()
    }
}

fn default_enabled() -> bool {
    true
}

/// Shared chat configuration at the top of the config file. Persistent
/// connections (Matrix login, Discord bot gateway) live here and are reused
/// across telescopes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<SharedDiscordConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matrix: Option<SharedMatrixConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord_bot: Option<DiscordBotConfig>,
}

/// Shared Discord bot configuration. One bot identity / token serves every
/// telescope; each telescope can map to a different channel via
/// `TelescopeChatOverrides::discord_channel_id`.
#[derive(Clone, Serialize, Deserialize)]
pub struct DiscordBotConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Bot token from the Discord Developer Portal.
    pub token: String,
    /// Discord application ID. Not required for gateway-based slash commands
    /// (Serenity infers it from the token), but useful to keep alongside the
    /// token for HTTP interaction endpoints and tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<u64>,
    /// Discord public key, used to verify interaction payloads when running
    /// command handlers over HTTP webhooks. Unused in the gateway path
    /// (Phase 1), reserved for future use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Optional fallback channel for telescopes that don't override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_channel_id: Option<u64>,
    /// Maintain a pinned live-status message per bot-routed telescope's
    /// channel, edited in place when the telescope's state changes. Default
    /// off — explicit opt-in so users who only want event notifications
    /// don't get a second message stream by surprise.
    #[serde(default)]
    pub live_status: bool,
    /// Where to persist the live-status message IDs (only used when
    /// `live_status` is true).
    #[serde(default = "default_state_file")]
    pub state_file: String,
    /// Explicit Discord user IDs allowed to invoke write commands. An empty
    /// list grants only managers of the invoking guild; direct messages and
    /// operations not locally approved in N.I.N.A. are always rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_acl: Vec<u64>,
}

impl std::fmt::Debug for DiscordBotConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordBotConfig")
            .field("enabled", &self.enabled)
            .field("token", &crate::security::secret_marker(&self.token))
            .field("application_id", &self.application_id)
            .field("public_key", &self.public_key)
            .field("default_channel_id", &self.default_channel_id)
            .field("live_status", &self.live_status)
            .field("state_file", &self.state_file)
            .field("write_acl", &self.write_acl)
            .finish()
    }
}

fn default_state_file() -> String {
    "./chatstronomy-state.json".to_string()
}

/// Per-telescope chat routing overrides. Either field, when present, replaces
/// the shared default for that service for this telescope only. Setting
/// `discord_channel_id` switches that telescope's Discord posts from the
/// webhook path to the bot path.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct TelescopeChatOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_webhook_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix_room_id: Option<String>,
    /// When set, this telescope's Discord posts go through the bot to this
    /// channel; the webhook path is ignored for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_channel_id: Option<u64>,
}

impl std::fmt::Debug for TelescopeChatOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelescopeChatOverrides")
            .field(
                "discord_webhook_url",
                &self
                    .discord_webhook_url
                    .as_deref()
                    .map(crate::security::redact_sensitive),
            )
            .field("matrix_room_id", &self.matrix_room_id)
            .field("discord_channel_id", &self.discord_channel_id)
            .finish()
    }
}

impl TelescopeChatOverrides {
    pub fn to_chat_target(&self) -> ChatTarget {
        ChatTarget {
            discord_webhook_url: self.discord_webhook_url.clone(),
            matrix_room_id: self.matrix_room_id.clone(),
            discord_channel_id: self.discord_channel_id,
            discord_channel_ids: Vec::new(),
        }
    }
}

/// A file to attach to a chat message
#[derive(Debug, Clone)]
pub struct ChatAttachment {
    pub data: Vec<u8>,
    pub filename: String,
}

/// Trait for chat service implementations
#[async_trait]
pub trait ChatService: Send + Sync {
    async fn send_message(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
    ) -> Result<(), ChatError>;

    async fn send_message_with_image(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        image_data: &[u8],
        filename: &str,
    ) -> Result<(), ChatError>;

    /// Send a message with any number of file attachments. The default
    /// implementation degrades to the single-image send (first attachment
    /// only) for services that don't override it.
    async fn send_message_with_attachments(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        attachments: &[ChatAttachment],
    ) -> Result<(), ChatError> {
        match attachments.first() {
            Some(first) => {
                self.send_message_with_image(message, target, &first.data, &first.filename)
                    .await
            }
            None => self.send_message(message, target).await,
        }
    }

    fn service_name(&self) -> &'static str;

    /// True if this service has a destination for the given target. Lets the
    /// manager skip services that would have no valid destination (e.g. a
    /// telescope without a webhook override on a Discord service with no
    /// default).
    fn can_route(&self, target: &ChatTarget) -> bool;

    /// Upsert a "live status" message: edit the previously-posted message
    /// for this telescope in place if one exists, otherwise post a new
    /// one and remember its ID. Default implementation is a no-op for
    /// services that don't support editing (webhooks, Matrix).
    async fn upsert_status(
        &self,
        _telescope: &str,
        _target: &ChatTarget,
        _message: &ChatMessage,
    ) -> Result<(), ChatError> {
        Ok(())
    }

    /// True if this service knows how to edit a previously-posted status
    /// message. Used to decide whether to bother building the embed.
    fn supports_status_upsert(&self) -> bool {
        false
    }
}

/// Chat service manager. One instance is shared across all telescopes; the
/// `ChatTarget` passed to each send selects the per-telescope destination.
pub struct ChatServiceManager {
    services: Vec<Box<dyn ChatService>>,
}

impl ChatServiceManager {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
        }
    }

    pub fn add_service(&mut self, service: Box<dyn ChatService>) {
        self.services.push(service);
    }

    /// Refresh the live status message for a telescope across every service
    /// that supports editing. Currently only the Discord bot acts on this.
    pub async fn upsert_status(&self, telescope: &str, target: &ChatTarget, message: &ChatMessage) {
        for service in &self.services {
            if !service.supports_status_upsert() || !service.can_route(target) {
                continue;
            }
            if let Err(e) = service.upsert_status(telescope, target, message).await {
                eprintln!(
                    "Failed to upsert status on {} for {telescope}: {}",
                    service.service_name(),
                    e
                );
            }
        }
    }

    /// True when at least one service in the manager can edit live status
    /// messages for this target. Lets callers skip building the embed
    /// entirely when nothing would consume it.
    pub fn has_status_upsert(&self, target: &ChatTarget) -> bool {
        self.services
            .iter()
            .any(|s| s.supports_status_upsert() && s.can_route(target))
    }

    pub async fn send_message(&self, message: &ChatMessage, target: &ChatTarget) {
        for service in &self.services {
            if !service.can_route(target) {
                continue;
            }
            if let Err(e) = service.send_message(message, target).await {
                eprintln!(
                    "Failed to send message to {}: {}",
                    service.service_name(),
                    e
                );
            }
        }
    }

    /// Send an image-history notification: the thumbnail for `image_index`
    /// plus any extra attachments (e.g. the rendered guiding graph). If the
    /// thumbnail download fails the extras still go out; with nothing to
    /// attach this degrades to a plain message.
    pub async fn send_message_with_image(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        source: &SharedRigSource,
        image_index: u32,
        extra_attachments: Vec<ChatAttachment>,
    ) {
        let mut attachments = Vec::new();
        let mut attempt = 1;
        let thumbnail = loop {
            match source.get_thumbnail(image_index).await {
                Err(error)
                    if attempt < THUMBNAIL_READY_MAX_ATTEMPTS
                        && thumbnail_is_still_preparing(&error) =>
                {
                    attempt += 1;
                    tokio::time::sleep(THUMBNAIL_READY_RETRY_DELAY).await;
                }
                result => break result,
            }
        };
        match thumbnail {
            Ok(thumbnail_data) => {
                attachments.push(ChatAttachment {
                    data: thumbnail_data.data,
                    filename: format!("thumbnail_{}.jpg", image_index),
                });
            }
            Err(e) => {
                eprintln!(
                    "Failed to download thumbnail for image {}: {}",
                    image_index, e
                );
            }
        }
        attachments.extend(extra_attachments);
        self.send_message_with_attachments(message, target, &attachments)
            .await;
    }

    /// Send a message with pre-built attachments to every routable service.
    pub async fn send_message_with_attachments(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        attachments: &[ChatAttachment],
    ) {
        if attachments.is_empty() {
            self.send_message(message, target).await;
            return;
        }
        for service in &self.services {
            if !service.can_route(target) {
                continue;
            }
            if let Err(e) = service
                .send_message_with_attachments(message, target, attachments)
                .await
            {
                eprintln!(
                    "Failed to send message with attachments to {}: {}",
                    service.service_name(),
                    e
                );
            }
        }
    }

    pub fn service_count(&self) -> usize {
        self.services.len()
    }
}

impl Default for ChatServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod thumbnail_retry_tests {
    use super::*;
    use crate::api_types::CommandResponse;
    use crate::autofocus::AutofocusResponse;
    use crate::camera::CameraInfoResponse;
    use crate::events::EventHistoryResponse;
    use crate::filterwheel::FilterWheelInfoResponse;
    use crate::focuser::FocuserInfoResponse;
    use crate::guider::{GuiderGraphResponse, GuiderInfoResponse};
    use crate::images::{ImageHistoryResponse, ThumbnailResponse};
    use crate::mount::MountInfoResponse;
    use crate::rotator::RotatorInfoResponse;
    use crate::sequence::SequenceResponse;
    use crate::source::{RigCapabilities, RigCommand, RigSource, RigSourceKind, RigSourceResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::time::Instant;

    #[tokio::test]
    async fn resource_not_ready_retries_are_bounded_and_typed() {
        let attempts = AtomicUsize::new(0);
        let value = retry_resource_not_ready(
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if attempt < 3 {
                        Err(RigSourceError::NotReady {
                            kind: RigSourceKind::NinaDirect,
                            reason: "autofocus report is still being published".to_string(),
                        })
                    } else {
                        Ok(42)
                    }
                }
            },
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        let terminal_attempts = AtomicUsize::new(0);
        let terminal = retry_resource_not_ready(
            || {
                terminal_attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), _>(RigSourceError::InvalidResponse {
                        kind: RigSourceKind::NinaDirect,
                        reason: "malformed autofocus payload".to_string(),
                    })
                }
            },
            3,
            Duration::ZERO,
        )
        .await;
        assert!(matches!(
            terminal,
            Err(RigSourceError::InvalidResponse { .. })
        ));
        assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone, Copy)]
    enum ThumbnailBehavior {
        ReadyAfter(usize),
        TypedReadyAfter(usize),
        AlwaysPreparing,
        TerminalFailure,
    }

    struct ThumbnailSource {
        behavior: ThumbnailBehavior,
        attempts: AtomicUsize,
    }

    impl ThumbnailSource {
        fn new(behavior: ThumbnailBehavior) -> Self {
            Self {
                behavior,
                attempts: AtomicUsize::new(0),
            }
        }

        fn unexpected<T>() -> RigSourceResult<T> {
            panic!("unexpected RigSource query in thumbnail retry test")
        }

        fn still_preparing() -> RigSourceError {
            RigSourceError::Rejected {
                kind: RigSourceKind::NinaDirect,
                reason: "The image thumbnail is still being prepared.".to_string(),
            }
        }
    }

    #[async_trait]
    impl RigSource for ThumbnailSource {
        fn kind(&self) -> RigSourceKind {
            RigSourceKind::NinaDirect
        }

        fn capabilities(&self) -> RigCapabilities {
            let mut capabilities = RigCapabilities::none();
            capabilities.thumbnails = true;
            capabilities
        }

        async fn get_event_history(&self) -> RigSourceResult<EventHistoryResponse> {
            Self::unexpected()
        }

        async fn get_all_image_history(&self) -> RigSourceResult<ImageHistoryResponse> {
            Self::unexpected()
        }

        async fn get_sequence(&self) -> RigSourceResult<SequenceResponse> {
            Self::unexpected()
        }

        async fn get_thumbnail(&self, _index: u32) -> RigSourceResult<ThumbnailResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            match self.behavior {
                ThumbnailBehavior::ReadyAfter(pending_attempts) if attempt <= pending_attempts => {
                    Err(Self::still_preparing())
                }
                ThumbnailBehavior::TypedReadyAfter(pending_attempts)
                    if attempt <= pending_attempts =>
                {
                    Err(RigSourceError::NotReady {
                        kind: RigSourceKind::NinaDirect,
                        reason: "The image thumbnail is still being prepared.".to_string(),
                    })
                }
                ThumbnailBehavior::AlwaysPreparing => Err(Self::still_preparing()),
                ThumbnailBehavior::TerminalFailure => Err(RigSourceError::InvalidResponse {
                    kind: RigSourceKind::NinaDirect,
                    reason: "malformed thumbnail payload".to_string(),
                }),
                ThumbnailBehavior::ReadyAfter(_) | ThumbnailBehavior::TypedReadyAfter(_) => {
                    Ok(ThumbnailResponse {
                        data: vec![1, 2, 3, 4],
                        content_type: "image/jpeg".to_string(),
                        status_code: 200,
                    })
                }
            }
        }

        async fn get_last_autofocus(&self) -> RigSourceResult<AutofocusResponse> {
            Self::unexpected()
        }

        async fn get_mount_info(&self) -> RigSourceResult<MountInfoResponse> {
            Self::unexpected()
        }

        async fn get_camera_info(&self) -> RigSourceResult<CameraInfoResponse> {
            Self::unexpected()
        }

        async fn get_filterwheel_info(&self) -> RigSourceResult<FilterWheelInfoResponse> {
            Self::unexpected()
        }

        async fn get_guider_info(&self) -> RigSourceResult<GuiderInfoResponse> {
            Self::unexpected()
        }

        async fn get_guider_graph(&self) -> RigSourceResult<GuiderGraphResponse> {
            Self::unexpected()
        }

        async fn get_rotator_info(&self) -> RigSourceResult<RotatorInfoResponse> {
            Self::unexpected()
        }

        async fn get_focuser_info(&self) -> RigSourceResult<FocuserInfoResponse> {
            Self::unexpected()
        }

        async fn execute_command(&self, _command: RigCommand) -> RigSourceResult<CommandResponse> {
            Self::unexpected()
        }
    }

    #[derive(Default)]
    struct RecordingChatState {
        deliveries: Mutex<Vec<Vec<ChatAttachment>>>,
    }

    struct RecordingChatService {
        state: Arc<RecordingChatState>,
    }

    impl RecordingChatService {
        fn record(&self, attachments: Vec<ChatAttachment>) {
            self.state.deliveries.lock().unwrap().push(attachments);
        }
    }

    #[async_trait]
    impl ChatService for RecordingChatService {
        async fn send_message(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
        ) -> Result<(), ChatError> {
            self.record(Vec::new());
            Ok(())
        }

        async fn send_message_with_image(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
            image_data: &[u8],
            filename: &str,
        ) -> Result<(), ChatError> {
            self.record(vec![ChatAttachment {
                data: image_data.to_vec(),
                filename: filename.to_string(),
            }]);
            Ok(())
        }

        async fn send_message_with_attachments(
            &self,
            _message: &ChatMessage,
            _target: &ChatTarget,
            attachments: &[ChatAttachment],
        ) -> Result<(), ChatError> {
            self.record(attachments.to_vec());
            Ok(())
        }

        fn service_name(&self) -> &'static str {
            "recording"
        }

        fn can_route(&self, _target: &ChatTarget) -> bool {
            true
        }
    }

    fn manager_with_recording_service() -> (ChatServiceManager, Arc<RecordingChatState>) {
        let state = Arc::new(RecordingChatState::default());
        let mut manager = ChatServiceManager::new();
        manager.add_service(Box::new(RecordingChatService {
            state: state.clone(),
        }));
        (manager, state)
    }

    #[tokio::test(start_paused = true)]
    async fn thumbnail_readiness_retries_attach_eventual_thumbnail() {
        let source = Arc::new(ThumbnailSource::new(ThumbnailBehavior::ReadyAfter(2)));
        let shared_source: SharedRigSource = source.clone();
        let (manager, state) = manager_with_recording_service();
        let started = Instant::now();

        manager
            .send_message_with_image(
                &ChatMessage::new("Image ready"),
                &ChatTarget::default(),
                &shared_source,
                7,
                vec![ChatAttachment {
                    data: vec![9, 8],
                    filename: "guiding.png".to_string(),
                }],
            )
            .await;

        assert_eq!(source.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(started.elapsed(), THUMBNAIL_READY_RETRY_DELAY * 2);
        let deliveries = state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].len(), 2);
        assert_eq!(deliveries[0][0].filename, "thumbnail_7.jpg");
        assert_eq!(deliveries[0][0].data, vec![1, 2, 3, 4]);
        assert_eq!(deliveries[0][1].filename, "guiding.png");
    }

    #[tokio::test(start_paused = true)]
    async fn typed_thumbnail_readiness_retries_attach_eventual_thumbnail() {
        let source = Arc::new(ThumbnailSource::new(ThumbnailBehavior::TypedReadyAfter(2)));
        let shared_source: SharedRigSource = source.clone();
        let (manager, state) = manager_with_recording_service();
        let started = Instant::now();

        manager
            .send_message_with_image(
                &ChatMessage::new("Image ready"),
                &ChatTarget::default(),
                &shared_source,
                9,
                Vec::new(),
            )
            .await;

        assert_eq!(source.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(started.elapsed(), THUMBNAIL_READY_RETRY_DELAY * 2);
        let deliveries = state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].len(), 1);
        assert_eq!(deliveries[0][0].filename, "thumbnail_9.jpg");
    }

    #[tokio::test(start_paused = true)]
    async fn thumbnail_readiness_exhaustion_is_bounded_and_degrades() {
        let source = Arc::new(ThumbnailSource::new(ThumbnailBehavior::AlwaysPreparing));
        let shared_source: SharedRigSource = source.clone();
        let (manager, state) = manager_with_recording_service();
        let started = Instant::now();

        manager
            .send_message_with_image(
                &ChatMessage::new("Image without thumbnail"),
                &ChatTarget::default(),
                &shared_source,
                11,
                vec![ChatAttachment {
                    data: vec![6, 5],
                    filename: "guiding.png".to_string(),
                }],
            )
            .await;

        assert_eq!(
            source.attempts.load(Ordering::SeqCst),
            THUMBNAIL_READY_MAX_ATTEMPTS
        );
        assert_eq!(
            started.elapsed(),
            THUMBNAIL_READY_RETRY_DELAY * (THUMBNAIL_READY_MAX_ATTEMPTS as u32 - 1)
        );
        let deliveries = state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].len(), 1);
        assert_eq!(deliveries[0][0].filename, "guiding.png");
        assert_eq!(deliveries[0][0].data, vec![6, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_thumbnail_failure_degrades_without_retrying() {
        let source = Arc::new(ThumbnailSource::new(ThumbnailBehavior::TerminalFailure));
        let shared_source: SharedRigSource = source.clone();
        let (manager, state) = manager_with_recording_service();
        let started = Instant::now();

        manager
            .send_message_with_image(
                &ChatMessage::new("Image without attachment"),
                &ChatTarget::default(),
                &shared_source,
                3,
                Vec::new(),
            )
            .await;

        assert_eq!(source.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
        let deliveries = state.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].is_empty());
    }
}
