//! Native N.I.N.A. source over the plugin-owned current-user named pipe.

use crate::api_types::CommandResponse;
use crate::autofocus::AutofocusResponse;
use crate::camera::CameraInfoResponse;
use crate::direct::protocol::{DirectMessage, QueryKind, QueryRequest};
use crate::events::EventHistoryResponse;
use crate::filterwheel::FilterWheelInfoResponse;
use crate::focuser::FocuserInfoResponse;
use crate::guider::{GuiderGraphResponse, GuiderInfoResponse};
use crate::images::{ImageHistoryResponse, ThumbnailResponse};
use crate::mount::MountInfoResponse;
use crate::rotator::RotatorInfoResponse;
use crate::sequence::SequenceResponse;
use crate::source::{
    RigCapabilities, RigCommand, RigSource, RigSourceError, RigSourceKind, RigSourceResult,
};
use async_trait::async_trait;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::Mutex;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
// Loaded advanced sequences and long histories can legitimately exceed a
// megabyte. Keep a bounded response, but leave enough room for a real session.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub struct DirectPipeRigSource {
    capabilities: RigCapabilities,
    connection: Mutex<BufReader<NamedPipeClient>>,
}

impl DirectPipeRigSource {
    pub async fn connect(pipe_name: &str, capabilities: RigCapabilities) -> Result<Self, String> {
        let full_name = format!(r"\\.\pipe\{pipe_name}");
        let started = Instant::now();
        let pipe = loop {
            match ClientOptions::new().open(&full_name) {
                Ok(pipe) => break pipe,
                Err(error) if started.elapsed() < CONNECT_TIMEOUT => {
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) && error.raw_os_error() != Some(231)
                    {
                        return Err(format!("could not connect to Direct data pipe: {error}"));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => {
                    return Err(format!(
                        "could not connect to Direct data pipe within {} seconds: {error}",
                        CONNECT_TIMEOUT.as_secs()
                    ));
                }
            }
        };
        Ok(Self {
            capabilities,
            connection: Mutex::new(BufReader::new(pipe)),
        })
    }

    fn unavailable(reason: impl Into<String>) -> RigSourceError {
        RigSourceError::Unavailable {
            kind: RigSourceKind::NinaDirect,
            reason: reason.into(),
        }
    }

    fn unsupported(capability: &'static str) -> RigSourceError {
        RigSourceError::Unsupported {
            kind: RigSourceKind::NinaDirect,
            capability,
        }
    }

    fn invalid_response(reason: impl Into<String>) -> RigSourceError {
        RigSourceError::InvalidResponse {
            kind: RigSourceKind::NinaDirect,
            reason: reason.into(),
        }
    }

    async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        kind: QueryKind,
    ) -> RigSourceResult<T> {
        let id = Uuid::new_v4();
        // Pipe writes may still sit behind a stalled N.I.N.A. dispatcher after
        // our caller times out. Give hardware commands the same bounded
        // lifetime as the local exchange; preserve legacy deadline-free reads.
        let request = DirectMessage::Query(QueryRequest {
            id,
            expires_at: query_deadline(&kind),
            kind,
        });
        let mut frame = serde_json::to_vec(&request)
            .map_err(|error| Self::unavailable(format!("could not encode query: {error}")))?;
        frame.push(b'\n');

        let exchange = async {
            let mut connection = self.connection.lock().await;
            connection.write_all(&frame).await?;
            connection.flush().await?;

            let mut response = String::new();
            let bytes = connection.read_line(&mut response).await?;
            if bytes == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Direct data pipe closed",
                ));
            }
            if response.len() > MAX_FRAME_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Direct response exceeds the size limit",
                ));
            }
            Ok::<String, std::io::Error>(response)
        };

        let response = tokio::time::timeout(QUERY_TIMEOUT, exchange)
            .await
            .map_err(|_| Self::unavailable("Direct query timed out"))?
            .map_err(|error| Self::unavailable(error.to_string()))?;
        let message: DirectMessage = serde_json::from_str(&response)
            .map_err(|error| Self::invalid_response(format!("invalid Direct response: {error}")))?;
        let DirectMessage::QueryResult(result) = message else {
            return Err(Self::invalid_response("plugin returned a non-result frame"));
        };
        if result.id != id {
            return Err(Self::invalid_response(
                "plugin returned a mismatched query ID",
            ));
        }
        if !result.ok {
            return Err(RigSourceError::Rejected {
                kind: RigSourceKind::NinaDirect,
                reason: result.error.unwrap_or_else(|| "query failed".to_string()),
            });
        }
        serde_json::from_value(result.payload).map_err(|error| {
            Self::invalid_response(format!("invalid payload from plugin: {error}"))
        })
    }
}

