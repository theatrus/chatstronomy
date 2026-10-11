use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageResponse {
    pub response: String, // Base64 encoded image data
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailResponse {
    /// Raw JPG image data. Base64 on the wire so Direct-protocol frames
    /// stay compact.
    #[serde(with = "thumbnail_base64")]
    pub data: Vec<u8>,
    pub content_type: String,
    pub status_code: u16,
}

mod thumbnail_base64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(&text).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageHistoryResponse {
    pub response: Vec<ImageMetadata>,
    /// Additive local plugin settings; absent on older peers means no charts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_statistics: Option<ImageStatisticsOptions>,
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageStatisticsOptions {
    /// Zero disables periodic charts. Counts LIGHT frames, not chat posts.
    #[serde(default)]
    pub every_n_frames: u32,
    #[serde(default)]
    pub on_sequence_end: bool,
}

impl ImageStatisticsOptions {
    pub fn enabled(self) -> bool {
        self.every_n_frames > 0 || self.on_sequence_end
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageMetadata {
    #[serde(default = "default_true")]
    pub chat_enabled: bool,
    pub exposure_time: f64,
    pub image_type: String,
    pub filter: String,
    pub rms_text: String,
    pub temperature: f64,
    pub camera_name: String,
    pub gain: i32,
    pub offset: i32,
    pub date: String,
    pub telescope_name: String,
    pub focal_length: i32,
    pub st_dev: f64,
    pub mean: f64,
    pub median: f64,
    pub stars: i32,
    #[serde(rename = "HFR")]
    pub hfr: f64,
    pub is_bayered: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_finite_f64"
    )]
    pub focuser_temperature: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_nonnegative_i32"
    )]
    pub focuser_position: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_finite_f64"
    )]
    pub weather_temperature: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_finite_f64"
    )]
    pub dew_point: Option<f64>,
    /// Capture-time wind readings, in meters per second.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_finite_f64"
    )]
    pub wind_speed: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_helpers::de_optional_finite_f64"
    )]
    pub wind_gust: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_detection_available: Option<bool>,
    /// Offset-aware capture time from new plugins; `Date` remains unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_is_exposure_start: Option<bool>,
}

fn default_true() -> bool {
    true
}

// Image type constants for easier matching
pub mod image_types {
    pub const LIGHT: &str = "LIGHT";
    pub const DARK: &str = "DARK";
    pub const FLAT: &str = "FLAT";
    pub const BIAS: &str = "BIAS";
}

impl ImageHistoryResponse {
    /// Get all images of a specific type (LIGHT, DARK, FLAT, BIAS)
    pub fn get_images_by_type(&self, image_type: &str) -> Vec<&ImageMetadata> {
        self.response
            .iter()
            .filter(|image| image.image_type == image_type)
            .collect()
    }

    /// Get all images using a specific filter
    pub fn get_images_by_filter(&self, filter: &str) -> Vec<&ImageMetadata> {
        self.response
            .iter()
            .filter(|image| image.filter == filter)
            .collect()
    }

    /// Get all light frames (science images)
    pub fn get_light_frames(&self) -> Vec<&ImageMetadata> {
        self.get_images_by_type(image_types::LIGHT)
    }

    /// Get all calibration frames (darks, flats, bias)
    pub fn get_calibration_frames(&self) -> Vec<&ImageMetadata> {
        self.response
            .iter()
            .filter(|image| {
                image.image_type == image_types::DARK
                    || image.image_type == image_types::FLAT
                    || image.image_type == image_types::BIAS
            })
            .collect()
    }

