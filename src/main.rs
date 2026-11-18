#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
use windows::core::w;

use laptop_charge_monitor::sensors::SensorHub;
use laptop_charge_monitor::storage::{RETENTION_DAYS_MS, TelemetryDb, resolve_db_path};
use laptop_charge_monitor::ui::app::{LiveUpdate, PowerMonitorApp};

struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    unsafe {
        let handle =
            CreateMutexW(None, false, w!(r"Local\LaptopChargeMonitorSingleInstance")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            MessageBoxW(
                None,
                w!("Power Monitor is already running."),
                w!("Power Monitor"),
                MB_OK | MB_ICONINFORMATION,
            );
            return None;
        }
        Some(SingleInstanceGuard(handle))
    }
}

fn main() -> eframe::Result<()> {
    let Some(_instance_guard) = acquire_single_instance() else {
        return Ok(());
    };

    let db_path = resolve_db_path();
    let db_display = db_path.to_string_lossy().into_owned();
    let meta = SensorHub::metadata(db_display);

    // Initialize schema and perform startup 7-day retention prune before launching UI.
    let initial_total = match TelemetryDb::open(&db_path) {
        Ok(db) => {
            let cutoff = Utc::now().timestamp_millis() - RETENTION_DAYS_MS;
            let _ = db.prune_older_than(cutoff);
            db.total_count().unwrap_or(0)
        }
        Err(_) => 0,
    };

    let ui_db = TelemetryDb::open(&db_path).ok();
    let (tx, rx) = mpsc::channel::<LiveUpdate>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_worker = Arc::clone(&shutdown);
    let base_ghz = meta.base_ghz;
    let worker_db_path = db_path.clone();

    let worker_slot = Arc::new(std::sync::Mutex::new(None));
    let worker_slot_cb = Arc::clone(&worker_slot);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Power Monitor")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };

    let run_res = eframe::run_native(
        "Power Monitor",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let handle = thread::spawn(move || {
                sampler_loop(base_ghz, &worker_db_path, tx, ctx, shutdown_worker);
            });
            if let Ok(mut slot) = worker_slot_cb.lock() {
                *slot = Some(handle);
            }
            Ok(Box::new(PowerMonitorApp::new(
                cc,
                meta,
                ui_db,
                rx,
                initial_total,
            )))
        }),
    );

    shutdown.store(true, Ordering::Release);
    if let Ok(mut slot) = worker_slot.lock()
        && let Some(h) = slot.take()
    {
        let _ = h.join();
    }

    run_res
}

fn sampler_loop(
    base_ghz: f64,
    db_path: &std::path::Path,
    tx: mpsc::Sender<LiveUpdate>,
    ctx: egui::Context,
    shutdown: Arc<AtomicBool>,
) {
    let mut hub = SensorHub::new(base_ghz);
    let mut writer_db = TelemetryDb::open(db_path).ok();
    let mut total_readings = writer_db
        .as_ref()
        .and_then(|d| d.total_count().ok())
        .unwrap_or(0);
    let mut ticks_since_prune: u32 = 0;

    while !shutdown.load(Ordering::Acquire) {
        let tick_start = Instant::now();
        let sample = hub.sample();

        if let Some(db) = writer_db.as_mut() {
            if db.insert_sample(&sample).is_ok() {
                total_readings += 1;
            }
            ticks_since_prune += 1;
            if ticks_since_prune >= 3_600 {
                ticks_since_prune = 0;
                let cutoff = sample.ts_ms - RETENTION_DAYS_MS;
                let _ = db.prune_older_than(cutoff);
                total_readings = db.total_count().unwrap_or(total_readings);
            }
        }

        if tx
            .send(LiveUpdate {
                sample,
                total_readings,
            })
            .is_err()
        {
            break;
        }
        ctx.request_repaint();

        // Sleep in 100ms increments up to 1s so window close terminates fast.
        while !shutdown.load(Ordering::Acquire) {
            let elapsed = tick_start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                break;
            }
            let remaining = Duration::from_secs(1) - elapsed;
            thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}
