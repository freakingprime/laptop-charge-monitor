pub mod battery;
pub mod cpu_thermal;
pub mod intel_power;

use chrono::Utc;

use self::battery::BatteryReader;
use self::cpu_thermal::{CpuThermalReader, read_system_metadata};
use self::intel_power::IntelPowerReader;
use crate::model::{SystemMetadata, TelemetrySample};

pub struct SensorHub {
    intel: IntelPowerReader,
    battery: BatteryReader,
    cpu_thermal: CpuThermalReader,
}

impl SensorHub {
    pub fn new(base_ghz: f64) -> Self {
        Self {
            intel: IntelPowerReader::new(),
            battery: BatteryReader::new(),
            cpu_thermal: CpuThermalReader::new(base_ghz),
        }
    }

    pub fn metadata(db_path_display: String) -> SystemMetadata {
        read_system_metadata(db_path_display)
    }

    pub fn sample(&mut self) -> TelemetrySample {
        let ts_ms = Utc::now().timestamp_millis();
        let intel = self.intel.sample();
        let batt = self.battery.sample();
        let cpu = self.cpu_thermal.sample();

        TelemetrySample {
            ts_ms,
            power_source: batt.power_source,
            into_battery_w: batt.into_battery_w,
            out_of_battery_w: batt.out_of_battery_w,
            battery_pct: batt.battery_pct,
            battery_wh: batt.battery_wh,
            battery_full_wh: batt.battery_full_wh,
            time_left_s: batt.time_left_s,
            battery_health_pct: batt.battery_health_pct,
            battery_voltage_v: batt.battery_voltage_v,
            battery_cycles: batt.battery_cycles,
            draining_on_charger: batt.draining_on_charger,
            cpu_ghz: cpu.cpu_ghz,
            cpu_busy_pct: cpu.cpu_busy_pct,
            cpu_power_w: intel.cpu_power_w,
            igpu_busy_pct: intel.igpu_busy_pct,
            igpu_mhz: intel.igpu_mhz,
            igpu_voltage_v: intel.igpu_voltage_v,
            fan_rpm: cpu.fan_rpm,
            temperatures_c: cpu.temperatures_c,
        }
    }
}
