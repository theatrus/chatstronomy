use super::{ChatAttachment, ChatMessage, ChatService, ChatTarget};
use crate::discord::{DiscordWebhook, Embed, colors};
use crate::error::ChatError;
use async_trait::async_trait;

/// Discord chat service. Holds an optional default webhook URL; per-telescope
/// `ChatTarget::discord_webhook_url` overrides it. A new `DiscordWebhook` is
/// constructed per send so each telescope can route to a different channel.
pub struct DiscordChatService {
    default_webhook_url: Option<String>,
}

impl DiscordChatService {
    pub fn new(default_webhook_url: Option<String>) -> Self {
        Self {
            default_webhook_url,
        }
    }

    fn resolve_url<'a>(&'a self, target: &'a ChatTarget) -> Option<&'a str> {
        target
            .discord_webhook_url
            .as_deref()
            .or(self.default_webhook_url.as_deref())
    }

    fn build_webhook(&self, target: &ChatTarget) -> Result<DiscordWebhook, ChatError> {
        let url = self.resolve_url(target).ok_or_else(|| ChatError::Discord {
            message: "No Discord webhook URL available (no default and no telescope override)"
                .to_string(),
        })?;
        DiscordWebhook::new(url.to_string()).map_err(|e| ChatError::Discord {
            message: e.to_string(),
        })
    }

    fn build_embed(message: &ChatMessage) -> Embed {
        let mut embed = Embed::new().title(&message.title);
        embed = embed.color(message.color.unwrap_or(colors::GRAY));
        if let Some(timestamp) = &message.timestamp {
            embed = embed.timestamp(timestamp);
        }
        for field in &message.fields {
            embed = embed.field(
                &field.name,
                field.discord_value.as_deref().unwrap_or(&field.value),
                field.inline,
            );
        }
        if let Some(footer_text) = &message.footer {
            embed = embed.footer(footer_text, None);
        }
        embed
    }

    fn with_first_image<'a>(
        mut embed: Embed,
        filenames: impl IntoIterator<Item = &'a str>,
    ) -> Embed {
        if let Some(filename) = filenames.into_iter().find(|filename| {
            matches!(
                filename
                    .rsplit('.')
                    .next()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("jpg" | "jpeg" | "png" | "gif" | "webp")
            )
        }) {
            embed = embed.image(&format!("attachment://{filename}"));
        }
        embed
    }
}

#[async_trait]
impl ChatService for DiscordChatService {
    async fn send_message(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
    ) -> Result<(), ChatError> {
        let webhook = self.build_webhook(target)?;
        let embed = Self::build_embed(message);
        webhook
            .execute_with_embed(None, embed)
            .await
            .map_err(|e| ChatError::Discord {
                message: e.to_string(),
            })?;
        Ok(())
    }

    async fn send_message_with_image(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
        image_data: &[u8],
        filename: &str,
    ) -> Result<(), ChatError> {
        let webhook = self.build_webhook(target)?;
        // Referencing the uploaded image from the embed makes Discord render
        // the captured-frame preview at full embed width instead of as a small
        // generic attachment tile.
        let embed = Self::with_first_image(Self::build_embed(message), [filename]);
        webhook
            .execute_with_file(None, Some(embed), image_data, filename)
            .await
            .map_err(|e| ChatError::Discord {
                message: e.to_string(),
            })?;
        Ok(())
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
        let webhook = self.build_webhook(target)?;
        // Referencing the first uploaded image from the embed gives captured
        // frames a full-width Discord preview. Remaining graph attachments are
        // still uploaded and shown normally.
        let embed = Self::with_first_image(
            Self::build_embed(message),
            attachments
                .iter()
                .map(|attachment| attachment.filename.as_str()),
        );
        let files: Vec<(&[u8], &str)> = attachments
            .iter()
            .map(|a| (a.data.as_slice(), a.filename.as_str()))
            .collect();
        webhook
            .execute_with_files(None, Some(embed), &files)
            .await
            .map_err(|e| ChatError::Discord {
                message: e.to_string(),
            })?;
        Ok(())
    }

    fn service_name(&self) -> &'static str {
        "Discord"
    }

    fn can_route(&self, target: &ChatTarget) -> bool {
        // Defer to the Discord bot when any channel is configured for this
        // telescope — bot path takes precedence over the webhook.
        if !target.all_discord_channels().is_empty() {
            return false;
        }
        self.resolve_url(target).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_attachment_is_used_as_the_embed_preview() {
        let message = ChatMessage::new("Captured image");
        let embed = DiscordChatService::with_first_image(
            DiscordChatService::build_embed(&message),
            ["thumbnail_42.jpg", "guide.png"],
        );

        assert_eq!(
            embed.image.as_ref().map(|image| image.url.as_str()),
            Some("attachment://thumbnail_42.jpg")
        );
    }

    #[test]
    fn non_image_attachment_is_not_embedded() {
        let message = ChatMessage::new("Diagnostics");
        let embed = DiscordChatService::with_first_image(
            DiscordChatService::build_embed(&message),
            ["diagnostics.txt"],
        );

        assert!(embed.image.is_none());
    }

    #[test]
    fn embed_prefers_discord_field_value_and_falls_back_to_portable_value() {
        let occurred_at = chrono::DateTime::parse_from_rfc3339("2026-08-17T04:00:00-07:00")
            .expect("valid occurrence timestamp")
            .with_timezone(&chrono::Utc);
        let message = ChatMessage::new("Timed wait")
            .field_with_discord_value(
                "Until",
                "2026-08-17 11:00:00 UTC",
                "<t:1786964400:F>",
                false,
            )
            .field("Status", "Waiting", true)
            .occurred_at("Started", occurred_at);

        let embed = DiscordChatService::build_embed(&message);
        let fields = embed.fields.expect("embed fields");

        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "Until");
        assert_eq!(fields[0].value, "<t:1786964400:F>");
        assert_eq!(fields[1].name, "Status");
        assert_eq!(fields[1].value, "Waiting");
        assert_eq!(
            embed.timestamp.as_deref(),
            Some("2026-08-17T11:00:00+00:00")
        );
    }
}
