//! Renders NINA graph data as PNG charts for chat notifications:
//!
//! * the Direct guide history graph in the style of the
//!   PHD2/NINA guiding chart — RA/Dec error traces on the left axis,
//!   signed correction-pulse bars on the right axis, dither markers,
//!   and an RMS summary in the title;
//! * the Direct autofocus run — measured HFR
//!   points with error bars plus initial/calculated position markers.
//!
//! Charts are drawn with rizzma, which embeds its own font and encodes PNG in
//! memory, so rendering needs no system font libraries on any release target.

use crate::autofocus::AutofocusData;
use crate::guider::GuideStepsHistory;
use rizzma::artist::{MarkerStyle, Patch, Rgba};
use rizzma::{Axes, Figure, RcParams};
use thiserror::Error;

const RA_COLOR: Rgba = rgb(77, 139, 232);
const DEC_COLOR: Rgba = rgb(232, 77, 77);
const DITHER_COLOR: Rgba = rgb(160, 160, 90);
const HFR_COLOR: Rgba = rgb(96, 189, 232);
const FOCUS_COLOR: Rgba = rgb(96, 209, 122);
const FIT_COLOR: Rgba = rgb(213, 143, 255);

const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba {
        r: r as f64 / 255.0,
        g: g as f64 / 255.0,
        b: b as f64 / 255.0,
        a: 1.0,
    }
}

#[derive(Debug, Error)]
pub enum ChartError {
    #[error("not enough guide steps to draw a graph ({0} steps)")]
    NotEnoughData(usize),
    #[error("failed to render guide chart: {0}")]
    Render(String),
}

/// A 900×480 dark figure with one axes holding the plot.
fn dark_figure() -> Figure {
    let mut fig = Figure::new(9.0, 4.8).with_rcparams(RcParams::dark());
    fig.add_subplot(1, 1, 1);
    fig
}

fn encode(fig: &Figure) -> Result<Vec<u8>, ChartError> {
    fig.encode_png()
        .map_err(|e| ChartError::Render(e.to_string()))
}

