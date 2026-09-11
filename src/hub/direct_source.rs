//! [`RigSource`] over a live Direct connection.
//!
//! Every read becomes a query round trip to the connected rig; the payload
//! is JSON for Chatstronomy's shared response types,
//! so everything downstream (chat updater, bot commands, charts) works
//! unchanged.

use super::direct_server::{QUERY_TIMEOUT, RigConnection, RigConnections};
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
use uuid::Uuid;

pub struct DirectRigSource {
    connection: DirectConnection,
}

enum DirectConnection {
    /// A short-lived command/request keeps the exact connection it resolved.
    Pinned(Arc<RigConnection>),
    /// A long-lived Hub updater follows the currently authenticated socket for
    /// a telescope so transport-only reconnects do not destroy its delivery
    /// state or pending notifications.
    Current {
        connections: Arc<RigConnections>,
        telescope_id: i64,
        session_id: Uuid,
        profile_id: Uuid,
        lifecycle_generation: u64,
        expected_capabilities: RigCapabilities,
    },
}

impl DirectRigSource {
    pub fn new(connection: Arc<RigConnection>) -> Self {
        Self {
            connection: DirectConnection::Pinned(connection),
        }
    }

    pub fn current(connections: Arc<RigConnections>, connection: Arc<RigConnection>) -> Self {
        let lifecycle_generation = connections.lifecycle_generation(connection.telescope_id);
        Self {
            connection: DirectConnection::Current {
                connections,
                telescope_id: connection.telescope_id,
                session_id: connection.session_id,
                profile_id: connection.profile_id,
                lifecycle_generation,
                expected_capabilities: connection.capabilities,
            },
        }
    }

    fn unavailable(reason: String) -> RigSourceError {
        RigSourceError::Unavailable {
            kind: RigSourceKind::NinaDirect,
            reason,
        }
    }

    fn invalid_response(reason: String) -> RigSourceError {
        RigSourceError::InvalidResponse {
            kind: RigSourceKind::NinaDirect,
            reason,
        }
    }

    fn current_connection(&self) -> RigSourceResult<Arc<RigConnection>> {
        match &self.connection {
            DirectConnection::Pinned(connection) => Ok(connection.clone()),
            DirectConnection::Current {
                connections,
                telescope_id,
                session_id,
                profile_id,
                lifecycle_generation,
                ..
            } => connections
                .get_if_generation(*telescope_id, *lifecycle_generation)
                .filter(|connection| {
                    connection.session_id == *session_id && connection.profile_id == *profile_id
                })
                .ok_or_else(|| {
                    Self::unavailable(
                        "telescope transport session is not connected to the Hub".to_string(),
                    )
                }),
        }
    }

    fn ensure_connection_still_current(&self, used: &Arc<RigConnection>) -> RigSourceResult<()> {
        match &self.connection {
            DirectConnection::Pinned(_) => Ok(()),
            DirectConnection::Current {
                connections,
                telescope_id,
                session_id,
                profile_id,
                lifecycle_generation,
                ..
            } => connections
                .get_if_generation(*telescope_id, *lifecycle_generation)
                .filter(|current| {
                    current.connection_id == used.connection_id
                        && current.session_id == *session_id
                        && current.profile_id == *profile_id
                })
                .map(|_| ())
                .ok_or_else(|| {
                    Self::unavailable(
                        "telescope transport changed while a Direct query was in flight"
                            .to_string(),
                    )
                }),
        }
    }

    async fn query_value_on(
        &self,
        connection: Arc<RigConnection>,
        kind: QueryKind,
    ) -> RigSourceResult<serde_json::Value> {
        let result = connection
            .query(kind, QUERY_TIMEOUT)
            .await
            .map_err(Self::unavailable)?;
        // Validate the exact authenticated socket before examining either
        // branch of its result. A replaced profile/session must not leak even
        // an old rig's rejection text through the new logical source.
        self.ensure_connection_still_current(&connection)?;
        if !result.ok {
            return Err(result.into_source_error(RigSourceKind::NinaDirect));
        }
        Ok(result.payload)
    }

    async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        kind: QueryKind,
    ) -> RigSourceResult<T> {
        let connection = self.current_connection()?;
        let payload = self.query_value_on(connection, kind).await?;
        serde_json::from_value(payload)
            .map_err(|e| Self::invalid_response(format!("invalid payload from rig: {e}")))
    }
}

