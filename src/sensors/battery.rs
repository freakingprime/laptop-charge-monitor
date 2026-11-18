use std::ffi::c_void;
use std::mem::{size_of, zeroed};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, GUID_DEVCLASS_BATTERY, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::{
    BATTERY_CHARGING, BATTERY_DISCHARGING, BATTERY_INFORMATION, BATTERY_POWER_ON_LINE,
    BATTERY_QUERY_INFORMATION, BATTERY_STATUS, BATTERY_TAG_INVALID, BATTERY_UNKNOWN_CAPACITY,
    BATTERY_UNKNOWN_RATE, BATTERY_UNKNOWN_TIME, BATTERY_UNKNOWN_VOLTAGE, BATTERY_WAIT_STATUS,
    BatteryEstimatedTime, BatteryInformation, IOCTL_BATTERY_QUERY_INFORMATION,
    IOCTL_BATTERY_QUERY_STATUS, IOCTL_BATTERY_QUERY_TAG,
};
use windows::core::PCWSTR;

#[derive(Debug, Clone)]
pub struct BatteryReading {
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
}

impl Default for BatteryReading {
    fn default() -> Self {
        Self {
            power_source: "Unknown".to_string(),
            into_battery_w: None,
            out_of_battery_w: None,
            battery_pct: None,
            battery_wh: None,
            battery_full_wh: None,
            time_left_s: None,
            battery_health_pct: None,
            battery_voltage_v: None,
            battery_cycles: None,
            draining_on_charger: false,
        }
    }
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct OwnedDevInfo(HDEVINFO);
impl Drop for OwnedDevInfo {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub struct BatteryReader {
    device_path: Option<Vec<u16>>,
}

impl BatteryReader {
    pub fn new() -> Self {
        Self {
            device_path: find_first_battery_device_path(),
        }
    }
}

impl Default for BatteryReader {
    fn default() -> Self {
        Self::new()
    }
}

impl BatteryReader {
    pub fn sample(&mut self) -> BatteryReading {
        if self.device_path.is_none() {
            self.device_path = find_first_battery_device_path();
        }
        let Some(path) = self.device_path.as_ref() else {
            return BatteryReading::default();
        };

        match read_battery_ioctl(path) {
            Some(r) => r,
            None => {
                // Re-discover on next tick in case battery tag/device interface changed.
                self.device_path = None;
                BatteryReading::default()
            }
        }
    }
}

fn find_first_battery_device_path() -> Option<Vec<u16>> {
    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_BATTERY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .ok()?;
        let _guard = OwnedDevInfo(dev_info);

        let mut if_data = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..zeroed()
        };
        SetupDiEnumDeviceInterfaces(dev_info, None, &GUID_DEVCLASS_BATTERY, 0, &mut if_data)
            .ok()?;

        let mut required = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &if_data,
            None,
            0,
            Some(&mut required),
            None,
        );
        if required < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
            return None;
        }

        let mut buffer = vec![0u8; required as usize];
        let detail_ptr = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        (*detail_ptr).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &if_data,
            Some(detail_ptr),
            required,
            Some(&mut required),
            None,
        )
        .ok()?;

        let char_offset = offset_of_device_path();
        let wide_bytes = &buffer[char_offset..required as usize];
        let wide: Vec<u16> = wide_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some(wide)
    }
}

const fn offset_of_device_path() -> usize {
    // `cbSize` (u32) + `DevicePath` ([u16; 1]) -> offset of `DevicePath` is 4.
    4
}

