use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Result, params};

use crate::model::{AppSettings, TelemetrySample};

pub const RETENTION_DAYS_MS: i64 = 7 * 86_400_000;

/// Resolves `<exe_dir>\power-monitor.db`.
pub fn resolve_db_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("power-monitor.db")
}

pub struct TelemetryDb {
    conn: Connection,
}

impl TelemetryDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS readings (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms              INTEGER NOT NULL,
                power_source       TEXT    NOT NULL,
                into_battery_w     REAL,
                out_of_battery_w   REAL,
                battery_pct        REAL,
                battery_wh         REAL,
                battery_full_wh    REAL,
                time_left_s        INTEGER,
                battery_health_pct REAL,
                battery_voltage_v  REAL,
                battery_cycles     INTEGER,
                draining_on_charger INTEGER NOT NULL DEFAULT 0,
                cpu_ghz            REAL,
                cpu_busy_pct       REAL,
                cpu_power_w        REAL,
                igpu_busy_pct      REAL,
                igpu_mhz           REAL,
                igpu_voltage_v     REAL,
                fan_rpm            INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_readings_ts_ms ON readings(ts_ms);

            CREATE TABLE IF NOT EXISTS temperature_readings (
                reading_id  INTEGER NOT NULL REFERENCES readings(id) ON DELETE CASCADE,
                sensor_name TEXT    NOT NULL,
                temp_c      REAL    NOT NULL,
                PRIMARY KEY (reading_id, sensor_name)
            );

            CREATE TABLE IF NOT EXISTS app_settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )?;
        Ok(Self { conn })
    }

    pub fn insert_sample(&mut self, s: &TelemetrySample) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO readings (
                ts_ms, power_source, into_battery_w, out_of_battery_w,
                battery_pct, battery_wh, battery_full_wh, time_left_s,
                battery_health_pct, battery_voltage_v, battery_cycles, draining_on_charger,
                cpu_ghz, cpu_busy_pct, cpu_power_w,
                igpu_busy_pct, igpu_mhz, igpu_voltage_v, fan_rpm
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                s.ts_ms,
                s.power_source,
                s.into_battery_w,
                s.out_of_battery_w,
                s.battery_pct,
                s.battery_wh,
                s.battery_full_wh,
                s.time_left_s,
                s.battery_health_pct,
                s.battery_voltage_v,
                s.battery_cycles.map(|c| c as i64),
                i32::from(s.draining_on_charger),
                s.cpu_ghz,
                s.cpu_busy_pct,
                s.cpu_power_w,
                s.igpu_busy_pct,
                s.igpu_mhz,
                s.igpu_voltage_v,
                s.fan_rpm.map(|r| r as i64),
            ],
        )?;
        let reading_id = tx.last_insert_rowid();
        if !s.temperatures_c.is_empty() {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO temperature_readings (reading_id, sensor_name, temp_c)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (name, &temp_c) in &s.temperatures_c {
                stmt.execute(params![reading_id, name, temp_c])?;
            }
        }
        tx.commit()
    }

    pub fn prune_older_than(&self, cutoff_ts_ms: i64) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM readings WHERE ts_ms < ?1",
            params![cutoff_ts_ms],
        )
    }

    pub fn total_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM readings", [], |r| r.get(0))
    }

    pub fn draining_secs_in_range(&self, min_ts_ms: i64, max_ts_ms: i64) -> Result<i64> {
        self.conn.query_row(
            "SELECT COALESCE(SUM(draining_on_charger), 0)
             FROM readings WHERE ts_ms >= ?1 AND ts_ms <= ?2",
            params![min_ts_ms, max_ts_ms],
            |r| r.get(0),
        )
    }

    pub fn load_window(
        &self,
        min_ts_ms: i64,
        max_ts_ms: i64,
        downsample_threshold_ms: i64,
        target_buckets: usize,
    ) -> Result<Vec<TelemetrySample>> {
        let span = (max_ts_ms - min_ts_ms).max(1);
        if span <= downsample_threshold_ms {
            self.load_raw_window(min_ts_ms, max_ts_ms)
        } else {
            let bucket_ms = (span / target_buckets.max(1) as i64).max(1_000);
            self.load_bucketed_window(min_ts_ms, max_ts_ms, bucket_ms)
        }
    }

    fn load_raw_window(&self, min_ts_ms: i64, max_ts_ms: i64) -> Result<Vec<TelemetrySample>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_ms, power_source, into_battery_w, out_of_battery_w,
                    battery_pct, battery_wh, battery_full_wh, time_left_s,
                    battery_health_pct, battery_voltage_v, battery_cycles, draining_on_charger,
                    cpu_ghz, cpu_busy_pct, cpu_power_w,
                    igpu_busy_pct, igpu_mhz, igpu_voltage_v, fan_rpm
             FROM readings
             WHERE ts_ms >= ?1 AND ts_ms <= ?2
             ORDER BY ts_ms ASC",
        )?;

        let mut samples = Vec::new();
        let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
        let rows = stmt.query_map(params![min_ts_ms, max_ts_ms], |r| {
            let id: i64 = r.get(0)?;
            let cycles: Option<i64> = r.get(11)?;
            let draining_int: i32 = r.get(12)?;
            let fan: Option<i64> = r.get(19)?;
            Ok((
                id,
                TelemetrySample {
                    ts_ms: r.get(1)?,
                    power_source: r.get(2)?,
                    into_battery_w: r.get(3)?,
                    out_of_battery_w: r.get(4)?,
                    battery_pct: r.get(5)?,
                    battery_wh: r.get(6)?,
                    battery_full_wh: r.get(7)?,
                    time_left_s: r.get(8)?,
                    battery_health_pct: r.get(9)?,
                    battery_voltage_v: r.get(10)?,
                    battery_cycles: cycles.and_then(|c| u32::try_from(c).ok()),
                    draining_on_charger: draining_int != 0,
                    cpu_ghz: r.get(13)?,
                    cpu_busy_pct: r.get(14)?,
                    cpu_power_w: r.get(15)?,
                    igpu_busy_pct: r.get(16)?,
                    igpu_mhz: r.get(17)?,
                    igpu_voltage_v: r.get(18)?,
                    fan_rpm: fan.and_then(|f| u32::try_from(f).ok()),
                    temperatures_c: BTreeMap::new(),
                },
            ))
        })?;

        for row in rows {
            let (id, sample) = row?;
            id_to_idx.insert(id, samples.len());
            samples.push(sample);
        }

        if !samples.is_empty() {
            let mut tstmt = self.conn.prepare(
                "SELECT t.reading_id, t.sensor_name, t.temp_c
                 FROM temperature_readings t
                 JOIN readings r ON r.id = t.reading_id
                 WHERE r.ts_ms >= ?1 AND r.ts_ms <= ?2",
            )?;
            let trows = tstmt.query_map(params![min_ts_ms, max_ts_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })?;
            for trow in trows {
                let (rid, name, temp_c) = trow?;
                if let Some(&idx) = id_to_idx.get(&rid) {
                    samples[idx].temperatures_c.insert(name, temp_c);
                }
            }
        }

        Ok(samples)
    }

    fn load_bucketed_window(
        &self,
        min_ts_ms: i64,
        max_ts_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<TelemetrySample>> {
        let mut stmt = self.conn.prepare(
            "SELECT (ts_ms / ?3) AS bucket,
                    CAST(AVG(ts_ms) AS INTEGER),
                    MAX(power_source),
                    AVG(into_battery_w),
                    AVG(out_of_battery_w),
                    AVG(battery_pct),
                    AVG(battery_wh),
                    AVG(battery_full_wh),
                    CAST(AVG(time_left_s) AS INTEGER),
                    AVG(battery_health_pct),
                    AVG(battery_voltage_v),
                    MAX(battery_cycles),
                    MAX(draining_on_charger),
                    AVG(cpu_ghz),
                    AVG(cpu_busy_pct),
                    AVG(cpu_power_w),
                    AVG(igpu_busy_pct),
                    AVG(igpu_mhz),
                    AVG(igpu_voltage_v),
                    CAST(AVG(fan_rpm) AS INTEGER)
             FROM readings
             WHERE ts_ms >= ?1 AND ts_ms <= ?2
             GROUP BY bucket
             ORDER BY bucket ASC",
        )?;

        let mut samples = Vec::new();
        let mut bucket_to_idx: HashMap<i64, usize> = HashMap::new();
        let rows = stmt.query_map(params![min_ts_ms, max_ts_ms, bucket_ms], |r| {
            let bucket: i64 = r.get(0)?;
            let cycles: Option<i64> = r.get(11)?;
            let draining_int: i32 = r.get(12)?;
            let fan: Option<i64> = r.get(19)?;
            Ok((
                bucket,
                TelemetrySample {
                    ts_ms: r.get(1)?,
                    power_source: r.get(2)?,
                    into_battery_w: r.get(3)?,
                    out_of_battery_w: r.get(4)?,
                    battery_pct: r.get(5)?,
                    battery_wh: r.get(6)?,
                    battery_full_wh: r.get(7)?,
                    time_left_s: r.get(8)?,
                    battery_health_pct: r.get(9)?,
                    battery_voltage_v: r.get(10)?,
                    battery_cycles: cycles.and_then(|c| u32::try_from(c).ok()),
                    draining_on_charger: draining_int != 0,
                    cpu_ghz: r.get(13)?,
                    cpu_busy_pct: r.get(14)?,
                    cpu_power_w: r.get(15)?,
                    igpu_busy_pct: r.get(16)?,
                    igpu_mhz: r.get(17)?,
                    igpu_voltage_v: r.get(18)?,
                    fan_rpm: fan.and_then(|f| u32::try_from(f).ok()),
                    temperatures_c: BTreeMap::new(),
                },
            ))
        })?;

        for row in rows {
            let (bucket, sample) = row?;
            bucket_to_idx.insert(bucket, samples.len());
            samples.push(sample);
        }

        if !samples.is_empty() {
            let mut tstmt = self.conn.prepare(
                "SELECT (r.ts_ms / ?3) AS bucket, t.sensor_name, AVG(t.temp_c)
                 FROM temperature_readings t
                 JOIN readings r ON r.id = t.reading_id
                 WHERE r.ts_ms >= ?1 AND r.ts_ms <= ?2
                 GROUP BY bucket, t.sensor_name",
            )?;
            let trows = tstmt.query_map(params![min_ts_ms, max_ts_ms, bucket_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })?;
            for trow in trows {
                let (bucket, name, temp_c) = trow?;
                if let Some(&idx) = bucket_to_idx.get(&bucket) {
                    samples[idx].temperatures_c.insert(name, temp_c);
                }
            }
        }

        Ok(samples)
    }

    pub fn load_settings(&self) -> Result<AppSettings> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM app_settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut pairs = Vec::new();
        for row in rows {
            pairs.push(row?);
        }
        Ok(AppSettings::from_kv_pairs(
            pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ))
    }

    pub fn save_settings(&mut self, settings: &AppSettings) -> Result<()> {
        let pairs = settings.to_kv_pairs();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?;
            for (key, value) in &pairs {
                stmt.execute(params![key, value])?;
            }
        }
        tx.commit()
    }
}
