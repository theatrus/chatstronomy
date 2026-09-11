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
            return Err(result.into_source_error(RigSourceKind::NinaDirect));
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

    async fn acknowledge_autofocus_delivery(&self, report_timestamp: &str) -> RigSourceResult<()> {
        if !self.capabilities.autofocus_delivery_ack {
            return Ok(());
        }
        let _: serde_json::Value = self
            .query_as(QueryKind::AcknowledgeAutofocus {
                report_timestamp: report_timestamp.to_string(),
            })
            .await?;
        Ok(())
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
        if command.is_target_command() && !self.capabilities.target_commands {
            return Err(Self::unsupported(
                "current-target commands (update the Chatstronomy N.I.N.A. plugin)",
            ));
        }
        self.query_as(QueryKind::Command { command }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::protocol::QueryResult;
    use tokio::net::windows::named_pipe::ServerOptions;

    #[tokio::test]
    async fn local_commands_preserve_sequence_queueing_and_rejections() {
        let cases = [
            (
                RigCommand::ChangeFilter { filter_id: 3 },
                true,
                "Filter change queued before the next light exposure",
            ),
            (
                RigCommand::SlewToTarget,
                true,
                "Target slew queued before the next light exposure",
            ),
            (
                RigCommand::CenterTarget,
                true,
                "Target centering queued before the next light exposure",
            ),
            (
                RigCommand::CenterRotateTarget,
                true,
                "Target centering and rotation queued before the next light exposure",
            ),
            (
                RigCommand::StartAutofocus,
                true,
                "Autofocus queued for the next exposure boundary",
            ),
            (
                RigCommand::CancelAutofocus,
                true,
                "Autofocus cancellation requested",
            ),
            (
                RigCommand::StartSequence {
                    skip_validation: false,
                },
                true,
                "Sequence start requested",
            ),
            (RigCommand::StopSequence, true, "Sequence stop requested"),
            (
                RigCommand::AbortExposure,
                false,
                "A sequence owns the camera. Use /stop-sequence to stop it.",
            ),
        ];
        let pipe_name = format!("chatstronomy-command-test-{}", Uuid::new_v4());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(format!(r"\\.\pipe\{pipe_name}"))
            .unwrap();
        let expected = cases.clone();
        let serve = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut server = BufReader::new(server);
            for (command, ok, message) in expected {
                let mut request = String::new();
                server.read_line(&mut request).await.unwrap();
                let DirectMessage::Query(request) = serde_json::from_str(&request).unwrap() else {
                    panic!("expected command query");
                };
                assert_eq!(request.kind, QueryKind::Command { command });
                assert!(request.expires_at.is_some());
                let result = DirectMessage::QueryResult(QueryResult {
                    id: request.id,
                    ok,
                    payload: if ok {
                        serde_json::json!({
                            "Response": message,
                            "Error": "",
                            "StatusCode": 202,
                            "Success": true,
                            "Type": "API"
                        })
                    } else {
                        serde_json::Value::Null
                    },
                    error: (!ok).then(|| message.to_string()),
                    error_code: None,
                });
                let mut frame = serde_json::to_vec(&result).unwrap();
                frame.push(b'\n');
                server.write_all(&frame).await.unwrap();
                server.flush().await.unwrap();
            }
        });
        let source = DirectPipeRigSource::connect(&pipe_name, RigCapabilities::all())
            .await
            .unwrap();
        for (command, ok, message) in cases {
            let result = source.execute_command(command).await;
            if ok {
                let response = result.unwrap();
                assert!(response.is_pending());
                assert_eq!(response.summary(), message);
            } else {
                assert!(
                    matches!(result, Err(RigSourceError::Rejected { reason, .. }) if reason == message)
                );
            }
        }
        serve.await.unwrap();
    }

    #[tokio::test]
    async fn local_target_commands_cannot_bypass_read_only_capabilities() {
        let pipe_name = format!("chatstronomy-locked-command-test-{}", Uuid::new_v4());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(format!(r"\\.\pipe\{pipe_name}"))
            .unwrap();
        let source = DirectPipeRigSource::connect(&pipe_name, RigCapabilities::none())
            .await
            .unwrap();
        server.connect().await.unwrap();
        for command in [
            RigCommand::SlewToTarget,
            RigCommand::CenterTarget,
            RigCommand::CenterRotateTarget,
        ] {
            assert!(matches!(
                source.execute_command(command).await,
                Err(RigSourceError::Unsupported {
                    capability: "commands",
                    ..
                })
            ));
        }
        let mut byte = [0];
        assert_eq!(
            server.try_read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn legacy_local_plugins_never_receive_unknown_target_command_kinds() {
        for target_support in [None, Some(false)] {
            let pipe_name = format!("chatstronomy-legacy-command-test-{}", Uuid::new_v4());
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(format!(r"\\.\pipe\{pipe_name}"))
                .unwrap();
            let mut capabilities = serde_json::to_value(RigCapabilities::all()).unwrap();
            capabilities
                .as_object_mut()
                .unwrap()
                .remove("target_commands");
            if let Some(supported) = target_support {
                capabilities["target_commands"] = serde_json::json!(supported);
            }
            let source = DirectPipeRigSource::connect(
                &pipe_name,
                serde_json::from_value(capabilities).unwrap(),
            )
            .await
            .unwrap();
            server.connect().await.unwrap();
            for command in [
                RigCommand::SlewToTarget,
                RigCommand::CenterTarget,
                RigCommand::CenterRotateTarget,
            ] {
                assert!(
                    matches!(source.execute_command(command).await, Err(RigSourceError::Unsupported { capability, .. }) if capability.contains("current-target commands"))
                );
            }
            let mut byte = [0];
            assert_eq!(
                server.try_read(&mut byte).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

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