    /// Count images by type
    pub fn count_images_by_type(&self) -> std::collections::HashMap<String, usize> {
        let mut counts = std::collections::HashMap::new();
        for image in &self.response {
            *counts.entry(image.image_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Count images by filter
    pub fn count_images_by_filter(&self) -> std::collections::HashMap<String, usize> {
        let mut counts = std::collections::HashMap::new();
        for image in &self.response {
            *counts.entry(image.filter.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Get basic statistics about the image session
    pub fn get_session_stats(&self) -> SessionStats {
        let total_images = self.response.len();
        let light_frames = self.get_light_frames().len();
        let calibration_frames = self.get_calibration_frames().len();

        let total_exposure_time: f64 = self
            .get_light_frames()
            .iter()
            .map(|img| img.exposure_time)
            .sum();

        let unique_filters: std::collections::HashSet<_> = self
            .response
            .iter()
            .map(|img| img.filter.as_str())
            .collect();

        SessionStats {
            total_images,
            light_frames,
            calibration_frames,
            total_exposure_time,
            unique_filters: unique_filters.len(),
        }
    }
}

#[derive(Debug)]
pub struct SessionStats {
    pub total_images: usize,
    pub light_frames: usize,
    pub calibration_frames: usize,
    pub total_exposure_time: f64,
    pub unique_filters: usize,
}

impl std::fmt::Display for SessionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Session Stats: {} total images ({} light frames, {} calibration frames), {:.1}s total exposure, {} unique filters",
            self.total_images,
            self.light_frames,
            self.calibration_frames,
            self.total_exposure_time,
            self.unique_filters
        )
    }
}

impl ImageMetadata {
    /// Check if this is a light frame (science image)
    pub fn is_light_frame(&self) -> bool {
        self.image_type == image_types::LIGHT
    }

    /// Check if this is a calibration frame
    pub fn is_calibration_frame(&self) -> bool {
        self.image_type == image_types::DARK
            || self.image_type == image_types::FLAT
            || self.image_type == image_types::BIAS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_capture_statistics_fixture_decodes_through_direct_v1() {
        let message: crate::direct::protocol::DirectMessage = serde_json::from_str(include_str!(
            "../contracts/direct/v1/fixtures/query-result-image-statistics.json"
        ))
        .unwrap();
        let crate::direct::protocol::DirectMessage::QueryResult(result) = message else {
            panic!("expected query result")
        };
        let history: ImageHistoryResponse = serde_json::from_value(result.payload).unwrap();
        assert_eq!(
            history.image_statistics,
            Some(ImageStatisticsOptions {
                every_n_frames: 10,
                on_sequence_end: true
            })
        );
        assert_eq!(history.response[0].focuser_position, Some(30105));
        assert_eq!(history.response[0].weather_temperature, Some(8.2));
        assert_eq!(history.response[0].timestamp_is_exposure_start, Some(true));
        assert_eq!(history.response[1].focuser_temperature, None);
        assert_eq!(history.response[1].star_detection_available, Some(false));
        assert_eq!(history.response[1].timestamp_is_exposure_start, Some(false));
    }

    #[test]
    fn additive_capture_fields_accept_legacy_missing_and_unknown_values() {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../example_image-history.json")).unwrap();
        let legacy: ImageHistoryResponse = serde_json::from_value(value.clone()).unwrap();
        assert!(legacy.image_statistics.is_none());
        assert!(legacy.response[0].focuser_temperature.is_none());
        assert!(legacy.response[0].timestamp.is_none());
        value["ImageStatistics"] =
            serde_json::json!({"EveryNFrames": 10, "OnSequenceEnd": true, "FutureOption": true});
        value["Response"][0]["FocuserTemperature"] = serde_json::json!("NaN");
        value["Response"][0]["FocuserPosition"] = serde_json::json!(-1);
        value["Response"][0]["WeatherTemperature"] = serde_json::json!(-2.5);
        value["Response"][0]["DewPoint"] = serde_json::json!(null);
        value["Response"][0]["WindSpeed"] = serde_json::json!([]);
        value["Response"][0]["WindGust"] = serde_json::json!(3.0);
        value["Response"][0]["StarDetectionAvailable"] = serde_json::json!(false);
        value["Response"][0]["Timestamp"] = serde_json::json!("2026-10-10T04:30:00Z");
        let response: ImageHistoryResponse = serde_json::from_value(value).unwrap();
        assert_eq!(
            response.image_statistics,
            Some(ImageStatisticsOptions {
                every_n_frames: 10,
                on_sequence_end: true
            })
        );
        let image = &response.response[0];
        assert_eq!(image.focuser_temperature, None);
        assert_eq!(image.focuser_position, None);
        assert_eq!(image.weather_temperature, Some(-2.5));
        assert_eq!(image.dew_point, None);
        assert_eq!(image.wind_speed, None);
        assert_eq!(image.wind_gust, Some(3.0));
        assert_eq!(image.star_detection_available, Some(false));
        assert_eq!(image.timestamp.as_deref(), Some("2026-10-10T04:30:00Z"));
    }

    #[test]
    fn test_image_metadata_parsing() {
        let image_json = r#"{
            "ExposureTime": 300.0,
            "ImageType": "LIGHT",
            "Filter": "HA",
            "RmsText": "1.23",
            "Temperature": -10.5,
            "CameraName": "ZWO ASI533MC Pro",
            "Gain": 100,
            "Offset": 10,
            "Date": "2025-08-06 19:18:39",
            "TelescopeName": "William Optics RedCat 71",
            "FocalLength": 350,
            "StDev": 456.78,
            "Mean": 1234.56,
            "Median": 1200.00,
            "Stars": 145,
            "HFR": 2.45,
            "IsBayered": true
        }"#;

        let image: ImageMetadata = serde_json::from_str(image_json).unwrap();
        assert_eq!(image.exposure_time, 300.0);
        assert_eq!(image.image_type, "LIGHT");
        assert_eq!(image.filter, "HA");
        assert_eq!(image.temperature, -10.5);
        assert_eq!(image.camera_name, "ZWO ASI533MC Pro");
        assert_eq!(image.gain, 100);
        assert_eq!(image.offset, 10);
        assert_eq!(image.stars, 145);
        assert_eq!(image.hfr, 2.45);
        assert!(image.is_bayered);
        assert!(image.chat_enabled);
    }

    #[test]
    fn test_image_methods() {
        let light_frame = ImageMetadata {
            chat_enabled: true,
            exposure_time: 180.0,
            image_type: "LIGHT".to_string(),
            filter: "OIII".to_string(),
            rms_text: "1.50".to_string(),
            temperature: -5.0,
            camera_name: "Test Camera".to_string(),
            gain: 120,
            offset: 15,
            date: "2025-08-06 20:00:00".to_string(),
            telescope_name: "Test Telescope".to_string(),
            focal_length: 400,
            st_dev: 300.0,
            mean: 2000.0,
            median: 1950.0,
            stars: 200,
            hfr: 2.8,
            is_bayered: false,
            focuser_temperature: None,
            focuser_position: None,
            weather_temperature: None,
            dew_point: None,
            wind_speed: None,
            wind_gust: None,
            star_detection_available: None,
            timestamp: None,
            timestamp_is_exposure_start: None,
        };

        assert!(light_frame.is_light_frame());
        assert!(!light_frame.is_calibration_frame());

        let flat_frame = ImageMetadata {
            chat_enabled: true,
            exposure_time: 1.0,
            image_type: "FLAT".to_string(),
            filter: "L".to_string(),
            rms_text: "0.0".to_string(),
            temperature: 15.0,
            camera_name: "Test Camera".to_string(),
            gain: 100,
            offset: 10,
            date: "2025-08-06 12:00:00".to_string(),
            telescope_name: "Test Telescope".to_string(),
            focal_length: 400,
            st_dev: 100.0,
            mean: 30000.0,
            median: 30000.0,
            stars: 0,
            hfr: 0.0,
            is_bayered: false,
            focuser_temperature: None,
            focuser_position: None,
            weather_temperature: None,
            dew_point: None,
            wind_speed: None,
            wind_gust: None,
            star_detection_available: None,
            timestamp: None,
            timestamp_is_exposure_start: None,
        };

        assert!(!flat_frame.is_light_frame());
        assert!(flat_frame.is_calibration_frame());
    }

    #[test]
    fn test_image_history_methods() {
        let images_json = r#"{
            "Response": [
                {
                    "ExposureTime": 300.0,
                    "ImageType": "LIGHT",
                    "Filter": "HA",
                    "RmsText": "1.23",
                    "Temperature": -10.5,
                    "CameraName": "ZWO ASI533MC Pro",
                    "Gain": 100,
                    "Offset": 10,
                    "Date": "2025-08-06 19:18:39",
                    "TelescopeName": "William Optics RedCat 71",
                    "FocalLength": 350,
                    "StDev": 456.78,
                    "Mean": 1234.56,
                    "Median": 1200.00,
                    "Stars": 145,
                    "HFR": 2.45,
                    "IsBayered": true
                },
                {
                    "ExposureTime": 1.0,
                    "ImageType": "FLAT",
                    "Filter": "HA",
                    "RmsText": "0.0",
                    "Temperature": 15.0,
                    "CameraName": "ZWO ASI533MC Pro",
                    "Gain": 100,
                    "Offset": 10,
                    "Date": "2025-08-06 12:00:00",
                    "TelescopeName": "William Optics RedCat 71",
                    "FocalLength": 350,
                    "StDev": 100.0,
                    "Mean": 30000.0,
                    "Median": 30000.0,
                    "Stars": 0,
                    "HFR": 0.0,
                    "IsBayered": true
                }
            ],
            "Error": "",
            "StatusCode": 200,
            "Success": true,
            "Type": "API"
        }"#;

        let images: ImageHistoryResponse = serde_json::from_str(images_json).unwrap();

        // Test filtering by type
        let light_frames = images.get_light_frames();
        assert_eq!(light_frames.len(), 1);
        assert_eq!(light_frames[0].image_type, "LIGHT");

        let calibration_frames = images.get_calibration_frames();
        assert_eq!(calibration_frames.len(), 1);
        assert_eq!(calibration_frames[0].image_type, "FLAT");

        // Test filtering by filter
        let ha_images = images.get_images_by_filter("HA");
        assert_eq!(ha_images.len(), 2);

        // Test session stats
        let stats = images.get_session_stats();
        let stats_string = stats.to_string();
        assert!(stats_string.contains("2 total images"));
        assert!(stats_string.contains("1 light frames"));
        assert!(stats_string.contains("1 calibration frames"));

        // Test type and filter counting
        let type_counts = images.count_images_by_type();
        assert_eq!(type_counts.get("LIGHT"), Some(&1));
        assert_eq!(type_counts.get("FLAT"), Some(&1));

        let filter_counts = images.count_images_by_filter();
        assert_eq!(filter_counts.get("HA"), Some(&2));
    }

    #[test]
    fn test_load_image_history_from_file() {
        // Test loading the example image history file if it exists
        if let Ok(json_content) = std::fs::read_to_string("example_image-history.json") {
            let images: Result<ImageHistoryResponse, _> = serde_json::from_str(&json_content);
            assert!(
                images.is_ok(),
                "Should be able to parse example_image-history.json"
            );

            let images = images.unwrap();
            assert!(images.success, "Images should indicate success");
            assert_eq!(images.status_code, 200, "Should have status code 200");
            assert!(!images.response.is_empty(), "Should have images");

            println!("Found {} images in example file", images.response.len());

            // Test image analysis
            let stats = images.get_session_stats();
            println!("Session stats:\n{stats}");

            let type_counts = images.count_images_by_type();
            println!("Image type counts: {type_counts:?}");

            let filter_counts = images.count_images_by_filter();
            println!("Filter counts: {filter_counts:?}");

            let light_frames = images.get_light_frames();
            println!("Found {} light frames", light_frames.len());

            let calibration = images.get_calibration_frames();
            println!("Found {} calibration frames", calibration.len());

            // Test temperature range
            if !images.response.is_empty() {
                let temperatures: Vec<f64> =
                    images.response.iter().map(|img| img.temperature).collect();
                let min_temp = temperatures.iter().fold(f64::INFINITY, |a, &b| a.min(b));
                let max_temp = temperatures
                    .iter()
                    .fold(f64::NEG_INFINITY, |a, &b| a.max(b));
                println!("Temperature range: {min_temp:.1}°C to {max_temp:.1}°C");
            }
        } else {
            println!("example_image-history.json not found, skipping file test");
        }
    }

    #[test]
    fn test_base64_image_processing() {
        // Test base64 decoding functionality that would be used by GetImage command
        let test_png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChAI9AAAAAElFTkSuQmCC";

        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, test_png_base64) {
            Ok(decoded) => {
                assert!(!decoded.is_empty(), "Should decode base64 data");
                println!("Successfully decoded {} bytes", decoded.len());

                // Check PNG header
                if decoded.starts_with(b"\x89PNG\r\n\x1a\n") {
                    println!("✓ Confirmed PNG format");
                } else {
                    println!(
                        "Decoded data header: {:02x?}",
                        &decoded[0..std::cmp::min(8, decoded.len())]
                    );
                }
            }
            Err(e) => {
                panic!("Failed to decode test base64: {e}");
            }
        }
    }
}
