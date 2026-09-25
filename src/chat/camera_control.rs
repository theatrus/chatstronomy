//! Sharing policy only; these commands can never operate camera hardware.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CameraTriggerRules {
    pub interval_minutes: u16,
    pub scene_changes: bool,
    pub day_night: bool,
    pub telescope_events: bool,
    pub burst_count: u8,
    pub spacing_seconds: u16,
}

impl CameraTriggerRules {
    pub fn valid(&self) -> bool {
        self.interval_minutes <= 1440
            && (1..=3).contains(&self.burst_count)
            && (60..=600).contains(&self.spacing_seconds)
    }
}
