//! Versioned messages exchanged between N.I.N.A. plugins and a Chatstronomy hub.

use crate::source::{RigCapabilities, RigCommand};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The first Chatstronomy Direct protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

/// Payload contract emitted by the first Direct v1 clients. Those clients did
/// not identify their payload contract in the hello frame, so an absent field
/// is deliberately classified as this legacy version.
pub const LEGACY_PAYLOAD_VERSION: u16 = 1;

/// Current additive payload contract carried inside Direct v1 envelopes.
///
/// This is separate from [`PROTOCOL_VERSION`]: the envelope and handshake stay
/// on Direct v1 while response objects gain optional Chatstronomy fields. A
/// future incompatible envelope still requires a new Direct protocol version.
///
/// | Version | Adds |
/// |---------|------|
/// | 1       | Direct v1 responses without a payload marker. |
/// | 2       | Sequence operation reporting and Hub image attachments. |
/// | 3       | `ChatEnabled` delivery flags, `TargetName`, and the `NINA-LOG` and `NINA-NOTIFICATION` event types. |
///
/// Peers negotiate the highest contract they both understand. Payload fields
/// remain additive, so readers must continue to default fields absent from an
/// older selected contract.
pub const CURRENT_PAYLOAD_VERSION: u16 = 3;

/// Payload contract that first carried the per-event delivery flags.
pub const DELIVERY_FLAG_PAYLOAD_VERSION: u16 = 3;
const _: () = assert!(DELIVERY_FLAG_PAYLOAD_VERSION > LEGACY_PAYLOAD_VERSION);
const _: () = assert!(DELIVERY_FLAG_PAYLOAD_VERSION <= CURRENT_PAYLOAD_VERSION);

/// Highest payload contract both peers understand.
///
/// A client advertising a newer contract than this build is clamped rather than
/// refused: the additive fields it sends are ignored, which is exactly what the
/// permissive serde defaults already do. Only a client below
/// [`LEGACY_PAYLOAD_VERSION`] is rejected outright.
pub fn negotiate_payload_version(client: u16) -> u16 {
    client.min(CURRENT_PAYLOAD_VERSION)
}

const fn legacy_payload_version() -> u16 {
    LEGACY_PAYLOAD_VERSION
}

/// Prefix for the per-installation Windows named pipe used by local plugins.
///
/// Windows APIs expect the pipe name without the `\\.\pipe\` prefix when
/// opening a `NamedPipeClientStream`.
pub const LOCAL_PIPE_PREFIX: &str = "chatstronomy-agent-v1";

/// Default route exposed by a remote Chatstronomy hub for Direct plugin clients.
pub const DIRECT_WEBSOCKET_PATH: &str = "/v1/direct";

/// Derive the local pipe name shared by Chatstronomy and every N.I.N.A. instance
/// in the same plugin installation. Including the node ID prevents collisions
/// between different Windows users or installations on one system.
pub fn local_pipe_name(node_id: Uuid) -> String {
    format!("{LOCAL_PIPE_PREFIX}-{}", node_id.simple())
}

/// Stable identity assigned to one N.I.N.A. profile on one host.
///
/// Profile GUIDs are only assumed to be unique within a node. Combining the
/// plugin's persistent node ID with the profile ID allows one Chatstronomy hub to
/// distinguish rigs on different systems without using addresses or ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RigId {
    pub node_id: Uuid,
    pub profile_id: Uuid,
}

impl std::fmt::Display for RigId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.node_id, self.profile_id)
    }
}

/// First message sent by each loaded Chatstronomy N.I.N.A. plugin instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u16,
    /// Version of the query-result payload contract. Missing on older clients,
    /// which the server explicitly marks as legacy v1.
    #[serde(default = "legacy_payload_version")]
    pub payload_version: u16,
    pub node_id: Uuid,
    pub session_id: Uuid,
    pub process_id: u32,
    pub profile_id: Uuid,
    pub profile_name: String,
    pub plugin_version: String,
    pub nina_version: String,
    pub capabilities: RigCapabilities,
}

/// Registration response returned by the Chatstronomy hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHello {
    pub protocol_version: u16,
    /// Payload contract selected for this connection. Older clients ignore the
    /// additive field; newer clients can display or diagnose legacy operation.
    #[serde(default = "legacy_payload_version")]
    pub payload_version: u16,
    pub connection_id: Uuid,
    pub rig_id: RigId,
}

/// First frame from a client that holds no credential yet: a one-time
/// pairing token minted on the hub web app plus the client's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub pairing_token: String,
    pub hello: ClientHello,
}

/// Successful pairing: the durable rig credential the client must store and
/// present on every later connection. This is the only time it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    pub credential: String,
    pub agent_hello: AgentHello,
}

