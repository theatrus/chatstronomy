use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

/// Discord rate-limits webhooks at roughly five requests per two seconds and
/// answers a breach with 429 plus `Retry-After`. Treating that as a permanent
/// failure silently drops the message, so honour the header and retry a
/// bounded number of times. Transient 5xx responses get the same treatment.
const MAX_SEND_ATTEMPTS: u32 = 4;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Never park a send task for longer than this, however large `Retry-After` is.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How long to wait before retrying, or `None` when the response is final.
fn retry_delay(response: &reqwest::Response) -> Option<Duration> {
    retry_delay_for(
        response.status(),
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
    )
}

fn retry_delay_for(status: reqwest::StatusCode, retry_after: Option<&str>) -> Option<Duration> {
    if status.as_u16() == 429 {
        let header = retry_after
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(|seconds| Duration::from_secs_f64(seconds.min(MAX_RETRY_DELAY.as_secs_f64())))
            .unwrap_or(DEFAULT_RETRY_DELAY);
        return Some(header);
    }
    if status.is_server_error() {
        return Some(DEFAULT_RETRY_DELAY);
    }
    None
}

/// Run a webhook request, retrying rate-limited and transient failures.
///
/// The request is rebuilt per attempt because a multipart body cannot be
/// cloned. The final failure is returned so the caller still sees the status.
async fn send_with_retry<F, Fut>(mut build: F) -> Result<(), DiscordError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt = 1;
    loop {
        let response = build().await?;
        if response.status().is_success() {
            return Ok(());
        }

        let delay = retry_delay(&response).filter(|_| attempt < MAX_SEND_ATTEMPTS);
        let Some(delay) = delay else {
            let status = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(DiscordError::Http { status, message });
        };

        eprintln!(
            "Discord returned {}; retrying in {:.1}s (attempt {attempt}/{MAX_SEND_ATTEMPTS})",
            response.status().as_u16(),
            delay.as_secs_f64()
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

#[derive(Clone)]
pub struct DiscordWebhook {
    client: Client,
    webhook_url: String,
}

impl std::fmt::Debug for DiscordWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordWebhook")
            .field("client", &self.client)
            .field(
                "webhook_url",
                &crate::security::redact_sensitive(&self.webhook_url),
            )
            .finish()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WebhookMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeds: Option<Vec<Embed>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Embed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<EmbedFooter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EmbedImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<EmbedThumbnail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<EmbedVideo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EmbedProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<EmbedAuthor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<EmbedField>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedFooter {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_icon_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedImage {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedThumbnail {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedVideo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedProvider {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedAuthor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_icon_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AllowedMentions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replied_user: Option<bool>,
}

pub enum DiscordError {
    Network(crate::security::SafeHttpError),
    Parse(serde_json::Error),
    Http { status: u16, message: String },
    InvalidWebhookUrl,
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscordError::Network(e) => write!(f, "Network error: {e}"),
            DiscordError::Parse(e) => write!(f, "Parse error: {e}"),
            DiscordError::Http { status, message } => {
                write!(
                    f,
                    "HTTP error {status}: {}",
                    crate::security::redact_sensitive(message)
                )
            }
            DiscordError::InvalidWebhookUrl => write!(f, "Invalid webhook URL"),
        }
    }
}

impl std::fmt::Debug for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DiscordError")
            .field(&self.to_string())
            .finish()
    }
}

impl std::error::Error for DiscordError {}

impl From<reqwest::Error> for DiscordError {
    fn from(err: reqwest::Error) -> Self {
        DiscordError::Network(err.into())
    }
}

impl From<serde_json::Error> for DiscordError {
    fn from(err: serde_json::Error) -> Self {
        DiscordError::Parse(err)
    }
}

impl DiscordWebhook {
    pub fn new(webhook_url: String) -> Result<Self, DiscordError> {
        // Basic validation of webhook URL
        if !webhook_url.starts_with("https://discord.com/api/webhooks/")
            && !webhook_url.starts_with("https://discordapp.com/api/webhooks/")
        {
            return Err(DiscordError::InvalidWebhookUrl);
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            webhook_url,
        })
    }

    pub async fn execute(&self, message: &WebhookMessage) -> Result<(), DiscordError> {
        self.execute_with_params(message, None).await
    }

    pub async fn execute_with_params(
        &self,
        message: &WebhookMessage,
        params: Option<HashMap<&str, &str>>,
    ) -> Result<(), DiscordError> {
        let mut url = self.webhook_url.clone();

        // Add query parameters if provided
        if let Some(params) = params {
            let query_string = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            url = format!("{url}?{query_string}");
        }

        send_with_retry(|| {
            self.client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(message)
                .send()
        })
        .await
    }

    pub async fn execute_with_embed(
        &self,
        content: Option<&str>,
        embed: Embed,
    ) -> Result<(), DiscordError> {
        let message = WebhookMessage {
            content: content.map(|s| s.to_string()),
            username: None,
            avatar_url: None,
            tts: None,
            embeds: Some(vec![embed]),
            allowed_mentions: None,
            components: None,
            files: None,
            payload_json: None,
            attachments: None,
            flags: None,
        };

        self.execute(&message).await
    }

    pub async fn execute_with_file(
        &self,
        content: Option<&str>,
        embed: Option<Embed>,
        file_data: &[u8],
        filename: &str,
    ) -> Result<(), DiscordError> {
        self.execute_with_files(content, embed, &[(file_data, filename)])
            .await
    }

    pub async fn execute_with_files(
        &self,
        content: Option<&str>,
        embed: Option<Embed>,
        files: &[(&[u8], &str)],
    ) -> Result<(), DiscordError> {
        // Create the payload
        let message = WebhookMessage {
            content: content.map(|s| s.to_string()),
            username: None,
            avatar_url: None,
            tts: None,
            embeds: embed.map(|e| vec![e]),
            allowed_mentions: None,
            components: None,
            files: None,
            payload_json: None,
            attachments: None,
            flags: None,
        };

        let payload_json = serde_json::to_string(&message)?;

        // A multipart body cannot be cloned, so rebuild the whole form on each
        // attempt rather than sharing one across retries.
        send_with_retry(|| {
            let mut form = reqwest::multipart::Form::new();
            // Discord's webhook API expects multipart part names files[0],
            // files[1], ... for attachments
            for (i, (file_data, filename)) in files.iter().enumerate() {
                let file_part = reqwest::multipart::Part::bytes(file_data.to_vec())
                    .file_name(filename.to_string());
                form = form.part(format!("files[{i}]"), file_part);
            }
            form = form.text("payload_json", payload_json.clone());

            self.client.post(&self.webhook_url).multipart(form).send()
        })
        .await
    }
}

