//! Renders NINA graph data as PNG charts for chat notifications:
//!
//! * the Direct guide history graph in the style of the
//!   PHD2/NINA guiding chart — RA/Dec error traces on the left axis,
//!   signed correction-pulse bars on the right axis, dither markers,
//!   and an RMS summary in the title;
//! * the Direct autofocus run — measured HFR
//!   points with error bars plus initial/calculated position markers;
//! * captured LIGHT-frame HFR and detected-star history, aligned by UTC time.
//!
//! Text uses an embedded Liberation Sans (SIL OFL, see
//! `assets/LiberationSans-LICENSE`) via plotters' `ab_glyph` backend, so
//! rendering needs no system font libraries on any release target.

use crate::autofocus::AutofocusData;
use crate::guider::GuideStepsHistory;
use crate::image_statistics::ImageStatisticsPoint;
use chrono::{DateTime, TimeDelta, Utc};
use plotters::prelude::*;
use plotters::style::register_font;
use std::collections::BTreeMap;
use std::sync::Once;
use thiserror::Error;

const WIDTH: u32 = 900;
const HEIGHT: u32 = 480;
const IMAGE_STATISTICS_HEIGHT: u32 = 600;

const BACKGROUND: RGBColor = RGBColor(24, 26, 31);
const GRID: RGBColor = RGBColor(58, 62, 70);
const TEXT: RGBColor = RGBColor(200, 204, 210);
const RA_COLOR: RGBColor = RGBColor(77, 139, 232);
const DEC_COLOR: RGBColor = RGBColor(232, 77, 77);
const DITHER_COLOR: RGBColor = RGBColor(160, 160, 90);
const HFR_COLOR: RGBColor = RGBColor(96, 189, 232);
const FOCUS_COLOR: RGBColor = RGBColor(96, 209, 122);
const FIT_COLOR: RGBColor = RGBColor(213, 143, 255);
const FILTER_COLORS: [RGBColor; 7] = [
    HFR_COLOR,
    FOCUS_COLOR,
    FIT_COLOR,
    RGBColor(244, 177, 88),
    RGBColor(232, 125, 159),
    RGBColor(72, 210, 205),
    TEXT,
];

#[derive(Debug, Error)]
pub enum ChartError {
    #[error("not enough measurements to draw a graph ({0} measurements)")]
    NotEnoughData(usize),
    #[error("failed to render chart: {0}")]
    Render(String),
}

static FONT_INIT: Once = Once::new();

fn ensure_font() {
    FONT_INIT.call_once(|| {
        let font = include_bytes!("../assets/LiberationSans-Regular.ttf");
        for style in [
            FontStyle::Normal,
            FontStyle::Bold,
            FontStyle::Italic,
            FontStyle::Oblique,
        ] {
            // Registration only fails on an invalid font file, which would
            // be a build asset problem — surface it at render time instead.
            let _ = register_font("sans-serif", style, font);
        }
    });
}

