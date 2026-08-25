//! Discord OAuth2 and REST calls used by the hub's login flow.
//!
//! Plain reqwest against the Discord HTTP API — no OAuth crate. Access
//! tokens are used within the login request and then dropped; only the
//! resulting identity is persisted.

use serde::Deserialize;
use std::time::Duration;

/// Scopes requested at login. `email` is the reason the hub can notify
/// operators outside Discord; `guilds` powers server-level management.
pub const OAUTH_SCOPES: &str = "identify email guilds";

#[derive(Debug, thiserror::Error)]
pub enum DiscordApiError {
    #[error("Discord request failed: {0}")]
    Http(#[from] crate::security::SafeHttpError),
    #[error("Discord returned status {0}")]
    Status(reqwest::StatusCode),
    #[error("Discord returned an invalid snowflake '{0}'")]
    BadSnowflake(String),
}

impl From<reqwest::Error> for DiscordApiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.into())
    }
}

/// Client for the OAuth dance. `base_url` is `https://discord.com` in
/// production and a local stub in tests.
pub struct DiscordOauthClient {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field(
                "access_token",
                &crate::security::secret_marker(&self.access_token),
            )
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub verified: bool,
}

// NOTE: every Discord API path here is pinned to /api/v10. An unversioned
// /api/... path is served as the OLDEST version Discord still honours, where
// `permissions` on a guild is an integer rather than the decimal string below --
// so the response decodes cleanly right up until serde reaches that field, and
// the login fails with "error decoding response body" rather than anything that
// names a version.
#[derive(Debug, Deserialize)]
pub struct DiscordGuild {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub owner: bool,
    /// Permission bits as a decimal string (snowflake-style, to survive JS).
    #[serde(default)]
    pub permissions: String,
}

/// Parse a Discord snowflake or permission string into the i64 we store
/// (u64 bit-cast so the full range fits SQLite's INTEGER).
pub fn parse_snowflake(value: &str) -> Result<i64, DiscordApiError> {
    value
        .parse::<u64>()
        .map(|v| v as i64)
        .map_err(|_| DiscordApiError::BadSnowflake(value.to_string()))
}

/// Render a stored snowflake back to its decimal string form for JSON.
pub fn snowflake_string(value: i64) -> String {
    (value as u64).to_string()
}

impl DiscordUser {
    /// Display name, preferring the newer global name.
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.avatar
            .as_ref()
            .map(|hash| format!("https://cdn.discordapp.com/avatars/{}/{hash}.png", self.id))
    }
}

impl DiscordOauthClient {
    pub fn new(
        base_url: &str,
        client_id: &str,
        client_secret: &str,
        public_base_url: &str,
    ) -> Result<Self, DiscordApiError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            redirect_uri: format!("{}/oauth/callback", public_base_url.trim_end_matches('/')),
        })
    }

    /// The Discord authorize URL the browser is sent to.
    pub fn authorize_url(&self, state: &str, pkce_challenge: &str) -> String {
        let mut url = url::Url::parse(&format!("{}/oauth2/authorize", self.base_url))
            .expect("base_url validated at construction");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", OAUTH_SCOPES)
            .append_pair("state", state)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("prompt", "none");
        url.to_string()
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
    ) -> Result<TokenResponse, DiscordApiError> {
        let response = self
            .http
            .post(format!("{}/api/v10/oauth2/token", self.base_url))
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("code_verifier", pkce_verifier),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(DiscordApiError::Status(response.status()));
        }
        Ok(response.json().await?)
    }

    pub async fn fetch_user(&self, access_token: &str) -> Result<DiscordUser, DiscordApiError> {
        self.get_json(access_token, "/api/v10/users/@me").await
    }

    pub async fn fetch_guilds(
        &self,
        access_token: &str,
    ) -> Result<Vec<DiscordGuild>, DiscordApiError> {
        self.get_json(access_token, "/api/v10/users/@me/guilds")
            .await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        access_token: &str,
        path: &str,
    ) -> Result<T, DiscordApiError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(access_token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(DiscordApiError::Status(response.status()));
        }
        Ok(response.json().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_token_debug_is_redacted_without_affecting_deserialization() {
        let response: TokenResponse =
            serde_json::from_str(r#"{"access_token":"discord-oauth-private"}"#).unwrap();
        assert_eq!(response.access_token, "discord-oauth-private");
        let debug = format!("{response:?}");
        assert!(!debug.contains("discord-oauth-private"));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn user_parses_with_email() {
        let json = r#"{
            "id": "80351110224678912",
            "username": "nelly",
            "global_name": "Nelly",
            "avatar": "8342729096ea3675442027381ff50dfe",
            "email": "nelly@example.com",
            "verified": true
        }"#;
        let user: DiscordUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.display_name(), "Nelly");
        assert_eq!(user.email.as_deref(), Some("nelly@example.com"));
        assert!(user.verified);
        assert!(user.avatar_url().unwrap().contains("80351110224678912"));
    }

    #[test]
    fn user_parses_without_email_fields() {
        // A user who denies the email scope still logs in.
        let json = r#"{"id": "1", "username": "nelly", "global_name": null, "avatar": null}"#;
        let user: DiscordUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.email, None);
        assert!(!user.verified);
        assert_eq!(user.display_name(), "nelly");
        assert_eq!(user.avatar_url(), None);
    }

    #[test]
    fn guild_parses_permission_string() {
        let json = r#"[{"id": "197038439483310086", "name": "Test", "owner": false,
                        "permissions": "2147483647"}]"#;
        let guilds: Vec<DiscordGuild> = serde_json::from_str(json).unwrap();
        assert_eq!(parse_snowflake(&guilds[0].permissions).unwrap(), 2147483647);
    }

    #[test]
    fn snowflake_roundtrips_above_i64_max() {
        // Snowflakes are u64; values above i64::MAX must survive the bit-cast.
        let big = u64::MAX.to_string();
        let stored = parse_snowflake(&big).unwrap();
        assert_eq!(snowflake_string(stored), big);
        assert!(parse_snowflake("not-a-number").is_err());
    }

    #[test]
    fn authorize_url_contains_pkce_and_scopes() {
        let client = DiscordOauthClient::new(
            "https://discord.com",
            "123",
            "secret",
            "https://hub.example.com",
        )
        .unwrap();
        let url = client.authorize_url("nonce-1", "challenge-1");
        assert!(url.starts_with("https://discord.com/oauth2/authorize?"));
        assert!(url.contains("client_id=123"));
        assert!(url.contains("scope=identify+email+guilds"));
        assert!(url.contains("state=nonce-1"));
        assert!(url.contains("code_challenge=challenge-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fhub.example.com%2Foauth%2Fcallback"));
    }
}
