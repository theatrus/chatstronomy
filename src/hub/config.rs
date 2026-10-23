//! Hub configuration.
//!
//! The hub has its own configuration file (default `hub.json`), separate from
//! the rig configuration, because a hub has no `telescopes` list — telescopes
//! live in the database and are managed through the web app.

use crate::config::ConfigError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

/// Minimum length for the session cookie signing key. Anything shorter is too
/// easy to brute-force offline from a captured cookie.
pub const MIN_SIGNING_KEY_LEN: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubConfig {
    /// Address the web server listens on. TLS comes from a reverse proxy.
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    /// Path of the SQLite database file. Created on first run.
    #[serde(default = "default_database_path")]
    pub database_path: String,
    /// Public HTTPS origin of this hub (e.g. "https://hub.example.com").
    /// Used to build OAuth redirect URLs. Required once Discord login is
    /// configured.
    #[serde(default)]
    pub public_base_url: String,
    /// Trust the last X-Forwarded-For hop as the client IP. Enable only
    /// when the hub sits behind a reverse proxy that overwrites or appends
    /// that header; otherwise the client controls it and rate limits key on
    /// attacker-chosen values.
    #[serde(default)]
    pub trust_x_forwarded_for: bool,
    /// Operator storage/relay ceiling, not a promise of Discord acceptance.
    #[serde(default = "default_max_timelapse_bytes")]
    pub max_timelapse_bytes: u64,
    #[serde(default)]
    pub discord: HubDiscordConfig,
    #[serde(default)]
    pub session: HubSessionConfig,
}

/// Credentials of the central Discord application. All empty until the
/// operator registers the app with Discord.
#[derive(Clone, Serialize, Deserialize)]
pub struct HubDiscordConfig {
    /// Base URL for Discord's site and API. Only tests change this.
    #[serde(default = "default_discord_base_url")]
    pub base_url: String,
    /// OAuth2 client ID of the Discord application.
    #[serde(default)]
    pub client_id: String,
    /// OAuth2 client secret. Redacted from Debug output.
    #[serde(default)]
    pub client_secret: String,
    /// Bot token of the Discord application. Redacted from Debug output.
    #[serde(default)]
    pub bot_token: String,
}

impl Default for HubDiscordConfig {
    fn default() -> Self {
        Self {
            base_url: default_discord_base_url(),
            client_id: String::new(),
            client_secret: String::new(),
            bot_token: String::new(),
        }
    }
}

// Hand-written so secrets never land in logs or panic messages.
impl std::fmt::Debug for HubDiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubDiscordConfig")
            .field("base_url", &self.base_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &redact(&self.client_secret))
            .field("bot_token", &redact(&self.bot_token))
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HubSessionConfig {
    /// HMAC key for signing session cookies. Redacted from Debug output.
    /// Generate with e.g. `openssl rand -hex 32`.
    #[serde(default)]
    pub signing_key: String,
    /// Session lifetime in hours. Default 720 (30 days).
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
}

impl Default for HubSessionConfig {
    fn default() -> Self {
        Self {
            signing_key: String::new(),
            session_hours: default_session_hours(),
        }
    }
}

impl std::fmt::Debug for HubSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubSessionConfig")
            .field("signing_key", &redact(&self.signing_key))
            .field("session_hours", &self.session_hours)
            .finish()
    }
}

fn redact(value: &str) -> &'static str {
    if value.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

fn default_discord_base_url() -> String {
    "https://discord.com".to_string()
}

fn default_bind_address() -> String {
    "127.0.0.1:8092".to_string()
}

fn default_database_path() -> String {
    "chatstronomy-hub.db".to_string()
}

fn default_session_hours() -> u64 {
    720
}

pub const MAX_TIMELAPSE_BYTES: u64 = 512 * 1024 * 1024;
pub const fn default_max_timelapse_bytes() -> u64 {
    128 * 1024 * 1024
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            bind_address: default_bind_address(),
            database_path: default_database_path(),
            public_base_url: String::new(),
            trust_x_forwarded_for: false,
            max_timelapse_bytes: default_max_timelapse_bytes(),
            discord: HubDiscordConfig::default(),
            session: HubSessionConfig::default(),
        }
    }
}

