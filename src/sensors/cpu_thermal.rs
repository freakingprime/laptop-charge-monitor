use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoInitializeSecurity, CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
    STORAGE_TEMPERATURE_DATA_DESCRIPTOR, STORAGE_TEMPERATURE_VALUE_NOT_REPORTED,
    StorageDeviceTemperatureProperty,
};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4, VT_UI4, VariantClear};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WbemLocator,
};
use windows::core::{BSTR, HSTRING, PCWSTR, w};

use crate::model::{CpuTimes, SystemMetadata, collapse_esif_temps, compute_cpu_busy_pct};

const RPC_C_AUTHN_WINNT: u32 = 10;
const RPC_C_AUTHZ_NONE: u32 = 0;

#[derive(Debug, Clone, Default)]
pub struct CpuThermalReading {
    pub cpu_ghz: Option<f64>,
    pub cpu_busy_pct: Option<f64>,
    pub fan_rpm: Option<u32>,
    pub temperatures_c: BTreeMap<String, f64>,
}

/// Reads static CPU model name and base clock from Windows registry.
pub fn read_system_metadata(db_path_display: String) -> SystemMetadata {
    let cpu_name = read_reg_string(
        w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"),
        w!("ProcessorNameString"),
    )
    .unwrap_or_else(|| "Unknown CPU".to_string());
    let base_mhz = read_reg_dword(
        w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"),
        w!("~MHz"),
    )
    .unwrap_or(0);
    SystemMetadata {
        cpu_name,
        base_ghz: base_mhz as f64 / 1_000.0,
        db_path_display,
    }
}

fn read_reg_string(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    unsafe {
        let mut size = 0u32;
        let status = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if status.0 != 0 || size < 2 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let status = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        );
        if status.0 != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let cleaned = String::from_utf16_lossy(&buf[..len])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        (!cleaned.is_empty()).then_some(cleaned)
    }
}

fn read_reg_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    unsafe {
        let mut out = 0u32;
        let mut size = size_of::<u32>() as u32;
        let status = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut out as *mut u32).cast()),
            Some(&mut size),
        );
        (status.0 == 0).then_some(out)
    }
}

pub struct CpuThermalReader {
    base_ghz: f64,
    prev_cpu: Option<CpuTimes>,
    pdh: Option<PdhSession>,
    wmi: Option<WmiSensorSession>,
    nvme_drives: Vec<u32>,
}

impl CpuThermalReader {
    pub fn new(base_ghz: f64) -> Self {
        let prev_cpu = read_cpu_times();
        let pdh = PdhSession::open();
        let wmi = WmiSensorSession::open();
        let nvme_drives = discover_temp_drives();
        Self {
            base_ghz,
            prev_cpu,
            pdh,
            wmi,
            nvme_drives,
        }
    }

    pub fn sample(&mut self) -> CpuThermalReading {
        let mut out = CpuThermalReading::default();

        if let Some(curr) = read_cpu_times() {
            if let Some(prev) = self.prev_cpu {
                out.cpu_busy_pct = compute_cpu_busy_pct(prev, curr);
            }
            self.prev_cpu = Some(curr);
        }

        if let Some(pdh) = self.pdh.as_mut() {
            let (perf_pct, tz00_c) = pdh.sample();
            if let Some(pct) = perf_pct
                && self.base_ghz > 0.0
            {
                out.cpu_ghz = Some((self.base_ghz * (pct / 100.0)).max(0.0));
            }
            if let Some(t) = tz00_c {
                out.temperatures_c.insert("TZ00".to_string(), t);
            }
        }

        if let Some(wmi) = self.wmi.as_mut() {
            wmi.sample(&mut out.temperatures_c, &mut out.fan_rpm);
        }

        for (idx, &drive_num) in self.nvme_drives.iter().enumerate() {
            if let Some(temp_c) = read_nvme_temperature_c(drive_num) {
                let label = if self.nvme_drives.len() == 1 {
                    "NVMe SSD".to_string()
                } else {
                    format!("NVMe SSD #{}", idx + 1)
                };
                out.temperatures_c.insert(label, temp_c);
            }
        }

        out
    }
}

fn read_cpu_times() -> Option<CpuTimes> {
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()?;
        Some(CpuTimes {
            idle: filetime_to_u64(idle),
            kernel: filetime_to_u64(kernel),
            user: filetime_to_u64(user),
        })
    }
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

