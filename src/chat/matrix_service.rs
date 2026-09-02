use super::{ChatAttachment, ChatMessage, ChatService, ChatTarget};
use crate::error::ChatError;
use async_trait::async_trait;
use matrix_sdk::{
    Client, EncryptionState, Room,
    config::SyncSettings,
    ruma::{OwnedRoomId, events::room::message::RoomMessageEventContent},
};
use url::Url;

/// Matrix chat service. Holds one logged-in `Client` shared across every
/// telescope; per-telescope `ChatTarget::matrix_room_id` selects which room
/// each post lands in, falling back to `default_room_id`.
pub struct MatrixChatService {
    client: Client,
    default_room_id: Option<OwnedRoomId>,
}

impl MatrixChatService {
    pub async fn new(
        homeserver_url: &str,
        username: &str,
        password: &str,
        default_room_id: Option<&str>,
    ) -> Result<Self, ChatError> {
        let homeserver_url = Url::parse(homeserver_url).map_err(|e| ChatError::Initialization {
            service_name: "Matrix".to_string(),
            reason: format!("Invalid homeserver URL: {}", e),
        })?;
        let client = Client::new(homeserver_url)
            .await
            .map_err(|e| ChatError::Initialization {
                service_name: "Matrix".to_string(),
                reason: format!(
                    "Failed to create Matrix client: {}",
                    crate::security::redact_sensitive(&e.to_string())
                ),
            })?;

        client
            .matrix_auth()
            .login_username(username, password)
            .initial_device_display_name("Chatstronomy")
            .await?;
        println!("Successfully logged into Matrix as {}", username);

        println!("Syncing with Matrix server...");
        client.sync_once(SyncSettings::default()).await?;

        let invited_rooms = client.invited_rooms();
        if !invited_rooms.is_empty() {
            println!("Found {} room invitation(s):", invited_rooms.len());
            for room in &invited_rooms {
                let room_name = room.name().unwrap_or_else(|| room.room_id().to_string());
                println!("  - Joining room: {} ({})", room_name, room.room_id());
                match room.join().await {
                    Ok(_) => println!("    ✅ Successfully joined"),
                    Err(e) => println!(
                        "    ❌ Failed to join: {}",
                        crate::security::redact_sensitive(&e.to_string())
                    ),
                }
            }
            client.sync_once(SyncSettings::default()).await?;
        } else {
            println!("No pending room invitations");
        }

        let joined_rooms = client.joined_rooms();
        println!("Currently joined to {} room(s):", joined_rooms.len());
        for room in &joined_rooms {
            let room_name = room.name().unwrap_or("Unnamed room".to_string());
            let member_count = room.active_members_count();
            let encryption_status = match room.encryption_state() {
                EncryptionState::Encrypted => "🔒",
                _ => "🔓",
            };
            println!(
                "  - {} {} ({}) - {} members",
                encryption_status,
                room_name,
                room.room_id(),
                member_count
            );
        }

        // Start background sync once.
        tokio::spawn({
            let client = client.clone();
            async move {
                if let Err(e) = client.sync(SyncSettings::default()).await {
                    eprintln!(
                        "Matrix sync error: {}",
                        crate::security::redact_sensitive(&e.to_string())
                    );
                }
            }
        });

        let default_room_id = if let Some(id) = default_room_id {
            let owned: OwnedRoomId = id.try_into().map_err(|e| ChatError::Initialization {
                service_name: "Matrix".to_string(),
                reason: format!("Invalid default room ID: {}", e),
            })?;
            if client.get_room(&owned).is_some() {
                println!("✅ Default Matrix room found: {}", owned);
            } else {
                println!(
                    "⚠️  Default Matrix room {} not found in joined rooms",
                    owned
                );
            }
            Some(owned)
        } else {
            None
        };

        Ok(Self {
            client,
            default_room_id,
        })
    }

    fn resolve_room_id(&self, target: &ChatTarget) -> Option<OwnedRoomId> {
        if let Some(s) = &target.matrix_room_id {
            // Per-telescope override
            s.as_str().try_into().ok()
        } else {
            self.default_room_id.clone()
        }
    }