impl Default for Embed {
    fn default() -> Self {
        Self::new()
    }
}

// Helper functions for creating embeds
impl Embed {
    pub fn new() -> Self {
        Self {
            title: None,
            description: None,
            url: None,
            timestamp: None,
            color: None,
            footer: None,
            image: None,
            thumbnail: None,
            video: None,
            provider: None,
            author: None,
            fields: None,
        }
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    pub fn description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    pub fn color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn field(mut self, name: &str, value: &str, inline: bool) -> Self {
        let field = EmbedField {
            name: name.to_string(),
            value: value.to_string(),
            inline: Some(inline),
        };

        match &mut self.fields {
            Some(fields) => fields.push(field),
            None => self.fields = Some(vec![field]),
        }

        self
    }

    pub fn footer(mut self, text: &str, icon_url: Option<&str>) -> Self {
        self.footer = Some(EmbedFooter {
            text: text.to_string(),
            icon_url: icon_url.map(|s| s.to_string()),
            proxy_icon_url: None,
        });
        self
    }

    pub fn author(mut self, name: &str, url: Option<&str>, icon_url: Option<&str>) -> Self {
        self.author = Some(EmbedAuthor {
            name: name.to_string(),
            url: url.map(|s| s.to_string()),
            icon_url: icon_url.map(|s| s.to_string()),
            proxy_icon_url: None,
        });
        self
    }

    pub fn timestamp(mut self, timestamp: &str) -> Self {
        self.timestamp = Some(timestamp.to_string());
        self
    }

    pub fn image(mut self, url: &str) -> Self {
        self.image = Some(EmbedImage {
            url: url.to_string(),
            proxy_url: None,
            height: None,
            width: None,
        });
        self
    }

    pub fn thumbnail(mut self, url: &str) -> Self {
        self.thumbnail = Some(EmbedThumbnail {
            url: url.to_string(),
            proxy_url: None,
            height: None,
            width: None,
        });
        self
    }
}

// Color constants for common embed colors
pub mod colors {
    pub const RED: u32 = 0xFF0000;
    pub const GREEN: u32 = 0x00FF00;
    pub const BLUE: u32 = 0x0000FF;
    pub const YELLOW: u32 = 0xFFFF00;
    pub const PURPLE: u32 = 0x800080;
    pub const ORANGE: u32 = 0xFFA500;
    pub const CYAN: u32 = 0x00FFFF;
    pub const GRAY: u32 = 0x808080;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_retry_delay_honors_rate_limits_and_transient_errors() {
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("0.25")),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("120")),
            Some(MAX_RETRY_DELAY)
        );
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("1e300")),
            Some(MAX_RETRY_DELAY)
        );
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("invalid")),
            Some(DEFAULT_RETRY_DELAY)
        );
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::BAD_GATEWAY, None),
            Some(DEFAULT_RETRY_DELAY)
        );
        assert_eq!(
            retry_delay_for(reqwest::StatusCode::BAD_REQUEST, None),
            None
        );
    }
}