/// Render the guide graph to PNG bytes. Fails when fewer than two guide
/// steps are present.
pub fn render_guider_graph_png(history: &GuideStepsHistory) -> Result<Vec<u8>, ChartError> {
    if !history.has_graph_data() {
        return Err(ChartError::NotEnoughData(history.guide_steps.len()));
    }
    ensure_font();

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

    let mut buffer = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&BACKGROUND)
            .map_err(|e| ChartError::Render(e.to_string()))?;

        let mut chart = ChartBuilder::on(&root)
            .caption(&title, ("sans-serif", 20).into_font().color(&TEXT))
            .margin(12)
            .x_label_area_size(32)
            .y_label_area_size(48)
            .right_y_label_area_size(56)
            .build_cartesian_2d(0f64..(n as f64), min_y..max_y)
            .map_err(|e| ChartError::Render(e.to_string()))?
            .set_secondary_coord(0f64..(n as f64), -dur_limit..dur_limit);

        chart
            .configure_mesh()
            .disable_x_mesh()
            .bold_line_style(GRID.mix(0.8))
            .light_line_style(GRID.mix(0.3))
            .axis_style(GRID)
            .label_style(("sans-serif", 14).into_font().color(&TEXT))
            .y_desc(format!("Error ({})", unit))
            .x_desc("Guide step")
            .draw()
            .map_err(|e| ChartError::Render(e.to_string()))?;

        chart
            .configure_secondary_axes()
            .axis_style(GRID)
            .label_style(("sans-serif", 14).into_font().color(&TEXT))
            .y_desc("Correction (ms)")
            .draw()
            .map_err(|e| ChartError::Render(e.to_string()))?;

        // Correction pulses as thin bars on the secondary (ms) axis,
        // drawn first so the error traces stay on top.
        let bar_half = 0.18;
        for (i, s) in steps.iter().enumerate() {
            let x = i as f64 + 0.5;
            for (duration, color) in [(s.ra_duration, RA_COLOR), (s.dec_duration, DEC_COLOR)] {
                if duration.is_finite() && duration != 0.0 {
                    // Offset RA bars slightly left and Dec bars slightly
                    // right so simultaneous pulses stay distinguishable.
                    let shift = if color == RA_COLOR {
                        -bar_half
                    } else {
                        bar_half
                    };
                    chart
                        .draw_secondary_series(std::iter::once(Rectangle::new(
                            [
                                (x + shift - bar_half, 0.0),
                                (x + shift + bar_half, duration),
                            ],
                            color.mix(0.35).filled(),
                        )))
                        .map_err(|e| ChartError::Render(e.to_string()))?;
                }
            }
        }

        // Dither markers: vertical lines across the error axis.
        for (i, s) in steps.iter().enumerate() {
            if GuideStepsHistory::is_dither_step(s) {
                let x = i as f64 + 0.5;
                chart
                    .draw_series(std::iter::once(PathElement::new(
                        vec![(x, min_y), (x, max_y)],
                        DITHER_COLOR.mix(0.6),
                    )))
                    .map_err(|e| ChartError::Render(e.to_string()))?;
            }
        }

        // Error traces. NaN samples (guider gaps) split the trace instead
        // of drawing bogus segments.
        for (color, label, values) in [
            (
                RA_COLOR,
                "RA",
                steps
                    .iter()
                    .map(|s| s.ra_distance_raw_display)
                    .collect::<Vec<_>>(),
            ),
            (
                DEC_COLOR,
                "Dec",
                steps
                    .iter()
                    .map(|s| s.dec_distance_raw_display)
                    .collect::<Vec<_>>(),
            ),
        ] {
            for segment in contiguous_finite_runs(&values) {
                let series = chart
                    .draw_series(LineSeries::new(
                        segment
                            .iter()
                            .map(|&(i, v)| (i as f64 + 0.5, v))
                            .collect::<Vec<_>>(),
                        color.stroke_width(2),
                    ))
                    .map_err(|e| ChartError::Render(e.to_string()))?;
                // Attach the legend entry once per axis, on the first run.
                if segment.first().map(|&(i, _)| i) == values.iter().position(|v| v.is_finite()) {
                    series.label(label).legend(move |(x, y)| {
                        PathElement::new(vec![(x, y), (x + 16, y)], color.stroke_width(2))
                    });
                }
            }
        }

        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperRight)
            .background_style(BACKGROUND.mix(0.8))
            .border_style(GRID)
            .label_font(("sans-serif", 14).into_font().color(&TEXT))
            .draw()
            .map_err(|e| ChartError::Render(e.to_string()))?;

        root.present()
            .map_err(|e| ChartError::Render(e.to_string()))?;
    }

    encode_png(&buffer, WIDTH, HEIGHT)
}

