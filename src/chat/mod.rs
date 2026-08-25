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
use crate::source::SharedRigSource;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Represents a field in a chat message
#[derive(Debug, Clone)]
pub struct ChatField {
    pub name: String,
    pub value: String,
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
}

impl ChatMessage {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            color: None,
            fields: Vec::new(),
            footer: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
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
            inline,
        });
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
        match source.get_thumbnail(image_index).await {
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
