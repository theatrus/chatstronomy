use crate::serde_helpers::{de_f64_tolerant, de_i32_tolerant, de_string_tolerant};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Map, Value};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EventHistoryResponse {
    pub response: Vec<Event>,
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Event {
    pub time: String,
    pub event: String,
    #[serde(default = "default_true")]
    pub chat_enabled: bool,
    #[serde(flatten)]
    pub details: Option<EventDetails>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum EventDetails {
    FilterWheelChange {
        #[serde(rename = "New")]
        new: FilterInfo,
        #[serde(rename = "Previous")]
        previous: FilterInfo,
    },
    /// Target Scheduler start. Only `TargetName` is guaranteed: the plugin
    /// reads the rest from optional broker custom headers, and a target with
    /// no coordinates, project, or end time omits them. `TargetName` stays
    /// required because it is what distinguishes this variant from the other
    /// untagged arms — do not make it optional.
    TargetStart {
        #[serde(rename = "TargetName", deserialize_with = "de_string_tolerant")]
        target_name: String,
        #[serde(rename = "ProjectName", default)]
        project_name: Option<String>,
        #[serde(rename = "Rotation", default)]
        rotation: Option<f64>,
        #[serde(rename = "TargetEndTime", default)]
        target_end_time: Option<String>,
        #[serde(rename = "Coordinates", default)]
        coordinates: Option<TargetCoordinates>,
    },
    WaitStart {
        #[serde(rename = "WaitEndTime")]
        wait_end_time: String,
    },
    /// Autofocus measurement point. NINA emits these in flurries (~one per
    /// step) during an autofocus run, each carrying the focuser position
    /// and the half-flux radius measured at that position.
    AutofocusPointAdded {
        #[serde(rename = "Position")]
        position: i32,
        #[serde(rename = "HFR")]
        hfr: f64,
    },
    /// A completed autofocus callback. The report timestamp is required to
    /// keep this untagged arm distinct, while the other fields let newer
    /// plugins identify the exact report without breaking older runtimes.
    AutofocusFinished {
        #[serde(rename = "ReportTimestamp")]
        report_timestamp: String,
        #[serde(rename = "Filter", default)]
        filter: Option<String>,
        #[serde(rename = "Position", default)]
        position: Option<f64>,
        #[serde(rename = "Temperature", default)]
        temperature: Option<f64>,
    },
    /// Authoritative state from N.I.N.A.'s configured safety monitor.
    SafetyChanged {
        #[serde(rename = "IsSafe")]
        is_safe: bool,
    },
    /// Rotator moved. Emitted for both ROTATOR-MOVED and
    /// ROTATOR-MOVED-MECHANICAL — both share `{From, To}` in degrees.
    RotatorMoved {
        #[serde(rename = "From")]
        from: f64,
        #[serde(rename = "To")]
        to: f64,
    },
    /// A completed telescope slew. Coordinates are deliberately kept in a
    /// compact wire type instead of serializing N.I.N.A. objects.
    MountSlewed {
        #[serde(rename = "From")]
        from: EventCoordinates,
        #[serde(rename = "To")]
        to: EventCoordinates,
    },
    /// A completed dome azimuth slew, in degrees.
    DomeSlewed {
        #[serde(rename = "From")]
        from: f64,
        #[serde(rename = "To")]
        to: f64,
    },
    SequenceEntityFailed {
        #[serde(rename = "Entity")]
        entity: String,
        #[serde(rename = "EntityType")]
        entity_type: String,
        #[serde(rename = "Error")]
        error: String,
    },
    SequenceFinished {
        #[serde(rename = "Outcome")]
        outcome: String,
        #[serde(rename = "Status")]
        status: String,
        #[serde(rename = "HadFailures")]
        had_failures: bool,
    },
    ImageSaveFailed {
        #[serde(rename = "Stage")]
        stage: String,
        #[serde(rename = "DiskFull")]
        disk_full: bool,
        #[serde(rename = "Error")]
        error: String,
    },
    FlatBrightnessChanged {
        #[serde(rename = "Previous")]
        previous: i32,
        #[serde(rename = "New")]
        new: i32,
    },
    FlatLightToggled {
        #[serde(rename = "On")]
        on: Option<bool>,
    },
    /// An asynchronously accepted hardware command later failed in N.I.N.A.
    /// Both fields are required so the untagged shape cannot swallow another
    /// event's details.
    CommandFailed {
        #[serde(rename = "Command")]
        command: String,
        #[serde(rename = "Error")]
        error: String,
    },
    NinaNotification {
        #[serde(rename = "Level")]
        level: String,
        #[serde(rename = "Header")]
        header: String,
        #[serde(rename = "Message")]
        message: String,
    },
    NinaLog {
        #[serde(rename = "Level")]
        level: String,
        #[serde(rename = "Source")]
        source: String,
        #[serde(rename = "Member")]
        member: String,
        #[serde(rename = "Line")]
        line: i32,
        #[serde(rename = "Message")]
        message: String,
    },
    /// Fields from an event name this version does not understand. Keeping
    /// them round-trippable makes mixed-version Hub connections tolerant
    /// without guessing a typed shape from coincidentally matching keys.
    Unknown(Map<String, Value>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct EventCoordinates {
    #[serde(rename = "RA", default)]
    pub ra: Option<f64>,
    #[serde(rename = "RADegrees", default)]
    pub ra_degrees: Option<f64>,
    #[serde(rename = "RAString", default)]
    pub ra_string: Option<String>,
    #[serde(rename = "Dec", default)]
    pub dec: Option<f64>,
    #[serde(rename = "DecString", default)]
    pub dec_string: Option<String>,
    #[serde(rename = "Epoch", default)]
    pub epoch: Option<String>,
    #[serde(rename = "Altitude", default)]
    pub altitude: Option<f64>,
    #[serde(rename = "Azimuth", default)]
    pub azimuth: Option<f64>,
}

impl EventCoordinates {
    pub fn display(&self) -> String {
        if self.altitude.is_some() || self.azimuth.is_some() {
            return format!(
                "Alt {} · Az {}",
                self.altitude
                    .map(|value| format!("{value:.2}°"))
                    .unwrap_or_else(|| "--".to_string()),
                self.azimuth
                    .map(|value| format!("{value:.2}°"))
                    .unwrap_or_else(|| "--".to_string())
            );
        }

        let ra = self
            .ra_string
            .clone()
            .or_else(|| self.ra.map(|value| format!("{value:.5} h")))
            .or_else(|| self.ra_degrees.map(|value| format!("{value:.5}°")))
            .unwrap_or_else(|| "--".to_string());
        let dec = self
            .dec_string
            .clone()
            .or_else(|| self.dec.map(|value| format!("{value:.5}°")))
            .unwrap_or_else(|| "--".to_string());
        match self.epoch.as_deref() {
            Some(epoch) if !epoch.is_empty() => format!("RA {ra} · Dec {dec} ({epoch})"),
            _ => format!("RA {ra} · Dec {dec}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawEvent {
    time: String,
    event: String,
    #[serde(default = "default_true")]
    chat_enabled: bool,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

macro_rules! wire_details {
    ($name:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct $name { $( $field: $ty ),+ }
    };
}

wire_details!(FilterWheelChangeWire {
    new: FilterInfo,
    previous: FilterInfo
});
wire_details!(TargetStartWire {
    target_name: String,
    project_name: Option<String>,
    rotation: Option<f64>,
    target_end_time: Option<String>,
    coordinates: Option<TargetCoordinates>,
});
wire_details!(WaitStartWire {
    wait_end_time: String
});
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AutofocusPointAddedWire {
    position: i32,
    #[serde(rename = "HFR")]
    hfr: f64,
}
wire_details!(AutofocusFinishedWire {
    report_timestamp: String,
    filter: Option<String>,
    position: Option<f64>,
    temperature: Option<f64>,
});
wire_details!(SafetyChangedWire { is_safe: bool });
wire_details!(NumericMoveWire { from: f64, to: f64 });
wire_details!(MountSlewedWire {
    from: EventCoordinates,
    to: EventCoordinates
});
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DomeSlewedWire {
    #[serde(alias = "From")]
    from_azimuth: f64,
    #[serde(alias = "To")]
    to_azimuth: f64,
}
wire_details!(SequenceEntityFailedWire {
    entity: String,
    entity_type: String,
    error: String
});
wire_details!(SequenceFinishedWire {
    outcome: String,
    status: String,
    had_failures: bool
});
wire_details!(ImageSaveFailedWire {
    stage: String,
    disk_full: bool,
    error: String
});
wire_details!(FlatBrightnessChangedWire {
    previous: i32,
    new: i32
});
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FlatLightToggledWire {
    #[serde(default)]
    on: Option<bool>,
}
wire_details!(CommandFailedWire {
    command: String,
    error: String
});
wire_details!(NinaNotificationWire {
    level: String,
    header: String,
    message: String
});
wire_details!(NinaLogWire {
    level: String,
    source: String,
    member: String,
    line: i32,
    message: String
});

fn decode_wire<T: for<'de> Deserialize<'de>>(fields: &Map<String, Value>) -> Option<T> {
    serde_json::from_value(Value::Object(fields.clone())).ok()
}

fn unknown_details(fields: Map<String, Value>) -> Option<EventDetails> {
    (!fields.is_empty()).then_some(EventDetails::Unknown(fields))
}

fn decode_event_details(event: &str, fields: Map<String, Value>) -> Option<EventDetails> {
    let details = match event {
        event_types::FILTERWHEEL_CHANGED => {
            decode_wire::<FilterWheelChangeWire>(&fields).map(|wire| {
                EventDetails::FilterWheelChange {
                    new: wire.new,
                    previous: wire.previous,
                }
            })
        }
        event_types::TS_TARGETSTART | event_types::TS_NEWTARGETSTART => {
            decode_wire::<TargetStartWire>(&fields).map(|wire| EventDetails::TargetStart {
                target_name: wire.target_name,
                project_name: wire.project_name,
                rotation: wire.rotation,
                target_end_time: wire.target_end_time,
                coordinates: wire.coordinates,
            })
        }
        event_types::TS_WAITSTART => {
            decode_wire::<WaitStartWire>(&fields).map(|wire| EventDetails::WaitStart {
                wait_end_time: wire.wait_end_time,
            })
        }
        event_types::AUTOFOCUS_POINT_ADDED => {
            decode_wire::<AutofocusPointAddedWire>(&fields).map(|wire| {
                EventDetails::AutofocusPointAdded {
                    position: wire.position,
                    hfr: wire.hfr,
                }
            })
        }
        event_types::AUTOFOCUS_FINISHED => {
            decode_wire::<AutofocusFinishedWire>(&fields).map(|wire| {
                EventDetails::AutofocusFinished {
                    report_timestamp: wire.report_timestamp,
                    filter: wire.filter,
                    position: wire.position,
                    temperature: wire.temperature,
                }
            })
        }
        event_types::SAFETY_CHANGED => {
            decode_wire::<SafetyChangedWire>(&fields).map(|wire| EventDetails::SafetyChanged {
                is_safe: wire.is_safe,
            })
        }
        event_types::ROTATOR_MOVED | event_types::ROTATOR_MOVED_MECHANICAL => {
            decode_wire::<NumericMoveWire>(&fields).map(|wire| EventDetails::RotatorMoved {
                from: wire.from,
                to: wire.to,
            })
        }
        event_types::MOUNT_SLEWED => {
            decode_wire::<MountSlewedWire>(&fields).map(|wire| EventDetails::MountSlewed {
                from: wire.from,
                to: wire.to,
            })
        }
        event_types::DOME_SLEWED => {
            decode_wire::<DomeSlewedWire>(&fields).map(|wire| EventDetails::DomeSlewed {
                from: wire.from_azimuth,
                to: wire.to_azimuth,
            })
        }
        event_types::SEQUENCE_ENTITY_FAILED => decode_wire::<SequenceEntityFailedWire>(&fields)
            .map(|wire| EventDetails::SequenceEntityFailed {
                entity: wire.entity,
                entity_type: wire.entity_type,
                error: wire.error,
            }),
        event_types::SEQUENCE_FINISHED => {
            decode_wire::<SequenceFinishedWire>(&fields).map(|wire| {
                EventDetails::SequenceFinished {
                    outcome: wire.outcome,
                    status: wire.status,
                    had_failures: wire.had_failures,
                }
            })
        }
        event_types::IMAGE_SAVE_FAILED => {
            decode_wire::<ImageSaveFailedWire>(&fields).map(|wire| EventDetails::ImageSaveFailed {
                stage: wire.stage,
                disk_full: wire.disk_full,
                error: wire.error,
            })
        }
        event_types::FLAT_BRIGHTNESS_CHANGED => decode_wire::<FlatBrightnessChangedWire>(&fields)
            .map(|wire| EventDetails::FlatBrightnessChanged {
                previous: wire.previous,
                new: wire.new,
            }),
        event_types::FLAT_LIGHT_TOGGLED => decode_wire::<FlatLightToggledWire>(&fields)
            .map(|wire| EventDetails::FlatLightToggled { on: wire.on }),
        event_types::CHATSTRONOMY_COMMAND_FAILED => {
            decode_wire::<CommandFailedWire>(&fields).map(|wire| EventDetails::CommandFailed {
                command: wire.command,
                error: wire.error,
            })
        }
        event_types::NINA_NOTIFICATION => {
            decode_wire::<NinaNotificationWire>(&fields).map(|wire| {
                EventDetails::NinaNotification {
                    level: wire.level,
                    header: wire.header,
                    message: wire.message,
                }
            })
        }
        event_types::NINA_LOG => {
            decode_wire::<NinaLogWire>(&fields).map(|wire| EventDetails::NinaLog {
                level: wire.level,
                source: wire.source,
                member: wire.member,
                line: wire.line,
                message: wire.message,
            })
        }
        _ => None,
    };

    details.or_else(|| unknown_details(fields))
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEvent::deserialize(deserializer).map_err(D::Error::custom)?;
        Ok(Self {
            details: decode_event_details(&raw.event, raw.fields),
            time: raw.time,
            event: raw.event,
            chat_enabled: raw.chat_enabled,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FilterInfo {
    // NINA occasionally returns Name:[] / Id:[] (empty arrays) when the slot is unknown.
    // Tolerant deserializers fall back to empty/-1 sentinels (see serde_helpers).
    #[serde(deserialize_with = "de_string_tolerant")]
    pub name: String,
    #[serde(deserialize_with = "de_i32_tolerant")]
    pub id: i32,
}

impl FilterInfo {
    pub fn is_unknown(&self) -> bool {
        self.name.is_empty() || self.id < 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TargetCoordinates {
    // TS-TARGETSTART events on c925 (and likely elsewhere) sometimes ship
    // every Coordinates field as an empty array when the target lacks
    // coords — same NINA quirk that produces empty `FilterInfo`. Each
    // field accepts `[]` and falls back to a sentinel; the whole struct
    // remains parseable so the target name + project still survive.
    #[serde(rename = "RA", deserialize_with = "de_f64_tolerant")]
    pub ra: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub dec: f64,
    #[serde(rename = "RAString", deserialize_with = "de_string_tolerant")]
    pub ra_string: String,
    #[serde(deserialize_with = "de_string_tolerant")]
    pub dec_string: String,
    #[serde(deserialize_with = "de_string_tolerant")]
    pub epoch: String,
    #[serde(rename = "RADegrees", deserialize_with = "de_f64_tolerant")]
    pub ra_degrees: f64,
}

impl TargetCoordinates {
    /// True when every coord field came back as the empty-array sentinel
    /// (NINA's "unknown" shape). Display sites should suppress the
    /// Coordinates field in this case.
    pub fn is_unknown(&self) -> bool {
        self.ra_string.is_empty() && self.dec_string.is_empty() && self.ra.is_nan()
    }

    /// `"RA: ...\nDec: ..."` if the coords are known, otherwise None.
    pub fn display(&self) -> Option<String> {
        if self.is_unknown() {
            None
        } else {
            Some(format!("RA: {}\nDec: {}", self.ra_string, self.dec_string))
        }
    }
}

// Event type constants emitted by the N.I.N.A. plugin's Direct projector.
pub mod event_types {
    pub const CAMERA_DISCONNECTED: &str = "CAMERA-DISCONNECTED";
    pub const CAMERA_CONNECTED: &str = "CAMERA-CONNECTED";
    pub const CAMERA_DOWNLOAD_TIMEOUT: &str = "CAMERA-DOWNLOAD-TIMEOUT";
    pub const FILTERWHEEL_DISCONNECTED: &str = "FILTERWHEEL-DISCONNECTED";
    pub const FILTERWHEEL_CONNECTED: &str = "FILTERWHEEL-CONNECTED";
    pub const FILTERWHEEL_CHANGED: &str = "FILTERWHEEL-CHANGED";
    pub const MOUNT_DISCONNECTED: &str = "MOUNT-DISCONNECTED";
    pub const MOUNT_CONNECTED: &str = "MOUNT-CONNECTED";
    pub const MOUNT_UNPARKED: &str = "MOUNT-UNPARKED";
    pub const MOUNT_PARKED: &str = "MOUNT-PARKED";
    pub const MOUNT_BEFORE_FLIP: &str = "MOUNT-BEFORE-FLIP";
    pub const MOUNT_AFTER_FLIP: &str = "MOUNT-AFTER-FLIP";
    pub const MOUNT_HOMED: &str = "MOUNT-HOMED";
    pub const MOUNT_CENTER: &str = "MOUNT-CENTER";
    pub const MOUNT_SLEWED: &str = "MOUNT-SLEWED";
    pub const FOCUSER_DISCONNECTED: &str = "FOCUSER-DISCONNECTED";
    pub const FOCUSER_CONNECTED: &str = "FOCUSER-CONNECTED";
    pub const FOCUSER_USER_FOCUSED: &str = "FOCUSER-USER-FOCUSED";
    pub const ROTATOR_DISCONNECTED: &str = "ROTATOR-DISCONNECTED";
    pub const ROTATOR_CONNECTED: &str = "ROTATOR-CONNECTED";
    pub const ROTATOR_MOVED: &str = "ROTATOR-MOVED";
    pub const ROTATOR_MOVED_MECHANICAL: &str = "ROTATOR-MOVED-MECHANICAL";
    pub const ROTATOR_SYNCED: &str = "ROTATOR-SYNCED";
    pub const GUIDER_CONNECTED: &str = "GUIDER-CONNECTED";
    pub const GUIDER_DISCONNECTED: &str = "GUIDER-DISCONNECTED";
    pub const GUIDER_START: &str = "GUIDER-START";
    pub const GUIDER_STOP: &str = "GUIDER-STOP";
    pub const GUIDER_DITHER: &str = "GUIDER-DITHER";
    pub const FLAT_CONNECTED: &str = "FLAT-CONNECTED";
    pub const FLAT_DISCONNECTED: &str = "FLAT-DISCONNECTED";
    pub const FLAT_BRIGHTNESS_CHANGED: &str = "FLAT-BRIGHTNESS-CHANGED";
    pub const FLAT_LIGHT_TOGGLED: &str = "FLAT-LIGHT-TOGGLED";
    pub const FLAT_COVER_OPENED: &str = "FLAT-COVER-OPENED";
    pub const FLAT_COVER_CLOSED: &str = "FLAT-COVER-CLOSED";
    pub const WEATHER_CONNECTED: &str = "WEATHER-CONNECTED";
    pub const WEATHER_DISCONNECTED: &str = "WEATHER-DISCONNECTED";
    pub const SWITCH_CONNECTED: &str = "SWITCH-CONNECTED";
    pub const SWITCH_DISCONNECTED: &str = "SWITCH-DISCONNECTED";
    pub const DOME_CONNECTED: &str = "DOME-CONNECTED";
    pub const DOME_DISCONNECTED: &str = "DOME-DISCONNECTED";
    pub const DOME_SHUTTER_OPENED: &str = "DOME-SHUTTER-OPENED";
    pub const DOME_SHUTTER_CLOSED: &str = "DOME-SHUTTER-CLOSED";
    pub const DOME_HOMED: &str = "DOME-HOMED";
    pub const DOME_PARKED: &str = "DOME-PARKED";
    pub const DOME_SLEWED: &str = "DOME-SLEWED";
    pub const DOME_SYNCED: &str = "DOME-SYNCED";
    pub const SAFETY_CONNECTED: &str = "SAFETY-CONNECTED";
    pub const SAFETY_DISCONNECTED: &str = "SAFETY-DISCONNECTED";
    pub const SAFETY_CHANGED: &str = "SAFETY-CHANGED";
    pub const IMAGE_SAVE: &str = "IMAGE-SAVE";
    pub const IMAGE_SAVE_FAILED: &str = "IMAGE-SAVE-FAILED";
    pub const IMAGE_PREPARED: &str = "IMAGE-PREPARED";
    pub const API_CAPTURE_FINISHED: &str = "API-CAPTURE-FINISHED";
    pub const AUTOFOCUS_STARTING: &str = "AUTOFOCUS-STARTING";
    pub const AUTOFOCUS_FINISHED: &str = "AUTOFOCUS-FINISHED";
    pub const AUTOFOCUS_POINT_ADDED: &str = "AUTOFOCUS-POINT-ADDED";
    pub const ERROR_AF: &str = "ERROR-AF";
    pub const ERROR_PLATESOLVE: &str = "ERROR-PLATESOLVE";
    pub const SEQUENCE_STARTING: &str = "SEQUENCE-STARTING";
    pub const SEQUENCE_FINISHED: &str = "SEQUENCE-FINISHED";
    pub const SEQUENCE_ENTITY_FAILED: &str = "SEQUENCE-ENTITY-FAILED";
    pub const TS_TARGETSTART: &str = "TS-TARGETSTART";
    pub const TS_NEWTARGETSTART: &str = "TS-NEWTARGETSTART";
    pub const TS_WAITSTART: &str = "TS-WAITSTART";
    pub const NINA_NOTIFICATION: &str = "NINA-NOTIFICATION";
    pub const NINA_LOG: &str = "NINA-LOG";
    pub const CHATSTRONOMY_COMMAND_FAILED: &str = "CHATSTRONOMY-COMMAND-FAILED";
}

/// The local N.I.N.A. delivery switch that governs an event. Keeping this
/// classification alongside the wire event names lets legacy payload-v3
/// consumers treat `ChatEnabled: false` as a category revocation without
/// inspecting or retaining the disabled event's details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EventDeliveryScope {
    Images,
    Autofocus,
    Guiding,
    Mount,
    Sequence,
    Safety,
    TargetScheduler,
    FilterFocuserRotator,
    EquipmentConnections,
    Observatory,
    CommandFailures,
    NinaNotifications,
    NinaLogs,
    Other,
}

pub(crate) fn event_delivery_scope(event: &str) -> EventDeliveryScope {
    if event == event_types::NINA_NOTIFICATION {
        return EventDeliveryScope::NinaNotifications;
    }
    if event == event_types::NINA_LOG {
        return EventDeliveryScope::NinaLogs;
    }
    if event.starts_with("TS-") {
        return EventDeliveryScope::TargetScheduler;
    }
    if event.starts_with("SEQUENCE-") {
        return EventDeliveryScope::Sequence;
    }
    if event.starts_with("SAFETY-") {
        return EventDeliveryScope::Safety;
    }
    if event == event_types::CHATSTRONOMY_COMMAND_FAILED {
        return EventDeliveryScope::CommandFailures;
    }
    if event.starts_with("IMAGE-")
        || event == event_types::API_CAPTURE_FINISHED
        || event == event_types::CAMERA_DOWNLOAD_TIMEOUT
    {
        return EventDeliveryScope::Images;
    }
    if event.starts_with("AUTOFOCUS-") || event == event_types::ERROR_AF {
        return EventDeliveryScope::Autofocus;
    }
    if event.ends_with("-CONNECTED") || event.ends_with("-DISCONNECTED") {
        return EventDeliveryScope::EquipmentConnections;
    }
    if event.starts_with("GUIDER-") {
        return EventDeliveryScope::Guiding;
    }
    if event.starts_with("MOUNT-") || event == event_types::ERROR_PLATESOLVE {
        return EventDeliveryScope::Mount;
    }
    if event.starts_with("FILTERWHEEL-")
        || event.starts_with("ROTATOR-")
        || event.starts_with("FOCUSER-")
    {
        return EventDeliveryScope::FilterFocuserRotator;
    }
    if event.starts_with("DOME-") || event.starts_with("FLAT-") {
        return EventDeliveryScope::Observatory;
    }
    EventDeliveryScope::Other
}

impl EventHistoryResponse {
    /// Get all events of a specific type
    pub fn get_events_by_type(&self, event_type: &str) -> Vec<&Event> {
        self.response
            .iter()
            .filter(|event| event.event == event_type)
            .collect()
    }

    /// Get all filter wheel change events
    pub fn get_filterwheel_changes(&self) -> Vec<&Event> {
        self.get_events_by_type(event_types::FILTERWHEEL_CHANGED)
    }

    /// Get all image save events
    pub fn get_image_saves(&self) -> Vec<&Event> {
        self.get_events_by_type(event_types::IMAGE_SAVE)
    }

    /// Get connection events (connected/disconnected)
    pub fn get_connection_events(&self) -> Vec<&Event> {
        self.response
            .iter()
            .filter(|event| {
                event.event.ends_with("-CONNECTED") || event.event.ends_with("-DISCONNECTED")
            })
            .collect()
    }

    /// Count events by type
    pub fn count_events_by_type(&self) -> std::collections::HashMap<String, usize> {
        let mut counts = std::collections::HashMap::new();
        for event in &self.response {
            *counts.entry(event.event.clone()).or_insert(0) += 1;
        }
        counts
    }
}

impl Event {
    /// Check if this is a connection event
    pub fn is_connection_event(&self) -> bool {
        self.event.ends_with("-CONNECTED") || self.event.ends_with("-DISCONNECTED")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_failures_preserve_the_operation_and_error_details() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-24T04:00:00Z",
            "Event": "CHATSTRONOMY-COMMAND-FAILED",
            "ChatEnabled": true,
            "Command": "Start sequence",
            "Error": "Sequence validation failed",
        }))
        .unwrap();

        assert_eq!(event.event, event_types::CHATSTRONOMY_COMMAND_FAILED);
        match event.details {
            Some(EventDetails::CommandFailed { command, error }) => {
                assert_eq!(command, "Start sequence");
                assert_eq!(error, "Sequence validation failed");
            }
            details => panic!("command failure was not preserved: {details:?}"),
        }
    }

    #[test]
    fn test_event_parsing() {
        let event_json = r#"{
            "Time": "2025-08-06T21:50:56.545923-07:00",
            "Event": "AUTOFOCUS-FINISHED"
        }"#;

        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.time, "2025-08-06T21:50:56.545923-07:00");
        assert_eq!(event.event, event_types::AUTOFOCUS_FINISHED);
        assert!(event.chat_enabled);
        assert!(event.details.is_none());
    }

    #[test]
    fn native_autofocus_completion_details_parse_without_breaking_legacy_events() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-25T04:15:01Z",
            "Event": "AUTOFOCUS-FINISHED",
            "ChatEnabled": true,
            "Filter": "L",
            "Position": 4068.0,
            "Temperature": -8.5,
            "ReportTimestamp": "2026-08-25T04:15:00Z"
        }))
        .unwrap();

        assert!(matches!(
            event.details,
            Some(EventDetails::AutofocusFinished {
                report_timestamp,
                filter: Some(filter),
                position: Some(position),
                temperature: Some(temperature),
            }) if report_timestamp == "2026-08-25T04:15:00Z"
                && filter == "L"
                && position == 4068.0
                && temperature == -8.5
        ));
    }

    #[test]
    fn native_safety_state_details_parse() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-25T04:20:00Z",
            "Event": "SAFETY-CHANGED",
            "ChatEnabled": true,
            "IsSafe": false
        }))
        .unwrap();

        assert!(matches!(
            event.details,
            Some(EventDetails::SafetyChanged { is_safe: false })
        ));
    }

    #[test]
    fn direct_delivery_flag_and_nina_log_details_parse() {
        let event: Event = serde_json::from_str(
            r#"{
                "Time": "2026-08-16T12:00:00-07:00",
                "Event": "NINA-LOG",
                "ChatEnabled": false,
                "Level": "WARNING",
                "Source": "CameraVM.cs",
                "Member": "Connect",
                "Line": 42,
                "Message": "Camera connection is slow"
            }"#,
        )
        .unwrap();

        assert!(!event.chat_enabled);
        assert!(matches!(
            event.details,
            Some(EventDetails::NinaLog {
                level,
                source,
                line: 42,
                ..
            }) if level == "WARNING" && source == "CameraVM.cs"
        ));
    }

    #[test]
    fn test_filterwheel_change_event() {
        let event_json = r#"{
            "Time": "2025-08-06T19:18:09.4045633-07:00",
            "New": {"Name": "HA", "Id": 0},
            "Previous": {"Name": "OIII", "Id": 1},
            "Event": "FILTERWHEEL-CHANGED"
        }"#;

        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.event, event_types::FILTERWHEEL_CHANGED);

        if let Some(EventDetails::FilterWheelChange { new, previous }) = event.details {
            assert_eq!(new.name, "HA");
            assert_eq!(new.id, 0);
            assert_eq!(previous.name, "OIII");
            assert_eq!(previous.id, 1);
        } else {
            panic!("Expected FilterWheelChange details");
        }
    }

    #[test]
    fn test_event_methods() {
        let camera_connected = Event {
            time: "2025-08-06T18:45:40.1430956-07:00".to_string(),
            event: event_types::CAMERA_CONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        };
        assert!(camera_connected.is_connection_event());

        let mount_disconnected = Event {
            time: "2025-08-06T19:20:35.2068582-07:00".to_string(),
            event: event_types::MOUNT_DISCONNECTED.to_string(),
            chat_enabled: true,
            details: None,
        };
        assert!(mount_disconnected.is_connection_event());
    }

    #[test]
    fn test_event_history_methods() {
        let events_json = r#"{
            "Response": [
                {
                    "Time": "2025-08-06T19:18:39.2067156-07:00",
                    "Event": "IMAGE-SAVE"
                },
                {
                    "Time": "2025-08-06T19:18:09.4045633-07:00",
                    "New": {"Name": "HA", "Id": 0},
                    "Previous": {"Name": "OIII", "Id": 1},
                    "Event": "FILTERWHEEL-CHANGED"
                },
                {
                    "Time": "2025-08-06T21:50:56.545923-07:00",
                    "Event": "AUTOFOCUS-FINISHED"
                },
                {
                    "Time": "2025-08-06T18:45:40.1430956-07:00",
                    "Event": "CAMERA-CONNECTED"
                }
            ],
            "Error": "",
            "StatusCode": 200,
            "Success": true,
            "Type": "API"
        }"#;

        let events: EventHistoryResponse = serde_json::from_str(events_json).unwrap();

        // Test filtering by type
        let image_saves = events.get_image_saves();
        assert_eq!(image_saves.len(), 1);
        assert_eq!(image_saves[0].event, event_types::IMAGE_SAVE);

        let filter_changes = events.get_filterwheel_changes();
        assert_eq!(filter_changes.len(), 1);
        assert_eq!(filter_changes[0].event, event_types::FILTERWHEEL_CHANGED);

        let connection_events = events.get_connection_events();
        assert_eq!(connection_events.len(), 1);
        assert_eq!(connection_events[0].event, event_types::CAMERA_CONNECTED);

        // Test autofocus events
        let autofocus_events = events.get_events_by_type(event_types::AUTOFOCUS_FINISHED);
        assert_eq!(autofocus_events.len(), 1);
        assert_eq!(autofocus_events[0].event, event_types::AUTOFOCUS_FINISHED);

        // Test counting
        let counts = events.count_events_by_type();
        assert_eq!(counts.get(event_types::IMAGE_SAVE), Some(&1));
        assert_eq!(counts.get(event_types::FILTERWHEEL_CHANGED), Some(&1));
        assert_eq!(counts.get(event_types::AUTOFOCUS_FINISHED), Some(&1));
        assert_eq!(counts.get(event_types::CAMERA_CONNECTED), Some(&1));
    }

    #[test]
    fn test_ts_targetstart_event() {
        let event_json = r#"{
            "TargetEndTime": "2025-08-17T04:10:06.5191486",
            "Time": "2025-08-17T05:21:02.1200663",
            "Event": "TS-TARGETSTART",
            "TargetName": "Pickering Triangle",
            "Rotation": 90,
            "ProjectName": "Pickering Triangle",
            "Coordinates": {
                "RA": 20.822777777777777,
                "Dec": 31.415,
                "RAString": "20:49:22",
                "DecString": "31° 24' 54\"",
                "Epoch": "J2000",
                "RADegrees": 312.34166666666664
            }
        }"#;

        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.event, event_types::TS_TARGETSTART);

        if let Some(EventDetails::TargetStart {
            target_name,
            project_name,
            coordinates,
            rotation,
            target_end_time,
        }) = event.details
        {
            assert_eq!(target_name, "Pickering Triangle");
            assert_eq!(project_name.as_deref(), Some("Pickering Triangle"));
            assert_eq!(rotation, Some(90.0));
            assert_eq!(
                target_end_time.as_deref(),
                Some("2025-08-17T04:10:06.5191486")
            );
            let coordinates = coordinates.expect("coordinates");
            assert_eq!(coordinates.ra, 20.822777777777777);
            assert_eq!(coordinates.dec, 31.415);
            assert_eq!(coordinates.ra_string, "20:49:22");
            assert_eq!(coordinates.dec_string, "31° 24' 54\"");
            assert_eq!(coordinates.epoch, "J2000");
            assert_eq!(coordinates.ra_degrees, 312.34166666666664);
        } else {
            panic!("Expected TargetStart details");
        }
    }

    #[test]
    fn test_filterwheel_change_empty_arrays() {
        // NINA can return Name:[] / Id:[] when the wheel slot is unknown.
        let event_json = r#"{
            "Time": "2026-05-18T20:12:14.1465888-07:00",
            "Previous": {"Name": [], "Id": []},
            "New": {"Name": [], "Id": []},
            "Event": "FILTERWHEEL-CHANGED"
        }"#;
        let event: Event = serde_json::from_str(event_json).unwrap();
        match event.details {
            Some(EventDetails::FilterWheelChange { new, previous }) => {
                assert!(new.is_unknown());
                assert!(previous.is_unknown());
                assert_eq!(new.name, "");
                assert_eq!(new.id, -1);
            }
            other => panic!("expected FilterWheelChange, got {other:?}"),
        }
    }

    #[test]
    fn test_ts_targetstart_with_empty_array_coords() {
        // c925 emits TS-TARGETSTART with empty-array coord fields when the
        // target lacks coords. The struct should still parse and surface
        // the target name + project name.
        let event_json = r#"{
            "Time": "2026-05-19T05:40:29.6220877",
            "Coordinates": {
                "RA": [], "Dec": [], "RAString": [], "DecString": [],
                "Epoch": [], "RADegrees": []
            },
            "TargetEndTime": "2026-05-19T04:00:28.7026313",
            "ProjectName": "Sunflower Galaxy",
            "Event": "TS-TARGETSTART",
            "TargetName": "Sunflower Galaxy",
            "Rotation": 0
        }"#;
        let event: Event = serde_json::from_str(event_json).unwrap();
        match event.details {
            Some(EventDetails::TargetStart {
                target_name,
                project_name,
                coordinates,
                ..
            }) => {
                assert_eq!(target_name, "Sunflower Galaxy");
                assert_eq!(project_name.as_deref(), Some("Sunflower Galaxy"));
                let coordinates = coordinates.expect("coordinates");
                assert!(coordinates.is_unknown());
                assert!(coordinates.ra_string.is_empty());
            }
            other => panic!("expected TargetStart, got {other:?}"),
        }
    }

    #[test]
    fn test_ts_targetstart_requires_only_target_name() {
        let event_json = r#"{
            "Time": "2026-08-16T20:00:00Z",
            "Event": "TS-NEWTARGETSTART",
            "TargetName": "North America Nebula"
        }"#;

        let event: Event = serde_json::from_str(event_json).unwrap();
        match event.details {
            Some(EventDetails::TargetStart {
                target_name,
                project_name,
                coordinates,
                rotation,
                target_end_time,
            }) => {
                assert_eq!(target_name, "North America Nebula");
                assert_eq!(project_name, None);
                assert!(coordinates.is_none());
                assert_eq!(rotation, None);
                assert_eq!(target_end_time, None);
            }
            other => panic!("expected TargetStart, got {other:?}"),
        }
    }

    #[test]
    fn test_ts_targetstart_tolerates_explicit_nulls() {
        // The plugin builds events from a dictionary and writes a missing
        // Target Scheduler broker header through as a null rather than
        // omitting the key. Both shapes have to survive.
        let event_json = r#"{
            "Time": "2026-08-16T20:00:00Z",
            "Event": "TS-TARGETSTART",
            "TargetName": "M31",
            "ProjectName": null,
            "Coordinates": null,
            "Rotation": null,
            "TargetEndTime": null
        }"#;

        let event: Event = serde_json::from_str(event_json).unwrap();
        match event.details {
            Some(EventDetails::TargetStart {
                target_name,
                coordinates,
                rotation,
                ..
            }) => {
                assert_eq!(target_name, "M31");
                assert!(coordinates.is_none());
                assert_eq!(rotation, None);
            }
            other => panic!("expected TargetStart, got {other:?}"),
        }
    }

    #[test]
    fn target_start_does_not_swallow_unrelated_events() {
        // EventDetails is untagged, so variants are chosen by field shape.
        // TargetName is the only remaining required field on TargetStart —
        // if it ever becomes optional the variant matches every event.
        for event_json in [
            r#"{"Time": "t", "Event": "MOUNT-PARKED"}"#,
            r#"{"Time": "t", "Event": "SEQUENCE-STARTING", "ChatEnabled": false}"#,
            r#"{"Time": "t", "Event": "ROTATOR-MOVED", "From": 0.0, "To": 10.0}"#,
        ] {
            let event: Event = serde_json::from_str(event_json).unwrap();
            assert!(
                !matches!(event.details, Some(EventDetails::TargetStart { .. })),
                "TargetStart greedily matched {event_json}"
            );
        }
    }

    #[test]
    fn test_rotator_moved_event() {
        let event_json = r#"{
            "To": 104.04,
            "Time": "2026-05-18T22:03:30.0844644-07:00",
            "Event": "ROTATOR-MOVED",
            "From": 0
        }"#;
        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.event, event_types::ROTATOR_MOVED);
        match event.details {
            Some(EventDetails::RotatorMoved { from, to }) => {
                assert_eq!(from, 0.0);
                assert!((to - 104.04).abs() < 1e-6);
            }
            other => panic!("expected RotatorMoved, got {other:?}"),
        }
    }

    #[test]
    fn test_autofocus_point_added_event() {
        let event_json = r#"{
            "Position": 3325,
            "Time": "2026-05-18T22:43:41.8412779-07:00",
            "HFR": 4.3494099367270405,
            "Event": "AUTOFOCUS-POINT-ADDED"
        }"#;
        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.event, event_types::AUTOFOCUS_POINT_ADDED);
        match event.details {
            Some(EventDetails::AutofocusPointAdded { position, hfr }) => {
                assert_eq!(position, 3325);
                assert!((hfr - 4.3494099367270405).abs() < 1e-9);
            }
            other => panic!("expected AutofocusPointAdded, got {other:?}"),
        }
    }

    #[test]
    fn test_ts_waitstart_event() {
        let event_json = r#"{
            "WaitEndTime": "2026-05-18T22:02:21.3561448-07:00",
            "Time": "2026-05-19T03:20:43.4843722",
            "Event": "TS-WAITSTART"
        }"#;
        let event: Event = serde_json::from_str(event_json).unwrap();
        assert_eq!(event.event, event_types::TS_WAITSTART);
        match event.details {
            Some(EventDetails::WaitStart { wait_end_time }) => {
                assert_eq!(wait_end_time, "2026-05-18T22:02:21.3561448-07:00");
            }
            other => panic!("expected WaitStart, got {other:?}"),
        }
    }

    #[test]
    fn from_to_payloads_are_typed_by_event_name() {
        let rotator: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:00Z",
            "Event": "ROTATOR-MOVED",
            "From": 12.0,
            "To": 15.5
        }))
        .unwrap();
        assert!(matches!(
            rotator.details,
            Some(EventDetails::RotatorMoved {
                from: 12.0,
                to: 15.5
            })
        ));

        let dome: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:01Z",
            "Event": "DOME-SLEWED",
            "FromAzimuth": 180.0,
            "ToAzimuth": 225.0
        }))
        .unwrap();
        assert!(matches!(
            dome.details,
            Some(EventDetails::DomeSlewed {
                from: 180.0,
                to: 225.0
            })
        ));

        let mount: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:02Z",
            "Event": "MOUNT-SLEWED",
            "From": { "RA": 1.0, "Dec": 2.0, "Epoch": "J2000" },
            "To": { "RA": 3.0, "Dec": 4.0, "Epoch": "J2000" }
        }))
        .unwrap();
        match mount.details {
            Some(EventDetails::MountSlewed { from, to }) => {
                assert_eq!(from.ra, Some(1.0));
                assert_eq!(to.dec, Some(4.0));
            }
            details => panic!("mount slew was not typed correctly: {details:?}"),
        }
    }

    #[test]
    fn dome_slew_accepts_legacy_numeric_field_names() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:00Z",
            "Event": "DOME-SLEWED",
            "From": 10,
            "To": 20
        }))
        .unwrap();
        assert!(matches!(
            event.details,
            Some(EventDetails::DomeSlewed {
                from: 10.0,
                to: 20.0
            })
        ));
    }

    #[test]
    fn new_failure_and_flat_payloads_are_typed() {
        let sequence: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:00Z",
            "Event": "SEQUENCE-ENTITY-FAILED",
            "Entity": "Wait until safe",
            "EntityType": "SafetyMonitor",
            "Error": "Monitor disconnected"
        }))
        .unwrap();
        assert!(matches!(
            sequence.details,
            Some(EventDetails::SequenceEntityFailed { ref entity, ref error, .. })
                if entity == "Wait until safe" && error == "Monitor disconnected"
        ));

        let image: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:00Z",
            "Event": "IMAGE-SAVE-FAILED",
            "Stage": "Write",
            "DiskFull": true,
            "Error": "No space left"
        }))
        .unwrap();
        assert!(matches!(
            image.details,
            Some(EventDetails::ImageSaveFailed {
                disk_full: true,
                ..
            })
        ));

        let flat: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:00Z",
            "Event": "FLAT-BRIGHTNESS-CHANGED",
            "Previous": 20,
            "New": 40
        }))
        .unwrap();
        assert!(matches!(
            flat.details,
            Some(EventDetails::FlatBrightnessChanged {
                previous: 20,
                new: 40
            })
        ));

        let finished: Event = serde_json::from_value(serde_json::json!({
            "Time": "2026-08-26T01:00:01Z",
            "Event": "SEQUENCE-FINISHED",
            "Outcome": "completed_with_failures",
            "Status": "FINISHED",
            "HadFailures": true
        }))
        .unwrap();
        assert!(matches!(
            finished.details,
            Some(EventDetails::SequenceFinished {
                ref outcome,
                had_failures: true,
                ..
            }) if outcome == "completed_with_failures"
        ));
    }

    #[test]
    fn flat_light_unknown_state_remains_typed_and_clears_stale_state() {
        for (payload, expected) in [
            (serde_json::json!({ "On": true }), Some(true)),
            (serde_json::json!({ "On": null }), None),
            (serde_json::json!({}), None),
        ] {
            let mut object = payload.as_object().unwrap().clone();
            object.insert(
                "Time".to_string(),
                serde_json::json!("2026-08-26T01:00:00Z"),
            );
            object.insert("Event".to_string(), serde_json::json!("FLAT-LIGHT-TOGGLED"));
            let event: Event = serde_json::from_value(serde_json::Value::Object(object)).unwrap();
            assert!(matches!(
                event.details,
                Some(EventDetails::FlatLightToggled { on }) if on == expected
            ));
        }
    }

    #[test]
    fn scopes_separate_observatory_connections_and_failures() {
        assert_eq!(
            event_delivery_scope(event_types::DOME_SLEWED),
            EventDeliveryScope::Observatory
        );
        assert_eq!(
            event_delivery_scope(event_types::FLAT_LIGHT_TOGGLED),
            EventDeliveryScope::Observatory
        );
        assert_eq!(
            event_delivery_scope(event_types::DOME_CONNECTED),
            EventDeliveryScope::EquipmentConnections
        );
        assert_eq!(
            event_delivery_scope(event_types::CAMERA_DOWNLOAD_TIMEOUT),
            EventDeliveryScope::Images
        );
        assert_eq!(
            event_delivery_scope(event_types::FOCUSER_USER_FOCUSED),
            EventDeliveryScope::FilterFocuserRotator
        );
        assert_eq!(
            event_delivery_scope(event_types::CHATSTRONOMY_COMMAND_FAILED),
            EventDeliveryScope::CommandFailures
        );
    }

    #[test]
    fn test_load_event_history_from_file() {
        // Test loading the example event history file if it exists
        if let Ok(json_content) = std::fs::read_to_string("example_event-history.json") {
            let events: Result<EventHistoryResponse, _> = serde_json::from_str(&json_content);
            assert!(
                events.is_ok(),
                "Should be able to parse example_event-history.json"
            );

            let events = events.unwrap();
            assert!(events.success, "Events should indicate success");
            assert_eq!(events.status_code, 200, "Should have status code 200");
            assert!(!events.response.is_empty(), "Should have events");

            println!("Found {} events in example file", events.response.len());

            // Test event analysis
            let counts = events.count_events_by_type();
            println!("Event type counts: {counts:?}");

            let filter_changes = events.get_filterwheel_changes();
            println!("Found {} filter wheel changes", filter_changes.len());

            let image_saves = events.get_image_saves();
            println!("Found {} image saves", image_saves.len());

            let autofocus_events = events.get_events_by_type(event_types::AUTOFOCUS_FINISHED);
            println!("Found {} autofocus events", autofocus_events.len());
        } else {
            println!("example_event-history.json not found, skipping file test");
        }
    }
}
