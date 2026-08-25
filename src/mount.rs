use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MountInfoResponse {
    pub response: MountInfo,
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MountInfo {
    #[serde(default)]
    pub sidereal_time: f64,
    pub right_ascension: f64,
    pub declination: f64,
    #[serde(default)]
    pub site_latitude: f64,
    #[serde(default)]
    pub site_longitude: f64,
    #[serde(default)]
    pub site_elevation: i32,
    pub right_ascension_string: String,
    pub declination_string: String,
    pub coordinates: Coordinates,
    #[serde(default)]
    pub time_to_meridian_flip: f64,
    pub side_of_pier: String,
    #[serde(default)]
    pub altitude: f64,
    #[serde(default)]
    pub altitude_string: String,
    #[serde(default)]
    pub azimuth: f64,
    #[serde(default)]
    pub azimuth_string: String,
    #[serde(default)]
    pub sidereal_time_string: String,
    #[serde(default)]
    pub hours_to_meridian_string: String,
    pub at_park: bool,
    pub tracking_rate: TrackingRate,
    pub tracking_enabled: bool,
    pub tracking_modes: Vec<String>,
    pub at_home: bool,
    pub can_find_home: bool,
    pub can_park: bool,
    pub can_set_park: bool,
    pub can_set_tracking_enabled: bool,
    pub can_set_declination_rate: bool,
    pub can_set_right_ascension_rate: bool,
    pub equatorial_system: String,
    pub has_unknown_epoch: bool,
    #[serde(default)]
    pub time_to_meridian_flip_string: String,
    pub slewing: bool,
    pub guide_rate_right_ascension_arcsec_per_sec: f64,
    pub guide_rate_declination_arcsec_per_sec: f64,
    pub can_move_primary_axis: bool,
    pub can_move_secondary_axis: bool,
    pub primary_axis_rates: Vec<AxisRate>,
    pub secondary_axis_rates: Vec<AxisRate>,
    pub supported_actions: Vec<String>,
    pub alignment_mode: String,
    pub can_pulse_guide: bool,
    pub is_pulse_guiding: bool,
    pub can_set_pier_side: bool,
    pub can_slew: bool,
    #[serde(rename = "UTCDate")]
    pub utc_date: String,
    pub connected: bool,
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub location_redacted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Coordinates {
    #[serde(rename = "RA")]
    pub ra: f64,
    #[serde(rename = "RAString")]
    pub ra_string: String,
    #[serde(rename = "RADegrees")]
    pub ra_degrees: f64,
    pub dec: f64,
    pub dec_string: String,
    pub epoch: String,
    #[serde(rename = "DateTime")]
    pub date_time: DateTime,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DateTime {
    pub now: String,
    pub utc_now: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrackingRate {
    // The JSON shows this as an empty object, so we'll leave it flexible
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AxisRate {
    // The JSON shows this as empty objects, so we'll leave it flexible for now
}

impl MountInfoResponse {
    /// Check if the mount is connected
    pub fn is_connected(&self) -> bool {
        self.success && self.response.connected
    }

    /// Check if the mount is currently slewing
    pub fn is_slewing(&self) -> bool {
        self.response.slewing
    }

    /// Check if the mount is parked
    pub fn is_parked(&self) -> bool {
        self.response.at_park
    }

    /// Check if tracking is enabled
    pub fn is_tracking(&self) -> bool {
        self.response.tracking_enabled
    }

    /// Get the time to meridian flip in hours
    pub fn get_time_to_meridian_flip_hours(&self) -> f64 {
        if self.response.location_redacted {
            0.0
        } else {
            self.response.time_to_meridian_flip
        }
    }

    /// Get the formatted time to meridian flip string
    pub fn get_time_to_meridian_flip_string(&self) -> &str {
        if self.response.location_redacted {
            ""
        } else {
            &self.response.time_to_meridian_flip_string
        }
    }

    /// Get current coordinates
    pub fn get_coordinates(&self) -> (&str, &str) {
        (
            &self.response.right_ascension_string,
            &self.response.declination_string,
        )
    }

    /// Get current altitude and azimuth
    pub fn get_alt_az(&self) -> (&str, &str) {
        if self.response.location_redacted {
            ("", "")
        } else {
            (
                &self.response.altitude_string,
                &self.response.azimuth_string,
            )
        }
    }

    /// Get side of pier
    pub fn get_side_of_pier(&self) -> &str {
        &self.response.side_of_pier
    }

    /// Get site information (latitude, longitude, elevation)
    pub fn get_site_info(&self) -> (f64, f64, i32) {
        if self.response.location_redacted {
            (0.0, 0.0, 0)
        } else {
            (
                self.response.site_latitude,
                self.response.site_longitude,
                self.response.site_elevation,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_info_deserialization() {
        // Test parsing the example mount info JSON file if it exists
        if let Ok(json_content) = std::fs::read_to_string("example_equipment_mount_info.json") {
            let mount_info: Result<MountInfoResponse, _> = serde_json::from_str(&json_content);
            assert!(
                mount_info.is_ok(),
                "Should be able to parse example_equipment_mount_info.json"
            );

            let mount_info = mount_info.unwrap();
            assert!(mount_info.success, "Mount info should indicate success");
            assert_eq!(mount_info.status_code, 200, "Should have status code 200");

            // Test some specific fields from the example
            assert!(mount_info.is_connected());
            assert!(mount_info.is_tracking());
            assert!(!mount_info.is_parked());
            assert!(!mount_info.is_slewing());

            // Test coordinate parsing
            let (ra, dec) = mount_info.get_coordinates();
            assert_eq!(ra, "20:02:45");
            assert!(dec.contains("35°"));

            // Test time to meridian flip
            let flip_time = mount_info.get_time_to_meridian_flip_hours();
            assert!(flip_time > 0.0);

            // Test site info
            let (lat, lon, elev) = mount_info.get_site_info();
            assert!((lat - 38.661).abs() < 0.001);
            assert!((lon - (-121.166)).abs() < 0.001);
            assert_eq!(elev, 100);

            println!("Mount info parsed successfully:");
            println!("  Connected: {}", mount_info.is_connected());
            println!("  Tracking: {}", mount_info.is_tracking());
            println!("  Coordinates: {ra} {dec}");
            println!("  Time to flip: {flip_time:.3}h");
            println!("  Side of pier: {}", mount_info.get_side_of_pier());
        } else {
            println!("example_equipment_mount_info.json not found, skipping file test");
        }
    }

    #[test]
    fn test_mount_info_convenience_methods() {
        let mount_json = r#"{
            "Response": {
                "SiderealTime": 20.46,
                "RightAscension": 20.045,
                "Declination": 35.541,
                "SiteLatitude": 38.661,
                "SiteLongitude": -121.166,
                "SiteElevation": 100,
                "RightAscensionString": "20:02:45",
                "DeclinationString": "35° 32' 29\"",
                "Coordinates": {
                    "RA": 20.045,
                    "RAString": "20:02:45",
                    "RADegrees": 300.689,
                    "Dec": 35.541,
                    "DecString": "35° 32' 29\"",
                    "Epoch": "JNOW",
                    "DateTime": {
                        "Now": "2025-08-09T00:20:10.1340528-07:00",
                        "UtcNow": "2025-08-09T07:20:10.1340586Z"
                    }
                },
                "TimeToMeridianFlip": 11.618,
                "SideOfPier": "pierEast",
                "Altitude": 84.14,
                "AltitudeString": "84° 08' 25\"",
                "Azimuth": 239.755,
                "AzimuthString": "239° 45' 19\"",
                "SiderealTimeString": "20:27:39",
                "HoursToMeridianString": "11:35:07",
                "AtPark": false,
                "TrackingRate": {},
                "TrackingEnabled": true,
                "TrackingModes": ["Sidereal", "King"],
                "AtHome": false,
                "CanFindHome": true,
                "CanPark": true,
                "CanSetPark": true,
                "CanSetTrackingEnabled": true,
                "CanSetDeclinationRate": true,
                "CanSetRightAscensionRate": true,
                "EquatorialSystem": "JNOW",
                "HasUnknownEpoch": false,
                "TimeToMeridianFlipString": "11:37:07",
                "Slewing": false,
                "GuideRateRightAscensionArcsecPerSec": 7.52,
                "GuideRateDeclinationArcsecPerSec": 7.52,
                "CanMovePrimaryAxis": true,
                "CanMoveSecondaryAxis": true,
                "PrimaryAxisRates": [{}],
                "SecondaryAxisRates": [{}],
                "SupportedActions": ["Telescope:SetParkPosition"],
                "AlignmentMode": "GermanPolar",
                "CanPulseGuide": true,
                "IsPulseGuiding": false,
                "CanSetPierSide": true,
                "CanSlew": true,
                "UTCDate": "2025-08-09T07:20:08.459",
                "Connected": true,
                "Name": "ASCOM GS Sky Telescope",
                "DisplayName": "ASCOM GS Sky Telescope",
                "DeviceId": "ASCOM.GS.Sky.Telescope"
            },
            "Error": "",
            "StatusCode": 200,
            "Success": true,
            "Type": "API"
        }"#;

        let mount_info: MountInfoResponse = serde_json::from_str(mount_json).unwrap();

        assert!(mount_info.is_connected());
        assert!(mount_info.is_tracking());
        assert!(!mount_info.is_parked());
        assert!(!mount_info.is_slewing());

        let (ra, dec) = mount_info.get_coordinates();
        assert_eq!(ra, "20:02:45");
        assert_eq!(dec, "35° 32' 29\"");

        let (alt, az) = mount_info.get_alt_az();
        assert_eq!(alt, "84° 08' 25\"");
        assert_eq!(az, "239° 45' 19\"");

        assert_eq!(mount_info.get_side_of_pier(), "pierEast");

        let flip_time = mount_info.get_time_to_meridian_flip_hours();
        assert!((flip_time - 11.618).abs() < 0.001);

        let (lat, lon, elev) = mount_info.get_site_info();
        assert!((lat - 38.661).abs() < 0.001);
        assert!((lon - (-121.166)).abs() < 0.001);
        assert_eq!(elev, 100);
    }

    #[test]
    fn private_mount_snapshots_may_omit_location_derived_fields() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../example_equipment_mount_info.json")).unwrap();
        let response = value["Response"].as_object_mut().unwrap();
        for field in [
            "SiteLatitude",
            "SiteLongitude",
            "SiteElevation",
            "DeviceId",
            "SiderealTime",
            "SiderealTimeString",
            "Altitude",
            "AltitudeString",
            "Azimuth",
            "AzimuthString",
            "HoursToMeridianString",
            "TimeToMeridianFlip",
            "TimeToMeridianFlipString",
        ] {
            response.remove(field);
        }

        let mount: MountInfoResponse = serde_json::from_value(value)
            .expect("a position-redacted snapshot remains compatible with Direct v1");

        assert!(mount.is_connected());
        assert_eq!(mount.get_coordinates().0, "20:02:45");
        assert_eq!(mount.get_alt_az(), ("", ""));
        assert_eq!(mount.get_site_info(), (0.0, 0.0, 0));
        assert_eq!(mount.get_time_to_meridian_flip_string(), "");
        assert_eq!(mount.response.sidereal_time_string, "");
        assert_eq!(mount.response.device_id, "");
        assert!(!mount.response.location_redacted);
    }

    #[test]
    fn redaction_marker_hides_location_derived_mount_fields() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../example_equipment_mount_info.json")).unwrap();
        value["Response"]["LocationRedacted"] = serde_json::Value::Bool(true);

        let mount: MountInfoResponse = serde_json::from_value(value)
            .expect("legacy mount payloads accept the additive redaction marker");

        assert!(mount.response.location_redacted);
        assert_eq!(mount.get_coordinates().0, "20:02:45");
        assert_eq!(mount.get_alt_az(), ("", ""));
        assert_eq!(mount.get_site_info(), (0.0, 0.0, 0));
        assert_eq!(mount.get_time_to_meridian_flip_hours(), 0.0);
        assert_eq!(mount.get_time_to_meridian_flip_string(), "");
    }
}