// ============================================================================
// PDH Performance Counters (CPU % Performance & TZ00)
// ============================================================================

struct PdhSession {
    query: PDH_HQUERY,
    cpu_perf_counter: Option<PDH_HCOUNTER>,
    tz00_counter: Option<PDH_HCOUNTER>,
}

impl Drop for PdhSession {
    fn drop(&mut self) {
        if !self.query.is_invalid() {
            unsafe {
                let _ = PdhCloseQuery(self.query);
            }
        }
    }
}

impl PdhSession {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 || query.is_invalid() {
                return None;
            }

            let mut cpu_counter = PDH_HCOUNTER::default();
            let cpu_ok = PdhAddEnglishCounterW(
                query,
                w!(r"\Processor Information(_Total)\% Processor Performance"),
                0,
                &mut cpu_counter,
            ) == 0;

            let mut tz_counter = PDH_HCOUNTER::default();
            let tz_ok = PdhAddEnglishCounterW(
                query,
                w!(r"\Thermal Zone Information(\_tz.tz00)\High Precision Temperature"),
                0,
                &mut tz_counter,
            ) == 0;

            if !cpu_ok && !tz_ok {
                let _ = PdhCloseQuery(query);
                return None;
            }

            // Prime rate counters so the next collect succeeds.
            let _ = PdhCollectQueryData(query);

            Some(Self {
                query,
                cpu_perf_counter: cpu_ok.then_some(cpu_counter),
                tz00_counter: tz_ok.then_some(tz_counter),
            })
        }
    }

    fn sample(&mut self) -> (Option<f64>, Option<f64>) {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return (None, None);
            }
            let perf_pct = self
                .cpu_perf_counter
                .and_then(|c| read_pdh_double(c))
                .filter(|v| v.is_finite() && *v >= 0.0);

            let tz00_c = self
                .tz00_counter
                .and_then(|c| read_pdh_double(c))
                .map(|tenths_k| (tenths_k / 10.0) - 273.15)
                .filter(|c| c.is_finite() && *c > -20.0 && *c < 150.0);

            (perf_pct, tz00_c)
        }
    }
}

unsafe fn read_pdh_double(counter: PDH_HCOUNTER) -> Option<f64> {
    let mut val = PDH_FMT_COUNTERVALUE::default();
    let status = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut val) };
    if status == 0 && val.CStatus == 0 {
        Some(unsafe { val.Anonymous.doubleValue })
    } else {
        None
    }
}

// ============================================================================
// NVMe / Storage SMART Temperature via IOCTL_STORAGE_QUERY_PROPERTY
// ============================================================================

struct OwnedFileHandle(HANDLE);
impl Drop for OwnedFileHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn discover_temp_drives() -> Vec<u32> {
    (0..4u32)
        .filter(|&n| read_nvme_temperature_c(n).is_some())
        .collect()
}

fn read_nvme_temperature_c(drive_num: u32) -> Option<f64> {
    let path = HSTRING::from(format!(r"\\.\PhysicalDrive{drive_num}"));
    unsafe {
        let handle = CreateFileW(
            &path,
            GENERIC_READ.0,
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
        let _guard = OwnedFileHandle(handle);

        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceTemperatureProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut desc = STORAGE_TEMPERATURE_DATA_DESCRIPTOR::default();
        let mut bytes = 0u32;

        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&query as *const STORAGE_PROPERTY_QUERY).cast::<c_void>()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&mut desc as *mut STORAGE_TEMPERATURE_DATA_DESCRIPTOR).cast::<c_void>()),
            size_of::<STORAGE_TEMPERATURE_DATA_DESCRIPTOR>() as u32,
            Some(&mut bytes),
            None,
        )
        .ok()?;

        if desc.InfoCount == 0 {
            return None;
        }
        let raw_temp = desc.TemperatureInfo[0].Temperature;
        if raw_temp as i32 == STORAGE_TEMPERATURE_VALUE_NOT_REPORTED as i32
            || raw_temp <= -20
            || raw_temp >= 140
        {
            return None;
        }
        Some(raw_temp as f64)
    }
}

// ============================================================================
// Elevated WMI: EsifDeviceInformation, MSAcpi_ThermalZoneTemperature, Acer Fan
// ============================================================================

struct WmiSensorSession {
    services: IWbemServices,
    com_initialized: bool,
    acer_fan_probes_remaining: u8,
}