/// Render an autofocus run to PNG bytes: measured HFR vs focuser position
/// with error bars, a connecting line, and vertical markers for the
/// initial and calculated focus positions. Fails when fewer than two
/// finite measurement points are present.
pub fn render_autofocus_graph_png(af: &AutofocusData) -> Result<Vec<u8>, ChartError> {
    let points: Vec<(f64, f64, f64)> = af
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
    ensure_font();

    let initial_pos = af.initial_focus_point.position;
    let final_pos = af.calculated_focus_point.position;
    let fit_points: Vec<_> = af
        .selected_fit_points()
        .into_iter()
        .filter(|(_, point)| point.value.is_finite())
        .collect();

    let x_lo = points
        .iter()
        .map(|p| p.0)
        .chain(fit_points.iter().map(|(_, point)| point.position))
        .fold(initial_pos.min(final_pos), f64::min);
    let x_hi = points
        .iter()
        .map(|p| p.0)
        .chain(fit_points.iter().map(|(_, point)| point.position))
        .fold(initial_pos.max(final_pos), f64::max);
    let x_pad = ((x_hi - x_lo) * 0.05).max(1.0);

    let y_lo = points
        .iter()
        .map(|(_, v, e)| v - e)
        .chain(fit_points.iter().map(|(_, point)| point.value))
        .fold(f64::INFINITY, f64::min);
    let y_hi = points
        .iter()
        .map(|(_, v, e)| v + e)
        .chain(fit_points.iter().map(|(_, point)| point.value))
        .fold(f64::NEG_INFINITY, f64::max);
    let y_pad = ((y_hi - y_lo) * 0.1).max(0.1);

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

    let mut buffer = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&BACKGROUND)
            .map_err(|e| ChartError::Render(e.to_string()))?;

        let mut chart = ChartBuilder::on(&root)
            .caption(&title, ("sans-serif", 18).into_font().color(&TEXT))
            .margin(12)
            .x_label_area_size(36)
            .y_label_area_size(52)
            .build_cartesian_2d(
                (x_lo - x_pad)..(x_hi + x_pad),
                (y_lo - y_pad)..(y_hi + y_pad),
            )
            .map_err(|e| ChartError::Render(e.to_string()))?;

        chart
            .configure_mesh()
            .bold_line_style(GRID.mix(0.8))
            .light_line_style(GRID.mix(0.3))
            .axis_style(GRID)
            .label_style(("sans-serif", 14).into_font().color(&TEXT))
            .x_desc("Focuser position")
            .y_desc(measurement_name)
            .draw()
            .map_err(|e| ChartError::Render(e.to_string()))?;

        // Position markers first so data draws on top of them
        for (pos, color, label) in [
            (initial_pos, DITHER_COLOR, "Initial"),
            (final_pos, FOCUS_COLOR, "Calculated"),
        ] {
            chart
                .draw_series(std::iter::once(PathElement::new(
                    vec![(pos, y_lo - y_pad), (pos, y_hi + y_pad)],
                    color.mix(0.7).stroke_width(2),
                )))
                .map_err(|e| ChartError::Render(e.to_string()))?
                .label(label)
                .legend(move |(x, y)| {
                    PathElement::new(vec![(x, y), (x + 16, y)], color.stroke_width(2))
                });
        }

        // Error bars
        let cap = x_pad * 0.3;
        for &(x, v, e) in &points {
            if e > 0.0 {
                chart
                    .draw_series(
                        [
                            vec![(x, v - e), (x, v + e)],
                            vec![(x - cap, v - e), (x + cap, v - e)],
                            vec![(x - cap, v + e), (x + cap, v + e)],
                        ]
                        .into_iter()
                        .map(|seg| PathElement::new(seg, HFR_COLOR.mix(0.5))),
                    )
                    .map_err(|e| ChartError::Render(e.to_string()))?;
            }
        }

        // Connecting line through the measurements, then the points
        let mut sorted = points.clone();
        sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
        chart
            .draw_series(LineSeries::new(
                sorted.iter().map(|&(x, v, _)| (x, v)),
                HFR_COLOR.stroke_width(2),
            ))
            .map_err(|e| ChartError::Render(e.to_string()))?
            .label(format!("Measured {measurement_name}"))
            .legend(|(x, y)| {
                PathElement::new(vec![(x, y), (x + 16, y)], HFR_COLOR.stroke_width(2))
            });
        chart
            .draw_series(
                sorted
                    .iter()
                    .map(|&(x, v, _)| Circle::new((x, v), 4, HFR_COLOR.filled())),
            )
            .map_err(|e| ChartError::Render(e.to_string()))?;

        for (label, point) in fit_points {
            chart
                .draw_series(std::iter::once(TriangleMarker::new(
                    (point.position, point.value),
                    7,
                    FIT_COLOR.filled(),
                )))
                .map_err(|e| ChartError::Render(e.to_string()))?
                .label(label)
                .legend(|(x, y)| TriangleMarker::new((x + 8, y), 5, FIT_COLOR.filled()));
        }

        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperRight)
            .background_style(BACKGROUND.mix(0.8))
            .border_style(GRID)
            .label_font(("sans-serif", 14).into_font().color(&TEXT))
            .draw()
            .map_err(|e| ChartError::Render(e.to_string()))?;

        root.present()
            .map_err(|e| ChartError::Render(e.to_string()))?;
    }

    encode_png(&buffer, WIDTH, HEIGHT)
}

