//! Bounded, in-memory LIGHT-frame statistics. No new database or raw images.

use crate::image_statistics::ImageStatisticsPoint;
use crate::images::{ImageMetadata, ImageStatisticsOptions};
use chrono::{DateTime, Duration, Utc};

pub(crate) const MAX_STATISTICS_POINTS: usize = 2_000;
const HISTORY_HOURS: i64 = 24;

#[derive(Default)]
pub(crate) struct LightFrameHistory {
    pub options: ImageStatisticsOptions,
    points: Vec<ImageStatisticsPoint>,
    frames_since_plot: u32,
    periodic_due: bool,
}

impl LightFrameHistory {
    pub fn configure(&mut self, options: ImageStatisticsOptions, now: DateTime<Utc>) {
        if self.options != options {
            self.frames_since_plot = 0;
            self.periodic_due = false;
        }
        self.options = options;
        if !options.enabled() {
            self.clear();
        }
        self.trim(now);
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.frames_since_plot = 0;
        self.periodic_due = false;
    }

    /// The caller deduplicates source records. Baseline samples seed the plot
    /// without advancing the live cadence or replaying historical messages.
    pub fn record(
        &mut self,
        image: &ImageMetadata,
        captured_at: DateTime<Utc>,
        now: DateTime<Utc>,
        live: bool,
    ) {
        if !self.options.enabled()
            || !image.chat_enabled
            || !image.is_light_frame()
            || captured_at < now - Duration::hours(HISTORY_HOURS)
            || captured_at > now + Duration::minutes(5)
        {
            return;
        }
        let available = image
            .star_detection_available
            .unwrap_or(image.hfr > 0.0 || image.stars > 0);
        self.points.push(ImageStatisticsPoint {
            captured_at,
            exposure_start_known: image.timestamp_is_exposure_start == Some(true),
            hfr: (available && image.hfr.is_finite() && image.hfr > 0.0).then_some(image.hfr),
            stars: (available && image.stars >= 0).then_some(image.stars),
            filter: image.filter.chars().take(80).collect(),
        });
        self.trim(now);
        if live && self.options.every_n_frames > 0 {
            self.frames_since_plot += 1;
            if self.frames_since_plot >= self.options.every_n_frames {
                self.frames_since_plot = 0;
                // A backlog produces one latest chart per poll, not a burst.
                self.periodic_due = true;
            }
        }
    }

    pub fn take_periodic_due(&mut self) -> bool {
        std::mem::take(&mut self.periodic_due)
    }

    pub fn snapshot(&mut self, now: DateTime<Utc>) -> Vec<ImageStatisticsPoint> {
        self.trim(now);
        self.points.clone()
    }

    fn trim(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::hours(HISTORY_HOURS);
        self.points.retain(|point| point.captured_at >= cutoff);
        self.points.sort_by_key(|point| point.captured_at);
        if self.points.len() > MAX_STATISTICS_POINTS {
            self.points
                .drain(..self.points.len() - MAX_STATISTICS_POINTS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ImageMetadata {
        let history: crate::images::ImageHistoryResponse =
            serde_json::from_str(include_str!("../example_image-history.json")).unwrap();
        let mut image = history.response.into_iter().next().unwrap();
        image.image_type = "LIGHT".into();
        image.hfr = 1.5;
        image.stars = 120;
        image
    }

    fn enabled() -> LightFrameHistory {
        let mut history = LightFrameHistory::default();
        history.configure(
            ImageStatisticsOptions {
                every_n_frames: 2,
                on_sequence_end: true,
            },
            Utc::now(),
        );
        history
    }

    #[test]
    fn cadence_counts_light_frames_not_posts_and_coalesces_backlogs() {
        let now = Utc::now();
        let mut history = enabled();
        history.record(&image(), now, now, false);
        assert!(!history.take_periodic_due());
        let mut flat = image();
        flat.image_type = "FLAT".into();
        history.record(&flat, now, now, true);
        history.record(&image(), now, now, true);
        assert!(!history.take_periodic_due());
        for _ in 0..5 {
            history.record(&image(), now, now, true);
        }
        assert!(history.take_periodic_due());
        assert!(!history.take_periodic_due());
        assert_eq!(history.snapshot(now).len(), 7);
    }

    #[test]
    fn unknown_detection_is_not_a_zero_star_measurement() {
        let now = Utc::now();
        let mut history = enabled();
        let mut sample = image();
        sample.star_detection_available = Some(false);
        history.record(&sample, now, now, true);
        let points = history.snapshot(now);
        assert_eq!(points[0].hfr, None);
        assert_eq!(points[0].stars, None);
        sample.star_detection_available = Some(true);
        sample.hfr = 0.0;
        sample.stars = 0;
        history.record(&sample, now, now, true);
        assert_eq!(history.snapshot(now)[1].stars, Some(0));
    }

    #[test]
    fn privacy_disable_erases_history_and_pending_cadence() {
        let now = Utc::now();
        let mut history = enabled();
        let mut sample = image();
        sample.chat_enabled = false;
        history.record(&sample, now, now, true);
        assert!(history.snapshot(now).is_empty());
        history.record(&image(), now, now, true);
        history.record(&image(), now, now, true);
        history.configure(ImageStatisticsOptions::default(), now);
        assert!(history.snapshot(now).is_empty());
        assert!(!history.take_periodic_due());
    }

    #[test]
    fn rolling_history_expires_and_is_bounded() {
        let now = Utc::now();
        let mut history = enabled();
        let sample = image();
        history.record(&sample, now - Duration::hours(25), now, true);
        history.record(&sample, now + Duration::hours(1), now, true);
        assert!(history.snapshot(now).is_empty());
        for index in 0..MAX_STATISTICS_POINTS + 5 {
            history.record(&sample, now - Duration::seconds(index as i64), now, true);
        }
        let points = history.snapshot(now);
        assert_eq!(points.len(), MAX_STATISTICS_POINTS);
        assert!(
            points
                .windows(2)
                .all(|pair| pair[0].captured_at <= pair[1].captured_at)
        );
        assert!(history.snapshot(now + Duration::hours(25)).is_empty());
    }
}
