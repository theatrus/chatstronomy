//! Secure bootstrap contract used by the N.I.N.A. plugin-owned runtime.
//!
//! Delivery credentials arrive over a current-user-only Windows named pipe.
//! They are converted directly into the existing in-memory [`Config`] and are
//! never accepted through command-line flags or written to a JSON file.

use crate::chat::{
    ChatConfig, DiscordBotConfig, SharedDiscordConfig, SharedMatrixConfig, TelescopeChatOverrides,
};
use crate::config::{Config, TelescopeConfig, is_valid_discord_webhook_url, is_valid_https_url};
use crate::source::RigCapabilities;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const PLUGIN_RUNTIME_PROTOCOL_VERSION: u16 = 1;
pub const MAX_BOOTSTRAP_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
pub struct PluginRuntimeBootstrap {
    pub protocol_version: u16,
    pub profile: PluginRuntimeProfile,
    pub source: PluginRuntimeSource,
    #[serde(default)]
    pub delivery: Option<PluginRuntimeDelivery>,
    #[serde(default)]
    pub matrix: Option<PluginRuntimeMatrix>,
    pub data_directory: String,
    #[serde(default = "default_exit_on_control_disconnect")]
    pub exit_on_control_disconnect: bool,
}

#[derive(Deserialize)]
pub struct PluginRuntimeProfile {
    pub node_id: Uuid,
    pub profile_id: Uuid,
    pub profile_name: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRuntimeSource {
    NinaDirect {
        pipe_name: String,
        capabilities: RigCapabilities,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRuntimeDelivery {
    DiscordWebhook {
        webhook_url: String,
    },
    DiscordBot {
        bot_token: String,
        #[serde(default)]
        application_id: Option<u64>,
        default_channel_id: u64,
    },
}

#[derive(Deserialize)]
pub struct PluginRuntimeMatrix {
    pub homeserver_url: String,
    pub username: String,
    pub password: String,
    pub default_room_id: String,
}

fn default_exit_on_control_disconnect() -> bool {
    true
}

impl PluginRuntimeBootstrap {
    pub fn from_json(json: &str) -> Result<Self, String> {
        if json.len() > MAX_BOOTSTRAP_BYTES {
            return Err(format!(
                "plugin bootstrap exceeds the {MAX_BOOTSTRAP_BYTES}-byte limit"
            ));
        }
        serde_json::from_str(json).map_err(|error| format!("invalid plugin bootstrap: {error}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != PLUGIN_RUNTIME_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported plugin runtime protocol {}; expected {}",
                self.protocol_version, PLUGIN_RUNTIME_PROTOCOL_VERSION
            ));
        }
        if self.profile.node_id.is_nil() || self.profile.profile_id.is_nil() {
            return Err("plugin runtime profile IDs cannot be empty".to_string());
        }
        if self.profile.profile_name.trim().is_empty() {
            return Err("plugin runtime profile name cannot be empty".to_string());
        }
        if self.delivery.is_none() && self.matrix.is_none() {
            return Err("at least one local chat delivery must be configured".to_string());
        }
        if !Path::new(&self.data_directory).is_absolute() {
            return Err("plugin runtime data directory must be an absolute path".to_string());
        }

        match &self.source {
            PluginRuntimeSource::NinaDirect { pipe_name, .. } => validate_pipe_name(pipe_name)?,
        }

        match &self.delivery {
            Some(PluginRuntimeDelivery::DiscordWebhook { webhook_url }) => {
                if !is_valid_discord_webhook_url(webhook_url) {
                    return Err("Discord webhook URL is incomplete or invalid".to_string());
                }
            }
            Some(PluginRuntimeDelivery::DiscordBot {
                bot_token,
                application_id,
                default_channel_id,
            }) => {
                if bot_token.trim().is_empty() {
                    return Err("Discord bot token cannot be empty".to_string());
                }
                if application_id == &Some(0) {
                    return Err("Discord application ID cannot be zero".to_string());
                }
                if *default_channel_id == 0 {
                    return Err("Discord default channel ID cannot be zero".to_string());
                }
            }
            None => {}
        }

        if let Some(matrix) = &self.matrix {
            if !is_valid_https_url(&matrix.homeserver_url) {
                return Err("Matrix homeserver URL must be an absolute https:// URL".to_string());
            }
            if matrix.username.trim().is_empty()
                || matrix.password.is_empty()
                || matrix.default_room_id.trim().is_empty()
            {
                return Err("Matrix username, password, and default room are required".to_string());
            }
        }

        Ok(())
    }

    pub fn poll_interval_seconds(&self) -> u64 {
        5
    }

    pub fn into_config(self) -> Result<Config, String> {
        self.validate()?;

        let mut chat = ChatConfig::default();
        let mut telescope_chat = TelescopeChatOverrides::default();

        match self.delivery {
            Some(PluginRuntimeDelivery::DiscordWebhook { webhook_url }) => {
                chat.discord = Some(SharedDiscordConfig {
                    enabled: true,
                    default_webhook_url: Some(webhook_url),
                });
            }
            Some(PluginRuntimeDelivery::DiscordBot {
                bot_token,
                application_id,
                default_channel_id,
            }) => {
                let state_file = PathBuf::from(&self.data_directory)
                    .join(format!(
                        "chatstronomy-state-{}.json",
                        self.profile.profile_id.simple()
                    ))
                    .to_string_lossy()
                    .into_owned();
                chat.discord_bot = Some(DiscordBotConfig {
                    enabled: true,
                    token: bot_token,
                    application_id,
                    public_key: None,
                    default_channel_id: Some(default_channel_id),
                    live_status: false,
                    state_file,
                    write_acl: Vec::new(),
                });
                telescope_chat.discord_channel_id = Some(default_channel_id);
            }
            None => {}
        }

        if let Some(matrix) = self.matrix {
            chat.matrix = Some(SharedMatrixConfig {
                enabled: true,
                homeserver_url: matrix.homeserver_url,
                username: matrix.username,
                password: matrix.password,
                default_room_id: Some(matrix.default_room_id),
            });
        }

        let config = Config {
            chat,
            telescopes: vec![TelescopeConfig {
                name: self.profile.profile_name,
                chat: telescope_chat,
                ..TelescopeConfig::default()
            }],
            ..Config::default()
        };
        config.validate()?;
        Ok(config)
    }
}

fn validate_pipe_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(
            "Direct pipe name must contain only ASCII letters, digits, '-' or '_'".to_string(),
        );
    }
    Ok(())
}