/// Render captured LIGHT-frame HFR and detected stars against UTC time.
///
/// Invalid HFR values (nonfinite or nonpositive) and negative star counts
/// are omitted independently, preserving valid measurements from the same
/// frame. Missing measurements and filter changes split the connecting
/// traces. A single usable frame is sufficient; an entirely empty history
/// returns `NotEnoughData(0)`. Callers should bound their retained history.
pub fn render_image_statistics_png(points: &[ImageStatisticsPoint]) -> Result<Vec<u8>, ChartError> {
    if !points
        .iter()
        .any(|point| valid_image_hfr(point).is_some() || valid_image_stars(point).is_some())
    {
        return Err(ChartError::NotEnoughData(0));
    }
    ensure_font();

    let mut sorted: Vec<_> = points.iter().collect();
    sorted.sort_by_key(|point| point.captured_at);
    // The usable-data check above guarantees at least one frame.
    let start = sorted[0].captured_at;
    let end = sorted[sorted.len() - 1].captured_at;
    let duration = image_statistics_elapsed(start, end);
    let padding = (duration * 0.02).max(30.0);
    let range = -padding..(duration + padding);
    let includes_date = start.date_naive() != end.date_naive();
    let includes_seconds = duration < 120.0;

    // Bound legend entries and label lengths even for unexpected filter
    // metadata. The final color groups any additional filters visually.
    let mut filters = BTreeMap::new();
    for point in &sorted {
        filters.entry(point.filter.trim()).or_insert(0usize);
    }
    let mut filter_labels = Vec::new();
    for (index, (name, color_index)) in filters.iter_mut().enumerate() {
        *color_index = index.min(FILTER_COLORS.len() - 1);
        if index < FILTER_COLORS.len() - 1 {
            let label: String = name
                .chars()
                .take(18)
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect();
            filter_labels.push(if label.is_empty() {
                "No filter".to_string()
            } else {
                label
            });
        } else if index == FILTER_COLORS.len() - 1 {
            filter_labels.push("Other filters".to_string());
        }
    }

    let title = format!(
        "LIGHT-frame history  —  {} frames  —  {} UTC",
        sorted.len(),
        start.format("%Y-%m-%d")
    );
    let mut buffer = vec![0u8; (WIDTH * IMAGE_STATISTICS_HEIGHT * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buffer, (WIDTH, IMAGE_STATISTICS_HEIGHT))
            .into_drawing_area();
        root.fill(&BACKGROUND)
            .map_err(|error| ChartError::Render(error.to_string()))?;
        let titled = root
            .titled(&title, ("sans-serif", 20).into_font().color(&TEXT))
            .map_err(|error| ChartError::Render(error.to_string()))?;
        let panels = titled.split_evenly((2, 1));

        for (panel_index, panel) in panels.iter().enumerate() {
            let hfr_panel = panel_index == 0;
            let values: Vec<_> = sorted
                .iter()
                .map(|point| {
                    if hfr_panel {
                        valid_image_hfr(point)
                    } else {
                        valid_image_stars(point)
                    }
                })
                .collect();
            let max_value = values.iter().flatten().copied().fold(0.0, f64::max);
            // Normalize HFR internally so even extreme finite metadata
            // cannot overflow Plotters' axis/key-point arithmetic. Labels
            // still show the original pixel values.
            let scale = if hfr_panel && max_value > 0.0 {
                max_value
            } else {
                1.0
            };
            let (y_min, y_max) = if hfr_panel && max_value > 0.0 {
                let minimum = values
                    .iter()
                    .flatten()
                    .map(|value| value / scale)
                    .fold(1.0, f64::min);
                let headroom = ((1.0 - minimum) * 0.1).max(0.05);
                ((minimum - headroom).max(0.0), 1.0 + headroom)
            } else {
                (0.0, (max_value * 1.12).max(1.0))
            };
            let y_label = if hfr_panel {
                "HFR (px)"
            } else {
                "Detected stars"
            };
            let formatter = |seconds: &f64| {
                image_statistics_time_label(start, *seconds, includes_date, includes_seconds)
            };
            let y_formatter = |value: &f64| {
                if hfr_panel {
                    let hfr = value * scale;
                    if !hfr.is_finite() {
                        String::new()
                    } else if hfr >= 1_000_000.0 || (hfr > 0.0 && hfr < 0.01) {
                        format!("{hfr:.2e}")
                    } else {
                        format!("{hfr:.2}")
                    }
                } else {
                    format!("{value:.0}")
                }
            };
            let mut chart = ChartBuilder::on(panel)
                .margin(12)
                .x_label_area_size(40)
                .y_label_area_size(78)
                .build_cartesian_2d(range.clone(), y_min..y_max)
                .map_err(|error| ChartError::Render(error.to_string()))?;
            chart
                .configure_mesh()
                .x_labels(6)
                .y_labels(5)
                .x_label_formatter(&formatter)
                .y_label_formatter(&y_formatter)
                .bold_line_style(GRID.mix(0.8))
                .light_line_style(GRID.mix(0.3))
                .axis_style(GRID)
                .label_style(("sans-serif", 14).into_font().color(&TEXT))
                .x_desc(if hfr_panel { "" } else { "Capture time (UTC)" })
                .y_desc(y_label)
                .draw()
                .map_err(|error| ChartError::Render(error.to_string()))?;

            let mut segments: Vec<(usize, Vec<(f64, f64)>)> = Vec::new();
            let mut current = Vec::new();
            let mut previous_filter = "";
            let mut current_color = 0;
            for (point, value) in sorted.iter().zip(values.iter()) {
                let filter = point.filter.trim();
                if (value.is_none() || filter != previous_filter) && !current.is_empty() {
                    segments.push((current_color, std::mem::take(&mut current)));
                }
                if let Some(value) = value {
                    current_color = filters[filter];
                    current.push((
                        image_statistics_elapsed(start, point.captured_at),
                        *value / scale,
                    ));
                }
                previous_filter = filter;
            }
            if !current.is_empty() {
                segments.push((current_color, current));
            }

            let mut labeled = [false; FILTER_COLORS.len()];
            for (color_index, segment) in segments {
                let color = FILTER_COLORS[color_index];
                chart
                    .draw_series(LineSeries::new(
                        segment.iter().copied(),
                        color.stroke_width(2),
                    ))
                    .map_err(|error| ChartError::Render(error.to_string()))?;
                let annotation = chart
                    .draw_series(
                        segment
                            .iter()
                            .map(|point| Circle::new(*point, 3, color.filled())),
                    )
                    .map_err(|error| ChartError::Render(error.to_string()))?;
                if !labeled[color_index] {
                    annotation
                        .label(&filter_labels[color_index])
                        .legend(move |(x, y)| Circle::new((x + 8, y), 3, color.filled()));
                    labeled[color_index] = true;
                }
            }
            if values.iter().any(Option::is_some) {
                chart
                    .configure_series_labels()
                    .position(SeriesLabelPosition::UpperRight)
                    .background_style(BACKGROUND.mix(0.9))
                    .border_style(GRID)
                    .label_font(("sans-serif", 13).into_font().color(&TEXT))
                    .draw()
                    .map_err(|error| ChartError::Render(error.to_string()))?;
            } else {
                chart
                    .draw_series(std::iter::once(Text::new(
                        format!("No {y_label} measurements"),
                        (duration / 2.0, y_max / 2.0),
                        ("sans-serif", 16).into_font().color(&TEXT),
                    )))
                    .map_err(|error| ChartError::Render(error.to_string()))?;
            }
        }
        root.present()
            .map_err(|error| ChartError::Render(error.to_string()))?;
    }
    encode_png(&buffer, WIDTH, IMAGE_STATISTICS_HEIGHT)
}