/// First frame from a client that already holds a rig credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthRequest {
    pub credential: String,
    pub hello: ClientHello,
}

/// A read or command the hub asks the connected rig to perform. The rig
/// answers with a [`QueryResult`] carrying the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    pub id: Uuid,
    /// Unix time after which the rig must not execute this query. Guards
    /// against stale commands running after a hang or reconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(flatten)]
    pub kind: QueryKind,
}

/// Legacy clock-skew tolerance for read-only queries. Preserving this keeps
/// older Direct peers with imprecise clocks interoperable.
pub const EXPIRY_CLOCK_SKEW_GRACE_SECONDS: i64 = 120;

/// Hardware commands must never linger behind a stalled transport for two
/// minutes after the caller has timed out. Keep only enough skew tolerance
/// for ordinary, synchronized Windows clocks.
pub const COMMAND_EXPIRY_CLOCK_SKEW_GRACE_SECONDS: i64 = 5;

/// Current unix time in seconds. Both sides of the expiry contract use this
/// one definition. Returns 0 when the system clock is before the epoch,
/// which reads as "nothing is expired".
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl QueryRequest {
    /// True when the query must be rejected rather than executed. `now` is
    /// the executing side's clock. Reads retain legacy skew tolerance, while
    /// hardware commands expire promptly after the caller's deadline.
    pub fn expired_at(&self, now: i64) -> bool {
        let grace = if matches!(&self.kind, QueryKind::Command { .. }) {
            COMMAND_EXPIRY_CLOCK_SKEW_GRACE_SECONDS
        } else {
            EXPIRY_CLOCK_SKEW_GRACE_SECONDS
        };
        self.expires_at
            .is_some_and(|deadline| now > deadline.saturating_add(grace))
    }
}

/// The operations a rig can be asked to perform. These mirror the
/// [`crate::source::RigSource`] surface; each result payload is the JSON of
/// the corresponding response type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryKind {
    EventHistory,
    ImageHistory,
    Sequence,
    Thumbnail { index: u32 },
    LastAutofocus,
    MountInfo,
    CameraInfo,
    FilterwheelInfo,
    GuiderInfo,
    GuiderGraph,
    RotatorInfo,
    FocuserInfo,
    Command { command: RigCommand },
}

/// Answer to a [`QueryRequest`]. On success `payload` holds the JSON of the
/// response type matching the query kind; on failure `error` explains why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub id: Uuid,
    pub ok: bool,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// Direct protocol messages. The identity handshake (pair/auth → hello) is