impl HubConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        crate::config::load_json_file(path)
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    /// True once the Discord application credentials needed for OAuth login
    /// are all present.
    pub fn oauth_configured(&self) -> bool {
        !self.discord.client_id.is_empty()
            && !self.discord.client_secret.is_empty()
            && !self.public_base_url.is_empty()
            && !self.session.signing_key.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_TIMELAPSE_BYTES).contains(&self.max_timelapse_bytes) {
            return Err(format!(
                "max_timelapse_bytes must be between 1 and {MAX_TIMELAPSE_BYTES}"
            ));
        }
        self.bind_address
            .parse::<SocketAddr>()
            .map_err(|e| format!("Invalid bind_address '{}': {e}", self.bind_address))?;

        if self.database_path.is_empty() {
            return Err("database_path cannot be empty".to_string());
        }

        if !self.public_base_url.is_empty()
            && !self.public_base_url.starts_with("https://")
            && !self.public_base_url.starts_with("http://")
        {
            return Err("public_base_url must start with http:// or https://".to_string());
        }

        if self.session.session_hours == 0 {
            return Err("session.session_hours must be greater than 0".to_string());
        }

        if !self.session.signing_key.is_empty()
            && self.session.signing_key.len() < MIN_SIGNING_KEY_LEN
        {
            return Err(format!(
                "session.signing_key must be at least {MIN_SIGNING_KEY_LEN} characters"
            ));
        }

        // Discord credentials are all-or-nothing: a partial set means a typo,
        // not an intentionally disabled login.
        let any_oauth =
            !self.discord.client_id.is_empty() || !self.discord.client_secret.is_empty();
        if any_oauth && !self.oauth_configured() {
            return Err(
                "Discord login needs discord.client_id, discord.client_secret, \
                 public_base_url, and session.signing_key all set"
                    .to_string(),
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(HubConfig::default().validate().is_ok());
    }

    #[test]
    fn empty_json_gets_defaults() {
        let config: HubConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.bind_address, "127.0.0.1:8092");
        assert_eq!(config.database_path, "chatstronomy-hub.db");
        assert_eq!(config.session.session_hours, 720);
        assert_eq!(config.max_timelapse_bytes, 128 * 1024 * 1024);
        assert!(!config.oauth_configured());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn bad_bind_address_rejected() {
        let config = HubConfig {
            bind_address: "not-an-address".to_string(),
            ..Default::default()
        };
        assert!(config.validate().unwrap_err().contains("bind_address"));
    }

    #[test]
    fn timelapse_limit_is_operator_configurable_but_bounded() {
        for value in [0, MAX_TIMELAPSE_BYTES + 1, u64::MAX] {
            let config = HubConfig {
                max_timelapse_bytes: value,
                ..Default::default()
            };
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .contains("max_timelapse_bytes")
            );
        }
        let config = HubConfig {
            max_timelapse_bytes: MAX_TIMELAPSE_BYTES,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn partial_discord_credentials_rejected() {
        let mut config = HubConfig::default();
        config.discord.client_id = "12345".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.contains("client_secret"));
    }

    #[test]
    fn short_signing_key_rejected() {
        let mut config = HubConfig::default();
        config.session.signing_key = "short".to_string();
        assert!(config.validate().unwrap_err().contains("signing_key"));
    }

    #[test]
    fn full_oauth_config_accepted() {
        let mut config = HubConfig::default();
        config.discord.client_id = "12345".to_string();
        config.discord.client_secret = "secret".to_string();
        config.public_base_url = "https://hub.example.com".to_string();
        config.session.signing_key = "0123456789abcdef0123456789abcdef".to_string();
        assert!(config.validate().is_ok());
        assert!(config.oauth_configured());
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut config = HubConfig::default();
        config.discord.client_secret = "hunter2".to_string();
        config.discord.bot_token = "token-value".to_string();
        config.session.signing_key = "0123456789abcdef0123456789abcdef".to_string();
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("token-value"));
        assert!(!debug.contains("0123456789abcdef"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn roundtrips_through_json() {
        let config = HubConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: HubConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bind_address, config.bind_address);
        assert_eq!(back.database_path, config.database_path);
    }
}
