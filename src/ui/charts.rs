use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Local};
use egui::{Color32, RichText, Stroke, Ui, Vec2b};
use egui_plot::{AxisHints, GridMark, HoverPosition, Line, Plot, PlotPoints};

use crate::model::{MAX_CHART_POINTS, TelemetrySample, downsample_series};

#[derive(Clone)]
pub struct SeriesSpec {
    pub name: String,
    pub color: Color32,
    pub segments: Vec<Vec<[f64; 2]>>,
    /// If `Some(scale)`, plot values are `raw * scale`, and hover tooltips invert via `/ scale` with `%`.
    pub right_axis_pct_scale: Option<f64>,
}

pub struct ChartOutput {
    pub x_bounds: (f64, f64),
    pub user_interacted: bool,
    pub double_clicked: bool,
}

/// Splits `(x, y)` pairs into separate contiguous segments whenever consecutive points
/// have a gap larger than `gap_secs`, avoiding misleading straight lines across sleep/pauses.
pub fn split_segments_by_gap(points: &[(f64, f64)], gap_secs: f64) -> Vec<Vec<[f64; 2]>> {
    let mut segments: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();
    let mut prev_x: Option<f64> = None;

    for &(x, y) in points {
        if let Some(px) = prev_x
            && (x - px) > gap_secs
            && !current.is_empty()
        {
            segments.push(std::mem::take(&mut current));
        }
        current.push([x, y]);
        prev_x = Some(x);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

pub fn format_local_time(ts_secs: f64) -> String {
    let secs = ts_secs.floor() as i64;
    DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

pub fn collect_temp_series(samples: &[TelemetrySample], gap_secs: f64) -> Vec<SeriesSpec> {
    let mut sensor_names: BTreeSet<String> = BTreeSet::new();
    for s in samples {
        for k in s.temperatures_c.keys() {
            sensor_names.insert(k.clone());
        }
    }

    let mut by_name: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for name in sensor_names {
        by_name.insert(name, Vec::new());
    }
    for s in samples {
        let x = s.ts_secs();
        for (name, &val) in &s.temperatures_c {
            if let Some(vec) = by_name.get_mut(name) {
                vec.push((x, val));
            }
        }
    }

    by_name
        .into_iter()
        .enumerate()
        .map(|(idx, (name, pts))| SeriesSpec {
            color: temperature_color(&name, idx),
            segments: split_segments_by_gap(&pts, gap_secs),
            name,
            right_axis_pct_scale: None,
        })
        .collect()
}

fn temperature_color(name: &str, idx: usize) -> Color32 {
    match name {
        "CPU (IPF)" => Color32::from_rgb(234, 88, 12),
        "NVMe SSD" => Color32::from_rgb(13, 148, 136),
        "TZ00" => Color32::from_rgb(79, 70, 229),
        _ => {
            const PALETTE: &[Color32] = &[
                Color32::from_rgb(219, 39, 119),
                Color32::from_rgb(37, 99, 235),
                Color32::from_rgb(101, 163, 13),
                Color32::from_rgb(180, 83, 9),
            ];
            PALETTE[idx % PALETTE.len()]
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_time_chart(
    ui: &mut Ui,
    plot_id: &str,
    title: &str,
    height: f32,
    y_bounds_floor: (f64, f64),
    y_axes: Vec<AxisHints<'_>>,
    x_range: (f64, f64),
    force_x_bounds: bool,
    series: &[SeriesSpec],
    hidden_series: &mut HashSet<String>,
    left_unit_suffix: &str,
) -> ChartOutput {
    ui.label(RichText::new(title).strong().size(13.0));

    let pct_lookup: BTreeMap<String, f64> = series
        .iter()
        .filter_map(|s| s.right_axis_pct_scale.map(|sc| (s.name.clone(), sc)))
        .collect();
    let unit = left_unit_suffix.to_string();

    let mut max_visible_y = y_bounds_floor.1;
    let mut min_visible_y = y_bounds_floor.0;
    for s in series {
        if hidden_series.contains(&s.name) {
            continue;
        }
        for seg in &s.segments {
            for pt in seg {
                if pt[0] >= x_range.0 && pt[0] <= x_range.1 {
                    if pt[1] > max_visible_y {
                        max_visible_y = pt[1];
                    }
                    if pt[1] < min_visible_y {
                        min_visible_y = pt[1];
                    }
                }
            }
        }
    }
    let y_max = if max_visible_y > y_bounds_floor.1 {
        (max_visible_y * 1.1).ceil()
    } else {
        y_bounds_floor.1
    };
    let y_min = if min_visible_y < y_bounds_floor.0 {
        min_visible_y.floor()
    } else {
        y_bounds_floor.0
    };

    let plot = Plot::new(plot_id)
        .height(height)
        .link_axis(
            egui::Id::unique("power_monitor_x_link"),
            Vec2b::new(true, false),
        )
        .link_cursor(
            egui::Id::unique("power_monitor_cursor"),
            Vec2b::new(true, false),
        )
        .allow_zoom(Vec2b::new(true, false))
        .allow_scroll(Vec2b::new(true, false))
        .allow_drag(Vec2b::new(true, false))
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .default_x_bounds(x_range.0, x_range.1)
        .default_y_bounds(y_min, y_max)
        .custom_y_axes(y_axes)
        .x_axis_formatter(|mark: GridMark, _| format_local_time(mark.value))
        .label_formatter(move |pos| match pos {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } if !plot_name.is_empty() => {
                let time_str = format_local_time(position.x);
                if let Some(&scale) = pct_lookup.get(*plot_name) {
                    let pct = if scale > 0.0 { position.y / scale } else { 0.0 };
                    Some(format!("{plot_name}: {pct:.1} %\n{time_str}"))
                } else {
                    Some(format!("{plot_name}: {:.2}{unit}\n{time_str}", position.y))
                }
            }
            _ => None,
        });

    let mut interacted = false;

    let resp = plot.show(ui, |plot_ui| {
        if force_x_bounds {
            plot_ui.set_plot_bounds_x(x_range.0..=x_range.1);
        }
        plot_ui.set_plot_bounds_y(y_min..=y_max);

        for s in series {
            if hidden_series.contains(&s.name) {
                continue;
            }
            for seg in &s.segments {
                if seg.is_empty() {
                    continue;
                }
                let pts = downsample_series(seg, MAX_CHART_POINTS);
                let line = Line::new(s.name.as_str(), PlotPoints::from(pts))
                    .color(s.color)
                    .stroke(Stroke::new(1.5, s.color))
                    .allow_band(false);
                plot_ui.line(line);
            }
        }

        let pointer_drag = plot_ui.response().dragged();
        let scroll_used = plot_ui.response().hovered()
            && plot_ui.ctx().input(|i| {
                i.smooth_scroll_delta.y.abs() > 0.0 || (i.zoom_delta() - 1.0).abs() > 1e-4
            });
        if pointer_drag || scroll_used {
            interacted = true;
        }
    });

    let b = resp.transform.bounds();
    let new_bounds = (b.min()[0], b.max()[0]);

    // Clickable legend pills under the chart
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        for s in series {
            let is_hidden = hidden_series.contains(&s.name);
            let dot_color = if is_hidden {
                Color32::from_gray(185)
            } else {
                s.color
            };
            let text_color = if is_hidden {
                Color32::from_gray(155)
            } else {
                Color32::from_gray(45)
            };
            let label_resp = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, dot_color);
                ui.add(
                    egui::Label::new(RichText::new(&s.name).size(11.5).color(text_color))
                        .sense(egui::Sense::click()),
                )
            });
            if label_resp.inner.clicked() {
                if is_hidden {
                    hidden_series.remove(&s.name);
                } else {
                    hidden_series.insert(s.name.clone());
                }
            }
        }
    });

    ChartOutput {
        x_bounds: new_bounds,
        user_interacted: interacted,
        double_clicked: resp.response.double_clicked(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_splitting_breaks_on_sleep_pauses() {
        let pts = [(1.0, 5.0), (2.0, 6.0), (15.0, 7.0), (16.0, 8.0)];
        let segs = split_segments_by_gap(&pts, 5.0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], vec![[1.0, 5.0], [2.0, 6.0]]);
        assert_eq!(segs[1], vec![[15.0, 7.0], [16.0, 8.0]]);
    }
}
