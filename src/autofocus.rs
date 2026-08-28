use crate::serde_helpers::{
    de_f64_tolerant, de_finite_f64, de_optional_finite_f64, de_optional_nonnegative_i32,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct AutofocusResponse {
    pub response: AutofocusData,
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct AutofocusData {
    pub version: i32,
    pub filter: String,
    pub auto_focuser_name: String,
    pub star_detector_name: String,
    pub timestamp: String,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub temperature: f64,
    pub method: String,
    pub fitting: String,
    pub initial_focus_point: FocusPoint,
    pub calculated_focus_point: FocusPoint,
    pub previous_focus_point: PreviousFocusPoint,
    pub measure_points: Vec<FocusPoint>,
    pub intersections: Intersections,
    pub fittings: Fittings,
    #[serde(rename = "RSquares")]
    pub r_squares: RSquares,
    pub backlash_compensation: BacklashCompensation,
    pub duration: String,
    #[serde(
        default,
        rename = "FinalHFR",
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub final_hfr: Option<f64>,
    #[serde(
        default,
        rename = "FinalHFRSource",
        skip_serializing_if = "Option::is_none"
    )]
    pub final_hfr_source: Option<String>,
    #[serde(
        default,
        rename = "InitialHFRMeasured",
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_hfr_measured: Option<bool>,
    #[serde(
        default,
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub hyperbolic_minimum_std_error: Option<f64>,
    #[serde(
        default,
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub hyperbolic_reduced_chi_squared: Option<f64>,
    #[serde(
        default,
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub hyperbolic_leave_one_out_std_error: Option<f64>,
    #[serde(
        default,
        deserialize_with = "de_optional_nonnegative_i32",
        skip_serializing_if = "Option::is_none"
    )]
    pub accepted_star_count_min: Option<i32>,
    #[serde(
        default,
        deserialize_with = "de_optional_nonnegative_i32",
        skip_serializing_if = "Option::is_none"
    )]
    pub accepted_star_count_max: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperbolic_fit_model_chosen: Option<HocusHyperbolicFitModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<AutofocusRegion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hocus_focus_algorithm: Option<HocusFocusAlgorithm>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum HocusHyperbolicFitModel {
    Name(String),
    Code(i32),
}

