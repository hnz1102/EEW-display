// Thread heartbeats, a periodic diagnostics line and a software watchdog.
//
// The quake and audio threads mark a `Beat` with what they are doing every time
// round their loops. The main loop calls `tick()`: every DIAG_SECS it logs where
// each thread is, and when a thread has not moved for its limit it logs which
// stage it is stuck in and restarts the chip. (On 2026-11-06 both threads
// stopped silently right after a new-earthquake announcement and the device
// missed an M6.1 ten hours later; the screen and button threads kept running.)
// The reason is also kept in RTC memory, which survives `esp_restart()`, and
// reported after the next boot in case the syslog line before it was lost.
// The quake thread also uses `restart()` when the WebSocket stays down too long.
// SPDX-License-Identifier: MIT

use log::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

/// Seconds since boot (monotonic, unaffected by NTP steps).
pub fn uptime_s() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1_000_000) as u32
}

pub struct Beat {
    name:   &'static str,
    stages: &'static [&'static str],
    /// A thread that has not moved for this long is considered stuck.
    limit:  u32,
    t:      AtomicU32,
    stage:  AtomicUsize,
    armed:  AtomicBool,
}

impl Beat {
    const fn new(name: &'static str, stages: &'static [&'static str], limit: u32) -> Self {
        Beat { name, stages, limit, t: AtomicU32::new(0), stage: AtomicUsize::new(0), armed: AtomicBool::new(false) }
    }
    /// Record that the thread is alive and entering `stage`.
    pub fn mark(&self, stage: usize) {
        self.stage.store(stage, Ordering::Relaxed);
        self.t.store(uptime_s(), Ordering::Relaxed);
        self.armed.store(true, Ordering::Relaxed);
    }
    /// The thread ended on purpose (e.g. audio init failed): stop watching it.
    pub fn disarm(&self) { self.armed.store(false, Ordering::Relaxed); }
    fn age(&self) -> u32 { uptime_s().saturating_sub(self.t.load(Ordering::Relaxed)) }
    fn stage_name(&self) -> &'static str {
        self.stages.get(self.stage.load(Ordering::Relaxed)).copied().unwrap_or("?")
    }
    fn describe(&self) -> String {
        if !self.armed.load(Ordering::Relaxed) { return format!("{}: -", self.name); }
        format!("{}: {} s ago at {}", self.name, self.age(), self.stage_name())
    }
}

/// Quake thread stages (index into the names below).
pub mod q {
    pub const LOOP: usize = 0;
    pub const WS_PUSHED: usize = 1;
    pub const WS_EEW: usize = 2;
    pub const WS_CREATE: usize = 3;
    pub const WS_DROP: usize = 4;
    pub const REST_EEW: usize = 5;
    pub const REST_QUAKE: usize = 6;
    pub const SLEEP: usize = 7;
}
/// Its longest legitimate step is a REST fetch (several 5 s network timeouts).
pub static QUAKE: Beat = Beat::new("quake",
    &["loop", "ws_pushed_551", "ws_pushed_556", "ws_create", "ws_drop", "rest_eew", "rest_quake", "sleep"], 180);

/// Audio thread stages.
pub mod a {
    pub const RECV: usize = 0;
    pub const SYNTH: usize = 1;
    pub const I2S_WRITE: usize = 2;
}
/// It goes round every ~10 ms (one I2S chunk), playing or not.
pub static AUDIO: Beat = Beat::new("audio", &["recv", "synth", "i2s_write"], 60);

const DIAG_SECS: u32 = 600;
static LAST_DIAG: AtomicU32 = AtomicU32::new(0);

fn diag_line() -> String {
    let (free, min, psram) = unsafe {
        use esp_idf_sys::*;
        (heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
         heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL),
         heap_caps_get_free_size(MALLOC_CAP_SPIRAM))
    };
    let (rx_n, rx_t) = crate::quake::ws_rx_stats();
    let rx_age = if rx_t == 0 { "-".to_string() } else { format!("{} s ago", uptime_s().saturating_sub(rx_t)) };
    format!("{} | {} playing={} | ws={} rx {} msgs, last {} map_busy={} | internal {} B (min {}) psram {} KB",
            QUAKE.describe(), AUDIO.describe(), crate::audio::playing(),
            crate::quake::ws_connected(), rx_n, rx_age, crate::map_display::is_busy(),
            free, min, psram / 1024)
}

// ─── Restart reason kept across esp_restart() ────────────────────────────────

const RTC_MAGIC: u32 = 0x5744_4f47; // "WDOG"
#[link_section = ".rtc_noinit"]
static mut RTC_REASON_MAGIC: u32 = 0;
#[link_section = ".rtc_noinit"]
static mut RTC_REASON: [u8; 96] = [0; 96];

fn save_reason(s: &str) {
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(RTC_REASON);
        let n = s.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        buf[n] = 0;
        core::ptr::addr_of_mut!(RTC_REASON_MAGIC).write_volatile(RTC_MAGIC);
    }
}

/// Call once after the logger is up: reports a watchdog restart of the previous run.
pub fn report_previous() {
    unsafe {
        if core::ptr::addr_of!(RTC_REASON_MAGIC).read_volatile() != RTC_MAGIC { return; }
        core::ptr::addr_of_mut!(RTC_REASON_MAGIC).write_volatile(0);
        let buf = &*core::ptr::addr_of!(RTC_REASON);
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        warn!("[WDOG] previous run was restarted by the watchdog: {}",
              String::from_utf8_lossy(&buf[..end]));
    }
}

/// Log why, keep the reason for the next boot, and restart the chip.
pub fn restart(reason: &str) -> ! {
    warn!("[WDOG] {} -- restarting", reason);
    warn!("[DIAG] {}", diag_line());
    save_reason(reason);
    // Give the UDP syslog a moment to get the lines out.
    std::thread::sleep(Duration::from_secs(2));
    unsafe { esp_idf_sys::esp_restart() };
    #[allow(unreachable_code)]
    loop { std::thread::sleep(Duration::from_secs(1)); }
}

/// Call from the main loop (cheap; rate-limits itself).
pub fn tick() {
    let now = uptime_s();
    for b in [&QUAKE, &AUDIO] {
        if b.armed.load(Ordering::Relaxed) && b.age() > b.limit {
            restart(&format!("{} thread stuck {} s at {}", b.name, b.age(), b.stage_name()));
        }
    }
    if now.saturating_sub(LAST_DIAG.load(Ordering::Relaxed)) >= DIAG_SECS {
        LAST_DIAG.store(now, Ordering::Relaxed);
        info!("[DIAG] {}", diag_line());
    }
}