/// Render the guide graph to PNG bytes. Fails when fewer than two guide
/// steps are present.
pub fn render_guider_graph_png(history: &GuideStepsHistory) -> Result<Vec<u8>, ChartError> {
    if !history.has_graph_data() {
        return Err(ChartError::NotEnoughData(history.guide_steps.len()));
    }

    let steps = &history.guide_steps;
    let n = steps.len();

    // Error axis range: prefer NINA's configured range, fall back to the
    // data with a little headroom when the payload range is degenerate.
    let (mut min_y, mut max_y) = (history.min_y, history.max_y);
    let range_valid = min_y.is_finite() && max_y.is_finite() && min_y < max_y;
    if !range_valid {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for s in steps {
            for v in [s.ra_distance_raw_display, s.dec_distance_raw_display] {
                if v.is_finite() {
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
        }
        if lo.is_finite() && hi.is_finite() && lo < hi {
            let pad = (hi - lo) * 0.15;
            min_y = lo - pad;
            max_y = hi + pad;
        } else {
            min_y = -1.0;
            max_y = 1.0;
        }
    }

    // Duration axis: symmetric around zero so signed pulses read naturally.
    let mut dur_limit = history
        .max_duration_y
        .abs()
        .max(history.min_duration_y.abs());
    if dur_limit.is_nan() || dur_limit <= 0.0 {
        dur_limit = steps
            .iter()
            .flat_map(|s| [s.ra_duration.abs(), s.dec_duration.abs()])
            .filter(|v| v.is_finite())
            .fold(0.0_f64, f64::max);
    }
    if dur_limit.is_nan() || dur_limit <= 0.0 {
        dur_limit = 1.0;
    }

    let unit = history.scale_unit();
    let title = match history.rms_summary() {
        Some(rms) => format!("Guiding  —  {}", rms),
        None => "Guiding".to_string(),
    };

    let mut fig = dark_figure();
    let pulses = fig.twinx(0);
    let ax: &mut Axes = &mut fig.axes_mut()[0];
    ax.set_title(title)
        .set_xlim(0.0, n as f64)
        .set_ylim(min_y, max_y)
        .set_xlabel("Guide step")
        .set_ylabel(format!("Error ({})", unit));
    ax.yaxis_mut().set_grid(true);

    // Dither markers: vertical lines across the error axis.
    for (i, s) in steps.iter().enumerate() {
        if GuideStepsHistory::is_dither_step(s) {
            ax.axvline_with(i as f64 + 0.5, DITHER_COLOR.with_alpha(0.6), 1.0);
        }
    }

    // Error traces. NaN samples (guider gaps) break the line instead of
    // drawing bogus segments.
    let x: Vec<f64> = (0..n).map(|i| i as f64 + 0.5).collect();
    let ra: Vec<f64> = steps.iter().map(|s| s.ra_distance_raw_display).collect();
    let dec: Vec<f64> = steps.iter().map(|s| s.dec_distance_raw_display).collect();
    for (color, y) in [(RA_COLOR, ra), (DEC_COLOR, dec)] {
        ax.plot_with_color(&x, &y, color).set_linewidth(2.0);
    }
    ax.legend(vec![(RA_COLOR, "RA".into()), (DEC_COLOR, "Dec".into())]);

    // Correction pulses as thin translucent bars on the right (ms) axis. RA
    // bars sit slightly left and Dec bars slightly right so simultaneous
    // pulses stay distinguishable.
    let ax = &mut fig.axes_mut()[pulses];
    ax.set_ylim(-dur_limit, dur_limit)
        .set_ylabel("Correction (ms)");
    let bar_half = 0.18;
    for (i, s) in steps.iter().enumerate() {
        for (duration, color, shift) in [
            (s.ra_duration, RA_COLOR, -bar_half),
            (s.dec_duration, DEC_COLOR, bar_half),
        ] {
            if duration.is_finite() && duration != 0.0 {
                let left = i as f64 + 0.5 + shift - bar_half;
                ax.add_patch(
                    Patch::rectangle(left, 0.0, 2.0 * bar_half, duration)
                        .facecolor(Some(color.with_alpha(0.35)))
                        .edgecolor(None),
                );
            }
        }
    }

    encode(&fig)
}

/// Render an autofocus run to PNG bytes: measured HFR vs focuser position
/// with error bars, a connecting line, and vertical markers for the
/// initial and calculated focus positions. Fails when fewer than two
/// finite measurement points are present.
pub fn render_autofocus_graph_png(af: &AutofocusData) -> Result<Vec<u8>, ChartError> {
    let mut points: Vec<(f64, f64, f64)> = af
        .measure_points
        .iter()
        .filter(|p| p.value.is_finite())
        .map(|p| {
            let error = if p.error.is_finite() {
                p.error.max(0.0)
            } else {
                0.0
            };
            (p.position, p.value, error)
        })
        .collect();
    if points.len() < 2 {
        return Err(ChartError::NotEnoughData(points.len()));
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));

    let measurement_name = af.measurement_name();
    let measurement_change = match (af.initial_hfr(), af.final_hfr()) {
        (Some(before), Some(after)) => {
            format!("{measurement_name} {before:.2} → {after:.2}")
        }
        (None, Some(after)) => format!("{measurement_name} → {after:.2}"),
        _ => measurement_name.to_string(),
    };
    let engine = if af.is_hocus_focus() {
        "Hocus Focus"
    } else {
        "Autofocus"
    };
    let title = format!(
        "{engine}  —  {measurement_change}  —  {}  ({})",
        af.method_summary(),
        af.filter_name()
    );

    let mut fig = dark_figure();
    let ax = &mut fig.axes_mut()[0];
    ax.set_title(title)
        .set_xlabel("Focuser position")
        .set_ylabel(measurement_name);
    ax.grid(true);

    let markers = [
        (af.initial_focus_point.position, DITHER_COLOR, "Initial"),
        (
            af.calculated_focus_point.position,
            FOCUS_COLOR,
            "Calculated",
        ),
    ];
    let fit_points: Vec<_> = af
        .selected_fit_points()
        .into_iter()
        .filter(|(_, point)| point.value.is_finite())
        .collect();
    let (x, y): (Vec<f64>, Vec<f64>) = points.iter().map(|&(x, v, _)| (x, v)).unzip();

    // Frame every measurement, fit point and position marker.
    let (x_lo, x_hi) = x
        .iter()
        .copied()
        .chain(fit_points.iter().map(|(_, point)| point.position))
        .chain(markers.iter().map(|m| m.0))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p), hi.max(p))
        });
    let x_pad = ((x_hi - x_lo) * 0.05).max(1.0);
    ax.set_xlim(x_lo - x_pad, x_hi + x_pad);

    // Position markers first so data draws on top of them.
    let mut legend = Vec::new();
    for (pos, color, label) in markers {
        ax.axvline_with(pos, color.with_alpha(0.7), 2.0);
        legend.push((color, label.to_string()));
    }

    // The measurements joined in focuser order, with error bars and points.
    let first_line = ax.lines().len();
    let error: Vec<f64> = points.iter().map(|&(_, _, e)| e).collect();
    ax.errorbar(&x, &y, &error);
    for (i, line) in ax.lines_mut()[first_line..].iter_mut().enumerate() {
        match i {
            0 => line.set_color(HFR_COLOR).set_linewidth(2.0),
            _ => line.set_color(HFR_COLOR.with_alpha(0.5)),
        };
    }
    let dots = ax.scatter(&x, &y);
    *dots = dots.clone().with_facecolors(vec![HFR_COLOR]);
    legend.push((HFR_COLOR, format!("Measured {measurement_name}")));

    let triangle = MarkerStyle::from_char('^').expect("'^' is a known marker");
    for (label, point) in fit_points {
        let marker = ax.scatter(&[point.position], &[point.value]);
        *marker = marker
            .clone()
            .with_marker(triangle.path().clone())
            .with_facecolors(vec![FIT_COLOR])
            .with_sizes(vec![10.0]);
        legend.push((FIT_COLOR, label.to_string()));
    }
    ax.legend(legend);

    encode(&fig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guider::GuiderGraphResponse;

    fn sample_history() -> GuideStepsHistory {
        let json = std::fs::read_to_string("example_guider_graph.json").unwrap();
        let parsed: GuiderGraphResponse = serde_json::from_str(&json).unwrap();
        parsed.response
    }

    #[test]
    fn test_render_sample_graph() {
        let history = sample_history();
        let png = render_guider_graph_png(&history).unwrap();
        // PNG signature
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']
        );
        assert!(png.len() > 1000);
    }

    #[test]
    fn test_render_nina_direct_plugin_graph_contract() {
        // This is the exact PascalCase envelope and field shape emitted by
        // NinaDirectDataProvider, including a fractional interval, signed
        // correction pulses, and the plugin's string dither marker.
        let json = r#"{
            "Response": {
                "RMS": {
                    "RA": 1.0, "Dec": 1.0, "Total": 1.4142135623730951,
                    "RAText": "RA: 1.00 (2.00\")",
                    "DecText": "Dec: 1.00 (2.00\")",
                    "TotalText": "Tot: 1.41 (2.83\")",
                    "PeakRAText": "RA Peak: 1.00 (2.00\")",
                    "PeakDecText": "Dec Peak: 2.00 (4.00\")",
                    "Scale": 2.0, "PeakRA": 1.0, "PeakDec": 2.0,
                    "DataPoints": 2
                },
                "Interval": 1.1, "MaxY": 4.4, "MinY": -4.4,
                "MaxDurationY": 140.0, "MinDurationY": -140.0,
                "GuideSteps": [
                    {"Id":1,"IdOffsetLeft":0.85,"IdOffsetRight":1.15,"RADistanceRaw":-1.0,"RADistanceRawDisplay":-2.0,"RADuration":-120.0,"DECDistanceRaw":0.0,"DECDistanceRawDisplay":0.0,"DECDuration":80.0,"Dither":"NO"},
                    {"Id":2,"IdOffsetLeft":1.85,"IdOffsetRight":2.15,"RADistanceRaw":1.0,"RADistanceRawDisplay":2.0,"RADuration":140.0,"DECDistanceRaw":2.0,"DECDistanceRawDisplay":4.0,"DECDuration":-90.0,"Dither":"NO"},
                    {"Id":3,"IdOffsetLeft":2.85,"IdOffsetRight":3.15,"RADistanceRaw":0.0,"RADistanceRawDisplay":0.0,"RADuration":0.0,"DECDistanceRaw":0.0,"DECDistanceRawDisplay":0.0,"DECDuration":0.0,"Dither":"0.01"}
                ],
                "HistorySize": 500, "PixelScale": 2.0, "Scale": 1
            },
            "Error": "", "StatusCode": 200, "Success": true, "Type": "API"
        }"#;
        let graph: GuiderGraphResponse = serde_json::from_str(json).unwrap();
        assert_eq!(graph.response.interval, 1.1);
        assert!(GuideStepsHistory::is_dither_step(
            &graph.response.guide_steps[2]
        ));
        let png = render_guider_graph_png(&graph.response).unwrap();
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']
        );
        assert!(png.len() > 1000);
    }

    #[test]
    fn test_render_rejects_too_few_steps() {
        let mut history = sample_history();
        history.guide_steps.truncate(1);
        assert!(matches!(
            render_guider_graph_png(&history),
            Err(ChartError::NotEnoughData(1))
        ));
    }

    #[test]
    fn test_render_degenerate_ranges() {
        let mut history = sample_history();
        // Force fallback range computation
        history.min_y = 0.0;
        history.max_y = 0.0;
        history.min_duration_y = 0.0;
        history.max_duration_y = 0.0;
        history.rms = None;
        let png = render_guider_graph_png(&history).unwrap();
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    fn sample_autofocus() -> AutofocusData {
        let json = std::fs::read_to_string("example_last_af.json").unwrap();
        let parsed: crate::autofocus::AutofocusResponse = serde_json::from_str(&json).unwrap();
        parsed.response
    }

    #[test]
    fn test_render_autofocus_graph() {
        let af = sample_autofocus();
        let png = render_autofocus_graph_png(&af).unwrap();
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
        assert!(png.len() > 1000);
    }

    #[test]
    fn test_render_modern_hocus_focus_fractional_positions() {
        let json = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let parsed: crate::autofocus::AutofocusResponse = serde_json::from_str(&json).unwrap();
        assert!(
            (parsed.response.calculated_focus_point.position - 4188.955065493704).abs() < 1e-12
        );

        let png = render_autofocus_graph_png(&parsed.response).unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(png.len() > 1000);
    }

    #[test]
    fn test_render_every_nina_autofocus_feedback_mode() {
        let mut af = sample_autofocus();
        let marker = af
            .intersections
            .hyperbolic_minimum
            .clone()
            .expect("fixture has a fitted minimum");
        af.intersections.quadratic_minimum = Some(marker.clone());
        af.intersections.gaussian_maximum = Some(marker);

        for (method, fitting) in [
            ("STARHFR", "TRENDLINES"),
            ("STARHFR", "PARABOLIC"),
            ("STARHFR", "TRENDPARABOLIC"),
            ("STARHFR", "HYPERBOLIC"),
            ("STARHFR", "TRENDHYPERBOLIC"),
            ("CONTRASTDETECTION", "GAUSSIAN"),
        ] {
            af.method = method.to_string();
            af.fitting = fitting.to_string();
            let png = render_autofocus_graph_png(&af)
                .unwrap_or_else(|error| panic!("{method}/{fitting}: {error}"));
            assert_eq!(
                &png[..8],
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "{method}/{fitting}"
            );
            assert!(png.len() > 1000, "{method}/{fitting}");
        }
    }

    #[test]
    fn test_render_autofocus_ignores_nonfinite_measurement_error() {
        let json = std::fs::read_to_string("example_last_af_hocus_modern.json").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["Response"]["MeasurePoints"][0]["Error"] = serde_json::json!("Infinity");
        let parsed: crate::autofocus::AutofocusResponse = serde_json::from_value(value).unwrap();

        let png = render_autofocus_graph_png(&parsed.response).unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(png.len() > 1000);
    }

    #[test]
    fn test_render_autofocus_rejects_too_few_points() {
        let mut af = sample_autofocus();
        af.measure_points.truncate(1);
        assert!(matches!(
            render_autofocus_graph_png(&af),
            Err(ChartError::NotEnoughData(1))
        ));
    }

    #[test]
    fn test_autofocus_hfr_helpers() {
        let af = sample_autofocus();
        // The example's InitialFocusPoint.Value is "NaN", so the helper
        // falls back to the measured point at the initial position.
        let before = af.initial_hfr().unwrap();
        assert!((before - 3.2493022712759543).abs() < 1e-9);
        let after = af.final_hfr().unwrap();
        assert!((after - 2.90813054456021).abs() < 1e-9);
    }
}
