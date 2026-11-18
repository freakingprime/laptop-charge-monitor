use std::collections::{BTreeMap, BTreeSet};

/// Maximum elapsed time between two energy counter samples before treating the
/// interval as a sleep/pause gap and discarding the wattage reading.
pub const MAX_ENERGY_DELTA_SECS: f64 = 5.0;

/// Maximum physically plausible combined CPU+iGPU package power for this laptop.
/// Readings above this threshold are discarded as counter reset/wrap spikes.
pub const MAX_PLAUSIBLE_CPU_WATTS: f64 = 150.0;

/// Maximum visible span (3 days in seconds).
pub const MAX_ZOOM_SPAN_SECS: f64 = 3.0 * 86_400.0;

/// Minimum visible span (1 minute in seconds).
pub const MIN_ZOOM_SPAN_SECS: f64 = 60.0;

/// Default visible chart span on first launch (5 minutes in seconds).
pub const DEFAULT_SPAN_SECS: f64 = 300.0;

/// Default and minimum window dimensions (logical points).
pub const DEFAULT_WINDOW_WIDTH: f32 = 1360.0;
pub const DEFAULT_WINDOW_HEIGHT: f32 = 860.0;
pub const MIN_WINDOW_WIDTH: f32 = 960.0;
pub const MIN_WINDOW_HEIGHT: f32 = 640.0;
const MAX_WINDOW_DIM: f32 = 16_000.0;
const MIN_VALID_WINDOW_COORD: f32 = -10_000.0;
const MAX_VALID_WINDOW_COORD: f32 = 32_000.0;

/// Threshold above which chart queries downsample raw 1s points.
pub const DOWNSAMPLE_THRESHOLD_SECS: f64 = 1_800.0;

/// Target point budget per chart series when zoomed out.
pub const MAX_CHART_POINTS: usize = 1_500;

/// Static CPU/system metadata displayed in the toolbar subtitle.
#[derive(Debug, Clone)]
pub struct SystemMetadata {
    pub cpu_name: String,
    pub base_ghz: f64,
    pub db_path_display: String,
}

/// Raw OS CPU tick counters from `GetSystemTimes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

/// A single 1-second telemetry snapshot persisted to SQLite and rendered in UI.
#[derive(Debug, Clone)]
pub struct TelemetrySample {
    pub ts_ms: i64,
    pub power_source: String,
    pub into_battery_w: Option<f64>,
    pub out_of_battery_w: Option<f64>,
    pub battery_pct: Option<f64>,
    pub battery_wh: Option<f64>,
    pub battery_full_wh: Option<f64>,
    pub time_left_s: Option<i64>,
    pub battery_health_pct: Option<f64>,
    pub battery_voltage_v: Option<f64>,
    pub battery_cycles: Option<u32>,
    pub draining_on_charger: bool,
    pub cpu_ghz: Option<f64>,
    pub cpu_busy_pct: Option<f64>,
    pub cpu_power_w: Option<f64>,
    pub igpu_busy_pct: Option<f64>,
    pub igpu_mhz: Option<f64>,
    pub igpu_voltage_v: Option<f64>,
    pub fan_rpm: Option<u32>,
    pub temperatures_c: BTreeMap<String, f64>,
}

impl TelemetrySample {
    pub fn ts_secs(&self) -> f64 {
        self.ts_ms as f64 / 1_000.0
    }
}

/// Computes average power in Watts from two monotonic energy snapshots.
/// Returns `None` on negative deltas, intervals > `max_dt_secs`, or spikes > `max_watts`.
pub fn compute_energy_watts_u64(
    prev_energy_uj: u64,
    curr_energy_uj: u64,
    prev_ts_us: u64,
    curr_ts_us: u64,
    max_dt_secs: f64,
    max_watts: f64,
) -> Option<f64> {
    let de = curr_energy_uj.checked_sub(prev_energy_uj)?;
    let dt_us = curr_ts_us.checked_sub(prev_ts_us)?;
    if dt_us == 0 {
        return None;
    }
    let dt_secs = dt_us as f64 / 1_000_000.0;
    let watts = (de as f64 / 1_000_000.0) / dt_secs;
    validate_watts(watts, dt_secs, max_dt_secs, max_watts)
}