#[async_trait]
impl RigSource for DirectRigSource {
    fn kind(&self) -> RigSourceKind {
        RigSourceKind::NinaDirect
    }

    fn command_connection_id(&self) -> Option<Uuid> {
        match &self.connection {
            DirectConnection::Pinned(connection) => Some(connection.connection_id),
            DirectConnection::Current { .. } => None,
        }
    }

    fn capabilities(&self) -> RigCapabilities {
        match &self.connection {
            DirectConnection::Pinned(connection) => connection.capabilities,
            DirectConnection::Current {
                expected_capabilities,
                ..
            } => *expected_capabilities,
        }
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

    async fn acknowledge_autofocus_delivery(&self, report_timestamp: &str) -> RigSourceResult<()> {
        let connection = self.current_connection()?;
        if !connection.capabilities.autofocus_delivery_ack {
            return Ok(());
        }
        self.query_value_on(
            connection,
            QueryKind::AcknowledgeAutofocus {
                report_timestamp: report_timestamp.to_string(),
            },
        )
        .await?;
        Ok(())
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
        let connection = self.current_connection()?;
        // The N.I.N.A. plugin owns the hardware trust boundary. Never put a
        // command on the wire when its authenticated hello says local control
        // is disabled, even if a caller bypassed the Discord resolver.
        if !connection.capabilities.commands {
            return Err(RigSourceError::Unsupported {
                kind: RigSourceKind::NinaDirect,
                capability: "commands",
            });
        }
        if command.is_target_command() && !connection.capabilities.target_commands {
            return Err(RigSourceError::Unsupported {
                kind: RigSourceKind::NinaDirect,
                capability: "current-target commands (update the Chatstronomy N.I.N.A. plugin)",
            });
        }
        let payload = self
            .query_value_on(connection, QueryKind::Command { command })
            .await?;
        serde_json::from_value(payload)
            .map_err(|e| Self::invalid_response(format!("invalid payload from rig: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::protocol::{DirectMessage, QueryErrorCode, QueryResult};
    use tokio::sync::mpsc::error::TryRecvError;
    use uuid::Uuid;

    const AF_REPORT_TIMESTAMP: &str = "2026-09-11T01:02:03.4567890+00:00";

    #[tokio::test]
    async fn legacy_rigs_never_receive_unknown_autofocus_receipt_queries() {
        for receipt_support in [None, Some(false)] {
            let (mut connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
            let mut capabilities = serde_json::to_value(RigCapabilities::all()).unwrap();
            capabilities
                .as_object_mut()
                .unwrap()
                .remove("autofocus_delivery_ack");
            if let Some(supported) = receipt_support {
                capabilities["autofocus_delivery_ack"] = serde_json::json!(supported);
            }
            Arc::get_mut(&mut connection).unwrap().capabilities =
                serde_json::from_value(capabilities).unwrap();
            let source = DirectRigSource::new(connection);

            source
                .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                .await
                .unwrap();
            assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[tokio::test]
    async fn autofocus_receipts_do_not_require_local_hardware_command_permission() {
        let (mut connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        Arc::get_mut(&mut connection).unwrap().capabilities.commands = false;
        let source = DirectRigSource::new(connection.clone());
        let exchange = tokio::spawn(async move {
            source
                .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                .await
        });
        let request = match outgoing.recv().await.expect("receipt query") {
            DirectMessage::Query(request) => request,
            other => panic!("expected receipt query, got {other:?}"),
        };
        assert_eq!(
            request.kind,
            QueryKind::AcknowledgeAutofocus {
                report_timestamp: AF_REPORT_TIMESTAMP.to_string(),
            }
        );
        assert!(request.expires_at.is_some());
        connection.resolve(QueryResult {
            id: request.id,
            ok: true,
            payload: serde_json::json!({"Acknowledged": true}),
            error: None,
            error_code: None,
        });
        exchange.await.unwrap().unwrap();
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn failed_autofocus_receipt_is_not_reported_as_acknowledged_or_retried() {
        let (connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        let source = DirectRigSource::new(connection.clone());
        let exchange = tokio::spawn(async move {
            source
                .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                .await
        });
        let request = match outgoing.recv().await.expect("receipt query") {
            DirectMessage::Query(request) => request,
            other => panic!("expected receipt query, got {other:?}"),
        };
        connection.resolve(QueryResult {
            id: request.id,
            ok: false,
            payload: serde_json::Value::Null,
            error: Some("receipt rejected".to_string()),
            error_code: None,
        });
        assert!(matches!(
            exchange.await.unwrap(),
            Err(RigSourceError::Rejected { reason, .. }) if reason == "receipt rejected"
        ));
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn autofocus_receipt_negotiation_uses_the_current_socket_capabilities() {
        for replacement_supports_receipts in [false, true] {
            let connections = Arc::new(RigConnections::default());
            let (mut first, mut first_outgoing) = RigConnection::stub(7, Uuid::new_v4());
            Arc::get_mut(&mut first)
                .unwrap()
                .capabilities
                .autofocus_delivery_ack = !replacement_supports_receipts;
            let session_id = first.session_id;
            let profile_id = first.profile_id;
            connections.insert(first.clone());
            let source = DirectRigSource::current(connections.clone(), first);
            let (mut replacement, mut replacement_outgoing) =
                RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, profile_id);
            let replacement_capabilities =
                &mut Arc::get_mut(&mut replacement).unwrap().capabilities;
            replacement_capabilities.autofocus_delivery_ack = replacement_supports_receipts;
            replacement_capabilities.commands = false;
            connections.insert(replacement.clone());

            assert_eq!(
                source.capabilities().autofocus_delivery_ack,
                !replacement_supports_receipts
            );
            let exchange = tokio::spawn(async move {
                source
                    .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                    .await
            });
            if replacement_supports_receipts {
                let request = match replacement_outgoing
                    .recv()
                    .await
                    .expect("current receipt query")
                {
                    DirectMessage::Query(request) => request,
                    other => panic!("expected receipt query, got {other:?}"),
                };
                assert_eq!(
                    request.kind,
                    QueryKind::AcknowledgeAutofocus {
                        report_timestamp: AF_REPORT_TIMESTAMP.to_string(),
                    }
                );
                replacement.resolve(QueryResult {
                    id: request.id,
                    ok: true,
                    payload: serde_json::Value::Null,
                    error: None,
                    error_code: None,
                });
            }
            exchange.await.unwrap().unwrap();
            assert!(!matches!(
                first_outgoing.try_recv(),
                Ok(DirectMessage::Query(_))
            ));
            assert!(matches!(
                replacement_outgoing.try_recv(),
                Err(TryRecvError::Empty)
            ));
        }
    }

    #[tokio::test]
    async fn autofocus_receipts_never_reach_a_replacement_profile_or_session() {
        for change_profile in [true, false] {
            let connections = Arc::new(RigConnections::default());
            let (first, mut first_outgoing) = RigConnection::stub(7, Uuid::new_v4());
            let old_session = first.session_id;
            let old_profile = first.profile_id;
            connections.insert(first.clone());
            let source = DirectRigSource::current(connections.clone(), first.clone());
            let (replacement, mut replacement_outgoing) = RigConnection::stub_with_identity(
                7,
                Uuid::new_v4(),
                if change_profile {
                    old_session
                } else {
                    Uuid::new_v4()
                },
                if change_profile {
                    Uuid::new_v4()
                } else {
                    old_profile
                },
            );
            connections.insert(replacement);

            assert!(matches!(
                source
                    .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                    .await,
                Err(RigSourceError::Unavailable { .. })
            ));
            assert!(matches!(
                first_outgoing.try_recv(),
                Err(TryRecvError::Empty)
            ));
            assert!(matches!(
                replacement_outgoing.try_recv(),
                Err(TryRecvError::Empty)
            ));
        }
    }

    #[tokio::test]
    async fn in_flight_autofocus_receipt_is_not_accepted_after_socket_replacement() {
        let connections = Arc::new(RigConnections::default());
        let (first, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        let session_id = first.session_id;
        let profile_id = first.profile_id;
        connections.insert(first.clone());
        let source = DirectRigSource::current(connections.clone(), first.clone());
        let exchange = tokio::spawn(async move {
            source
                .acknowledge_autofocus_delivery(AF_REPORT_TIMESTAMP)
                .await
        });
        let request = match outgoing.recv().await.expect("receipt query") {
            DirectMessage::Query(request) => request,
            other => panic!("expected receipt query, got {other:?}"),
        };
        let (replacement, mut replacement_outgoing) =
            RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, profile_id);
        connections.insert(replacement);
        first.resolve(QueryResult {
            id: request.id,
            ok: true,
            payload: serde_json::Value::Null,
            error: None,
            error_code: None,
        });

        assert!(matches!(
            exchange.await.unwrap(),
            Err(RigSourceError::Unavailable { .. })
        ));
        assert!(matches!(
            replacement_outgoing.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn autofocus_and_sequence_dispatch_preserve_pending_outcomes() {
        for (command, message) in [
            (
                RigCommand::ChangeFilter { filter_id: 3 },
                "Filter change queued before the next light exposure",
            ),
            (
                RigCommand::SlewToTarget,
                "Target slew queued before the next light exposure",
            ),
            (
                RigCommand::CenterTarget,
                "Target centering queued before the next light exposure",
            ),
            (
                RigCommand::CenterRotateTarget,
                "Target centering and rotation queued before the next light exposure",
            ),
            (
                RigCommand::StartAutofocus,
                "Autofocus queued for the next exposure boundary",
            ),
            (
                RigCommand::CancelAutofocus,
                "Autofocus cancellation requested",
            ),
            (
                RigCommand::StartSequence {
                    skip_validation: false,
                },
                "Sequence start requested",
            ),
            (RigCommand::StopSequence, "Sequence stop requested"),
        ] {
            let (connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
            let source = DirectRigSource::new(connection.clone());
            let expected = command.clone();
            let exchange = tokio::spawn(async move { source.execute_command(command).await });
            let request = match outgoing.recv().await.expect("command request") {
                DirectMessage::Query(request) => request,
                other => panic!("expected command query, got {other:?}"),
            };
            assert_eq!(request.kind, QueryKind::Command { command: expected });
            assert!(request.expires_at.is_some());
            connection.resolve(QueryResult {
                id: request.id,
                ok: true,
                payload: serde_json::json!({
                    "Response": message,
                    "Error": "",
                    "StatusCode": 202,
                    "Success": true,
                    "Type": "API"
                }),
                error: None,
                error_code: None,
            });
            let response = exchange.await.unwrap().unwrap();
            assert!(response.is_pending());
            assert_eq!(response.summary(), message);
        }
    }

    #[tokio::test]
    async fn sequence_ownership_rejection_is_preserved_without_retry() {
        let (connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        let source = DirectRigSource::new(connection.clone());
        let exchange =
            tokio::spawn(async move { source.execute_command(RigCommand::AbortExposure).await });
        let request = match outgoing.recv().await.expect("command request") {
            DirectMessage::Query(request) => request,
            other => panic!("expected command query, got {other:?}"),
        };
        let reason = "A sequence owns the camera. Use /stop-sequence to stop it.";
        connection.resolve(QueryResult {
            id: request.id,
            ok: false,
            payload: serde_json::Value::Null,
            error: Some(reason.to_string()),
            error_code: None,
        });
        assert!(matches!(
            exchange.await.unwrap(),
            Err(RigSourceError::Rejected { reason: actual, .. }) if actual == reason
        ));
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn locally_disabled_commands_never_reach_the_rig() {
        let (mut connection, mut outgoing) = RigConnection::stub(1, Uuid::new_v4());
        Arc::get_mut(&mut connection)
            .expect("the new test connection has a single owner")
            .capabilities
            .commands = false;
        let source = DirectRigSource::new(connection);

        for command in [
            RigCommand::ParkMount,
            RigCommand::SlewToTarget,
            RigCommand::CenterTarget,
            RigCommand::CenterRotateTarget,
        ] {
            let error = source
                .execute_command(command)
                .await
                .expect_err("a read-only rig must reject commands");

            assert!(matches!(
                error,
                RigSourceError::Unsupported {
                    kind: RigSourceKind::NinaDirect,
                    capability: "commands",
                }
            ));
        }
        assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn legacy_rigs_never_receive_unknown_target_command_kinds() {
        for target_support in [None, Some(false)] {
            let (mut connection, mut outgoing) = RigConnection::stub(1, Uuid::new_v4());
            let mut capabilities = serde_json::to_value(RigCapabilities::all()).unwrap();
            capabilities
                .as_object_mut()
                .unwrap()
                .remove("target_commands");
            if let Some(supported) = target_support {
                capabilities["target_commands"] = serde_json::json!(supported);
            }
            Arc::get_mut(&mut connection).unwrap().capabilities =
                serde_json::from_value(capabilities).unwrap();
            let source = DirectRigSource::new(connection);
            for command in [
                RigCommand::SlewToTarget,
                RigCommand::CenterTarget,
                RigCommand::CenterRotateTarget,
            ] {
                assert!(
                    matches!(source.execute_command(command).await, Err(RigSourceError::Unsupported { capability, .. }) if capability.contains("current-target commands"))
                );
            }
            assert!(matches!(outgoing.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[test]
    fn current_source_follows_only_same_identity_and_keeps_baseline_capabilities() {
        let connections = Arc::new(RigConnections::default());
        let (mut first, first_outgoing) = RigConnection::stub(7, Uuid::new_v4());
        Arc::get_mut(&mut first)
            .expect("new stub has one owner")
            .capabilities
            .commands = false;
        let session_id = first.session_id;
        connections.insert(first);
        let source = DirectRigSource::current(connections.clone(), connections.get(7).unwrap());
        assert!(!source.capabilities().commands);

        let (replacement, replacement_outgoing) =
            RigConnection::stub_with_session(7, Uuid::new_v4(), session_id);
        connections.insert(replacement);
        assert!(source.current_connection().is_ok());
        assert!(!source.capabilities().commands);

        let (new_profile, new_profile_outgoing) =
            RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, Uuid::new_v4());
        connections.insert(new_profile);
        assert!(source.current_connection().is_err());

        let (new_session, new_session_outgoing) = RigConnection::stub(7, Uuid::new_v4());
        connections.insert(new_session);
        assert!(source.current_connection().is_err());
        assert!(!source.capabilities().commands);

        let current_id = connections.get(7).unwrap().connection_id;
        connections.remove_if_current(7, current_id);
        assert!(source.current_connection().is_err());
        assert!(!source.capabilities().commands);
        drop(first_outgoing);
        drop(replacement_outgoing);
        drop(new_profile_outgoing);
        drop(new_session_outgoing);
    }

    #[test]
    fn explicit_revocation_invalidates_same_identity_repair() {
        let connections = Arc::new(RigConnections::default());
        let session_id = Uuid::new_v4();
        let profile_id = Uuid::new_v4();
        let (first, first_outgoing) =
            RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, profile_id);
        connections.insert(first);
        let source = DirectRigSource::current(connections.clone(), connections.get(7).unwrap());

        connections.revoke(7);
        let (repaired, repaired_outgoing) =
            RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, profile_id);
        connections.insert(repaired);

        assert!(source.current_connection().is_err());
        drop(first_outgoing);
        drop(repaired_outgoing);
    }

    #[tokio::test]
    async fn stale_rejected_result_is_discarded_before_its_error_text() {
        let connections = Arc::new(RigConnections::default());
        let (first, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        let session_id = first.session_id;
        let profile_id = first.profile_id;
        connections.insert(first.clone());
        let source = DirectRigSource::current(connections.clone(), first.clone());

        let query = tokio::spawn(async move { source.get_event_history().await });
        let request = match outgoing.recv().await.expect("query request") {
            DirectMessage::Query(request) => request,
            other => panic!("expected query, got {other:?}"),
        };
        let (replacement, replacement_outgoing) =
            RigConnection::stub_with_identity(7, Uuid::new_v4(), session_id, profile_id);
        connections.insert(replacement);
        first.resolve(QueryResult {
            id: request.id,
            ok: false,
            payload: serde_json::Value::Null,
            error: Some("PRIVATE_OLD_PROFILE_ERROR".to_string()),
            error_code: None,
        });

        let error = query.await.unwrap().expect_err("stale result must fail");
        assert!(matches!(error, RigSourceError::Unavailable { .. }));
        assert!(!error.to_string().contains("PRIVATE_OLD_PROFILE_ERROR"));
        drop(replacement_outgoing);
    }

    #[tokio::test]
    async fn resource_not_ready_result_maps_through_hub_source() {
        let (connection, mut outgoing) = RigConnection::stub(7, Uuid::new_v4());
        let source = DirectRigSource::new(connection.clone());

        let query = tokio::spawn(async move { source.get_last_autofocus().await });
        let request = match outgoing.recv().await.expect("query request") {
            DirectMessage::Query(request) => request,
            other => panic!("expected query, got {other:?}"),
        };
        connection.resolve(QueryResult {
            id: request.id,
            ok: false,
            payload: serde_json::Value::Null,
            error: Some("autofocus report is still being published".to_string()),
            error_code: Some(QueryErrorCode::ResourceNotReady),
        });

        let error = query
            .await
            .unwrap()
            .expect_err("report should not be ready");
        assert!(matches!(
            error,
            RigSourceError::NotReady {
                kind: RigSourceKind::NinaDirect,
                reason,
            } if reason == "autofocus report is still being published"
        ));
    }
}