fn query_deadline(kind: &QueryKind) -> Option<i64> {
    matches!(kind, QueryKind::Command { .. })
        .then(|| crate::direct::protocol::unix_now().saturating_add(QUERY_TIMEOUT.as_secs() as i64))
}

#[async_trait]
impl RigSource for DirectPipeRigSource {
    fn kind(&self) -> RigSourceKind {
        RigSourceKind::NinaDirect
    }

    fn capabilities(&self) -> RigCapabilities {
        self.capabilities
    }

    async fn get_event_history(&self) -> RigSourceResult<EventHistoryResponse> {
        if !self.capabilities.event_history {
            return Err(Self::unsupported("event history"));
        }
        self.query_as(QueryKind::EventHistory).await
    }

    async fn get_all_image_history(&self) -> RigSourceResult<ImageHistoryResponse> {
        if !self.capabilities.image_history {
            return Err(Self::unsupported("image history"));
        }
        self.query_as(QueryKind::ImageHistory).await
    }

    async fn get_sequence(&self) -> RigSourceResult<SequenceResponse> {
        if !self.capabilities.sequence {
            return Err(Self::unsupported("sequence"));
        }
        self.query_as(QueryKind::Sequence).await
    }

    async fn get_thumbnail(&self, index: u32) -> RigSourceResult<ThumbnailResponse> {
        if !self.capabilities.thumbnails {
            return Err(Self::unsupported("thumbnails"));
        }
        self.query_as(QueryKind::Thumbnail { index }).await
    }

    async fn get_last_autofocus(&self) -> RigSourceResult<AutofocusResponse> {
        if !self.capabilities.autofocus_details {
            return Err(Self::unsupported("autofocus details"));
        }
        self.query_as(QueryKind::LastAutofocus).await
    }

    async fn get_mount_info(&self) -> RigSourceResult<MountInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::MountInfo).await
    }

    async fn get_camera_info(&self) -> RigSourceResult<CameraInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::CameraInfo).await
    }

    async fn get_filterwheel_info(&self) -> RigSourceResult<FilterWheelInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::FilterwheelInfo).await
    }

    async fn get_guider_info(&self) -> RigSourceResult<GuiderInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::GuiderInfo).await
    }

    async fn get_guider_graph(&self) -> RigSourceResult<GuiderGraphResponse> {
        if !self.capabilities.guider_graph {
            return Err(Self::unsupported("guider graph"));
        }
        self.query_as(QueryKind::GuiderGraph).await
    }

    async fn get_rotator_info(&self) -> RigSourceResult<RotatorInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::RotatorInfo).await
    }

    async fn get_focuser_info(&self) -> RigSourceResult<FocuserInfoResponse> {
        if !self.capabilities.equipment_snapshots {
            return Err(Self::unsupported("equipment snapshots"));
        }
        self.query_as(QueryKind::FocuserInfo).await
    }

    async fn execute_command(&self, command: RigCommand) -> RigSourceResult<CommandResponse> {
        if !self.capabilities.commands {
            return Err(Self::unsupported("commands"));
        }
        self.query_as(QueryKind::Command { command }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hardware_commands_have_deadlines_but_legacy_reads_do_not() {
        let before = crate::direct::protocol::unix_now();
        let command = QueryKind::Command {
            command: RigCommand::ParkMount,
        };
        let deadline = query_deadline(&command).expect("local commands must expire");
        let after = crate::direct::protocol::unix_now();

        assert!(deadline >= before + QUERY_TIMEOUT.as_secs() as i64);
        assert!(deadline <= after + QUERY_TIMEOUT.as_secs() as i64);
        assert_eq!(query_deadline(&QueryKind::MountInfo), None);
        assert_eq!(query_deadline(&QueryKind::EventHistory), None);
    }
}
