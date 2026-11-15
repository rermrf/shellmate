//! The sparklines of the cards, drawn with gpui paths: a line, and an area under it.

use gpui::prelude::*;
use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, canvas, point, px};

use super::model::SPARK_POINTS;

/// The points of a line of `values` (each 0 to `top`) in `bounds`, the oldest at the left. The line fills
/// the width once it has [`SPARK_POINTS`] values and starts at the right edge before that, so that a new
/// card is not stretched over the whole width. One value is a flat segment.
pub fn line_points(values: &[f64], top: f64, bounds: Bounds<Pixels>) -> Vec<Point<Pixels>> {
    if values.is_empty() || top <= 0.0 {
        return Vec::new();
    }
    let n = values.len().min(SPARK_POINTS);
    let values = &values[values.len() - n..];
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let step = w / (SPARK_POINTS.saturating_sub(1)).max(1) as f32;
    let (left, top_edge) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let right = left + w;
    // Leave room for the stroke at the top and the bottom.
    let (pad, usable) = (2.0f32, (h - 4.0).max(1.0));
    let y = |value: f64| top_edge + pad + usable * (1.0 - (value / top).clamp(0.0, 1.0) as f32);
    let mut points: Vec<Point<Pixels>> = values
        .iter()
        .enumerate()
        .map(|(i, value)| point(px(right - step * (n - 1 - i) as f32), px(y(*value))))
        .collect();
    if points.len() == 1 {
        // A single value is drawn as a short flat line so that there is something to see.
        let only = points[0];
        points.insert(0, point(px((f32::from(only.x) - step).max(left)), only.y));
    }
    points
}

/// A sparkline: the `fill` area (if any) and the `line`, for `values` between 0 and `top`.
pub fn sparkline(values: Vec<f64>, top: f64, line: Hsla, fill: Option<Hsla>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window: &mut Window, _| {
            let points = line_points(&values, top, bounds);
            if points.len() < 2 {
                return;
            }
            if let Some(fill) = fill {
                let mut area = PathBuilder::fill();
                let floor = bounds.origin.y + bounds.size.height;
                area.move_to(point(points[0].x, floor));
                for p in &points {
                    area.line_to(*p);
                }
                area.line_to(point(points[points.len() - 1].x, floor));
                area.close();
                if let Ok(path) = area.build() {
                    window.paint_path(path, fill);
                }
            }
            let mut stroke = PathBuilder::stroke(px(2.));
            stroke.move_to(points[0]);
            for p in &points[1..] {
                stroke.line_to(*p);
            }
            if let Ok(path) = stroke.build() {
                window.paint_path(path, line);
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use gpui::{bounds, size};

    use super::*;

    fn area() -> Bounds<Pixels> {
        bounds(point(px(10.), px(20.)), size(px(300.), px(64.)))
    }

    #[test]
    fn a_full_history_spans_the_width_and_the_scale() {
        let values: Vec<f64> = (0..SPARK_POINTS).map(|i| i as f64).collect();
        let points = line_points(&values, (SPARK_POINTS - 1) as f64, area());
        assert_eq!(points.len(), SPARK_POINTS);
        assert!(
            (f32::from(points[0].x) - 10.0).abs() < 1e-3
                && (f32::from(points[SPARK_POINTS - 1].x) - 310.0).abs() < 1e-3,
            "{points:?}"
        );
        // The lowest value is at the bottom (with the padding), the highest at the top.
        assert!((f32::from(points[0].y) - (20.0 + 2.0 + 60.0)).abs() < 1e-3, "{}", f32::from(points[0].y));
        assert!((f32::from(points[SPARK_POINTS - 1].y) - 22.0).abs() < 1e-3);
    }

    #[test]
    fn a_short_history_starts_at_the_right() {
        let points = line_points(&[10.0, 20.0, 30.0], 100.0, area());
        assert_eq!(points.len(), 3);
        assert!((f32::from(points[2].x) - 310.0).abs() < 1e-3, "the newest is at the right edge");
        assert!(
            f32::from(points[0].x) > 250.0,
            "and the line does not stretch over the card: {}",
            f32::from(points[0].x)
        );
        assert!(points[0].x < points[1].x && points[1].x < points[2].x);
    }

    #[test]
    fn nothing_and_one_value() {
        assert!(line_points(&[], 100.0, area()).is_empty());
        assert!(line_points(&[1.0], 0.0, area()).is_empty(), "no scale, no line");
        let one = line_points(&[50.0], 100.0, area());
        assert_eq!(one.len(), 2, "a flat segment");
        assert_eq!(one[0].y, one[1].y);
    }

    #[test]
    fn more_values_than_points_keep_the_newest() {
        let values: Vec<f64> = (0..SPARK_POINTS + 20).map(|i| i as f64).collect();
        let points = line_points(&values, 200.0, area());
        assert_eq!(points.len(), SPARK_POINTS);
        let newest_y = 20.0 + 2.0 + 60.0 * (1.0 - (SPARK_POINTS + 19) as f32 / 200.0);
        assert!((f32::from(points[SPARK_POINTS - 1].y) - newest_y).abs() < 1e-3);
    }

    #[test]
    fn values_over_the_scale_are_clipped_to_the_top() {
        let points = line_points(&[500.0, 5.0], 100.0, area());
        assert!((f32::from(points[0].y) - 22.0).abs() < 1e-3);
    }
}