/// stable; query and heartbeat frames flow after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DirectMessage {
    ClientHello(ClientHello),
    AgentHello(AgentHello),
    Pair(PairRequest),
    PairResult(PairResult),
    Auth(AuthRequest),
    Query(QueryRequest),
    QueryResult(QueryResult),
    Heartbeat {
        seq: u64,
    },
    HeartbeatAck {
        seq: u64,
    },
    /// Protocol or authentication error; the sender closes after this.
    /// `retryable` distinguishes transient conditions (rate limit, replaced
    /// connection) from ones where reconnecting can never help (bad
    /// credential). Absent on old hubs, which reads as fatal.
    Error {
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_client_hello() -> ClientHello {
        ClientHello {
            protocol_version: PROTOCOL_VERSION,
            payload_version: CURRENT_PAYLOAD_VERSION,
            node_id: Uuid::parse_str("363db028-9d79-4fdc-8940-1b1ff52b9e8d").unwrap(),
            session_id: Uuid::parse_str("7afcde18-b5a8-46fd-ad1f-ed54cf3bbc4e").unwrap(),
            process_id: 4242,
            profile_id: Uuid::parse_str("460a8c62-28ce-4781-92e5-ab2440982175").unwrap(),
            profile_name: "North Rig".to_string(),
            plugin_version: "0.1.0.0".to_string(),
            nina_version: "3.2.0.9001".to_string(),
            capabilities: RigCapabilities::none(),
        }
    }

    #[test]
    fn payload_negotiation_clamps_newer_clients_and_keeps_older_ones() {
        assert_eq!(
            negotiate_payload_version(LEGACY_PAYLOAD_VERSION),
            LEGACY_PAYLOAD_VERSION
        );
        assert_eq!(
            negotiate_payload_version(CURRENT_PAYLOAD_VERSION),
            CURRENT_PAYLOAD_VERSION
        );
        // A plugin from a future release must degrade to this build's contract
        // rather than be refused: its extra fields are additive.
        assert_eq!(
            negotiate_payload_version(CURRENT_PAYLOAD_VERSION + 7),
            CURRENT_PAYLOAD_VERSION
        );
    }

    #[test]
    fn delivery_flags_require_the_payload_version_that_introduced_them() {
        // Guards against bumping CURRENT_PAYLOAD_VERSION without deciding what
        // the new contract means for the delivery flags.
        assert!(
            negotiate_payload_version(DELIVERY_FLAG_PAYLOAD_VERSION - 1)
                < DELIVERY_FLAG_PAYLOAD_VERSION
        );
    }

    #[test]
    fn client_hello_has_stable_json_contract() {
        let message = DirectMessage::ClientHello(sample_client_hello());
        let value = serde_json::to_value(message).unwrap();

        assert_eq!(value["type"], "client_hello");
        assert_eq!(value["payload"]["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(value["payload"]["payload_version"], CURRENT_PAYLOAD_VERSION);
        assert_eq!(
            value["payload"]["node_id"],
            "363db028-9d79-4fdc-8940-1b1ff52b9e8d"
        );
        assert_eq!(value["payload"]["process_id"], 4242);
        assert_eq!(
            value["payload"]["profile_id"],
            "460a8c62-28ce-4781-92e5-ab2440982175"
        );
        assert_eq!(value["payload"]["profile_name"], "North Rig");
        assert_eq!(
            value["payload"]["capabilities"],
            json!({
                "event_history": false,
                "image_history": false,
                "thumbnails": false,
                "sequence": false,
                "equipment_snapshots": false,
                "autofocus_details": false,
                "guider_graph": false,
                "commands": false
            })
        );
    }

    #[test]
    fn query_request_has_stable_json_contract() {
        let message = DirectMessage::Query(QueryRequest {
            id: Uuid::parse_str("7afcde18-b5a8-46fd-ad1f-ed54cf3bbc4e").unwrap(),
            expires_at: None,
            kind: QueryKind::Thumbnail { index: 3 },
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["type"], "query");
        assert_eq!(value["payload"]["kind"], "thumbnail");
        assert_eq!(value["payload"]["index"], 3);
        let back: DirectMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn camera_info_query_uses_shared_equipment_wire_name() {
        let message = DirectMessage::Query(QueryRequest {
            id: Uuid::new_v4(),
            expires_at: None,
            kind: QueryKind::CameraInfo,
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["payload"]["kind"], "camera_info");
        assert_eq!(
            serde_json::from_value::<DirectMessage>(value).unwrap(),
            message
        );
    }

    #[test]
    fn command_query_roundtrips() {
        let message = DirectMessage::Query(QueryRequest {
            id: Uuid::new_v4(),
            expires_at: Some(1_900_000_000),
            kind: QueryKind::Command {
                command: RigCommand::StartSequence {
                    skip_validation: true,
                },
            },
        });
        let json = serde_json::to_string(&message).unwrap();
        let back: DirectMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
        assert!(json.contains("start_sequence"));
        assert!(!json.contains("/sequence/start"));
    }

    #[test]
    fn query_result_defaults_payload_and_error() {
        let json = r#"{"type":"query_result","payload":{"id":"7afcde18-b5a8-46fd-ad1f-ed54cf3bbc4e","ok":true}}"#;
        let message: DirectMessage = serde_json::from_str(json).unwrap();
        let DirectMessage::QueryResult(result) = message else {
            panic!("expected QueryResult");
        };
        assert!(result.ok);
        assert!(result.payload.is_null());
        assert!(result.error.is_none());
    }

    #[test]
    fn unmarked_v1_hello_is_classified_as_legacy_payloads() {
        let message: DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/client-hello-legacy.json"
        ))
        .unwrap();
        let DirectMessage::ClientHello(hello) = message else {
            panic!("expected client hello");
        };
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(hello.payload_version, LEGACY_PAYLOAD_VERSION);
    }

    #[test]
    fn old_agent_hello_without_payload_marker_remains_accepted() {
        let json = r#"{"type":"agent_hello","payload":{"protocol_version":1,"connection_id":"7afcde18-b5a8-46fd-ad1f-ed54cf3bbc4e","rig_id":{"node_id":"363db028-9d79-4fdc-8940-1b1ff52b9e8d","profile_id":"460a8c62-28ce-4781-92e5-ab2440982175"}}}"#;
        let message: DirectMessage = serde_json::from_str(json).unwrap();
        let DirectMessage::AgentHello(hello) = message else {
            panic!("expected agent hello");
        };
        assert_eq!(hello.payload_version, LEGACY_PAYLOAD_VERSION);
    }

    #[test]
    fn query_expiry_semantics() {
        let no_deadline = QueryRequest {
            id: Uuid::new_v4(),
            expires_at: None,
            kind: QueryKind::EventHistory,
        };
        assert!(!no_deadline.expired_at(i64::MAX));

        let with_deadline = QueryRequest {
            id: Uuid::new_v4(),
            expires_at: Some(100),
            kind: QueryKind::EventHistory,
        };
        // Within the deadline plus skew grace: executes.
        assert!(!with_deadline.expired_at(100));
        assert!(!with_deadline.expired_at(100 + EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
        // Past deadline plus grace: rejected.
        assert!(with_deadline.expired_at(101 + EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
    }

    #[test]
    fn hardware_commands_expire_promptly_but_reads_keep_legacy_skew_grace() {
        let command = QueryRequest {
            id: Uuid::new_v4(),
            expires_at: Some(100),
            kind: QueryKind::Command {
                command: RigCommand::ParkMount,
            },
        };
        assert!(!command.expired_at(100));
        assert!(!command.expired_at(100 + COMMAND_EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
        assert!(command.expired_at(101 + COMMAND_EXPIRY_CLOCK_SKEW_GRACE_SECONDS));

        let read = QueryRequest {
            kind: QueryKind::MountInfo,
            ..command
        };
        assert!(!read.expired_at(101 + COMMAND_EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
        assert!(!read.expired_at(100 + EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
        assert!(read.expired_at(101 + EXPIRY_CLOCK_SKEW_GRACE_SECONDS));
    }

    #[test]
    fn query_deadlines_cannot_overflow_when_clock_skew_is_added() {
        let request = QueryRequest {
            id: Uuid::new_v4(),
            expires_at: Some(i64::MAX),
            kind: QueryKind::Command {
                command: RigCommand::ParkMount,
            },
        };
        assert!(!request.expired_at(i64::MAX));
    }

    #[test]
    fn error_retryable_defaults_to_fatal() {
        // A frame from an older hub without the field parses as fatal.
        let json = r#"{"type":"error","payload":{"message":"nope"}}"#;
        let message: DirectMessage = serde_json::from_str(json).unwrap();
        let DirectMessage::Error { retryable, .. } = message else {
            panic!("expected Error");
        };
        assert!(!retryable);
    }

    #[test]
    fn pair_and_auth_roundtrip() {
        let pair = DirectMessage::Pair(PairRequest {
            pairing_token: "cspt_abc".to_string(),
            hello: sample_client_hello(),
        });
        let back: DirectMessage =
            serde_json::from_str(&serde_json::to_string(&pair).unwrap()).unwrap();
        assert_eq!(back, pair);

        let auth = DirectMessage::Auth(AuthRequest {
            credential: "csrc_def".to_string(),
            hello: sample_client_hello(),
        });
        let value = serde_json::to_value(&auth).unwrap();
        assert_eq!(value["type"], "auth");
        assert_eq!(value["payload"]["credential"], "csrc_def");
    }

    #[test]
    fn rig_id_display_is_stable_and_unambiguous() {
        let rig_id = RigId {
            node_id: Uuid::parse_str("363db028-9d79-4fdc-8940-1b1ff52b9e8d").unwrap(),
            profile_id: Uuid::parse_str("460a8c62-28ce-4781-92e5-ab2440982175").unwrap(),
        };

        assert_eq!(
            rig_id.to_string(),
            "363db028-9d79-4fdc-8940-1b1ff52b9e8d/460a8c62-28ce-4781-92e5-ab2440982175"
        );
    }

    #[test]
    fn local_pipe_name_is_scoped_to_the_node() {
        let node_id = Uuid::parse_str("363db028-9d79-4fdc-8940-1b1ff52b9e8d").unwrap();

        assert_eq!(
            local_pipe_name(node_id),
            "chatstronomy-agent-v1-363db0289d794fdc89401b1ff52b9e8d"
        );
    }

    #[test]
    fn published_v1_fixtures_are_accepted_by_the_normative_implementation() {
        let fixtures = [
            include_str!("../../contracts/direct/v1/fixtures/client-hello.json"),
            include_str!("../../contracts/direct/v1/fixtures/client-hello-legacy.json"),
            include_str!("../../contracts/direct/v1/fixtures/pair.json"),
            include_str!("../../contracts/direct/v1/fixtures/query-guider-graph.json"),
            include_str!("../../contracts/direct/v1/fixtures/query-command.json"),
            include_str!("../../contracts/direct/v1/fixtures/query-result.json"),
            include_str!("../../contracts/direct/v1/fixtures/heartbeat.json"),
            include_str!("../../contracts/direct/v1/fixtures/error.json"),
        ];

        for fixture in fixtures {
            let message: DirectMessage = serde_json::from_str(fixture).unwrap();
            serde_json::to_string(&message).unwrap();
        }

        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../contracts/direct/v1/schema.json")).unwrap();
        assert_eq!(schema["title"], "Chatstronomy Direct protocol v1");
    }
}