/// Computes average power in Watts from floating-point Joules and Seconds snapshots (IGCL).
pub fn compute_energy_watts_f64(
    prev_energy_j: f64,
    curr_energy_j: f64,
    prev_ts_s: f64,
    curr_ts_s: f64,
    max_dt_secs: f64,
    max_watts: f64,
) -> Option<f64> {
    if !prev_energy_j.is_finite()
        || !curr_energy_j.is_finite()
        || !prev_ts_s.is_finite()
        || !curr_ts_s.is_finite()
    {
        return None;
    }
    let dt_secs = curr_ts_s - prev_ts_s;
    let de_j = curr_energy_j - prev_energy_j;
    if dt_secs <= 0.0 || de_j < 0.0 {
        return None;
    }
    let watts = de_j / dt_secs;
    validate_watts(watts, dt_secs, max_dt_secs, max_watts)
}

fn validate_watts(watts: f64, dt_secs: f64, max_dt_secs: f64, max_watts: f64) -> Option<f64> {
    if dt_secs <= 0.0
        || dt_secs > max_dt_secs
        || !watts.is_finite()
        || watts < 0.0
        || watts > max_watts
    {
        return None;
    }
    Some(watts)
}

/// Computes percent utilization `[0.0, 100.0]` from monotonic active/timestamp microsecond counters.
pub fn compute_activity_pct_u64(
    prev_active: u64,
    curr_active: u64,
    prev_ts: u64,
    curr_ts: u64,
    max_dt_secs: f64,
) -> Option<f64> {
    let da = curr_active.checked_sub(prev_active)?;
    let dt = curr_ts.checked_sub(prev_ts)?;
    if dt == 0 || (dt as f64 / 1_000_000.0) > max_dt_secs {
        return None;
    }
    Some(((da as f64 / dt as f64) * 100.0).clamp(0.0, 100.0))
}