#[cfg(windows)]
pub async fn run_from_named_pipe(
    pipe_name: &str,
    log_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::direct::pipe_source::DirectPipeRigSource;
    use crate::service_wrapper::ServiceWrapper;
    use crate::source::SharedRigSource;
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;
    use winapi::um::processenv::SetStdHandle;
    use winapi::um::winbase::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    if let Some(parent) = Path::new(log_file).parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)?;
    let stderr = stdout.try_clone()?;
    // SAFETY: both files remain open for the process lifetime (forgotten below),
    // and SetStdHandle only replaces this process's stdout/stderr handles.
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, stdout.as_raw_handle().cast()) == 0
            || SetStdHandle(STD_ERROR_HANDLE, stderr.as_raw_handle().cast()) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    std::mem::forget(stdout);
    std::mem::forget(stderr);

    let full_pipe_name = format!(r"\\.\pipe\{pipe_name}");
    let started = Instant::now();
    let pipe = loop {
        match ClientOptions::new().open(&full_pipe_name) {
            Ok(pipe) => break pipe,
            Err(error) if started.elapsed() < Duration::from_secs(15) => {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) && error.raw_os_error() != Some(231)
                {
                    return Err(error.into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };

    let (read_half, mut write_half) = tokio::io::split(pipe);
    let mut lines = BufReader::new(read_half).lines();
    let bootstrap_json = lines
        .next_line()
        .await?
        .ok_or("bootstrap pipe closed before configuration arrived")?;
    let bootstrap = PluginRuntimeBootstrap::from_json(&bootstrap_json)?;
    bootstrap.validate()?;
    let poll_interval = bootstrap.poll_interval_seconds();
    let exit_on_disconnect = bootstrap.exit_on_control_disconnect;
    let telescope_name = bootstrap.profile.profile_name.clone();
    let direct_source: SharedRigSource = match &bootstrap.source {
        PluginRuntimeSource::NinaDirect {
            pipe_name,
            capabilities,
        } => std::sync::Arc::new(DirectPipeRigSource::connect(pipe_name, *capabilities).await?),
    };
    let config = bootstrap.into_config()?;

    write_half
        .write_all(
            format!(
                "{{\"type\":\"ready\",\"protocol_version\":{PLUGIN_RUNTIME_PROTOCOL_VERSION}}}\n"
            )
            .as_bytes(),
        )
        .await?;
    write_half.flush().await?;

    let service = ServiceWrapper::new(config)?;
    let mut source_overrides = HashMap::new();
    source_overrides.insert(telescope_name, direct_source);
    let control = async move {
        loop {
            match lines.next_line().await? {
                Some(line) => {
                    let message: RuntimeControlMessage = serde_json::from_str(&line)?;
                    if message.message_type == "shutdown" {
                        return Ok::<(), Box<dyn std::error::Error>>(());
                    }
                    return Err(format!(
                        "unsupported plugin runtime control message: {}",
                        message.message_type
                    )
                    .into());
                }
                None if exit_on_disconnect => return Ok(()),
                None => std::future::pending::<()>().await,
            }
        }
    };

    tokio::select! {
        result = service.run_cli_with_sources(poll_interval, source_overrides) => result.map_err(Into::into),
        result = control => result,
    }
}

#[cfg(windows)]
#[derive(Deserialize)]
struct RuntimeControlMessage {
    #[serde(rename = "type")]
    message_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json(delivery: &str, matrix: &str) -> String {
        format!(
            r#"{{
                "protocol_version": 1,
                "profile": {{
                    "node_id": "363db028-9d79-4fdc-8940-1b1ff52b9e8d",
                    "profile_id": "460a8c62-28ce-4781-92e5-ab2440982175",
                    "profile_name": "North Rig"
                }},
                "source": {{
                    "kind": "nina_direct",
                    "pipe_name": "chatstronomy-direct-test",
                    "capabilities": {{
                        "event_history": true,
                        "image_history": true,
                        "thumbnails": true,
                        "sequence": true,
                        "equipment_snapshots": true,
                        "autofocus_details": true,
                        "guider_graph": true,
                        "commands": true
                    }}
                }},
                "delivery": {delivery},
                "matrix": {matrix},
                "data_directory": "{}",
                "exit_on_control_disconnect": true
            }}"#,
            std::env::temp_dir().to_string_lossy().replace('\\', "\\\\")
        )
    }

    #[test]
    fn webhook_bootstrap_maps_to_existing_config() {
        let json = sample_json(
            r#"{"kind":"discord_webhook","webhook_url":"https://discord.com/api/v10/webhooks/123/token"}"#,
            "null",
        );
        let bootstrap = PluginRuntimeBootstrap::from_json(&json).unwrap();
        assert_eq!(bootstrap.poll_interval_seconds(), 5);
        let config = bootstrap.into_config().unwrap();
        assert_eq!(config.telescopes[0].name, "North Rig");
        assert_eq!(
            config.chat.discord.unwrap().default_webhook_url.as_deref(),
            Some("https://discord.com/api/v10/webhooks/123/token")
        );
    }

    #[test]
    fn discord_bot_maps_channel_for_commands() {
        let json = sample_json(
            r#"{"kind":"discord_bot","bot_token":"secret","default_channel_id":456}"#,
            "null",
        );
        let config = PluginRuntimeBootstrap::from_json(&json)
            .unwrap()
            .into_config()
            .unwrap();
        assert_eq!(config.telescopes[0].chat.discord_channel_id, Some(456));
        let bot = config.chat.discord_bot.unwrap();
        assert_eq!(bot.default_channel_id, Some(456));
        assert!(
            bot.write_acl.is_empty(),
            "the local bot's secure fallback permits guild managers only"
        );
    }

    #[test]
    fn matrix_requires_https() {
        let json = sample_json(
            "null",
            r#"{
                "homeserver_url":"http://matrix.example.test/",
                "username":"@bot:example.test",
                "password":"secret",
                "default_room_id":"!room:example.test"
            }"#,
        );
        let error = PluginRuntimeBootstrap::from_json(&json)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.contains("https://"));
    }

    #[test]
    fn native_direct_source_has_no_http_dependency() {
        let json = sample_json(
            r#"{"kind":"discord_webhook","webhook_url":"https://discord.com/api/webhooks/123/token"}"#,
            "null",
        );
        let bootstrap = PluginRuntimeBootstrap::from_json(&json).unwrap();
        bootstrap.validate().unwrap();
        assert_eq!(bootstrap.poll_interval_seconds(), 5);
        let config = bootstrap.into_config().unwrap();
        assert_eq!(config.telescopes[0].name, "North Rig");
        assert!(!json.contains("http://"));
    }
}
