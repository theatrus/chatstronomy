//! [`RigSource`] over a live Direct connection.
//!
//! Every read becomes a query round trip to the connected rig; the payload
//! is JSON for Chatstronomy's shared response types,
//! so everything downstream (chat updater, bot commands, charts) works
//! unchanged.

use super::direct_server::{QUERY_TIMEOUT, RigConnection};
use crate::api_types::CommandResponse;
use crate::autofocus::AutofocusResponse;
use crate::camera::CameraInfoResponse;
use crate::direct::protocol::QueryKind;
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
use std::sync::Arc;

pub struct DirectRigSource {
    connection: Arc<RigConnection>,
}

impl DirectRigSource {
    pub fn new(connection: Arc<RigConnection>) -> Self {
        Self { connection }
    }

    fn unavailable(reason: String) -> RigSourceError {
        RigSourceError::Unavailable {
            kind: RigSourceKind::NinaDirect,
            reason,
        }
    }

    async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        kind: QueryKind,
    ) -> RigSourceResult<T> {
        let result = self
            .connection
            .query(kind, QUERY_TIMEOUT)
            .await
            .map_err(Self::unavailable)?;
        if !result.ok {
            return Err(RigSourceError::Rejected {
                kind: RigSourceKind::NinaDirect,
                reason: result.error.unwrap_or_else(|| "query failed".to_string()),
            });
        }
        serde_json::from_value(result.payload)
            .map_err(|e| Self::unavailable(format!("invalid payload from rig: {e}")))
    }
}

#[async_trait]
impl RigSource for DirectRigSource {
    fn kind(&self) -> RigSourceKind {
        RigSourceKind::NinaDirect
    }

    fn capabilities(&self) -> RigCapabilities {
        self.connection.capabilities
    }

    async fn get_event_history(&self) -> RigSourceResult<EventHistoryResponse> {
        self.query_as(QueryKind::EventHistory).await
    }

    async fn get_all_image_history(&self) -> RigSourceResult<ImageHistoryResponse> {
        self.query_as(QueryKind::ImageHistory).await
    }

    async fn get_sequence(&self) -> RigSourceResult<SequenceResponse> {
        self.query_as(QueryKind::Sequence).await
    }

    async fn get_thumbnail(&self, index: u32) -> RigSourceResult<ThumbnailResponse> {
        self.query_as(QueryKind::Thumbnail { index }).await
    }

    async fn get_last_autofocus(&self) -> RigSourceResult<AutofocusResponse> {
        self.query_as(QueryKind::LastAutofocus).await
    }

    async fn get_mount_info(&self) -> RigSourceResult<MountInfoResponse> {
        self.query_as(QueryKind::MountInfo).await
    }

    async fn get_camera_info(&self) -> RigSourceResult<CameraInfoResponse> {
        self.query_as(QueryKind::CameraInfo).await
    }

    async fn get_filterwheel_info(&self) -> RigSourceResult<FilterWheelInfoResponse> {
        self.query_as(QueryKind::FilterwheelInfo).await
    }

    async fn get_guider_info(&self) -> RigSourceResult<GuiderInfoResponse> {
        self.query_as(QueryKind::GuiderInfo).await
    }

    async fn get_guider_graph(&self) -> RigSourceResult<GuiderGraphResponse> {
        self.query_as(QueryKind::GuiderGraph).await
    }

    async fn get_rotator_info(&self) -> RigSourceResult<RotatorInfoResponse> {
        self.query_as(QueryKind::RotatorInfo).await
    }

    async fn get_focuser_info(&self) -> RigSourceResult<FocuserInfoResponse> {
        self.query_as(QueryKind::FocuserInfo).await
    }

    async fn execute_command(&self, command: RigCommand) -> RigSourceResult<CommandResponse> {
        // The N.I.N.A. plugin owns the hardware trust boundary. Never put a
        // command on the wire when its authenticated hello says local control
        // is disabled, even if a caller bypassed the Discord resolver.
        if !self.connection.capabilities.commands {
            return Err(RigSourceError::Unsupported {
                kind: RigSourceKind::NinaDirect,
                capability: "commands",
            });
        }
        self.query_as(QueryKind::Command { command }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;
    use uuid::Uuid;

    #[tokio::test]
    async fn locally_disabled_commands_never_reach_the_rig() {
        let (mut connection, mut outgoing) = RigConnection::stub(1, Uuid::new_v4());
        Arc::get_mut(&mut connection)
            .expect("the new test connection has a single owner")
            .capabilities
            .commands = false;
        let source = DirectRigSource::new(connection);

        let error = source
            .execute_command(RigCommand::ParkMount)
            .await
            .expect_err("a read-only rig must reject commands");

        assert!(matches!(
            error,
            RigSourceError::Unsupported {
                kind: RigSourceKind::NinaDirect,
                capability: "commands",
            }
        ));
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
    }
}