/// Computes CPU utilization `[0.0, 100.0]` from two `GetSystemTimes` snapshots.
/// Note: Win32 `kernel` time includes `idle` time.
pub fn compute_cpu_busy_pct(prev: CpuTimes, curr: CpuTimes) -> Option<f64> {
    let idle = curr.idle.checked_sub(prev.idle)?;
    let kernel = curr.kernel.checked_sub(prev.kernel)?;
    let user = curr.user.checked_sub(prev.user)?;
    let total = kernel.saturating_add(user);
    if total == 0 || idle > total {
        return None;
    }
    let busy = total - idle;
    Some(((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0))
}

/// Collapses raw `EsifDeviceInformation` `(InstanceName, Temperature)` rows.
/// Matching `DEV_A71D` Intel IPF processor participants are averaged into `"CPU (IPF)"`,
/// ignoring invalid `<= 0` or `>= 150` readings.
pub fn collapse_esif_temps<'a, I>(rows: I) -> BTreeMap<String, f64>
where
    I: IntoIterator<Item = (&'a str, i32)>,
{
    let mut grouped: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (instance_name, temp) in rows {
        if temp <= 0 || temp >= 150 {
            continue;
        }
        let label = if instance_name.contains("DEV_A71D") {
            "CPU (IPF)".to_string()
        } else {
            let trimmed = instance_name
                .rsplit('\\')
                .next()
                .unwrap_or(instance_name)
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
            if trimmed.is_empty() {
                "ESIF".to_string()
            } else {
                format!("ESIF {trimmed}")
            }
        };
        grouped.entry(label).or_default().push(temp as f64);
    }

    grouped
        .into_iter()
        .map(|(k, vals)| {
            let max_val = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            (k, max_val)
        })
        .collect()
}

/// Counts how many seconds in `[min_ts_ms, max_ts_ms]` the laptop was draining while on charger.
pub fn compute_draining_on_charger_secs(
    samples: &[TelemetrySample],
    min_ts_ms: i64,
    max_ts_ms: i64,
) -> i64 {
    let mut total_ms: i64 = 0;
    let visible: Vec<&TelemetrySample> = samples
        .iter()
        .filter(|s| s.ts_ms >= min_ts_ms && s.ts_ms <= max_ts_ms)
        .collect();

    for pair in visible.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if a.draining_on_charger {
            let dt = (b.ts_ms - a.ts_ms).clamp(0, 5_000);
            total_ms += dt;
        }
    }
    if let Some(last) = visible.last()
        && last.draining_on_charger
        && visible.len() == 1
    {
        total_ms += 1_000;
    }
    total_ms / 1_000
}

/// Downsamples a sorted `(x, y)` series to at most `max_points` using min/max envelope buckets,
/// preserving peaks and valleys without losing transient spikes.
pub fn downsample_series(points: &[[f64; 2]], max_points: usize) -> Vec<[f64; 2]> {
    if points.len() <= max_points || max_points < 4 {
        return points.to_vec();
    }
    let buckets = (max_points / 2).max(1);
    let len = points.len();
    let mut out = Vec::with_capacity(buckets * 2 + 2);

    for b in 0..buckets {
        let start = b * len / buckets;
        let end = ((b + 1) * len / buckets).min(len);
        if start >= end {
            continue;
        }
        let slice = &points[start..end];
        let mut min_pt = slice[0];
        let mut max_pt = slice[0];
        for &p in slice.iter().skip(1) {
            if p[1] < min_pt[1] {
                min_pt = p;
            }
            if p[1] > max_pt[1] {
                max_pt = p;
            }
        }
        if min_pt[0] <= max_pt[0] {
            out.push(min_pt);
            if min_pt != max_pt {
                out.push(max_pt);
            }
        } else {
            out.push(max_pt);
            if min_pt != max_pt {
                out.push(min_pt);
            }
        }
    }
    out
}

/// Formats seconds into compact human-readable duration (e.g., `0s`, `45s`, `2h 12m`).
pub fn format_duration_compact(secs: i64) -> String {
    if secs < 0 {
        return "—".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        format!("{hours}h {rem_mins}m")
    } else {
        let days = hours / 24;
        let rem_hours = hours % 24;
        format!("{days}d {rem_hours}h")
    }
}

/// Formats visible chart span cleanly (`5 min`, `15 min`, `2.5 hours`, `1.2 days`).
pub fn format_span_label(span_secs: f64) -> String {
    if span_secs < 120.0 {
        format!("{:.0} sec", span_secs.max(1.0))
    } else if span_secs < 3_600.0 {
        format!("{:.0} min", (span_secs / 60.0).round())
    } else if span_secs < 86_400.0 {
        let hours = span_secs / 3_600.0;
        if (hours - hours.round()).abs() < 0.05 {
            format!("{:.0} h", hours.round())
        } else {
            format!("{hours:.1} h")
        }
    } else {
        let days = span_secs / 86_400.0;
        format!("{days:.1} d")
    }
}

/// Persisted UI, chart viewport, and series visibility settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AppSettings {
    pub window_x: Option<f32>,
    pub window_y: Option<f32>,
    pub window_width: f32,
    pub window_height: f32,
    pub window_maximized: bool,
    pub is_live: bool,
    pub span_secs: f64,
    pub paused_x_min: Option<f64>,
    pub paused_x_max: Option<f64>,
    pub hidden_series: BTreeSet<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            window_x: None,
            window_y: None,
            window_width: DEFAULT_WINDOW_WIDTH,
            window_height: DEFAULT_WINDOW_HEIGHT,
            window_maximized: false,
            is_live: true,
            span_secs: DEFAULT_SPAN_SECS,
            paused_x_min: None,
            paused_x_max: None,
            hidden_series: BTreeSet::new(),
        }
    }
}

/// Validates a native outer window coordinate, filtering Windows `-32000` minimized values.
pub fn is_valid_window_coord(v: f32) -> bool {
    v.is_finite() && v > MIN_VALID_WINDOW_COORD && v < MAX_VALID_WINDOW_COORD
}

/// Sanitizes window width to valid minimum/maximum logical point bounds.
pub fn sanitize_window_width(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_DIM)
    } else {
        DEFAULT_WINDOW_WIDTH
    }
}

/// Sanitizes window height to valid minimum/maximum logical point bounds.
pub fn sanitize_window_height(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_DIM)
    } else {
        DEFAULT_WINDOW_HEIGHT
    }
}

fn round_tenths(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

fn round_millis_f64(v: f64) -> f64 {
    (v * 1_000.0).round() / 1_000.0
}

fn parse_bool_str(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn encode_string_set(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (idx, item) in set.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        for ch in item.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                ',' => out.push_str("\\,"),
                _ => out.push(ch),
            }
        }
    }
    out
}

