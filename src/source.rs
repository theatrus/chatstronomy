//! Observatory data-source abstraction.
//!
//! Native [`RigSourceKind::NinaDirect`] is the Chatstronomy N.I.N.A. plugin
//! transport. Consumers such as the chat updater and Discord command handlers
//! depend on [`RigSource`] rather than a particular Direct connection.

use crate::api_types::CommandResponse;
use crate::autofocus::AutofocusResponse;
use crate::camera::CameraInfoResponse;
use crate::events::EventHistoryResponse;
use crate::filterwheel::FilterWheelInfoResponse;
use crate::focuser::FocuserInfoResponse;
use crate::guider::{GuiderGraphResponse, GuiderInfoResponse};
use crate::images::{ImageHistoryResponse, ThumbnailResponse};
use crate::mount::MountInfoResponse;
use crate::rotator::RotatorInfoResponse;
use crate::sequence::SequenceResponse;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// How a rig supplies N.I.N.A. data to Chatstronomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RigSourceKind {
    /// Receive events and execute commands through the Chatstronomy N.I.N.A. plugin.
    NinaDirect,
}

/// Features a source can expose to the source-neutral Chatstronomy runtime.
///
/// Direct mode will negotiate these flags with the N.I.N.A. plugin. Keeping
/// them explicit lets chat commands report an unsupported feature instead of
/// guessing or silently falling back to a second source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigCapabilities {
    pub event_history: bool,
    pub image_history: bool,
    pub thumbnails: bool,
    pub sequence: bool,
    pub equipment_snapshots: bool,
    pub autofocus_details: bool,
    pub guider_graph: bool,
    pub commands: bool,
}

impl RigCapabilities {
    pub const fn none() -> Self {
        Self {
            event_history: false,
            image_history: false,
            thumbnails: false,
            sequence: false,
            equipment_snapshots: false,
            autofocus_details: false,
            guider_graph: false,
            commands: false,
        }
    }

    pub const fn all() -> Self {
        Self {
            event_history: true,
            image_history: true,
            thumbnails: true,
            sequence: true,
            equipment_snapshots: true,
            autofocus_details: true,
            guider_graph: true,
            commands: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum RigSourceError {
    #[error("{kind:?} source does not support {capability}")]
    Unsupported {
        kind: RigSourceKind,
        capability: &'static str,
    },

    #[error("{kind:?} request was rejected: {reason}")]
    Rejected { kind: RigSourceKind, reason: String },

    #[error("{kind:?} source is unavailable: {reason}")]
    Unavailable { kind: RigSourceKind, reason: String },

    #[error("{kind:?} source returned an invalid response: {reason}")]
    InvalidResponse { kind: RigSourceKind, reason: String },
}

pub type RigSourceResult<T> = Result<T, RigSourceError>;
pub type SharedRigSource = Arc<dyn RigSource>;

/// Closed, transport-neutral write surface exposed to chat commands.
///
/// Direct sources send this typed value to the N.I.N.A. plugin, which never
/// needs to parse or authorize arbitrary endpoint strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RigCommand {
    UnparkMount,
    HomeMount,
    ChangeFilter { filter_id: i32 },
    StartGuiding { calibrate: bool },
    StopGuiding,
    CoolCamera { temperature: f64, minutes: f64 },
    WarmCamera { minutes: f64 },
    StartAutofocus,
    CancelAutofocus,
    ParkMount,
    AbortExposure,
    StopSequence,
    StartSequence { skip_validation: bool },
}

/// Source-neutral read and command surface used by Chatstronomy's runtime.
///
/// This intentionally describes the capabilities Chatstronomy consumes rather
/// than mirroring a transport. The direct N.I.N.A. implementation can satisfy
/// the same operations from cached snapshots and request/response messages.
#[async_trait]
pub trait RigSource: Send + Sync {
    fn kind(&self) -> RigSourceKind;
    fn capabilities(&self) -> RigCapabilities;

    async fn get_event_history(&self) -> RigSourceResult<EventHistoryResponse>;
    async fn get_all_image_history(&self) -> RigSourceResult<ImageHistoryResponse>;
    async fn get_sequence(&self) -> RigSourceResult<SequenceResponse>;
    async fn get_thumbnail(&self, index: u32) -> RigSourceResult<ThumbnailResponse>;
    async fn get_last_autofocus(&self) -> RigSourceResult<AutofocusResponse>;
    async fn get_mount_info(&self) -> RigSourceResult<MountInfoResponse>;
    async fn get_camera_info(&self) -> RigSourceResult<CameraInfoResponse>;
    async fn get_filterwheel_info(&self) -> RigSourceResult<FilterWheelInfoResponse>;
    async fn get_guider_info(&self) -> RigSourceResult<GuiderInfoResponse>;
    async fn get_guider_graph(&self) -> RigSourceResult<GuiderGraphResponse>;
    async fn get_rotator_info(&self) -> RigSourceResult<RotatorInfoResponse>;
    async fn get_focuser_info(&self) -> RigSourceResult<FocuserInfoResponse>;
    async fn execute_command(&self, command: RigCommand) -> RigSourceResult<CommandResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_has_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&RigSourceKind::NinaDirect).unwrap(),
            r#""nina_direct""#
        );
    }

    #[test]
    fn commands_have_stable_semantic_wire_names() {
        assert_eq!(
            serde_json::to_value(RigCommand::CoolCamera {
                temperature: -10.0,
                minutes: 15.0,
            })
            .unwrap(),
            serde_json::json!({
                "kind": "cool_camera",
                "temperature": -10.0,
                "minutes": 15.0,
            })
        );
        assert_eq!(
            serde_json::to_value(RigCommand::ParkMount).unwrap(),
            serde_json::json!({"kind": "park_mount"})
        );
    }

    #[test]
    fn explicit_plugin_denials_are_not_reported_as_disconnects() {
        let error = RigSourceError::Rejected {
            kind: RigSourceKind::NinaDirect,
            reason: "Parking is disabled in this N.I.N.A. profile".to_string(),
        };
        let message = error.to_string();
        assert!(message.contains("request was rejected"));
        assert!(message.contains("Parking is disabled"));
        assert!(!message.contains("unavailable"));
    }
}