fn read_battery_ioctl(device_path: &[u16]) -> Option<BatteryReading> {
    unsafe {
        let handle = CreateFileW(
            PCWSTR::from_raw(device_path.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .ok()?;
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let _guard = OwnedHandle(handle);

        let mut wait_timeout = 0u32;
        let mut tag = BATTERY_TAG_INVALID;
        let mut bytes = 0u32;
        DeviceIoControl(
            handle,
            IOCTL_BATTERY_QUERY_TAG,
            Some((&mut wait_timeout as *mut u32).cast::<c_void>()),
            size_of::<u32>() as u32,
            Some((&mut tag as *mut u32).cast::<c_void>()),
            size_of::<u32>() as u32,
            Some(&mut bytes),
            None,
        )
        .ok()?;
        if tag == BATTERY_TAG_INVALID {
            return None;
        }

        let bqi_info = BATTERY_QUERY_INFORMATION {
            BatteryTag: tag,
            InformationLevel: BatteryInformation,
            AtRate: 0,
        };
        let mut info = BATTERY_INFORMATION::default();
        DeviceIoControl(
            handle,
            IOCTL_BATTERY_QUERY_INFORMATION,
            Some((&bqi_info as *const BATTERY_QUERY_INFORMATION).cast::<c_void>()),
            size_of::<BATTERY_QUERY_INFORMATION>() as u32,
            Some((&mut info as *mut BATTERY_INFORMATION).cast::<c_void>()),
            size_of::<BATTERY_INFORMATION>() as u32,
            Some(&mut bytes),
            None,
        )
        .ok()?;

        let wait_status = BATTERY_WAIT_STATUS {
            BatteryTag: tag,
            ..Default::default()
        };
        let mut status = BATTERY_STATUS::default();
        DeviceIoControl(
            handle,
            IOCTL_BATTERY_QUERY_STATUS,
            Some((&wait_status as *const BATTERY_WAIT_STATUS).cast::<c_void>()),
            size_of::<BATTERY_WAIT_STATUS>() as u32,
            Some((&mut status as *mut BATTERY_STATUS).cast::<c_void>()),
            size_of::<BATTERY_STATUS>() as u32,
            Some(&mut bytes),
            None,
        )
        .ok()?;

        let mut est_time = BATTERY_UNKNOWN_TIME;
        let bqi_time = BATTERY_QUERY_INFORMATION {
            BatteryTag: tag,
            InformationLevel: BatteryEstimatedTime,
            AtRate: 0,
        };
        let _ = DeviceIoControl(
            handle,
            IOCTL_BATTERY_QUERY_INFORMATION,
            Some((&bqi_time as *const BATTERY_QUERY_INFORMATION).cast::<c_void>()),
            size_of::<BATTERY_QUERY_INFORMATION>() as u32,
            Some((&mut est_time as *mut u32).cast::<c_void>()),
            size_of::<u32>() as u32,
            Some(&mut bytes),
            None,
        );

        Some(build_battery_reading(&info, &status, est_time))
    }
}

pub fn build_battery_reading(
    info: &BATTERY_INFORMATION,
    status: &BATTERY_STATUS,
    est_time_secs: u32,
) -> BatteryReading {
    let power_online = (status.PowerState & BATTERY_POWER_ON_LINE) != 0;
    let charging = (status.PowerState & BATTERY_CHARGING) != 0;
    let discharging = (status.PowerState & BATTERY_DISCHARGING) != 0;

    let battery_wh =
        (status.Capacity != BATTERY_UNKNOWN_CAPACITY).then(|| status.Capacity as f64 / 1_000.0);
    let battery_full_wh =
        (info.FullChargedCapacity > 0).then(|| info.FullChargedCapacity as f64 / 1_000.0);

    let battery_pct = match (battery_wh, battery_full_wh) {
        (Some(rem), Some(full)) if full > 0.0 => Some(((rem / full) * 100.0).clamp(0.0, 100.0)),
        _ => None,
    };

    let battery_health_pct = if info.DesignedCapacity > 0 && info.FullChargedCapacity > 0 {
        Some(((info.FullChargedCapacity as f64 / info.DesignedCapacity as f64) * 100.0).min(100.0))
    } else {
        None
    };

    let battery_voltage_v =
        (status.Voltage != BATTERY_UNKNOWN_VOLTAGE).then(|| status.Voltage as f64 / 1_000.0);

    let time_left_s =
        (est_time_secs != BATTERY_UNKNOWN_TIME && !power_online).then_some(est_time_secs as i64);

    let rate_mw = if status.Rate == BATTERY_UNKNOWN_RATE as i32 {
        None
    } else {
        Some(status.Rate)
    };

    let (into_battery_w, out_of_battery_w) = match rate_mw {
        Some(r) if charging || r > 0 => (Some(r.unsigned_abs() as f64 / 1_000.0), Some(0.0)),
        Some(r) if discharging || r < 0 => (Some(0.0), Some(r.unsigned_abs() as f64 / 1_000.0)),
        Some(_) => (Some(0.0), Some(0.0)),
        None => (None, None),
    };

    let draining_on_charger =
        power_online && (discharging || out_of_battery_w.unwrap_or(0.0) > 0.1);

    let power_source = if power_online {
        if charging && into_battery_w.unwrap_or(0.0) > 0.05 {
            "Charger - charging".to_string()
        } else if draining_on_charger {
            "Charger - draining".to_string()
        } else if battery_pct.unwrap_or(0.0) >= 99.5 {
            "Charger - battery full".to_string()
        } else {
            "Charger".to_string()
        }
    } else {
        "Battery".to_string()
    };

    BatteryReading {
        power_source,
        into_battery_w,
        out_of_battery_w,
        battery_pct,
        battery_wh,
        battery_full_wh,
        time_left_s,
        battery_health_pct,
        battery_voltage_v,
        battery_cycles: Some(info.CycleCount),
        draining_on_charger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_full_on_charger_classification() {
        let info = BATTERY_INFORMATION {
            DesignedCapacity: 58140,
            FullChargedCapacity: 58140,
            CycleCount: 3,
            ..Default::default()
        };
        let status = BATTERY_STATUS {
            PowerState: BATTERY_POWER_ON_LINE,
            Capacity: 58140,
            Voltage: 13101,
            Rate: 0,
        };
        let r = build_battery_reading(&info, &status, BATTERY_UNKNOWN_TIME);
        assert_eq!(r.power_source, "Charger - battery full");
        assert_eq!(r.into_battery_w, Some(0.0));
        assert_eq!(r.out_of_battery_w, Some(0.0));
        assert_eq!(r.battery_cycles, Some(3));
        assert!(!r.draining_on_charger);
        assert_eq!(r.time_left_s, None);
    }

    #[test]
    fn battery_discharging_classification() {
        let info = BATTERY_INFORMATION {
            DesignedCapacity: 58140,
            FullChargedCapacity: 58140,
            CycleCount: 5,
            ..Default::default()
        };
        let status = BATTERY_STATUS {
            PowerState: BATTERY_DISCHARGING,
            Capacity: 50000,
            Voltage: 12129,
            Rate: -17647,
        };
        let r = build_battery_reading(&info, &status, 7958);
        assert_eq!(r.power_source, "Battery");
        assert!((r.out_of_battery_w.unwrap() - 17.647).abs() < 1e-6);
        assert_eq!(r.into_battery_w, Some(0.0));
        assert_eq!(r.time_left_s, Some(7958));
        assert!(!r.draining_on_charger);
    }

    #[test]
    fn battery_draining_on_charger_detected() {
        let info = BATTERY_INFORMATION {
            DesignedCapacity: 58140,
            FullChargedCapacity: 58140,
            ..Default::default()
        };
        let status = BATTERY_STATUS {
            PowerState: BATTERY_POWER_ON_LINE | BATTERY_DISCHARGING,
            Capacity: 40000,
            Voltage: 12000,
            Rate: -5200,
        };
        let r = build_battery_reading(&info, &status, 3600);
        assert_eq!(r.power_source, "Charger - draining");
        assert!(r.draining_on_charger);
        assert!((r.out_of_battery_w.unwrap() - 5.2).abs() < 1e-6);
    }
}