    async fn get_room(&self, target: &ChatTarget) -> Result<Room, ChatError> {
        let id = self
            .resolve_room_id(target)
            .ok_or_else(|| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: "No Matrix room ID available (no default and no telescope override)"
                    .to_string(),
            })?;
        self.client
            .get_room(&id)
            .ok_or_else(|| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: format!("Room {} not found", id),
            })
    }

    fn format_message(message: &ChatMessage) -> String {
        let mut formatted = format!("**{}**\n\n", message.title);
        if let (Some(label), Some(timestamp)) =
            (&message.matrix_timestamp_label, &message.timestamp)
        {
            let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp)
                .map(|value| {
                    value
                        .with_timezone(&chrono::Utc)
                        .format("%Y-%m-%d %H:%M:%S UTC")
                        .to_string()
                })
                .unwrap_or_else(|_| timestamp.clone());
            formatted.push_str(&format!("**{label}**: {timestamp}\n"));
        }
        if !message.fields.is_empty() {
            for field in &message.fields {
                formatted.push_str(&format!("**{}**: {}\n", field.name, field.value));
            }
            formatted.push('\n');
        }
        if let Some(footer) = &message.footer {
            formatted.push_str(&format!("_{}_", footer));
        }
        formatted
    }
}

#[async_trait]
impl ChatService for MatrixChatService {
    async fn send_message(
        &self,
        message: &ChatMessage,
        target: &ChatTarget,
    ) -> Result<(), ChatError> {
        let room = self.get_room(target).await?;
        let content = RoomMessageEventContent::notice_markdown(Self::format_message(message));
        room.send(content)
            .await
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: e.to_string(),
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
        let room = self.get_room(target).await?;

        let content = RoomMessageEventContent::notice_markdown(Self::format_message(message));
        room.send(content)
            .await
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: e.to_string(),
            })?;

        let mime_type = if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
            "image/jpeg"
        } else if filename.ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        let mime = mime_type
            .parse::<mime::Mime>()
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: format!("Invalid MIME type: {}", e),
            })?;
        room.send_attachment(filename, &mime, image_data.to_vec(), Default::default())
            .await
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: e.to_string(),
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
        let room = self.get_room(target).await?;

        let content = RoomMessageEventContent::notice_markdown(Self::format_message(message));
        room.send(content)
            .await
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: e.to_string(),
            })?;

        for attachment in attachments {
            let mime_type = if attachment.filename.ends_with(".png") {
                "image/png"
            } else {
                "image/jpeg"
            };
            let mime = mime_type
                .parse::<mime::Mime>()
                .map_err(|e| ChatError::MessageSend {
                    service_name: "Matrix".to_string(),
                    reason: format!("Invalid MIME type: {}", e),
                })?;
            room.send_attachment(
                &attachment.filename,
                &mime,
                attachment.data.clone(),
                Default::default(),
            )
            .await
            .map_err(|e| ChatError::MessageSend {
                service_name: "Matrix".to_string(),
                reason: e.to_string(),
            })?;
        }
        Ok(())
    }

    fn service_name(&self) -> &'static str {
        "Matrix"
    }

    fn can_route(&self, target: &ChatTarget) -> bool {
        target.matrix_room_id.is_some() || self.default_room_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_uses_portable_values_and_renders_labeled_occurrence_in_utc() {
        let occurred_at = chrono::DateTime::parse_from_rfc3339("2026-08-17T04:00:00-07:00")
            .expect("valid occurrence timestamp")
            .with_timezone(&chrono::Utc);
        let message = ChatMessage::new("Timed wait")
            .occurred_at("Started", occurred_at)
            .field_with_discord_value(
                "Until",
                "2026-08-17 12:00:00 UTC",
                "<t:1786968000:F>",
                false,
            )
            .field("Status", "Waiting", true);

        let formatted = MatrixChatService::format_message(&message);

        assert!(formatted.contains("**Started**: 2026-08-17 11:00:00 UTC"));
        assert!(formatted.contains("**Until**: 2026-08-17 12:00:00 UTC"));
        assert!(formatted.contains("**Status**: Waiting"));
        assert!(!formatted.contains("<t:"));
    }

    #[test]
    fn matrix_omits_unlabeled_message_timestamp() {
        let mut message = ChatMessage::new("Ordinary update").field("Status", "Ready", false);
        message.timestamp = Some("2026-08-17T04:00:00-07:00".to_string());

        let formatted = MatrixChatService::format_message(&message);

        assert!(!formatted.contains("2026-08-17"));
        assert!(!formatted.contains("**Started**:"));
        assert!(formatted.contains("**Status**: Ready"));
    }
}