impl Drop for WmiSensorSession {
    fn drop(&mut self) {
        if self.com_initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

impl WmiSensorSession {
    fn open() -> Option<Self> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com_initialized = hr.is_ok();

            let _ = CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            );

            let locator: IWbemLocator =
                CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
            let ns = BSTR::from(r"ROOT\WMI");
            let empty = BSTR::new();
            let services = locator
                .ConnectServer(&ns, &empty, &empty, &empty, 0, &empty, None)
                .ok()?;

            let _ = CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            );

            Some(Self {
                services,
                com_initialized,
                acer_fan_probes_remaining: 3,
            })
        }
    }

    fn sample(&mut self, temps: &mut BTreeMap<String, f64>, fan_rpm: &mut Option<u32>) {
        // 1. EsifDeviceInformation (Intel IPF / DPTF CPU temperature)
        if let Some(esif_rows) = self.query_esif_temps() {
            let borrowed: Vec<(&str, i32)> =
                esif_rows.iter().map(|(n, t)| (n.as_str(), *t)).collect();
            for (k, v) in collapse_esif_temps(borrowed) {
                temps.insert(k, v);
            }
        }

        // 2. Fallback/extra ACPI thermal zones if TZ00 wasn't obtained from PDH
        if !temps.contains_key("TZ00")
            && let Some(acpi_temps) = self.query_acpi_temps()
        {
            for (name, c) in acpi_temps {
                temps.entry(name).or_insert(c);
            }
        }

        // 3. Read-only AcerGamingFunction.GetGamingFanSpeed probe; disable if unsupported/0.
        if self.acer_fan_probes_remaining > 0 {
            if let Some(rpm) = self.probe_acer_fan_rpm() {
                *fan_rpm = Some(rpm);
            } else {
                self.acer_fan_probes_remaining -= 1;
            }
        }
    }

    fn query_esif_temps(&self) -> Option<Vec<(String, i32)>> {
        unsafe {
            let lang = BSTR::from("WQL");
            let query = BSTR::from("SELECT InstanceName, Temperature FROM EsifDeviceInformation");
            let enum_obj = self
                .services
                .ExecQuery(
                    &lang,
                    &query,
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                )
                .ok()?;

            let mut rows = Vec::new();
            let mut objs: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            while enum_obj.Next(-1, &mut objs, &mut returned).is_ok() && returned > 0 {
                if let Some(obj) = objs[0].take() {
                    let name = read_wmi_string(&obj, w!("InstanceName")).unwrap_or_default();
                    if let Some(temp) = read_wmi_i32(&obj, w!("Temperature")) {
                        rows.push((name, temp));
                    }
                }
            }
            Some(rows)
        }
    }

    fn query_acpi_temps(&self) -> Option<Vec<(String, f64)>> {
        unsafe {
            let lang = BSTR::from("WQL");
            let query = BSTR::from(
                "SELECT InstanceName, CurrentTemperature FROM MSAcpi_ThermalZoneTemperature",
            );
            let enum_obj = self
                .services
                .ExecQuery(
                    &lang,
                    &query,
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                )
                .ok()?;

            let mut rows = Vec::new();
            let mut objs: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            while enum_obj.Next(-1, &mut objs, &mut returned).is_ok() && returned > 0 {
                if let Some(obj) = objs[0].take() {
                    let inst = read_wmi_string(&obj, w!("InstanceName")).unwrap_or_default();
                    if let Some(raw) = read_wmi_i32(&obj, w!("CurrentTemperature")) {
                        let temp_c = (raw as f64 / 10.0) - 273.15;
                        if temp_c > -20.0 && temp_c < 150.0 {
                            let label = if inst.contains("TZ00") {
                                "TZ00".to_string()
                            } else {
                                inst.rsplit('\\').next().unwrap_or("ACPI").to_string()
                            };
                            rows.push((label, temp_c));
                        }
                    }
                }
            }
            Some(rows)
        }
    }

    fn probe_acer_fan_rpm(&self) -> Option<u32> {
        unsafe {
            let class_name = BSTR::from("AcerGamingFunction");
            let mut class_obj: Option<IWbemClassObject> = None;
            self.services
                .GetObject(
                    &class_name,
                    Default::default(),
                    None,
                    Some(&mut class_obj),
                    None,
                )
                .ok()?;
            let class_obj = class_obj?;

            let method_name = BSTR::from("GetGamingFanSpeed");
            let mut in_sig: Option<IWbemClassObject> = None;
            class_obj
                .GetMethod(w!("GetGamingFanSpeed"), 0, &mut in_sig, null_mut())
                .ok()?;
            let in_sig = in_sig?;
            let in_inst = in_sig.SpawnInstance(0).ok()?;

            let mut var_in: VARIANT = zeroed();
            (*var_in.Anonymous.Anonymous).vt = VT_UI4;
            (*var_in.Anonymous.Anonymous).Anonymous.ulVal = 1; // CPU fan index
            in_inst.Put(w!("gmInput"), 0, &var_in, 0).ok()?;
            let _ = VariantClear(&mut var_in);

            let obj_path = BSTR::from(r"AcerGamingFunction.InstanceName='ACPI\PNP0C14\APGe_0'");
            let mut out_params: Option<IWbemClassObject> = None;
            self.services
                .ExecMethod(
                    &obj_path,
                    &method_name,
                    Default::default(),
                    None,
                    &in_inst,
                    Some(&mut out_params),
                    None,
                )
                .ok()?;
            let out_params = out_params?;
            let raw = read_wmi_u64(&out_params, w!("gmOutput"))?;
            parse_acer_fan_rpm(raw)
        }
    }
}

