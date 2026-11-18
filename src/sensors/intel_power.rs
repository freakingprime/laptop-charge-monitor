use std::ffi::{CStr, c_void};
use std::mem::zeroed;
use std::ptr::null_mut;

use libloading::Library;
use windows::Win32::System::Environment::SetEnvironmentVariableA;
use windows::core::s;

use crate::model::{
    MAX_ENERGY_DELTA_SECS, MAX_PLAUSIBLE_CPU_WATTS, compute_activity_pct_u64,
    compute_energy_watts_f64, compute_energy_watts_u64,
};

#[derive(Debug, Clone, Default)]
pub struct IntelReading {
    pub cpu_power_w: Option<f64>,
    pub igpu_busy_pct: Option<f64>,
    pub igpu_mhz: Option<f64>,
    pub igpu_voltage_v: Option<f64>,
}

/// Reads combined CPU + iGPU package power (ZES primary, IGCL fallback) and iGPU telemetry.
pub struct IntelPowerReader {
    zes: Option<ZesSession>,
    igcl: Option<IgclSession>,
}

impl IntelPowerReader {
    pub fn new() -> Self {
        let zes = ZesSession::open();
        let igcl = IgclSession::open();
        Self { zes, igcl }
    }
}

impl Default for IntelPowerReader {
    fn default() -> Self {
        Self::new()
    }
}

impl IntelPowerReader {
    pub fn sample(&mut self) -> IntelReading {
        let mut out = IntelReading::default();
        if let Some(zes) = self.zes.as_mut() {
            zes.sample(&mut out);
        }
        if let Some(igcl) = self.igcl.as_mut() {
            igcl.fill_missing(&mut out);
        }
        out
    }
}

// ============================================================================
// Level Zero Sysman (ze_loader.dll)
// ============================================================================