fn decode_string_set(raw: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if raw.is_empty() {
        return set;
    }
    let mut current = String::new();
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ',' => {
                if !current.is_empty() {
                    set.insert(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        set.insert(current);
    }
    set
}

impl AppSettings {
    /// Returns a validated canonical copy suitable for dirty comparison and persistence.
    pub fn canonical(&self) -> Self {
        let (window_x, window_y) = match (self.window_x, self.window_y) {
            (Some(x), Some(y)) if is_valid_window_coord(x) && is_valid_window_coord(y) => {
                (Some(round_tenths(x)), Some(round_tenths(y)))
            }
            _ => (None, None),
        };
        let window_width = round_tenths(sanitize_window_width(self.window_width));
        let window_height = round_tenths(sanitize_window_height(self.window_height));

        let span_secs = if self.span_secs.is_finite() {
            round_millis_f64(self.span_secs.clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS))
        } else {
            DEFAULT_SPAN_SECS
        };

        let (is_live, paused_x_min, paused_x_max) = if self.is_live {
            (true, None, None)
        } else if let (Some(x_min), Some(x_max)) = (self.paused_x_min, self.paused_x_max)
            && x_min.is_finite()
            && x_max.is_finite()
            && x_max > x_min
        {
            (
                false,
                Some(round_millis_f64(x_min)),
                Some(round_millis_f64(x_max)),
            )
        } else {
            (true, None, None)
        };

        Self {
            window_x,
            window_y,
            window_width,
            window_height,
            window_maximized: self.window_maximized,
            is_live,
            span_secs,
            paused_x_min,
            paused_x_max,
            hidden_series: self.hidden_series.clone(),
        }
    }

    /// Serializes settings into key-value pairs for the `app_settings` SQLite table.
    pub fn to_kv_pairs(&self) -> Vec<(&'static str, String)> {
        let s = self.canonical();
        vec![
            (
                "window_x",
                s.window_x.map(|v| format!("{v:.1}")).unwrap_or_default(),
            ),
            (
                "window_y",
                s.window_y.map(|v| format!("{v:.1}")).unwrap_or_default(),
            ),
            ("window_width", format!("{:.1}", s.window_width)),
            ("window_height", format!("{:.1}", s.window_height)),
            (
                "window_maximized",
                if s.window_maximized { "true" } else { "false" }.to_string(),
            ),
            (
                "is_live",
                if s.is_live { "true" } else { "false" }.to_string(),
            ),
            ("span_secs", format!("{:.3}", s.span_secs)),
            (
                "paused_x_min",
                s.paused_x_min
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_default(),
            ),
            (
                "paused_x_max",
                s.paused_x_max
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_default(),
            ),
            ("hidden_series", encode_string_set(&s.hidden_series)),
        ]
    }

    /// Reconstructs settings from stored key-value pairs, falling back safely on bad values.
    pub fn from_kv_pairs<'a, I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut s = Self::default();
        for (k, v) in iter {
            match k {
                "window_x" => {
                    s.window_x = v
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|&x| is_valid_window_coord(x));
                }
                "window_y" => {
                    s.window_y = v
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|&y| is_valid_window_coord(y));
                }
                "window_width" => {
                    if let Ok(w) = v.trim().parse::<f32>()
                        && w.is_finite()
                        && w > 0.0
                    {
                        s.window_width = sanitize_window_width(w);
                    }
                }
                "window_height" => {
                    if let Ok(h) = v.trim().parse::<f32>()
                        && h.is_finite()
                        && h > 0.0
                    {
                        s.window_height = sanitize_window_height(h);
                    }
                }
                "window_maximized" => {
                    if let Some(b) = parse_bool_str(v) {
                        s.window_maximized = b;
                    }
                }
                "is_live" => {
                    if let Some(b) = parse_bool_str(v) {
                        s.is_live = b;
                    }
                }
                "span_secs" => {
                    if let Ok(span) = v.trim().parse::<f64>()
                        && span.is_finite()
                        && span > 0.0
                    {
                        s.span_secs = span.clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
                    }
                }
                "paused_x_min" => {
                    s.paused_x_min = v.trim().parse::<f64>().ok().filter(|x| x.is_finite());
                }
                "paused_x_max" => {
                    s.paused_x_max = v.trim().parse::<f64>().ok().filter(|x| x.is_finite());
                }
                "hidden_series" => {
                    s.hidden_series = decode_string_set(v);
                }
                _ => {}
            }
        }
        s.canonical()
    }
}