fn valid_image_hfr(point: &ImageStatisticsPoint) -> Option<f64> {
    point.hfr.filter(|value| value.is_finite() && *value > 0.0)
}

fn valid_image_stars(point: &ImageStatisticsPoint) -> Option<f64> {
    point.stars.filter(|value| *value >= 0).map(f64::from)
}

fn image_statistics_elapsed(start: DateTime<Utc>, captured_at: DateTime<Utc>) -> f64 {
    captured_at.signed_duration_since(start).num_milliseconds() as f64 / 1000.0
}

fn image_statistics_time_label(
    start: DateTime<Utc>,
    seconds: f64,
    includes_date: bool,
    includes_seconds: bool,
) -> String {
    let milliseconds = (seconds * 1000.0).round();
    if !milliseconds.is_finite() {
        return String::new();
    }
    TimeDelta::try_milliseconds(milliseconds as i64)
        .and_then(|elapsed| start.checked_add_signed(elapsed))
        .map(|timestamp| {
            timestamp
                .format(match (includes_date, includes_seconds) {
                    (true, true) => "%m-%d %H:%M:%S",
                    (true, false) => "%m-%d %H:%M",
                    (false, true) => "%H:%M:%S",
                    (false, false) => "%H:%M",
                })
                .to_string()
        })
        .unwrap_or_default()
}

