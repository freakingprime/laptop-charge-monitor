use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::Utc;
use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, Ui};
use egui_plot::{AxisHints, GridMark, HPlacement};

use crate::model::{
    AppSettings, DOWNSAMPLE_THRESHOLD_SECS, MAX_CHART_POINTS, MAX_ZOOM_SPAN_SECS,
    MIN_ZOOM_SPAN_SECS, SystemMetadata, TelemetrySample, compute_draining_on_charger_secs,
    format_duration_compact, format_span_label, is_valid_window_coord, resolve_startup_range,
    sanitize_window_height, sanitize_window_width,
};
use crate::storage::{RETENTION_DAYS_MS, TelemetryDb};
use crate::ui::charts::{
    SeriesSpec, collect_temp_series, render_time_chart, split_segments_by_gap,
};

const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct LiveUpdate {
    pub sample: TelemetrySample,
    pub total_readings: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetSpan {
    FiveMin,
    FifteenMin,
    OneHour,
    TwentyFourHours,
}

impl PresetSpan {
    fn secs(self) -> f64 {
        match self {
            Self::FiveMin => 300.0,
            Self::FifteenMin => 900.0,
            Self::OneHour => 3_600.0,
            Self::TwentyFourHours => 86_400.0,
        }
    }
}

pub struct PowerMonitorApp {
    meta: SystemMetadata,
    db: Option<TelemetryDb>,
    rx: Receiver<LiveUpdate>,
    settings_tx: Sender<AppSettings>,

    latest: Option<TelemetrySample>,
    total_readings: i64,
    samples: Vec<TelemetrySample>,
    draining_visible_secs: i64,

    is_live: bool,
    force_x_bounds_frames: u8,
    x_range: (f64, f64),
    loaded_range_ms: (i64, i64),
    loaded_downsampled: bool,
    last_db_refresh_secs: f64,
    hidden_series: BTreeSet<String>,

    window_x: Option<f32>,
    window_y: Option<f32>,
    window_width: f32,
    window_height: f32,
    window_maximized: bool,
    last_persisted_settings: AppSettings,
    pending_settings: Option<(AppSettings, Instant)>,
}

impl PowerMonitorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        meta: SystemMetadata,
        db: Option<TelemetryDb>,
        rx: Receiver<LiveUpdate>,
        settings_tx: Sender<AppSettings>,
        initial_total: i64,
        initial_settings: AppSettings,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());

        let canonical_settings = initial_settings.canonical();
        let now_ms = Utc::now().timestamp_millis();
        let now_secs = now_ms as f64 / 1_000.0;
        let retention_cutoff_secs = (now_ms - RETENTION_DAYS_MS) as f64 / 1_000.0;
        let (is_live, x_range) =
            resolve_startup_range(&canonical_settings, now_secs, retention_cutoff_secs);

        let mut app = Self {
            meta,
            db,
            rx,
            settings_tx,
            latest: None,
            total_readings: initial_total,
            samples: Vec::new(),
            draining_visible_secs: 0,
            is_live,
            force_x_bounds_frames: 2,
            x_range,
            loaded_range_ms: (0, 0),
            loaded_downsampled: false,
            last_db_refresh_secs: 0.0,
            hidden_series: canonical_settings.hidden_series.clone(),
            window_x: canonical_settings.window_x,
            window_y: canonical_settings.window_y,
            window_width: canonical_settings.window_width,
            window_height: canonical_settings.window_height,
            window_maximized: canonical_settings.window_maximized,
            last_persisted_settings: canonical_settings,
            pending_settings: None,
        };
        app.reload_window_from_db();
        app.latest = app.samples.last().cloned();
        app
    }

    fn drain_live_updates(&mut self) {
        let mut received_any = false;
        while let Ok(msg) = self.rx.try_recv() {
            self.total_readings = msg.total_readings;
            self.latest = Some(msg.sample.clone());
            if self.is_live && !self.loaded_downsampled {
                self.samples.push(msg.sample);
            }
            received_any = true;
        }

        if !received_any {
            return;
        }

        if self.is_live {
            let span =
                (self.x_range.1 - self.x_range.0).clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
            let now_secs = self
                .latest
                .as_ref()
                .map(|s| s.ts_secs())
                .unwrap_or_else(|| Utc::now().timestamp_millis() as f64 / 1_000.0);
            self.x_range = (now_secs - span, now_secs);
            self.force_x_bounds_frames = 1;

            if self.loaded_downsampled {
                if (now_secs - self.last_db_refresh_secs) >= 5.0 {
                    self.reload_window_from_db();
                }
            } else {
                let min_keep_ms = ((self.x_range.0 - 60.0) * 1_000.0) as i64;
                if let Some(pos) = self.samples.iter().position(|s| s.ts_ms >= min_keep_ms)
                    && pos > 32
                {
                    self.samples.drain(..pos);
                }
                self.recompute_visible_draining();
            }
        }
    }

    fn apply_preset(&mut self, preset: PresetSpan) {
        let now_secs = self
            .latest
            .as_ref()
            .map(|s| s.ts_secs())
            .unwrap_or_else(|| Utc::now().timestamp_millis() as f64 / 1_000.0);
        self.is_live = true;
        self.x_range = (now_secs - preset.secs(), now_secs);
        self.force_x_bounds_frames = 2;
        self.reload_window_from_db();
    }

    fn snap_to_live(&mut self) {
        let span = (self.x_range.1 - self.x_range.0).clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
        let now_secs = self
            .latest
            .as_ref()
            .map(|s| s.ts_secs())
            .unwrap_or_else(|| Utc::now().timestamp_millis() as f64 / 1_000.0);
        self.is_live = true;
        self.x_range = (now_secs - span, now_secs);
        self.force_x_bounds_frames = 2;
        self.reload_window_from_db();
    }

    fn maybe_reload_for_pan_zoom(&mut self) {
        let span = self.x_range.1 - self.x_range.0;
        let should_downsample = span > DOWNSAMPLE_THRESHOLD_SECS;
        let min_ms = (self.x_range.0 * 1_000.0) as i64;
        let max_ms = (self.x_range.1 * 1_000.0) as i64;

        let out_of_loaded = min_ms < self.loaded_range_ms.0 || max_ms > self.loaded_range_ms.1;
        let resolution_changed = should_downsample != self.loaded_downsampled;

        if out_of_loaded || resolution_changed {
            self.reload_window_from_db();
        } else {
            self.recompute_visible_draining();
        }
    }

    fn reload_window_from_db(&mut self) {
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let span_secs = (self.x_range.1 - self.x_range.0).max(MIN_ZOOM_SPAN_SECS);
        let pad_secs = (span_secs * 0.25).min(3_600.0);
        let query_min_ms = ((self.x_range.0 - pad_secs) * 1_000.0) as i64;
        let query_max_ms = ((self.x_range.1 + pad_secs) * 1_000.0) as i64;
        let threshold_ms = (DOWNSAMPLE_THRESHOLD_SECS * 1_000.0) as i64;

        if let Ok(loaded) =
            db.load_window(query_min_ms, query_max_ms, threshold_ms, MAX_CHART_POINTS)
        {
            self.samples = loaded;
            self.loaded_range_ms = (query_min_ms, query_max_ms);
            self.loaded_downsampled = span_secs > DOWNSAMPLE_THRESHOLD_SECS;
            self.last_db_refresh_secs = Utc::now().timestamp_millis() as f64 / 1_000.0;
            self.recompute_visible_draining();
        }
    }

    fn recompute_visible_draining(&mut self) {
        let min_ms = (self.x_range.0 * 1_000.0) as i64;
        let max_ms = (self.x_range.1 * 1_000.0) as i64;
        if self.loaded_downsampled
            && let Some(db) = self.db.as_ref()
            && let Ok(secs) = db.draining_secs_in_range(min_ms, max_ms)
        {
            self.draining_visible_secs = secs;
            return;
        }
        self.draining_visible_secs =
            compute_draining_on_charger_secs(&self.samples, min_ms, max_ms);
    }

    fn render_cards(&self, ui: &mut Ui) {
        let s = self.latest.as_ref();

        let power_src = s.map(|r| r.power_source.as_str()).unwrap_or("—");
        let into_w = s
            .and_then(|r| r.into_battery_w)
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "—".to_string());
        let into_color = Color32::from_rgb(22, 163, 74);
        let out_w = s
            .and_then(|r| r.out_of_battery_w)
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "—".to_string());
        let out_color = Color32::from_rgb(220, 38, 38);

        let battery_str = match (
            s.and_then(|r| r.battery_pct),
            s.and_then(|r| r.battery_wh),
            s.and_then(|r| r.battery_full_wh),
        ) {
            (Some(pct), Some(rem), Some(full)) => {
                format!("{pct:.1} % ({rem:.1} / {full:.1} Wh)")
            }
            (Some(pct), _, _) => format!("{pct:.1} %"),
            _ => "—".to_string(),
        };
        let time_left_str = s
            .and_then(|r| r.time_left_s)
            .map(format_duration_compact)
            .unwrap_or_else(|| "—".to_string());
        let health_str = s
            .and_then(|r| r.battery_health_pct)
            .map(|h| format!("{h:.0} %"))
            .unwrap_or_else(|| "—".to_string());
        let volt_str = s
            .and_then(|r| r.battery_voltage_v)
            .map(|v| format!("{v:.2} V"))
            .unwrap_or_else(|| "—".to_string());
        let cycles_str = s
            .and_then(|r| r.battery_cycles)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".to_string());

        // Row 1
        ui.horizontal_wrapped(|ui| {
            render_kpi_card(ui, "Power source", power_src, Color32::from_gray(45));
            render_kpi_card(ui, "Into battery", &into_w, into_color);
            render_kpi_card(ui, "Out of battery", &out_w, out_color);
            render_kpi_card(ui, "Battery", &battery_str, Color32::from_rgb(37, 99, 235));
            render_kpi_card(ui, "Time left", &time_left_str, Color32::from_gray(45));
            render_kpi_card(ui, "Battery health", &health_str, Color32::from_gray(45));
            render_kpi_card(ui, "Battery voltage", &volt_str, Color32::from_gray(45));
            render_kpi_card(ui, "Battery cycles", &cycles_str, Color32::from_gray(45));
        });

        ui.add_space(4.0);

        // Row 2
        let cpu_ghz = s
            .and_then(|r| r.cpu_ghz)
            .map(|g| format!("{g:.2} GHz"))
            .unwrap_or_else(|| "—".to_string());
        let cpu_busy = s
            .and_then(|r| r.cpu_busy_pct)
            .map(|p| format!("{p:.0} %"))
            .unwrap_or_else(|| "—".to_string());
        let cpu_power = s
            .and_then(|r| r.cpu_power_w)
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "—".to_string());

        let igpu_str = match (
            s.and_then(|r| r.igpu_mhz),
            s.and_then(|r| r.igpu_busy_pct),
            s.and_then(|r| r.igpu_voltage_v),
        ) {
            (Some(mhz), Some(pct), Some(v)) => format!("{mhz:.0} MHz · {pct:.1} % · {v:.2} V"),
            (Some(mhz), Some(pct), None) => format!("{mhz:.0} MHz · {pct:.1} %"),
            (Some(mhz), None, Some(v)) => format!("{mhz:.0} MHz · {v:.2} V"),
            (Some(mhz), None, None) => format!("{mhz:.0} MHz"),
            _ => "—".to_string(),
        };
        let draining_str = format_duration_compact(self.draining_visible_secs);

        ui.horizontal_wrapped(|ui| {
            render_kpi_card(ui, "CPU speed", &cpu_ghz, Color32::from_rgb(124, 58, 237));
            render_kpi_card(ui, "CPU busy", &cpu_busy, Color32::from_gray(45));
            render_kpi_card(ui, "CPU power", &cpu_power, Color32::from_rgb(124, 58, 237));
            render_kpi_card(ui, "iGPU", &igpu_str, Color32::from_rgb(13, 148, 136));
            render_kpi_card(
                ui,
                "Draining on charger (visible range)",
                &draining_str,
                Color32::from_gray(45),
            );
        });

        ui.add_space(4.0);

        // Row 3: Dedicated temperature cards (+ conditional Fan RPM)
        let ipf_temp = s
            .and_then(|r| r.temperatures_c.get("CPU (IPF)").copied())
            .map(|t| format!("{t:.1} °C"))
            .unwrap_or_else(|| "—".to_string());
        let nvme_temp = s
            .and_then(|r| {
                r.temperatures_c
                    .iter()
                    .find(|(k, _)| k.starts_with("NVMe"))
                    .map(|(_, &v)| v)
            })
            .map(|t| format!("{t:.1} °C"))
            .unwrap_or_else(|| "—".to_string());
        let tz00_temp = s
            .and_then(|r| r.temperatures_c.get("TZ00").copied())
            .map(|t| format!("{t:.1} °C"))
            .unwrap_or_else(|| "—".to_string());

        ui.horizontal_wrapped(|ui| {
            render_kpi_card(
                ui,
                "CPU (IPF) temp",
                &ipf_temp,
                Color32::from_rgb(234, 88, 12),
            );
            render_kpi_card(ui, "NVMe temp", &nvme_temp, Color32::from_rgb(13, 148, 136));
            render_kpi_card(ui, "TZ00 temp", &tz00_temp, Color32::from_rgb(37, 99, 235));
            if let Some(rpm) = s.and_then(|r| r.fan_rpm) {
                render_kpi_card(ui, "Fan RPM", &format!("{rpm} RPM"), Color32::from_gray(45));
            }
        });
    }

    fn render_toolbar(&mut self, ui: &mut Ui) {
        let span_secs = (self.x_range.1 - self.x_range.0).max(MIN_ZOOM_SPAN_SECS);
        let active_preset = if self.is_live {
            match span_secs.round() as i64 {
                295..=305 => Some(PresetSpan::FiveMin),
                895..=905 => Some(PresetSpan::FifteenMin),
                3590..=3610 => Some(PresetSpan::OneHour),
                86380..=86420 => Some(PresetSpan::TwentyFourHours),
                _ => None,
            }
        } else {
            None
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Show last:")
                    .size(12.0)
                    .color(Color32::from_gray(75)),
            );
            for (preset, label) in [
                (PresetSpan::FiveMin, "5m"),
                (PresetSpan::FifteenMin, "15m"),
                (PresetSpan::OneHour, "1h"),
                (PresetSpan::TwentyFourHours, "24h"),
            ] {
                let selected = active_preset == Some(preset);
                if ui.selectable_label(selected, label).clicked() {
                    self.apply_preset(preset);
                }
            }

            ui.separator();
            ui.label(
                RichText::new(format!("Visible range: {}", format_span_label(span_secs)))
                    .size(12.0)
                    .color(Color32::from_gray(55)),
            );
            ui.separator();

            let live_btn = if self.is_live {
                egui::Button::new(
                    RichText::new("LIVE")
                        .strong()
                        .size(11.5)
                        .color(Color32::from_rgb(22, 163, 74)),
                )
                .fill(Color32::from_rgb(220, 252, 231))
            } else {
                egui::Button::new(
                    RichText::new("LIVE")
                        .size(11.5)
                        .color(Color32::from_gray(95)),
                )
            };
            if ui.add(live_btn).clicked() {
                self.snap_to_live();
            }

            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{} · base {:.2} GHz · {} readings · {}",
                    self.meta.cpu_name,
                    self.meta.base_ghz,
                    self.total_readings,
                    self.meta.db_path_display
                ))
                .size(11.5)
                .color(Color32::from_gray(105)),
            );
        });

        ui.label(
            RichText::new(
                "scroll = zoom (1 min – 3 days) · drag = move in time · double-click a chart or press LIVE to follow · click a name under a chart to hide or show that line",
            )
            .size(11.0)
            .color(Color32::from_gray(125)),
        );
    }

    fn render_charts(&mut self, ui: &mut Ui) {
        let span_secs = (self.x_range.1 - self.x_range.0).max(MIN_ZOOM_SPAN_SECS);
        let gap_secs = if self.loaded_downsampled {
            ((span_secs / MAX_CHART_POINTS as f64) * 3.0).max(5.0)
        } else {
            5.0
        };

        let avail_h = ui.available_height();
        let chart_h = ((avail_h - 140.0) / 3.0).clamp(135.0, 280.0);

        let force_x = self.force_x_bounds_frames > 0;
        if self.force_x_bounds_frames > 0 {
            self.force_x_bounds_frames -= 1;
        }

        // 1. Power (W)
        let mut into_pts = Vec::new();
        let mut out_pts = Vec::new();
        let mut cpu_pwr_pts = Vec::new();
        for s in &self.samples {
            let x = s.ts_secs();
            if let Some(v) = s.into_battery_w {
                into_pts.push((x, v));
            }
            if let Some(v) = s.out_of_battery_w {
                out_pts.push((x, v));
            }
            if let Some(v) = s.cpu_power_w {
                cpu_pwr_pts.push((x, v));
            }
        }
        let power_series = vec![
            SeriesSpec {
                name: "Into battery (W)".to_string(),
                color: Color32::from_rgb(22, 163, 74),
                segments: split_segments_by_gap(&into_pts, gap_secs),
                right_axis_pct_scale: None,
            },
            SeriesSpec {
                name: "Out of battery (W)".to_string(),
                color: Color32::from_rgb(220, 38, 38),
                segments: split_segments_by_gap(&out_pts, gap_secs),
                right_axis_pct_scale: None,
            },
            SeriesSpec {
                name: "CPU power (W)".to_string(),
                color: Color32::from_rgb(124, 58, 237),
                segments: split_segments_by_gap(&cpu_pwr_pts, gap_secs),
                right_axis_pct_scale: None,
            },
        ];
        let power_axes = vec![
            AxisHints::new_y()
                .label("Watts")
                .placement(HPlacement::Left),
        ];
        let r1 = render_time_chart(
            ui,
            "chart_power_w",
            "Power (W)",
            chart_h,
            (0.0, 60.0),
            power_axes,
            self.x_range,
            force_x,
            &power_series,
            &mut self.hidden_series,
            " W",
        );

        ui.add_space(4.0);

        // 2. CPU speed (GHz, left) & CPU/iGPU busy (%, right)
        let mut max_ghz = 5.0_f64;
        for s in &self.samples {
            if let Some(g) = s.cpu_ghz
                && g > max_ghz
            {
                max_ghz = g.ceil();
            }
        }
        let pct_scale = max_ghz / 100.0;

        let mut ghz_pts = Vec::new();
        let mut cpu_busy_pts = Vec::new();
        let mut igpu_busy_pts = Vec::new();
        for s in &self.samples {
            let x = s.ts_secs();
            if let Some(g) = s.cpu_ghz {
                ghz_pts.push((x, g));
            }
            if let Some(p) = s.cpu_busy_pct {
                cpu_busy_pts.push((x, p * pct_scale));
            }
            if let Some(p) = s.igpu_busy_pct {
                igpu_busy_pts.push((x, p * pct_scale));
            }
        }
        let cpu_series = vec![
            SeriesSpec {
                name: "CPU speed (GHz)".to_string(),
                color: Color32::from_rgb(124, 58, 237),
                segments: split_segments_by_gap(&ghz_pts, gap_secs),
                right_axis_pct_scale: None,
            },
            SeriesSpec {
                name: "CPU busy (%)".to_string(),
                color: Color32::from_rgb(75, 85, 99),
                segments: split_segments_by_gap(&cpu_busy_pts, gap_secs),
                right_axis_pct_scale: Some(pct_scale),
            },
            SeriesSpec {
                name: "iGPU busy (%)".to_string(),
                color: Color32::from_rgb(13, 148, 136),
                segments: split_segments_by_gap(&igpu_busy_pts, gap_secs),
                right_axis_pct_scale: Some(pct_scale),
            },
        ];
        let cpu_axes = vec![
            AxisHints::new_y().label("GHz").placement(HPlacement::Left),
            AxisHints::new_y()
                .label("busy %")
                .placement(HPlacement::Right)
                .formatter(move |mark: GridMark, _| {
                    let pct = mark.value / pct_scale;
                    if (-0.5..=100.5).contains(&pct) {
                        format!("{pct:.0}")
                    } else {
                        String::new()
                    }
                }),
        ];
        let r2 = render_time_chart(
            ui,
            "chart_cpu_speed_busy",
            "CPU speed (GHz, left) and busy (%, right)",
            chart_h,
            (0.0, max_ghz),
            cpu_axes,
            self.x_range,
            force_x,
            &cpu_series,
            &mut self.hidden_series,
            " GHz",
        );

        ui.add_space(4.0);

        // 3. Temperatures (°C)
        let temp_series = collect_temp_series(&self.samples, gap_secs);
        let temp_axes = vec![AxisHints::new_y().label("°C").placement(HPlacement::Left)];
        let r3 = render_time_chart(
            ui,
            "chart_temperatures",
            "Temperatures (°C)",
            chart_h,
            (20.0, 60.0),
            temp_axes,
            self.x_range,
            force_x,
            &temp_series,
            &mut self.hidden_series,
            " °C",
        );

        // Apply pan/zoom or double-click back to LIVE
        if r1.double_clicked || r2.double_clicked || r3.double_clicked {
            self.snap_to_live();
            return;
        }

        let interacted_bounds = [r1, r2, r3]
            .into_iter()
            .find(|o| o.user_interacted)
            .map(|o| o.x_bounds);

        if let Some((b_min, b_max)) = interacted_bounds {
            self.is_live = false;
            let raw_span = b_max - b_min;
            let clamped_span = raw_span.clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
            let center = 0.5 * (b_min + b_max);
            self.x_range = (center - 0.5 * clamped_span, center + 0.5 * clamped_span);
            if (raw_span - clamped_span).abs() > 0.5 {
                self.force_x_bounds_frames = 1;
            }
            self.maybe_reload_for_pan_zoom();
        }
    }

    fn capture_window_geometry(&mut self, ctx: &egui::Context) {
        let vp = ctx.input(|i| i.viewport().clone());
        if vp.minimized == Some(true) {
            return;
        }
        if let Some(maximized) = vp.maximized {
            self.window_maximized = maximized;
            if maximized {
                // Keep the last normal (unmaximized) position and size for restore.
                return;
            }
        } else if self.window_maximized {
            return;
        }
        if let Some(outer) = vp.outer_rect
            && is_valid_window_coord(outer.min.x)
            && is_valid_window_coord(outer.min.y)
        {
            self.window_x = Some(outer.min.x);
            self.window_y = Some(outer.min.y);
        }
        if let Some(inner) = vp.inner_rect
            && inner.width() >= 100.0
            && inner.height() >= 100.0
        {
            self.window_width = sanitize_window_width(inner.width());
            self.window_height = sanitize_window_height(inner.height());
        }
    }

    fn current_settings(&self) -> AppSettings {
        let span_secs =
            (self.x_range.1 - self.x_range.0).clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
        AppSettings {
            window_x: self.window_x,
            window_y: self.window_y,
            window_width: self.window_width,
            window_height: self.window_height,
            window_maximized: self.window_maximized,
            is_live: self.is_live,
            span_secs,
            paused_x_min: if self.is_live {
                None
            } else {
                Some(self.x_range.0)
            },
            paused_x_max: if self.is_live {
                None
            } else {
                Some(self.x_range.1)
            },
            hidden_series: self.hidden_series.clone(),
        }
        .canonical()
    }

    fn flush_settings(&mut self, settings: AppSettings) {
        if self.settings_tx.send(settings.clone()).is_ok() {
            self.last_persisted_settings = settings;
        }
        self.pending_settings = None;
    }

    fn sync_settings(&mut self, ctx: &egui::Context) {
        let current = self.current_settings();
        if current == self.last_persisted_settings {
            self.pending_settings = None;
            return;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            self.flush_settings(current);
            return;
        }

        if let Some((pending, dirty_since)) = &self.pending_settings
            && *pending == current
        {
            if dirty_since.elapsed() >= SETTINGS_SAVE_DEBOUNCE {
                self.flush_settings(current);
            }
            return;
        }

        self.pending_settings = Some((current, Instant::now()));
        ctx.request_repaint_after(SETTINGS_SAVE_DEBOUNCE + Duration::from_millis(20));
    }
}

impl Drop for PowerMonitorApp {
    fn drop(&mut self) {
        let current = self.current_settings();
        if current != self.last_persisted_settings || self.pending_settings.is_some() {
            self.flush_settings(current);
        }
    }
}

impl eframe::App for PowerMonitorApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_live_updates();
        self.capture_window_geometry(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(Frame::central_panel(ui.style()).fill(Color32::from_rgb(248, 249, 251)))
            .show(ui, |ui| {
                self.render_cards(ui);
                ui.add_space(6.0);
                self.render_toolbar(ui);
                ui.add_space(4.0);
                self.render_charts(ui);
            });
        self.sync_settings(ui.ctx());
    }
}

fn render_kpi_card(ui: &mut Ui, title: &str, value: &str, value_color: Color32) {
    Frame::NONE
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_rgb(214, 219, 226)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new(title)
                        .size(10.5)
                        .color(Color32::from_gray(110)),
                );
                ui.label(RichText::new(value).size(15.5).color(value_color));
            });
        });
}