/// Resolves startup `(is_live, x_range)` from persisted settings.
///
/// Restores paused historical windows directly unless they fell completely outside the 7-day
/// retention window, in which case it falls back to `LIVE` with the saved `span_secs`.
pub fn resolve_startup_range(
    settings: &AppSettings,
    now_secs: f64,
    retention_cutoff_secs: f64,
) -> (bool, (f64, f64)) {
    let saved_span = if settings.span_secs.is_finite() {
        settings
            .span_secs
            .clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS)
    } else {
        DEFAULT_SPAN_SECS
    };

    if !settings.is_live
        && let (Some(x_min), Some(x_max)) = (settings.paused_x_min, settings.paused_x_max)
        && x_min.is_finite()
        && x_max.is_finite()
        && x_max > x_min
        && x_max > retention_cutoff_secs
    {
        let raw_span = x_max - x_min;
        let range = if (MIN_ZOOM_SPAN_SECS..=MAX_ZOOM_SPAN_SECS).contains(&raw_span) {
            (x_min, x_max)
        } else {
            let clamped = raw_span.clamp(MIN_ZOOM_SPAN_SECS, MAX_ZOOM_SPAN_SECS);
            let center = 0.5 * (x_min + x_max);
            (center - 0.5 * clamped, center + 0.5 * clamped)
        };
        return (false, range);
    }

    (true, (now_secs - saved_span, now_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_watts_u64_valid_and_outliers() {
        // 5.5 J over 1.0 s = 5.5 W
        let w = compute_energy_watts_u64(
            1_000_000,
            6_500_000,
            10_000_000,
            11_000_000,
            MAX_ENERGY_DELTA_SECS,
            MAX_PLAUSIBLE_CPU_WATTS,
        );
        assert!((w.unwrap() - 5.5).abs() < 1e-6);

        // Negative energy wrap -> None
        assert_eq!(
            compute_energy_watts_u64(9_000_000, 1_000_000, 1_000_000, 2_000_000, 5.0, 150.0),
            None
        );
        // Sleep gap (> 5s) -> None
        assert_eq!(
            compute_energy_watts_u64(1_000_000, 20_000_000, 0, 6_000_000, 5.0, 150.0),
            None
        );
        // Spike > 150 W -> None
        assert_eq!(
            compute_energy_watts_u64(0, 200_000_000, 0, 1_000_000, 5.0, 150.0),
            None
        );
    }

    #[test]
    fn energy_watts_f64_valid_and_outliers() {
        let w = compute_energy_watts_f64(100.0, 110.5, 10.0, 10.5, 5.0, 150.0);
        assert!((w.unwrap() - 21.0).abs() < 1e-6);
        assert_eq!(
            compute_energy_watts_f64(10.0, 9.0, 1.0, 2.0, 5.0, 150.0),
            None
        );
        assert_eq!(
            compute_energy_watts_f64(0.0, 50.0, 0.0, 0.0, 5.0, 150.0),
            None
        );
    }

    #[test]
    fn cpu_busy_calculation() {
        let prev = CpuTimes {
            idle: 100,
            kernel: 150,
            user: 50,
        };
        let curr = CpuTimes {
            idle: 170,
            kernel: 240,
            user: 60,
        };
        // d_kernel = 90, d_user = 10 -> total = 100, d_idle = 70 -> busy = 30%
        let pct = compute_cpu_busy_pct(prev, curr).unwrap();
        assert!((pct - 30.0).abs() < 1e-6);
    }

    #[test]
    fn esif_temps_collapse_dev_a71d() {
        let rows = [
            (
                "PCI\\VEN_8086&DEV_A71D&SUBSYS_72708086\\3&11583659&0&20_0",
                0,
            ),
            (
                "PCI\\VEN_8086&DEV_A71D&SUBSYS_72708086\\3&11583659&0&20_1",
                43,
            ),
            (
                "PCI\\VEN_8086&DEV_A71D&SUBSYS_72708086\\3&11583659&0&20_2",
                45,
            ),
            (
                "PCI\\VEN_8086&DEV_A71D&SUBSYS_72708086\\3&11583659&0&20_4",
                0,
            ),
        ];
        let map = collapse_esif_temps(rows);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("CPU (IPF)").copied(), Some(45.0));
    }

    #[test]
    fn downsample_preserves_peaks_and_order() {
        let pts: Vec<[f64; 2]> = (0..100)
            .map(|i| {
                let y = if i == 53 { 99.0 } else { (i % 10) as f64 };
                [i as f64, y]
            })
            .collect();
        let ds = downsample_series(&pts, 20);
        assert!(ds.len() <= 20);
        assert!(ds.iter().any(|p| (p[1] - 99.0).abs() < 1e-9));
        for w in ds.windows(2) {
            assert!(w[0][0] <= w[1][0]);
        }
    }

    #[test]
    fn draining_on_charger_duration() {
        let make = |ts_ms, draining| TelemetrySample {
            ts_ms,
            power_source: "Charger".to_string(),
            into_battery_w: None,
            out_of_battery_w: None,
            battery_pct: None,
            battery_wh: None,
            battery_full_wh: None,
            time_left_s: None,
            battery_health_pct: None,
            battery_voltage_v: None,
            battery_cycles: None,
            draining_on_charger: draining,
            cpu_ghz: None,
            cpu_busy_pct: None,
            cpu_power_w: None,
            igpu_busy_pct: None,
            igpu_mhz: None,
            igpu_voltage_v: None,
            fan_rpm: None,
            temperatures_c: BTreeMap::new(),
        };
        let samples = vec![
            make(1_000, true),
            make(2_000, true),
            make(3_000, false),
            make(4_000, true),
            make(5_000, false),
        ];
        assert_eq!(compute_draining_on_charger_secs(&samples, 1_000, 5_000), 3);
    }

    #[test]
    fn app_settings_kv_roundtrip_and_escaping() {
        let mut hidden = BTreeSet::new();
        hidden.insert("CPU power (W)".to_string());
        hidden.insert("Sensor, 1\\2".to_string());

        let original = AppSettings {
            window_x: Some(-120.5),
            window_y: Some(42.0),
            window_width: 1440.0,
            window_height: 900.0,
            window_maximized: true,
            is_live: false,
            span_secs: 3_600.0,
            paused_x_min: Some(1_700_000_000.125),
            paused_x_max: Some(1_700_003_600.125),
            hidden_series: hidden,
        };

        let pairs = original.to_kv_pairs();
        let parsed = AppSettings::from_kv_pairs(pairs.iter().map(|(k, v)| (*k, v.as_str())));
        assert_eq!(parsed, original);
    }

    #[test]
    fn app_settings_rejects_minimized_and_corrupt_values() {
        let pairs = [
            ("window_x", "-32000.0"),
            ("window_y", "-32000.0"),
            ("window_width", "200.0"),
            ("window_height", "not_a_number"),
            ("window_maximized", "maybe"),
            ("is_live", "false"),
            ("span_secs", "5.0"),
            ("paused_x_min", ""),
            ("paused_x_max", ""),
            ("unknown_future_key", "42"),
        ];
        let parsed = AppSettings::from_kv_pairs(pairs);
        assert_eq!(parsed.window_x, None);
        assert_eq!(parsed.window_y, None);
        assert_eq!(parsed.window_width, MIN_WINDOW_WIDTH);
        assert_eq!(parsed.window_height, DEFAULT_WINDOW_HEIGHT);
        assert!(!parsed.window_maximized);
        // Paused without valid bounds falls back to LIVE mode cleanly.
        assert!(parsed.is_live);
        assert_eq!(parsed.span_secs, MIN_ZOOM_SPAN_SECS);
    }

    #[test]
    fn startup_range_restores_paused_or_falls_back_after_retention() {
        let now_secs = 2_000_000.0;
        let cutoff_secs = now_secs - (7.0 * 86_400.0);

        let paused_recent = AppSettings {
            is_live: false,
            span_secs: 900.0,
            paused_x_min: Some(now_secs - 7_200.0),
            paused_x_max: Some(now_secs - 6_300.0),
            ..Default::default()
        };
        let (is_live, range) = resolve_startup_range(&paused_recent, now_secs, cutoff_secs);
        assert!(!is_live);
        assert!((range.0 - (now_secs - 7_200.0)).abs() < 1e-6);
        assert!((range.1 - (now_secs - 6_300.0)).abs() < 1e-6);

        let paused_expired = AppSettings {
            is_live: false,
            span_secs: 900.0,
            paused_x_min: Some(cutoff_secs - 3_600.0),
            paused_x_max: Some(cutoff_secs - 2_700.0),
            ..Default::default()
        };
        let (is_live_fb, range_fb) = resolve_startup_range(&paused_expired, now_secs, cutoff_secs);
        assert!(is_live_fb);
        assert!((range_fb.1 - now_secs).abs() < 1e-6);
        assert!((range_fb.1 - range_fb.0 - 900.0).abs() < 1e-6);
    }
}