/// Split a series into runs of consecutive finite samples, keeping the
/// original indices so gaps stay gaps on the x axis.
fn contiguous_finite_runs(values: &[f64]) -> Vec<Vec<(usize, f64)>> {
    let mut runs = Vec::new();
    let mut current: Vec<(usize, f64)> = Vec::new();
    for (i, &v) in values.iter().enumerate() {
        if v.is_finite() {
            current.push((i, v));
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ChartError> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ChartError::Render(e.to_string()))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| ChartError::Render(e.to_string()))?;
    }
    Ok(png)
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

    #[test]
    fn test_contiguous_finite_runs() {
        let runs = contiguous_finite_runs(&[1.0, 2.0, f64::NAN, 3.0]);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], vec![(0, 1.0), (1, 2.0)]);
        assert_eq!(runs[1], vec![(3, 3.0)]);
    }

    fn statistics_point(
        seconds: i64,
        hfr: Option<f64>,
        stars: Option<i32>,
        filter: &str,
    ) -> ImageStatisticsPoint {
        ImageStatisticsPoint {
            captured_at: DateTime::from_timestamp(1_791_591_000 + seconds, 0).unwrap(),
            exposure_start_known: true,
            hfr,
            stars,
            filter: filter.to_string(),
        }
    }

    fn assert_statistics_png_decodes(bytes: Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (WIDTH, IMAGE_STATISTICS_HEIGHT));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(
            info.buffer_size(),
            (WIDTH * IMAGE_STATISTICS_HEIGHT * 3) as usize
        );
        assert!(
            pixels
                .chunks_exact(3)
                .any(|pixel| pixel != [BACKGROUND.0, BACKGROUND.1, BACKGROUND.2])
        );
    }

    #[test]
    fn test_render_image_statistics_png_decodes() {
        let points = vec![
            statistics_point(0, Some(1.5), Some(3200), "L"),
            statistics_point(300, Some(1.7), Some(2900), "L"),
            statistics_point(600, None, Some(2700), "L"),
            statistics_point(900, Some(2.0), Some(2400), "R"),
            statistics_point(1200, Some(1.8), None, "R"),
        ];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_render_image_statistics_single_and_equal_timestamps() {
        let points = vec![statistics_point(0, Some(1.5), Some(3200), "L")];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
        let points = vec![
            statistics_point(0, Some(1.5), Some(3200), "L"),
            statistics_point(0, Some(1.5), Some(3200), "R"),
        ];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_render_image_statistics_missing_hfr_and_zero_stars() {
        let points = vec![
            statistics_point(0, None, Some(0), ""),
            statistics_point(300, None, Some(0), ""),
        ];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
        let points = vec![statistics_point(0, Some(1.5), None, "L")];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_render_image_statistics_rejects_missing_and_invalid_measurements() {
        for points in [
            vec![],
            vec![statistics_point(0, None, None, "L")],
            vec![
                statistics_point(0, Some(f64::NAN), Some(-1), "L"),
                statistics_point(300, Some(f64::INFINITY), None, "L"),
                statistics_point(600, Some(f64::NEG_INFINITY), None, "L"),
                statistics_point(900, Some(-1.0), None, "L"),
                statistics_point(1200, Some(0.0), None, "L"),
            ],
        ] {
            assert!(matches!(
                render_image_statistics_png(&points),
                Err(ChartError::NotEnoughData(0))
            ));
        }
        let points = vec![
            statistics_point(0, Some(f64::NAN), Some(0), "L"),
            statistics_point(300, Some(1.5), Some(-1), "L"),
        ];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_render_image_statistics_numeric_extremes_and_unordered_input() {
        let points = vec![
            statistics_point(300, Some(f64::MAX), Some(i32::MAX), "L"),
            statistics_point(0, Some(f64::MIN_POSITIVE), Some(0), "L"),
        ];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
        let points = vec![statistics_point(0, Some(f64::from_bits(1)), Some(0), "L")];
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_render_image_statistics_extreme_dates() {
        let mut earliest = statistics_point(0, Some(1.0), Some(1), "L");
        let mut latest = earliest.clone();
        earliest.captured_at = DateTime::<Utc>::MIN_UTC;
        latest.captured_at = DateTime::<Utc>::MAX_UTC;
        assert_statistics_png_decodes(render_image_statistics_png(&[latest, earliest]).unwrap());
    }

    #[test]
    fn test_render_image_statistics_many_filters_and_bounded_labels() {
        let points: Vec<_> = (0..10)
            .map(|index| {
                statistics_point(
                    index * 300,
                    Some(1.0),
                    Some(100),
                    &format!("Filter {index}\n{}", "Long name".repeat(100)),
                )
            })
            .collect();
        assert_statistics_png_decodes(render_image_statistics_png(&points).unwrap());
    }

    #[test]
    fn test_image_statistics_time_labels_are_utc_and_include_midnight_date() {
        let start = DateTime::parse_from_rfc3339("2026-10-10T23:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            image_statistics_time_label(start, 0.0, false, false),
            "23:55"
        );
        assert_eq!(
            image_statistics_time_label(start, 20.0, false, true),
            "23:55:20"
        );
        assert_eq!(
            image_statistics_time_label(start, 600.0, true, false),
            "10-11 00:05"
        );
        assert_eq!(
            image_statistics_time_label(DateTime::<Utc>::MIN_UTC, -30.0, true, false),
            ""
        );
        assert_eq!(
            image_statistics_time_label(DateTime::<Utc>::MAX_UTC, 30.0, true, false),
            ""
        );
        assert_eq!(
            image_statistics_time_label(start, f64::NAN, true, false),
            ""
        );
    }

    #[test]
    #[ignore = "writes a synthetic preview PNG to the system temporary directory"]
    fn write_image_statistics_preview() {
        let points: Vec<_> = (0..120)
            .map(|index| {
                let phase = f64::from(index) / 12.0;
                statistics_point(
                    i64::from(index) * 300,
                    Some(1.6 + phase.sin() * 0.12 + f64::from(index) / 300.0),
                    Some(4000 - index * 8 + (phase.cos() * 150.0) as i32),
                    ["L", "R", "G"][(index / 40) as usize],
                )
            })
            .collect();
        let png = render_image_statistics_png(&points).unwrap();
        let path = std::env::temp_dir().join(format!(
            "chatstronomy-light-frame-history-preview-{}.png",
            Utc::now().timestamp_millis()
        ));
        std::fs::write(&path, png).unwrap();
        println!("Synthetic LIGHT-frame history preview: {}", path.display());
    }
}