const ZE_RESULT_SUCCESS: u32 = 0;
const ZES_STRUCTURE_TYPE_ENGINE_PROPERTIES: u32 = 0x5;
const ZES_STRUCTURE_TYPE_FREQ_PROPERTIES: u32 = 0x9;
const ZES_STRUCTURE_TYPE_POWER_PROPERTIES: u32 = 0xd;
const ZES_STRUCTURE_TYPE_FREQ_STATE: u32 = 0x1b;
const ZES_STRUCTURE_TYPE_POWER_EXT_PROPERTIES: u32 = 0x28;
const ZES_POWER_DOMAIN_PACKAGE: u32 = 2;
const ZES_ENGINE_GROUP_ALL: u32 = 0;
const ZES_FREQ_DOMAIN_GPU: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct ZesPowerProperties {
    stype: u32,
    p_next: *mut c_void,
    on_subdevice: u8,
    subdevice_id: u32,
    can_control: u8,
    is_energy_threshold_supported: u8,
    default_limit: i32,
    min_limit: i32,
    max_limit: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZesPowerExtProperties {
    stype: u32,
    p_next: *mut c_void,
    domain: u32,
    default_limit: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZesPowerEnergyCounter {
    energy: u64,
    timestamp: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZesFreqProperties {
    stype: u32,
    p_next: *mut c_void,
    freq_type: u32,
    on_subdevice: u8,
    subdevice_id: u32,
    can_control: u8,
    is_throttle_event_supported: u8,
    min: f64,
    max: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZesFreqState {
    stype: u32,
    p_next: *const c_void,
    current_voltage: f64,
    request: f64,
    tdp: f64,
    efficient: f64,
    actual: f64,
    throttle_reasons: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZesEngineProperties {
    stype: u32,
    p_next: *mut c_void,
    engine_type: u32,
    on_subdevice: u8,
    subdevice_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZesEngineStats {
    active_time: u64,
    timestamp: u64,
}

type FnZesInit = unsafe extern "system" fn(u32) -> u32;
type FnZesDriverGet = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> u32;
type FnZesDeviceGet = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type FnZesEnumHandles = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type FnZesPowerGetProperties =
    unsafe extern "system" fn(*mut c_void, *mut ZesPowerProperties) -> u32;
type FnZesPowerGetEnergyCounter =
    unsafe extern "system" fn(*mut c_void, *mut ZesPowerEnergyCounter) -> u32;
type FnZesFreqGetProperties = unsafe extern "system" fn(*mut c_void, *mut ZesFreqProperties) -> u32;
type FnZesFreqGetState = unsafe extern "system" fn(*mut c_void, *mut ZesFreqState) -> u32;
type FnZesEngineGetProperties =
    unsafe extern "system" fn(*mut c_void, *mut ZesEngineProperties) -> u32;
type FnZesEngineGetActivity = unsafe extern "system" fn(*mut c_void, *mut ZesEngineStats) -> u32;

struct ZesSession {
    lib: Library,
    power_handle: *mut c_void,
    freq_handle: *mut c_void,
    engine_handle: *mut c_void,
    prev_energy: Option<ZesPowerEnergyCounter>,
    prev_engine: Option<ZesEngineStats>,
}

impl ZesSession {
    fn open() -> Option<Self> {
        unsafe {
            let _ = SetEnvironmentVariableA(s!("ZES_ENABLE_SYSMAN"), s!("1"));
            let lib = Library::new(r"C:\Windows\System32\ze_loader.dll").ok()?;

            let init: FnZesInit = *lib.get(b"zesInit\0").ok()?;
            if init(0) != ZE_RESULT_SUCCESS {
                return None;
            }
            let driver_get: FnZesDriverGet = *lib.get(b"zesDriverGet\0").ok()?;
            let device_get: FnZesDeviceGet = *lib.get(b"zesDeviceGet\0").ok()?;

            let mut drv_count = 0u32;
            if driver_get(&mut drv_count, null_mut()) != ZE_RESULT_SUCCESS || drv_count == 0 {
                return None;
            }
            let mut drivers = vec![null_mut(); drv_count as usize];
            if driver_get(&mut drv_count, drivers.as_mut_ptr()) != ZE_RESULT_SUCCESS {
                return None;
            }

            let mut target_dev = null_mut();
            for drv in drivers {
                let mut dev_count = 0u32;
                if device_get(drv, &mut dev_count, null_mut()) == ZE_RESULT_SUCCESS && dev_count > 0
                {
                    let mut devs = vec![null_mut(); dev_count as usize];
                    if device_get(drv, &mut dev_count, devs.as_mut_ptr()) == ZE_RESULT_SUCCESS {
                        target_dev = devs[0];
                        break;
                    }
                }
            }
            if target_dev.is_null() {
                return None;
            }

            let power_handle = Self::find_package_power_domain(&lib, target_dev);
            let freq_handle = Self::find_gpu_freq_domain(&lib, target_dev);
            let engine_handle = Self::find_all_engine_group(&lib, target_dev);

            if power_handle.is_null() && freq_handle.is_null() && engine_handle.is_null() {
                return None;
            }

            let mut session = Self {
                lib,
                power_handle,
                freq_handle,
                engine_handle,
                prev_energy: None,
                prev_engine: None,
            };
            // Prime baseline counters so the first 1s sample yields an immediate delta.
            session.prime();
            Some(session)
        }
    }

    unsafe fn find_package_power_domain(lib: &Library, dev: *mut c_void) -> *mut c_void {
        unsafe {
            let Ok(enum_pwr) = lib.get::<FnZesEnumHandles>(b"zesDeviceEnumPowerDomains\0") else {
                return null_mut();
            };
            let Ok(get_props) = lib.get::<FnZesPowerGetProperties>(b"zesPowerGetProperties\0")
            else {
                return null_mut();
            };
            let mut count = 0u32;
            if enum_pwr(dev, &mut count, null_mut()) != ZE_RESULT_SUCCESS || count == 0 {
                return null_mut();
            }
            let mut handles = vec![null_mut(); count as usize];
            if enum_pwr(dev, &mut count, handles.as_mut_ptr()) != ZE_RESULT_SUCCESS {
                return null_mut();
            }
            for &h in &handles {
                let mut ext = ZesPowerExtProperties {
                    stype: ZES_STRUCTURE_TYPE_POWER_EXT_PROPERTIES,
                    p_next: null_mut(),
                    domain: 0,
                    default_limit: null_mut(),
                };
                let mut props: ZesPowerProperties = zeroed();
                props.stype = ZES_STRUCTURE_TYPE_POWER_PROPERTIES;
                props.p_next = (&mut ext as *mut ZesPowerExtProperties).cast();
                if get_props(h, &mut props) == ZE_RESULT_SUCCESS
                    && ext.domain == ZES_POWER_DOMAIN_PACKAGE
                {
                    return h;
                }
            }
            handles.into_iter().next().unwrap_or(null_mut())
        }
    }

    unsafe fn find_gpu_freq_domain(lib: &Library, dev: *mut c_void) -> *mut c_void {
        unsafe {
            let Ok(enum_freq) = lib.get::<FnZesEnumHandles>(b"zesDeviceEnumFrequencyDomains\0")
            else {
                return null_mut();
            };
            let Ok(get_props) = lib.get::<FnZesFreqGetProperties>(b"zesFrequencyGetProperties\0")
            else {
                return null_mut();
            };
            let mut count = 0u32;
            if enum_freq(dev, &mut count, null_mut()) != ZE_RESULT_SUCCESS || count == 0 {
                return null_mut();
            }
            let mut handles = vec![null_mut(); count as usize];
            if enum_freq(dev, &mut count, handles.as_mut_ptr()) != ZE_RESULT_SUCCESS {
                return null_mut();
            }
            for &h in &handles {
                let mut props: ZesFreqProperties = zeroed();
                props.stype = ZES_STRUCTURE_TYPE_FREQ_PROPERTIES;
                if get_props(h, &mut props) == ZE_RESULT_SUCCESS
                    && props.freq_type == ZES_FREQ_DOMAIN_GPU
                {
                    return h;
                }
            }
            handles.into_iter().next().unwrap_or(null_mut())
        }
    }

    unsafe fn find_all_engine_group(lib: &Library, dev: *mut c_void) -> *mut c_void {
        unsafe {
            let Ok(enum_eng) = lib.get::<FnZesEnumHandles>(b"zesDeviceEnumEngineGroups\0") else {
                return null_mut();
            };
            let Ok(get_props) = lib.get::<FnZesEngineGetProperties>(b"zesEngineGetProperties\0")
            else {
                return null_mut();
            };
            let mut count = 0u32;
            if enum_eng(dev, &mut count, null_mut()) != ZE_RESULT_SUCCESS || count == 0 {
                return null_mut();
            }
            let mut handles = vec![null_mut(); count as usize];
            if enum_eng(dev, &mut count, handles.as_mut_ptr()) != ZE_RESULT_SUCCESS {
                return null_mut();
            }
            for &h in &handles {
                let mut props: ZesEngineProperties = zeroed();
                props.stype = ZES_STRUCTURE_TYPE_ENGINE_PROPERTIES;
                if get_props(h, &mut props) == ZE_RESULT_SUCCESS
                    && props.engine_type == ZES_ENGINE_GROUP_ALL
                {
                    return h;
                }
            }
            handles.into_iter().next().unwrap_or(null_mut())
        }
    }

    fn prime(&mut self) {
        if !self.power_handle.is_null() {
            self.prev_energy = self.read_energy();
        }
        if !self.engine_handle.is_null() {
            self.prev_engine = self.read_engine();
        }
    }

    fn read_energy(&self) -> Option<ZesPowerEnergyCounter> {
        unsafe {
            let f = self
                .lib
                .get::<FnZesPowerGetEnergyCounter>(b"zesPowerGetEnergyCounter\0")
                .ok()?;
            let mut c = ZesPowerEnergyCounter::default();
            if f(self.power_handle, &mut c) == ZE_RESULT_SUCCESS {
                Some(c)
            } else {
                None
            }
        }
    }

    fn read_engine(&self) -> Option<ZesEngineStats> {
        unsafe {
            let f = self
                .lib
                .get::<FnZesEngineGetActivity>(b"zesEngineGetActivity\0")
                .ok()?;
            let mut s = ZesEngineStats::default();
            if f(self.engine_handle, &mut s) == ZE_RESULT_SUCCESS {
                Some(s)
            } else {
                None
            }
        }
    }

    fn read_freq_state(&self) -> Option<ZesFreqState> {
        unsafe {
            let f = self
                .lib
                .get::<FnZesFreqGetState>(b"zesFrequencyGetState\0")
                .ok()?;
            let mut st: ZesFreqState = zeroed();
            st.stype = ZES_STRUCTURE_TYPE_FREQ_STATE;
            if f(self.freq_handle, &mut st) == ZE_RESULT_SUCCESS {
                Some(st)
            } else {
                None
            }
        }
    }

    fn sample(&mut self, out: &mut IntelReading) {
        if !self.power_handle.is_null()
            && let Some(curr) = self.read_energy()
        {
            if let Some(prev) = self.prev_energy {
                out.cpu_power_w = compute_energy_watts_u64(
                    prev.energy,
                    curr.energy,
                    prev.timestamp,
                    curr.timestamp,
                    MAX_ENERGY_DELTA_SECS,
                    MAX_PLAUSIBLE_CPU_WATTS,
                );
            }
            self.prev_energy = Some(curr);
        }

        if !self.engine_handle.is_null()
            && let Some(curr) = self.read_engine()
        {
            if let Some(prev) = self.prev_engine {
                out.igpu_busy_pct = compute_activity_pct_u64(
                    prev.active_time,
                    curr.active_time,
                    prev.timestamp,
                    curr.timestamp,
                    MAX_ENERGY_DELTA_SECS,
                );
            }
            self.prev_engine = Some(curr);
        }

        if !self.freq_handle.is_null()
            && let Some(st) = self.read_freq_state()
        {
            if st.actual >= 0.0 && st.actual.is_finite() {
                out.igpu_mhz = Some(st.actual);
            }
            if st.current_voltage >= 0.0 && st.current_voltage.is_finite() {
                out.igpu_voltage_v = Some(st.current_voltage);
            }
        }
    }
}

// ============================================================================
// Intel Graphics Control Library (ControlLib.dll) Fallback
// ============================================================================

const CTL_RESULT_SUCCESS: u32 = 0;
const CTL_IMPL_VERSION: u32 = (1 << 16) | 1;
const CTL_PSU_COUNT: usize = 5;
const CTL_FAN_COUNT: usize = 5;
const CTL_MAX_DEVICE_NAME_LEN: usize = 100;
const CTL_MAX_RESERVED_SIZE: usize = 108;

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlApplicationId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlInitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    application_uid: CtlApplicationId,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlFirmwareVersion {
    major_version: u64,
    minor_version: u64,
    build_number: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlAdapterBdf {
    bus: u8,
    device: u8,
    function: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlDeviceAdapterProperties {
    size: u32,
    version: u8,
    p_device_id: *mut c_void,
    device_id_size: u32,
    device_type: u32,
    supported_subfunction_flags: u32,
    driver_version: u64,
    firmware_version: CtlFirmwareVersion,
    pci_vendor_id: u32,
    pci_device_id: u32,
    rev_id: u32,
    num_eus_per_sub_slice: u32,
    num_sub_slices_per_slice: u32,
    num_slices: u32,
    name: [u8; CTL_MAX_DEVICE_NAME_LEN],
    graphics_adapter_properties: u32,
    frequency: u32,
    pci_subsys_id: u16,
    pci_subsys_vendor_id: u16,
    adapter_bdf: CtlAdapterBdf,
    num_xe_cores: u32,
    reserved: [u8; CTL_MAX_RESERVED_SIZE],
}

#[repr(C)]
#[derive(Clone, Copy)]
union CtlDataValue {
    data8: i8,
    datau8: u8,
    data16: i16,
    datau16: u16,
    data32: i32,
    datau32: u32,
    data64: i64,
    datau64: u64,
    datafloat: f32,
    datadouble: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlOcTelemetryItem {
    b_supported: u8,
    units: u32,
    data_type: u32,
    value: CtlDataValue,
}

impl CtlOcTelemetryItem {
    fn as_f64(&self) -> Option<f64> {
        if self.b_supported == 0 {
            return None;
        }
        unsafe {
            match self.data_type {
                0 => Some(self.value.data8 as f64),
                1 => Some(self.value.datau8 as f64),
                2 => Some(self.value.data16 as f64),
                3 => Some(self.value.datau16 as f64),
                4 => Some(self.value.data32 as f64),
                5 => Some(self.value.datau32 as f64),
                6 => Some(self.value.data64 as f64),
                7 => Some(self.value.datau64 as f64),
                8 => Some(self.value.datafloat as f64),
                9 => Some(self.value.datadouble),
                _ => None,
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPsuInfo {
    b_supported: u8,
    psu_type: u32,
    energy_counter: CtlOcTelemetryItem,
    voltage: CtlOcTelemetryItem,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPowerTelemetry {
    size: u32,
    version: u8,
    time_stamp: CtlOcTelemetryItem,
    gpu_energy_counter: CtlOcTelemetryItem,
    gpu_voltage: CtlOcTelemetryItem,
    gpu_current_clock_frequency: CtlOcTelemetryItem,
    gpu_current_temperature: CtlOcTelemetryItem,
    global_activity_counter: CtlOcTelemetryItem,
    render_compute_activity_counter: CtlOcTelemetryItem,
    media_activity_counter: CtlOcTelemetryItem,
    gpu_power_limited: u8,
    gpu_temperature_limited: u8,
    gpu_current_limited: u8,
    gpu_voltage_limited: u8,
    gpu_utilization_limited: u8,
    vram_energy_counter: CtlOcTelemetryItem,
    vram_voltage: CtlOcTelemetryItem,
    vram_current_clock_frequency: CtlOcTelemetryItem,
    vram_current_effective_frequency: CtlOcTelemetryItem,
    vram_read_bandwidth_counter: CtlOcTelemetryItem,
    vram_write_bandwidth_counter: CtlOcTelemetryItem,
    vram_current_temperature: CtlOcTelemetryItem,
    vram_power_limited: u8,
    vram_temperature_limited: u8,
    vram_current_limited: u8,
    vram_voltage_limited: u8,
    vram_utilization_limited: u8,
    total_card_energy_counter: CtlOcTelemetryItem,
    psu: [CtlPsuInfo; CTL_PSU_COUNT],
    fan_speed: [CtlOcTelemetryItem; CTL_FAN_COUNT],
    gpu_vr_temp: CtlOcTelemetryItem,
    vram_vr_temp: CtlOcTelemetryItem,
    sa_vr_temp: CtlOcTelemetryItem,
    gpu_effective_clock: CtlOcTelemetryItem,
    gpu_over_voltage_percent: CtlOcTelemetryItem,
    gpu_power_percent: CtlOcTelemetryItem,
    gpu_temperature_percent: CtlOcTelemetryItem,
    vram_read_bandwidth: CtlOcTelemetryItem,
    vram_write_bandwidth: CtlOcTelemetryItem,
}

type FnCtlInit = unsafe extern "C" fn(*mut CtlInitArgs, *mut *mut c_void) -> u32;
type FnCtlClose = unsafe extern "C" fn(*mut c_void) -> u32;
type FnCtlEnumDevices = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type FnCtlGetDevProps = unsafe extern "C" fn(*mut c_void, *mut CtlDeviceAdapterProperties) -> u32;
type FnCtlPowerTelemetryGet = unsafe extern "C" fn(*mut c_void, *mut CtlPowerTelemetry) -> u32;

struct IgclSession {
    lib: Library,
    api_handle: *mut c_void,
    device: *mut c_void,
    prev: Option<CtlPowerTelemetry>,
}

impl Drop for IgclSession {
    fn drop(&mut self) {
        if !self.api_handle.is_null() {
            unsafe {
                if let Ok(close) = self.lib.get::<FnCtlClose>(b"ctlClose\0") {
                    let _ = close(self.api_handle);
                }
            }
        }
    }
}

impl IgclSession {
    fn open() -> Option<Self> {
        unsafe {
            let lib = Library::new(r"C:\Windows\System32\ControlLib.dll").ok()?;
            let init: FnCtlInit = *lib.get(b"ctlInit\0").ok()?;
            let enum_dev: FnCtlEnumDevices = *lib.get(b"ctlEnumerateDevices\0").ok()?;
            let get_props: FnCtlGetDevProps = *lib.get(b"ctlGetDeviceProperties\0").ok()?;

            let mut args: CtlInitArgs = zeroed();
            args.size = size_of::<CtlInitArgs>() as u32;
            args.app_version = CTL_IMPL_VERSION;
            args.flags = 0;

            let mut api_handle = null_mut();
            if init(&mut args, &mut api_handle) != CTL_RESULT_SUCCESS || api_handle.is_null() {
                return None;
            }

            let mut count = 0u32;
            if enum_dev(api_handle, &mut count, null_mut()) != CTL_RESULT_SUCCESS || count == 0 {
                return None;
            }
            let mut devs = vec![null_mut(); count as usize];
            if enum_dev(api_handle, &mut count, devs.as_mut_ptr()) != CTL_RESULT_SUCCESS {
                return None;
            }

            // Pick the Intel adapter (vendor 0x8086) if multiple adapters exist.
            let mut chosen = devs[0];
            for &d in &devs {
                let mut luid = [0u8; 8];
                let mut props: CtlDeviceAdapterProperties = zeroed();
                props.size = size_of::<CtlDeviceAdapterProperties>() as u32;
                props.version = 2;
                props.p_device_id = luid.as_mut_ptr().cast();
                props.device_id_size = luid.len() as u32;
                if get_props(d, &mut props) == CTL_RESULT_SUCCESS {
                    let name_ok = CStr::from_bytes_until_nul(&props.name).is_ok();
                    if props.pci_vendor_id == 0x8086 && name_ok {
                        chosen = d;
                        break;
                    }
                }
            }

            let mut session = Self {
                lib,
                api_handle,
                device: chosen,
                prev: None,
            };
            session.prev = session.read_telemetry();
            Some(session)
        }
    }

    fn read_telemetry(&self) -> Option<CtlPowerTelemetry> {
        unsafe {
            let f = self
                .lib
                .get::<FnCtlPowerTelemetryGet>(b"ctlPowerTelemetryGet\0")
                .ok()?;
            let mut t: CtlPowerTelemetry = zeroed();
            t.size = size_of::<CtlPowerTelemetry>() as u32;
            t.version = 1;
            if f(self.device, &mut t) == CTL_RESULT_SUCCESS {
                Some(t)
            } else {
                None
            }
        }
    }

    fn fill_missing(&mut self, out: &mut IntelReading) {
        let Some(curr) = self.read_telemetry() else {
            return;
        };

        if let Some(prev) = self.prev
            && let (Some(t1), Some(t2)) = (prev.time_stamp.as_f64(), curr.time_stamp.as_f64())
        {
            if out.cpu_power_w.is_none()
                && let (Some(e1), Some(e2)) = (
                    prev.gpu_energy_counter.as_f64(),
                    curr.gpu_energy_counter.as_f64(),
                )
            {
                out.cpu_power_w = compute_energy_watts_f64(
                    e1,
                    e2,
                    t1,
                    t2,
                    MAX_ENERGY_DELTA_SECS,
                    MAX_PLAUSIBLE_CPU_WATTS,
                );
            }
            if out.igpu_busy_pct.is_none()
                && let (Some(a1), Some(a2)) = (
                    prev.global_activity_counter.as_f64(),
                    curr.global_activity_counter.as_f64(),
                )
            {
                let dt = t2 - t1;
                let da = a2 - a1;
                if dt > 0.0 && dt <= MAX_ENERGY_DELTA_SECS && da >= 0.0 {
                    out.igpu_busy_pct = Some(((da / dt) * 100.0).clamp(0.0, 100.0));
                }
            }
        }

        if out.igpu_mhz.is_none() {
            out.igpu_mhz = curr
                .gpu_current_clock_frequency
                .as_f64()
                .filter(|v| *v >= 0.0 && v.is_finite());
        }
        if out.igpu_voltage_v.is_none() {
            out.igpu_voltage_v = curr
                .gpu_voltage
                .as_f64()
                .filter(|v| *v >= 0.0 && v.is_finite());
        }

        self.prev = Some(curr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn igcl_item_and_struct_sanity() {
        assert_eq!(size_of::<CtlOcTelemetryItem>(), 24);
        let item = CtlOcTelemetryItem {
            b_supported: 1,
            units: 6,
            data_type: 9,
            value: CtlDataValue { datadouble: 42.5 },
        };
        assert_eq!(item.as_f64(), Some(42.5));
        let unsupported = CtlOcTelemetryItem {
            b_supported: 0,
            ..item
        };
        assert_eq!(unsupported.as_f64(), None);
    }
}