/// Decodes `AcerGamingFunction.GetGamingFanSpeed` `gmOutput`.
/// Low byte indicates status (0 = success), bytes 1..2 hold RPM.
pub fn parse_acer_fan_rpm(gm_output: u64) -> Option<u32> {
    let status = (gm_output & 0xFF) as u8;
    if status != 0 {
        return None;
    }
    let rpm = ((gm_output >> 8) & 0xFFFF) as u32;
    (rpm > 100 && rpm < 12_000).then_some(rpm)
}

unsafe fn read_wmi_string(obj: &IWbemClassObject, prop: PCWSTR) -> Option<String> {
    let mut var: VARIANT = unsafe { zeroed() };
    unsafe { obj.Get(prop, 0, &mut var, None, None).ok()? };
    let result = unsafe {
        if (*var.Anonymous.Anonymous).vt == VT_BSTR {
            Some((*var.Anonymous.Anonymous).Anonymous.bstrVal.to_string())
        } else {
            None
        }
    };
    unsafe {
        let _ = VariantClear(&mut var);
    }
    result
}

unsafe fn read_wmi_i32(obj: &IWbemClassObject, prop: PCWSTR) -> Option<i32> {
    let mut var: VARIANT = unsafe { zeroed() };
    unsafe { obj.Get(prop, 0, &mut var, None, None).ok()? };
    let result = unsafe {
        let vt = (*var.Anonymous.Anonymous).vt;
        if vt == VT_I4 {
            Some((*var.Anonymous.Anonymous).Anonymous.lVal)
        } else if vt == VT_UI4 {
            Some((*var.Anonymous.Anonymous).Anonymous.ulVal as i32)
        } else {
            None
        }
    };
    unsafe {
        let _ = VariantClear(&mut var);
    }
    result
}

unsafe fn read_wmi_u64(obj: &IWbemClassObject, prop: PCWSTR) -> Option<u64> {
    let mut var: VARIANT = unsafe { zeroed() };
    unsafe { obj.Get(prop, 0, &mut var, None, None).ok()? };
    let result = unsafe {
        let vt = (*var.Anonymous.Anonymous).vt;
        if vt == VT_BSTR {
            (*var.Anonymous.Anonymous)
                .Anonymous
                .bstrVal
                .to_string()
                .parse::<u64>()
                .ok()
        } else if vt == VT_UI4 {
            Some((*var.Anonymous.Anonymous).Anonymous.ulVal as u64)
        } else if vt == VT_I4 {
            u64::try_from((*var.Anonymous.Anonymous).Anonymous.lVal).ok()
        } else {
            Some((*var.Anonymous.Anonymous).Anonymous.ullVal)
        }
    };
    unsafe {
        let _ = VariantClear(&mut var);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acer_fan_rpm_decoding() {
        assert_eq!(parse_acer_fan_rpm(0), None);
        assert_eq!(parse_acer_fan_rpm(0x0001 | (2500 << 8)), None);
        assert_eq!(parse_acer_fan_rpm(2500 << 8), Some(2500));
        assert_eq!(parse_acer_fan_rpm(50 << 8), None);
    }
}