impl HocusHyperbolicFitModel {
    pub fn display_name(&self) -> String {
        match self {
            Self::Name(name) => name.clone(),
            Self::Code(0) => "Symmetric".to_string(),
            Self::Code(1) => "Uneven Blend".to_string(),
            Self::Code(2) => "Tilted Hyperbola".to_string(),
            Self::Code(3) => "Smooth Blend".to_string(),
            Self::Code(4) => "Hybrid (Best Fit)".to_string(),
            Self::Code(code) => format!("Model {code}"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct AutofocusRegion {
    pub index: i32,
    pub outer_boundary: AutofocusRatioRect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_crop_boundary: Option<AutofocusRatioRect>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct AutofocusRatioRect {
    #[serde(deserialize_with = "de_finite_f64")]
    pub start_x: f64,
    #[serde(deserialize_with = "de_finite_f64")]
    pub start_y: f64,
    #[serde(deserialize_with = "de_finite_f64")]
    pub width: f64,
    #[serde(deserialize_with = "de_finite_f64")]
    pub height: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "PascalCase")]
pub struct HocusFocusAlgorithm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate_hfr_improvement: Option<bool>,
    #[serde(
        default,
        rename = "HFRImprovementThreshold",
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub hfr_improvement_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weighted_hyperbolic_fit_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_hyperbolic_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_rejection_criterion: Option<String>,
    #[serde(
        default,
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub reduced_chi_squared_rejection_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_outlier_rejections: Option<i32>,
    #[serde(
        default,
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub outlier_rejection_confidence: Option<f64>,
    #[serde(
        default,
        rename = "RSquaredThreshold",
        deserialize_with = "de_optional_finite_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub r_squared_threshold: Option<f64>,
    #[serde(default, rename = "ModelPSF", skip_serializing_if = "Option::is_none")]
    pub model_psf: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_optimized_settings: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_optimized_settings: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_binning: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_average: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_detection_mode: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct FocusPoint {
    #[serde(deserialize_with = "de_finite_f64")]
    pub position: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub value: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub error: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct PreviousFocusPoint {
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub position: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub value: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub error: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct IntersectionPoint {
    #[serde(deserialize_with = "de_finite_f64")]
    pub position: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub value: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub error: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Intersections {
    pub trend_line_intersection: Option<IntersectionPoint>,
    pub hyperbolic_minimum: Option<IntersectionPoint>,
    pub quadratic_minimum: Option<IntersectionPoint>,
    pub gaussian_maximum: Option<IntersectionPoint>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Fittings {
    pub quadratic: String,
    pub hyperbolic: String,
    pub gaussian: String,
    pub left_trend: String,
    pub right_trend: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct RSquares {
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub quadratic: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub hyperbolic: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub left_trend: f64,
    #[serde(deserialize_with = "de_f64_tolerant")]
    pub right_trend: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct BacklashCompensation {
    pub backlash_compensation_model: String,
    #[serde(rename = "BacklashIN")]
    pub backlash_in: i32,
    #[serde(rename = "BacklashOUT")]
    pub backlash_out: i32,
}

impl AutofocusResponse {
    /// Get the best R-squared value among all fitting methods
    pub fn get_best_r_squared(&self) -> Option<f64> {
        let r_squares = &self.response.r_squares;
        [
            r_squares.quadratic,
            r_squares.hyperbolic,
            r_squares.left_trend,
            r_squares.right_trend,
        ]
        .into_iter()
        .filter(|value| value.is_finite())
        .max_by(f64::total_cmp)
    }

    /// Check if the autofocus was successful based on criteria
    pub fn is_successful(&self) -> bool {
        self.success && self.response.calculated_focus_point.error == 0.0
    }
}

impl AutofocusData {
    pub fn filter_name(&self) -> &str {
        let name = self.filter.trim();
        if name.is_empty() { "No filter" } else { name }
    }

    pub fn is_contrast(&self) -> bool {
        self.method.eq_ignore_ascii_case("CONTRASTDETECTION")
    }

    pub fn measurement_name(&self) -> &'static str {
        if self.is_contrast() {
            "Contrast"
        } else {
            "HFR"
        }
    }

    pub fn method_summary(&self) -> String {
        let method = if self.is_contrast() {
            "Contrast"
        } else if self.method.eq_ignore_ascii_case("STARHFR") {
            "Star HFR"
        } else {
            self.method.as_str()
        };
        let fitting = match self.fitting.to_ascii_uppercase().as_str() {
            "TRENDLINES" => "Trend lines".to_string(),
            "PARABOLIC" => "Parabolic".to_string(),
            "TRENDPARABOLIC" => "Trend + parabolic".to_string(),
            "HYPERBOLIC" => "Hyperbolic".to_string(),
            "TRENDHYPERBOLIC" => "Trend + hyperbolic".to_string(),
            "GAUSSIAN" => "Gaussian".to_string(),
            _ => self.fitting.clone(),
        };
        let mut summary = format!("{method} · {fitting}");
        if let Some(model) = &self.hyperbolic_fit_model_chosen
            && self.fitting.to_ascii_uppercase().contains("HYPERBOLIC")
        {
            summary.push_str(" · ");
            summary.push_str(&model.display_name());
        }
        summary
    }

    pub fn selected_r_squared_values(&self) -> Vec<(&'static str, f64)> {
        let values = match self.fitting.to_ascii_uppercase().as_str() {
            "TRENDLINES" => vec![
                ("Left", self.r_squares.left_trend),
                ("Right", self.r_squares.right_trend),
            ],
            "PARABOLIC" => vec![("Parabolic", self.r_squares.quadratic)],
            "TRENDPARABOLIC" => vec![
                ("Parabolic", self.r_squares.quadratic),
                ("Left", self.r_squares.left_trend),
                ("Right", self.r_squares.right_trend),
            ],
            "HYPERBOLIC" => vec![("Hyperbolic", self.r_squares.hyperbolic)],
            "TRENDHYPERBOLIC" => vec![
                ("Hyperbolic", self.r_squares.hyperbolic),
                ("Left", self.r_squares.left_trend),
                ("Right", self.r_squares.right_trend),
            ],
            // N.I.N.A.'s contrast/Gaussian report has no Gaussian R² field.
            "GAUSSIAN" => Vec::new(),
            _ => vec![
                ("Quadratic", self.r_squares.quadratic),
                ("Hyperbolic", self.r_squares.hyperbolic),
                ("Left", self.r_squares.left_trend),
                ("Right", self.r_squares.right_trend),
            ],
        };
        values
            .into_iter()
            .filter(|(_, value)| value.is_finite())
            .collect()
    }

    pub fn fit_quality_summary(&self) -> Option<String> {
        let values = self.selected_r_squared_values();
        if values.is_empty() {
            return None;
        }
        if values.len() == 1 {
            return Some(format!("{:.4}", values[0].1));
        }
        Some(
            values
                .into_iter()
                .map(|(name, value)| format!("{name} {value:.4}"))
                .collect::<Vec<_>>()
                .join(" · "),
        )
    }

    pub fn selected_fit_points(&self) -> Vec<(&'static str, &IntersectionPoint)> {
        let mut points = Vec::new();
        let fitting = self.fitting.to_ascii_uppercase();
        if fitting.contains("TREND")
            && let Some(point) = &self.intersections.trend_line_intersection
        {
            points.push(("Trend intersection", point));
        }
        if fitting.contains("PARABOLIC")
            && let Some(point) = &self.intersections.quadratic_minimum
        {
            points.push(("Parabolic minimum", point));
        }
        if fitting.contains("HYPERBOLIC")
            && let Some(point) = &self.intersections.hyperbolic_minimum
        {
            points.push(("Hyperbolic minimum", point));
        }
        if fitting == "GAUSSIAN"
            && let Some(point) = &self.intersections.gaussian_maximum
        {
            points.push(("Gaussian maximum", point));
        }
        points
    }

    pub fn is_hocus_focus(&self) -> bool {
        self.auto_focuser_name.eq_ignore_ascii_case("Hocus Focus")
            || self.final_hfr.is_some()
            || self.hyperbolic_fit_model_chosen.is_some()
    }

    pub fn accepted_star_count_summary(&self) -> Option<String> {
        match (
            self.accepted_star_count_min.filter(|value| *value >= 0),
            self.accepted_star_count_max.filter(|value| *value >= 0),
        ) {
            (Some(minimum), Some(maximum)) if minimum != maximum => {
                Some(format!("{minimum}–{maximum}"))
            }
            (Some(count), Some(_)) | (Some(count), None) | (None, Some(count)) => {
                Some(count.to_string())
            }
            (None, None) => None,
        }
    }

    pub fn region_summary(&self) -> Option<String> {
        let region = self.region.as_ref()?;
        let outer = &region.outer_boundary;
        let is_full = outer.start_x.abs() < f64::EPSILON
            && outer.start_y.abs() < f64::EPSILON
            && (outer.width - 1.0).abs() < f64::EPSILON
            && (outer.height - 1.0).abs() < f64::EPSILON
            && region.inner_crop_boundary.is_none();
        if is_full {
            Some("Full frame".to_string())
        } else {
            Some(format!(
                "Region {} · {:.0}% × {:.0}%",
                region.index,
                outer.width * 100.0,
                outer.height * 100.0
            ))
        }
    }

    /// HFR before the run: the initial focus point when NINA measured it,
    /// otherwise the measured point taken at the initial position (NINA
    /// reports the initial value as "NaN" when it skipped that exposure).
    pub fn initial_hfr(&self) -> Option<f64> {
        if self.initial_hfr_measured == Some(false) {
            return None;
        }
        if self.initial_focus_point.value.is_finite() {
            return Some(self.initial_focus_point.value);
        }
        self.measure_points
            .iter()
            .find(|p| p.position == self.initial_focus_point.position)
            .map(|p| p.value)
            .filter(|v| v.is_finite())
    }

    /// HFR after the run: Hocus Focus's post-move measurement when present,
    /// otherwise the fitted value at the calculated focus point.
    pub fn final_hfr(&self) -> Option<f64> {
        self.final_hfr
            .or_else(|| Some(self.calculated_focus_point.value).filter(|value| value.is_finite()))
    }

    pub fn final_measurement_label(&self) -> String {
        match self.final_hfr_source.as_deref() {
            Some("measured_validation") => format!("{} After (measured)", self.measurement_name()),
            Some("fitted_estimate") => format!("{} After (estimated)", self.measurement_name()),
            _ => format!("{} After", self.measurement_name()),
        }
    }

    pub fn fit_acceptance_summary(&self) -> Option<String> {
        let algorithm = self.hocus_focus_algorithm.as_ref()?;
        match algorithm.fit_rejection_criterion.as_deref() {
            Some("Reduced χ²") => algorithm
                .reduced_chi_squared_rejection_threshold
                .map(|threshold| format!("Reduced χ² ≤ {threshold:.3}")),
            Some("R²") => algorithm
                .r_squared_threshold
                .map(|threshold| format!("R² ≥ {threshold:.3}")),
            Some(criterion) => Some(criterion.to_string()),
            None => None,
        }
    }

    pub fn detection_summary(&self) -> Option<String> {
        let algorithm = self.hocus_focus_algorithm.as_ref()?;
        let mut parts = Vec::new();
        if let Some(mode) = &algorithm.star_detection_mode {
            parts.push(mode.clone());
        }
        if let Some(average) = &algorithm.measurement_average {
            parts.push(average.clone());
        }
        if let Some(binning) = algorithm.detection_binning {
            parts.push(format!("{binning}× binning"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Get focus positions in ascending order
    pub fn get_focus_positions(&self) -> Vec<f64> {
        let mut positions: Vec<f64> = self.measure_points.iter().map(|p| p.position).collect();
        positions.sort_by(f64::total_cmp);
        positions
    }

    /// Get the focus range (min to max position tested)
    pub fn get_focus_range(&self) -> (f64, f64) {
        let positions = self.get_focus_positions();
        (
            *positions.first().unwrap_or(&0.0),
            *positions.last().unwrap_or(&0.0),
        )
    }

    /// Get the best HFR (lowest value) from all measurement points
    pub fn get_best_measured_hfr(&self) -> Option<f64> {
        self.measure_points
            .iter()
            .map(|p| p.value)
            .filter(|value| value.is_finite())
            .min_by(f64::total_cmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_autofocus_response() {
        let json_content = std::fs::read_to_string("example_last_af.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();

        // Test basic response structure
        assert_eq!(response.status_code, 200);
        assert!(response.success);
        assert_eq!(response.response_type, "API");
        assert!(response.error.is_empty());

        // Test autofocus data
        let af_data = &response.response;
        assert_eq!(af_data.version, 2);
        assert_eq!(af_data.filter, "OIII");
        assert_eq!(af_data.auto_focuser_name, "NINA");
        assert_eq!(af_data.star_detector_name, "NINA");
        assert_eq!(af_data.temperature, 21.3);
        assert_eq!(af_data.method, "STARHFR");
        assert_eq!(af_data.fitting, "TRENDHYPERBOLIC");

        // Test focus points
        assert_eq!(af_data.calculated_focus_point.position, 4068.0);
        assert_eq!(af_data.calculated_focus_point.value, 2.90813054456021);
        assert_eq!(af_data.calculated_focus_point.error, 0.0);

        // Test that initial focus point has NaN value (properly parsed)
        assert_eq!(af_data.initial_focus_point.position, 4092.0);
        assert!(af_data.initial_focus_point.value.is_nan());
        assert_eq!(af_data.initial_focus_point.error, 0.0);

        // Test measure points
        assert_eq!(af_data.measure_points.len(), 10);
        assert_eq!(af_data.measure_points[0].position, 3992.0);
        assert!((af_data.measure_points[0].value - 3.9320351318958195).abs() < 1e-10);

        // Test R-squared values
        assert!((af_data.r_squares.hyperbolic - 0.9894178774335628).abs() < 1e-10);
        assert!((af_data.r_squares.quadratic - 0.9810757827720883).abs() < 1e-10);

        // Test backlash compensation
        assert_eq!(
            af_data.backlash_compensation.backlash_compensation_model,
            "OVERSHOOT"
        );
        assert_eq!(af_data.backlash_compensation.backlash_in, 0);
        assert_eq!(af_data.backlash_compensation.backlash_out, 20);
    }

    #[test]
    fn test_autofocus_response_methods() {
        let json_content = std::fs::read_to_string("example_last_af.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();

        // Test convenience methods
        // Test R-squared analysis
        let best_r_squared = response.get_best_r_squared().unwrap();
        assert!(best_r_squared > 0.98); // Should be the hyperbolic fit (0.9894)

        // Test success criteria
        assert!(response.is_successful());
    }

    #[test]
    fn test_autofocus_data_analysis() {
        let json_content = std::fs::read_to_string("example_last_af.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();
        let af_data = &response.response;

        // Test focus range analysis
        let (min_pos, max_pos) = af_data.get_focus_range();
        assert_eq!(min_pos, 3992.0);
        assert_eq!(max_pos, 4172.0);

        // Test position ordering
        let positions = af_data.get_focus_positions();
        assert_eq!(positions.len(), 10);
        assert!(positions.windows(2).all(|w| w[0] <= w[1])); // Check sorted

        // Test HFR analysis
        let best_hfr = af_data.get_best_measured_hfr().unwrap();
        assert!(best_hfr < 3.1); // Should find the minimum HFR (around 3.009)
    }

    #[test]
    fn test_parse_autofocus_response_2() {
        let json_content = std::fs::read_to_string("example_last_af_2.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();

        // Test basic response structure
        assert_eq!(response.status_code, 200);
        assert!(response.success);
        assert_eq!(response.response_type, "API");
        assert!(response.error.is_empty());

        // Test autofocus data
        let af_data = &response.response;
        assert_eq!(af_data.version, 2);
        assert_eq!(af_data.filter, "SII");
        assert_eq!(af_data.auto_focuser_name, "Hocus Focus");
        assert_eq!(af_data.star_detector_name, "NINA");
        assert_eq!(af_data.temperature, 24.6);
        assert_eq!(af_data.method, "STARHFR");
        assert_eq!(af_data.fitting, "TRENDHYPERBOLIC");

        // Test focus points
        assert_eq!(af_data.calculated_focus_point.position, 4186.0);
        assert!((af_data.calculated_focus_point.value - 2.6632989580477844).abs() < 1e-10);
        assert_eq!(af_data.calculated_focus_point.error, 0.0);

        // Test initial focus point
        assert_eq!(af_data.initial_focus_point.position, 4076.0);
        assert!((af_data.initial_focus_point.value - 5.024422888144213).abs() < 1e-10);

        // Test measure points
        assert_eq!(af_data.measure_points.len(), 11);
        assert_eq!(af_data.measure_points[0].position, 4056.0);
        assert_eq!(af_data.measure_points[10].position, 4256.0);

        // Test R-squared values
        assert!((af_data.r_squares.hyperbolic - 0.991774159363382).abs() < 1e-10);
        assert!(af_data.r_squares.quadratic.is_nan());

        // Test intersections (only some are present in this example)
        assert!(af_data.intersections.trend_line_intersection.is_some());
        assert!(af_data.intersections.hyperbolic_minimum.is_some());
        assert!(af_data.intersections.quadratic_minimum.is_none());
        assert!(af_data.intersections.gaussian_maximum.is_none());

        // Check the hyperbolic minimum has fractional position
        if let Some(hyperbolic) = &af_data.intersections.hyperbolic_minimum {
            assert!((hyperbolic.position - 4188.955065493704).abs() < 1e-10);
        }

        // Test that it's a successful autofocus
        assert!(response.is_successful());

        // Test focus change
        assert_eq!(af_data.initial_focus_point.position, 4076.0);
        assert_eq!(af_data.calculated_focus_point.position, 4186.0);
        let position_change =
            af_data.calculated_focus_point.position - af_data.initial_focus_point.position;
        assert_eq!(position_change, 110.0);
    }

    #[test]
    fn native_nina_decimal_focus_positions_are_accepted() {
        let json_content = std::fs::read_to_string("example_last_af.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        value["Response"]["InitialFocusPoint"]["Position"] = serde_json::json!(4092.0);
        value["Response"]["CalculatedFocusPoint"]["Position"] = serde_json::json!(4068.0);
        value["Response"]["PreviousFocusPoint"]["Position"] = serde_json::json!(4068.0);
        value["Response"]["MeasurePoints"][0]["Position"] = serde_json::json!(3992.0);

        let response: AutofocusResponse = serde_json::from_value(value).unwrap();
        assert_eq!(response.response.initial_focus_point.position, 4092.0);
        assert_eq!(response.response.calculated_focus_point.position, 4068.0);
        assert_eq!(response.response.measure_points[0].position, 3992.0);
    }

    #[test]
    fn modern_hocus_focus_fractional_positions_keep_full_precision() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();
        let encoded = serde_json::to_value(&response.response.calculated_focus_point).unwrap();
        assert_eq!(encoded["Position"].as_f64(), Some(4188.955065493704));
        let data = &response.response;

        assert_eq!(data.auto_focuser_name, "Hocus Focus");
        assert!((data.calculated_focus_point.position - 4188.955065493704).abs() < 1e-12);
        assert_eq!(data.measure_points[0].position, 4056.25);
        assert_eq!(data.measure_points[1].position, 4076.5);
        assert_eq!(data.get_focus_positions(), vec![4056.25, 4076.5, 4196.75]);
        assert_eq!(data.get_focus_range(), (4056.25, 4196.75));
        assert_eq!(data.final_hfr(), Some(2.22));
        assert_eq!(data.hyperbolic_minimum_std_error, Some(0.7));
        assert_eq!(data.hyperbolic_reduced_chi_squared, Some(0.0017));
        assert_eq!(data.hyperbolic_leave_one_out_std_error, Some(0.57));
        assert_eq!(
            data.accepted_star_count_summary().as_deref(),
            Some("83–119")
        );
        assert_eq!(data.final_measurement_label(), "HFR After (measured)");
        assert_eq!(
            data.fit_acceptance_summary().as_deref(),
            Some("Reduced χ² ≤ 5.000")
        );
        assert_eq!(
            data.detection_summary().as_deref(),
            Some("Optimized · Mean + outlier detection · 2× binning")
        );
        assert_eq!(
            data.hocus_focus_algorithm
                .as_ref()
                .and_then(|algorithm| algorithm.has_optimized_settings),
            Some(true)
        );
        assert_eq!(
            data.hyperbolic_fit_model_chosen
                .as_ref()
                .map(HocusHyperbolicFitModel::display_name)
                .as_deref(),
            Some("Tilted Hyperbola")
        );
        assert_eq!(
            data.method_summary(),
            "Star HFR · Trend + hyperbolic · Tilted Hyperbola"
        );
        assert_eq!(
            data.region_summary().as_deref(),
            Some("Region 3 · 50% × 50%")
        );
    }

    #[test]
    fn hocus_fit_model_accepts_string_and_unknown_future_codes() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        value["Response"]["HyperbolicFitModelChosen"] = serde_json::json!("Smooth Blend");
        let named: AutofocusResponse = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            named
                .response
                .hyperbolic_fit_model_chosen
                .as_ref()
                .unwrap()
                .display_name(),
            "Smooth Blend"
        );

        value["Response"]["HyperbolicFitModelChosen"] = serde_json::json!(42);
        let future: AutofocusResponse = serde_json::from_value(value).unwrap();
        assert_eq!(
            future
                .response
                .hyperbolic_fit_model_chosen
                .as_ref()
                .unwrap()
                .display_name(),
            "Model 42"
        );
    }

    #[test]
    fn selected_fit_quality_matches_every_nina_feedback_mode() {
        let json_content = std::fs::read_to_string("example_last_af.json").unwrap();
        let response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();
        let cases = [
            ("TRENDLINES", 2, 1),
            ("PARABOLIC", 1, 1),
            ("TRENDPARABOLIC", 3, 2),
            ("HYPERBOLIC", 1, 1),
            ("TRENDHYPERBOLIC", 3, 2),
        ];
        for (fitting, expected_quality_count, expected_marker_count) in cases {
            let mut report = response.clone();
            report.response.fitting = fitting.to_string();
            assert_eq!(
                report.response.selected_r_squared_values().len(),
                expected_quality_count,
                "{fitting}"
            );
            assert_eq!(
                report.response.selected_fit_points().len(),
                expected_marker_count,
                "{fitting}"
            );
            assert!(report.response.fit_quality_summary().is_some(), "{fitting}");
            assert!(report.is_successful(), "{fitting}");
        }

        let mut contrast = response;
        contrast.response.method = "CONTRASTDETECTION".to_string();
        contrast.response.fitting = "GAUSSIAN".to_string();
        contrast.response.r_squares.quadratic = f64::NAN;
        contrast.response.r_squares.hyperbolic = f64::NAN;
        contrast.response.r_squares.left_trend = f64::NAN;
        contrast.response.r_squares.right_trend = f64::NAN;
        assert!(contrast.response.selected_r_squared_values().is_empty());
        assert_eq!(contrast.response.method_summary(), "Contrast · Gaussian");
        assert_eq!(contrast.response.measurement_name(), "Contrast");
        assert_eq!(contrast.response.selected_fit_points().len(), 1);
        assert!(contrast.is_successful());
    }

    #[test]
    fn hocus_validation_flags_distinguish_measured_and_estimated_feedback() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();
        response.response.initial_hfr_measured = Some(false);
        response.response.final_hfr_source = Some("fitted_estimate".to_string());
        response.response.initial_focus_point.value = 0.0;

        assert_eq!(response.response.initial_hfr(), None);
        assert_eq!(
            response.response.final_measurement_label(),
            "HFR After (estimated)"
        );
        assert!(response.is_successful());
    }

    #[test]
    fn blank_hocus_filter_has_a_nonempty_chat_label() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut response: AutofocusResponse = serde_json::from_str(&json_content).unwrap();
        response.response.filter = "  ".to_string();
        assert_eq!(response.response.filter_name(), "No filter");
    }

    #[test]
    fn unavailable_hocus_enrichment_falls_back_without_rejecting_the_report() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        value["Response"]["AcceptedStarCountMin"] = serde_json::json!(-1);
        value["Response"]["AcceptedStarCountMax"] = serde_json::json!(-1);
        value["Response"]["HyperbolicMinimumStdError"] = serde_json::json!("NaN");
        value["Response"]["HyperbolicReducedChiSquared"] = serde_json::json!("Infinity");
        value["Response"]["HyperbolicLeaveOneOutStdError"] = serde_json::json!("-Infinity");
        value["Response"]["HocusFocusAlgorithm"]["RSquaredThreshold"] = serde_json::json!("NaN");

        let response: AutofocusResponse = serde_json::from_value(value).unwrap();
        let data = &response.response;
        assert_eq!(data.accepted_star_count_min, None);
        assert_eq!(data.accepted_star_count_max, None);
        assert_eq!(data.accepted_star_count_summary(), None);
        assert_eq!(data.hyperbolic_minimum_std_error, None);
        assert_eq!(data.hyperbolic_reduced_chi_squared, None);
        assert_eq!(data.hyperbolic_leave_one_out_std_error, None);
        assert_eq!(
            data.hocus_focus_algorithm
                .as_ref()
                .and_then(|algorithm| algorithm.r_squared_threshold),
            None
        );
        assert!(response.is_successful());
    }

    #[test]
    fn focus_point_positions_must_be_finite() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        value["Response"]["CalculatedFocusPoint"]["Position"] = serde_json::json!("NaN");
        assert!(serde_json::from_value::<AutofocusResponse>(value).is_err());
    }

    #[test]
    fn unavailable_autofocus_measurements_keep_their_nonfinite_meaning() {
        let json_content = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json_content).unwrap();
        value["Response"]["Temperature"] = serde_json::json!("NaN");
        value["Response"]["InitialFocusPoint"]["Error"] = serde_json::json!("-Infinity");
        value["Response"]["CalculatedFocusPoint"]["Error"] = serde_json::json!("Infinity");
        value["Response"]["PreviousFocusPoint"]["Position"] = serde_json::json!("NaN");
        value["Response"]["Intersections"]["HyperbolicMinimum"]["Error"] = serde_json::json!("NaN");
        value["Response"]["MeasurePoints"][0]["Value"] = serde_json::json!("NaN");
        value["Response"]["RSquares"]["Quadratic"] = serde_json::json!("NaN");
        value["Response"]["RSquares"]["Hyperbolic"] = serde_json::json!("Infinity");
        value["Response"]["RSquares"]["LeftTrend"] = serde_json::json!("-Infinity");
        value["Response"]["RSquares"]["RightTrend"] = serde_json::json!("NaN");

        let response: AutofocusResponse = serde_json::from_value(value).unwrap();
        let data = &response.response;
        assert!(data.temperature.is_nan());
        assert_eq!(data.initial_focus_point.error, f64::NEG_INFINITY);
        assert_eq!(data.calculated_focus_point.error, f64::INFINITY);
        assert!(data.previous_focus_point.position.is_nan());
        assert!(
            data.intersections
                .hyperbolic_minimum
                .as_ref()
                .expect("hyperbolic minimum")
                .error
                .is_nan()
        );
        assert!(data.get_best_measured_hfr().is_some());
        assert_eq!(response.get_best_r_squared(), None);
        assert!(!response.is_successful());
    }
}
