//! Host stub of the bits of `httpserver` the Info page reads.
use std::sync::{Arc, Mutex};

pub struct StatusInfo {
    pub ip_address:      Arc<Mutex<String>>,
    pub ssid:            Arc<Mutex<String>>,
    pub rssi:            Arc<Mutex<i32>>,
    pub battery_voltage: Arc<Mutex<f32>>,
    pub chip_temp:       Arc<Mutex<f32>>,
}

impl StatusInfo {
    pub fn new() -> Self {
        Self {
            ip_address:      Arc::new(Mutex::new("--".to_string())),
            ssid:            Arc::new(Mutex::new("--".to_string())),
            rssi:            Arc::new(Mutex::new(0)),
            battery_voltage: Arc::new(Mutex::new(0.0)),
            chip_temp:       Arc::new(Mutex::new(0.0)),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub int_free: u32,
    pub int_min: u32,
    pub psram_free: u32,
    pub psram_min: u32,
}

static MEM: Mutex<MemInfo> = Mutex::new(MemInfo { int_free: 0, int_min: 0, psram_free: 0, psram_min: 0 });
pub fn mem_info() -> MemInfo { *MEM.lock().unwrap() }
pub fn set_mem_info(m: MemInfo) { *MEM.lock().unwrap() = m; }

pub const FW_VERSION: &str = env!("FW_VERSION"); // from the firmware's Cargo.toml (build.rs)
pub const FW_BUILD: &str = "2026-10-12 03:21:47 JST"; // sample
