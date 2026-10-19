// HTTP Config Server for the EEW display
// Web UI for device configuration with NVS persistence
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Hiroshi Nakajima

use log::*;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use esp_idf_svc::io::{Read, Write};
use embedded_svc::http::Headers;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_sys::*;
use std::ffi::CString;

// ─── NVS namespace ────────────────────────────────────────────────────────────

const NVS_NS: &str = "usbotg_cfg";

// ─── Config struct ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct NvsConfig {
    pub wifi_ssid:     String,
    pub wifi_psk:      String,
    pub ip_mode:       String,
    pub ip_address:    String,
    pub subnet_mask:   String,
    pub gateway:       String,
    pub dns:           String,
    pub syslog_server: String,
    pub syslog_enable: String,
    pub syslog_host_name: String,
    pub syslog_app_name: String,
    pub web_user:      String,
    pub web_password:  String,
    pub display_enable: String,
    pub screen_timeout: String,
    pub audio_volume: String,
    pub voice_eew: String,
    pub voice_new: String,
    pub wps_enable:    String,
    pub ntp_server1:   String,
    pub ntp_server2:   String,
    pub ntp_server3:   String,
    pub ntp_server4:   String,
    pub map_type:      String,
    pub maps_api_key:  String,
    pub quake_enable:  String,
}

// ─── NVS helpers ─────────────────────────────────────────────────────────────

fn nvs_read(key: &str) -> Option<String> {
    let ns = CString::new(NVS_NS).ok()?;
    let k  = CString::new(key).ok()?;
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        // Get required length (pass null for out_value)
        let mut len: usize = 0;
        let ret = nvs_get_str(handle, k.as_ptr(), core::ptr::null_mut::<u8>(), &mut len);
        if ret != ESP_OK || len == 0 {
            nvs_close(handle);
            return None;
        }
        let mut buf = vec![0u8; len];
        let ret = nvs_get_str(handle, k.as_ptr(), buf.as_mut_ptr() as *mut u8, &mut len);
        nvs_close(handle);
        if ret != ESP_OK {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }
}

fn nvs_write_all(cfg: &NvsConfig) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let pairs: &[(&str, &str)] = &[
            ("wifi_ssid",     &cfg.wifi_ssid),
            ("wifi_psk",      &cfg.wifi_psk),
            ("ip_mode",       &cfg.ip_mode),
            ("ip_address",    &cfg.ip_address),
            ("subnet_mask",   &cfg.subnet_mask),
            ("gateway",       &cfg.gateway),
            ("dns",           &cfg.dns),
            ("syslog_server", &cfg.syslog_server),
            ("syslog_enable", &cfg.syslog_enable),
            ("syslog_hostname",    &cfg.syslog_host_name),
            ("syslog_app_name", &cfg.syslog_app_name),
            ("web_user",      &cfg.web_user),
            ("web_password",  &cfg.web_password),
            ("display_enable", &cfg.display_enable),
            ("screen_timeout", &cfg.screen_timeout),
            ("audio_volume", &cfg.audio_volume),
            ("voice_eew", &cfg.voice_eew),
            ("voice_new", &cfg.voice_new),
            ("wps_enable",    &cfg.wps_enable),
            ("ntp_server1",   &cfg.ntp_server1),
            ("ntp_server2",   &cfg.ntp_server2),
            ("ntp_server3",   &cfg.ntp_server3),
            ("ntp_server4",   &cfg.ntp_server4),
            ("map_type",      &cfg.map_type),
            ("maps_api_key", &cfg.maps_api_key),
        ];
        let mut ok = true;
        for (key, val) in pairs {
            match (CString::new(*key), CString::new(*val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => {
                    warn!("NVS: CString conversion failed for key '{}'", key);
                    ok = false;
                }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

fn nvs_erase_namespace() -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let ok = nvs_erase_all(handle) == ESP_OK;
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Write only wifi_ssid and wifi_psk to NVS (called after a successful WPS handshake).
pub fn nvs_write_wifi_creds(ssid: &str, psk: &str) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let mut ok = true;
        for (key, val) in [("wifi_ssid", ssid), ("wifi_psk", psk)] {
            match (CString::new(key), CString::new(val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => { ok = false; }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Erase all NVS settings and restart the device (returns to cfg.toml defaults on next boot).
pub fn factory_reset() -> ! {
    info!("Factory reset: erasing NVS namespace '{}'...", NVS_NS);
    nvs_erase_namespace();
    info!("NVS erased. Restarting device...");
    unsafe { esp_idf_sys::esp_restart() }
}

/// Load config from NVS, falling back to the provided defaults for any missing keys.
pub fn load_config(defaults: NvsConfig) -> NvsConfig {
    fn or(nvs_val: Option<String>, default: &str) -> String {
        match nvs_val {
            Some(s) if !s.is_empty() => s,
            _ => default.to_string(),
        }
    }
    info!("Loading config from NVS (namespace: {})", NVS_NS);
    NvsConfig {
        wifi_ssid:     or(nvs_read("wifi_ssid"),     &defaults.wifi_ssid),
        wifi_psk:      or(nvs_read("wifi_psk"),      &defaults.wifi_psk),
        ip_mode:       or(nvs_read("ip_mode"),       &defaults.ip_mode),
        ip_address:    or(nvs_read("ip_address"),    &defaults.ip_address),
        subnet_mask:   or(nvs_read("subnet_mask"),   &defaults.subnet_mask),
        gateway:       or(nvs_read("gateway"),       &defaults.gateway),
        dns:           or(nvs_read("dns"),           &defaults.dns),
        syslog_server: or(nvs_read("syslog_server"), &defaults.syslog_server),
        syslog_enable: or(nvs_read("syslog_enable"), &defaults.syslog_enable),
        syslog_host_name: or(nvs_read("syslog_hostname"), &defaults.syslog_host_name),
        syslog_app_name: or(nvs_read("syslog_app_name"), &defaults.syslog_app_name),
        web_user:      or(nvs_read("web_user"),      &defaults.web_user),
        web_password:  or(nvs_read("web_password"),  &defaults.web_password),
        display_enable: or(nvs_read("display_enable"), &defaults.display_enable),
        screen_timeout: or(nvs_read("screen_timeout"), &defaults.screen_timeout),
        audio_volume: or(nvs_read("audio_volume"), &defaults.audio_volume),
        voice_eew: or(nvs_read("voice_eew"), &defaults.voice_eew),
        voice_new: or(nvs_read("voice_new"), &defaults.voice_new),
        wps_enable:    or(nvs_read("wps_enable"),    &defaults.wps_enable),
        ntp_server1:   or(nvs_read("ntp_server1"),   &defaults.ntp_server1),
        ntp_server2:   or(nvs_read("ntp_server2"),   &defaults.ntp_server2),
        ntp_server3:   or(nvs_read("ntp_server3"),   &defaults.ntp_server3),
        ntp_server4:   or(nvs_read("ntp_server4"),   &defaults.ntp_server4),
        map_type:      or(nvs_read("map_type"),      &defaults.map_type),
        maps_api_key:  or(nvs_read("maps_api_key"), &defaults.maps_api_key),
        quake_enable:  or(nvs_read("quake_enable"), &defaults.quake_enable),
    }
}

// ─── Session management ───────────────────────────────────────────────────────

fn generate_token() -> String {
    (0..4)
        .map(|_| format!("{:08x}", unsafe { esp_random() }))
        .collect()
}

fn get_session_from_cookie_header(cookie_header: Option<&str>) -> Option<String> {
    let h = cookie_header?;
    for part in h.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("session=") {
            return Some(v.to_string());
        }
    }
    None
}

fn is_auth(cookie_header: Option<&str>, session: &Arc<Mutex<Option<String>>>) -> bool {
    let guard = session.lock().unwrap();
    if let Some(token) = guard.as_ref() {
        return get_session_from_cookie_header(cookie_header)
            .as_deref() == Some(token.as_str());
    }
    false
}

// ─── URL decode / form body parse ────────────────────────────────────────────

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(c) = u8::from_str_radix(hex, 16) {
                    out.push(c as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn parse_form(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in body.split('&') {
        if let Some(eq) = pair.find('=') {
            map.insert(url_decode(&pair[..eq]), url_decode(&pair[eq + 1..]));
        }
    }
    map
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// Runtime status shared between the main loop and the HTTP server.
#[derive(Clone)]
pub struct StatusInfo {
    pub ip_address:     Arc<Mutex<String>>,
    pub ssid:           Arc<Mutex<String>>,
    pub rssi:           Arc<Mutex<i32>>,
    pub battery_voltage: Arc<Mutex<f32>>,
    pub chip_temp:      Arc<Mutex<f32>>,
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
    pub fn set_wifi(&self, ip: &str, ssid: &str, rssi: i32) {
        *self.ip_address.lock().unwrap() = ip.to_string();
        *self.ssid.lock().unwrap()       = ssid.to_string();
        *self.rssi.lock().unwrap()       = rssi;
    }
    pub fn set_rssi(&self, rssi: i32) {
        *self.rssi.lock().unwrap() = rssi;
    }
    pub fn set_battery_voltage(&self, voltage: f32) {
        *self.battery_voltage.lock().unwrap() = voltage;
    }
    pub fn set_chip_temp(&self, temp: f32) {
        *self.chip_temp.lock().unwrap() = temp;
    }
}

#[derive(Clone)]
pub struct ConfigState {
    pub config:   Arc<Mutex<NvsConfig>>,
    pub defaults: Arc<NvsConfig>,
    pub status:   StatusInfo,
    session:      Arc<Mutex<Option<String>>>,
}

impl ConfigState {
    pub fn new(config: NvsConfig, defaults: NvsConfig, status: StatusInfo) -> Self {
        Self {
            config:   Arc::new(Mutex::new(config)),
            defaults: Arc::new(defaults),
            status,
            session:  Arc::new(Mutex::new(None)),
        }
    }
}

// ─── HTML helpers ─────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[allow(dead_code)]
fn sel(current: &str, value: &str) -> &'static str {
    if current == value { "selected" } else { "" }
}

fn chk(current: &str, value: &str) -> &'static str {
    if current == value { "checked" } else { "" }
}

// ─── Login page ───────────────────────────────────────────────────────────────

const LOGIN_HTML: &str = concat!(
    r#"<!DOCTYPE html><html lang="ja"><head>"#,
    r#"<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">"#,
    r#"<title>EEW Display Login</title><style>"#,
    r#"*{box-sizing:border-box}"#,
    r#"body{font-family:Arial,sans-serif;background:#1a1a2e;display:flex;justify-content:center;"#,
    r#"align-items:center;height:100vh;margin:0}"#,
    r#".box{background:#16213e;padding:40px;border-radius:12px;"#,
    r#"box-shadow:0 4px 20px rgba(0,0,0,.5);width:320px}"#,
    r#"h1{color:#e94560;text-align:center;margin:0 0 24px;font-size:1.4em}"#,
    r#"label{color:#a8b2d8;font-size:.9em;display:block;margin:12px 0 4px}"#,
    r#"input{width:100%;padding:10px;border:1px solid #304070;border-radius:6px;"#,
    r#"background:#0f3460;color:#e2e8f0;font-size:1em}"#,
    r#"input:focus{outline:none;border-color:#e94560}"#,
    r#"button{width:100%;margin-top:20px;padding:12px;background:#e94560;color:#fff;"#,
    r#"border:none;border-radius:6px;font-size:1em;font-weight:bold;cursor:pointer}"#,
    r#"button:hover{background:#c73652}"#,
    r#"#err{color:#f87171;text-align:center;margin-top:12px;font-size:.9em;display:none}"#,
    r#"</style></head><body>"#,
    r#"<div class="box"><h1>EEW Display Config</h1>"#,
    r#"<form method="POST" action="/login">"#,
    r#"<label>Username</label>"#,
    r#"<input type="text" name="username" autocomplete="username" required>"#,
    r#"<label>Password</label>"#,
    r#"<input type="password" name="password" autocomplete="current-password" required>"#,
    r#"<button type="submit">Login</button>"#,
    r#"</form>"#,
    r#"<div id="err">Username or password is incorrect</div></div>"#,
    r#"<script>if(location.search.includes('error=1'))"#,
    r#"document.getElementById('err').style.display='block';</script>"#,
    r#"</body></html>"#
);

// ─── Config page builder ──────────────────────────────────────────────────────

fn build_config_html(cfg: &NvsConfig) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>EEW Display Config</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:'Segoe UI',Arial,sans-serif;background:#111827;color:#e2e8f0;min-height:100vh}}
.page{{max-width:780px;margin:0 auto;padding:16px}}
h1{{color:#f87171;text-align:center;font-size:1.4em;padding:18px 0 14px;letter-spacing:.05em}}
/* Section headers with icon strip */
.section-title{{display:flex;align-items:center;gap:8px;color:#93c5fd;font-size:.82em;
  font-weight:700;text-transform:uppercase;letter-spacing:.08em;
  margin:0 0 12px;padding-bottom:6px;border-bottom:1px solid #1e3a5f}}
/* Cards */
.card{{background:#1e2d45;border-radius:10px;padding:16px 18px;margin-bottom:10px;
  border:1px solid #243b55;box-shadow:0 2px 8px rgba(0,0,0,.35)}}
/* Grid: 2-col on wide, 1-col on narrow */
.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:10px 14px;margin-bottom:4px}}
.grid-3{{display:grid;grid-template-columns:repeat(3,1fr);gap:10px 14px}}
@media(max-width:520px){{.grid-3{{grid-template-columns:1fr 1fr}}}}
/* Field */
.field label{{font-size:.76em;font-weight:600;color:#94a3b8;display:block;
  margin-bottom:4px;letter-spacing:.03em}}
.field input,.field select{{
  width:100%;padding:8px 10px;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:6px;color:#e2e8f0;font-size:.9em;transition:border-color .2s}}
.field input:focus,.field select:focus{{outline:none;border-color:#60a5fa;
  box-shadow:0 0 0 2px rgba(96,165,250,.15)}}
/* Radio toggle group */
.toggle-group{{display:inline-flex;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:8px;overflow:hidden;margin-bottom:10px}}
.toggle-group label{{padding:7px 18px;font-size:.85em;font-weight:600;cursor:pointer;
  color:#94a3b8;transition:background .2s,color .2s;user-select:none}}
.toggle-group input[type=radio]{{display:none}}
.toggle-group input[type=radio]:checked+span{{background:#1d4ed8;color:#fff;
  border-radius:7px;display:block;padding:7px 18px;margin:-7px -18px}}
/* Indent block shown/hidden based on IP mode */
.indent{{padding-left:4px;border-left:2px solid #1d4ed8;margin-top:6px}}
/* Status rows */
.stat-grid{{display:grid;grid-template-columns:auto 1fr;gap:6px 16px;align-items:center}}
.stat-key{{font-size:.78em;font-weight:600;color:#64748b;text-transform:uppercase;
  letter-spacing:.06em;white-space:nowrap}}
.stat-val{{font-size:.9em;color:#7dd3fc;font-weight:600}}
.badge{{display:inline-block;padding:2px 9px;border-radius:20px;font-size:.8em;font-weight:700}}
.badge-ok{{background:#14532d;color:#4ade80;border:1px solid #166534}}
.badge-warn{{background:#451a03;color:#fb923c;border:1px solid #7c2d12}}
.badge-err{{background:#4c0519;color:#fb7185;border:1px solid #881337}}
/* Message bar */
#msg{{padding:10px 14px;border-radius:7px;margin-bottom:10px;font-size:.88em;display:none;
  border-left:4px solid transparent}}
#msg.ok{{background:#052e16;border-color:#16a34a;color:#4ade80}}
#msg.err{{background:#350005;border-color:#dc2626;color:#fca5a5}}
/* Action buttons */
.actions{{display:flex;flex-wrap:wrap;gap:8px}}
.btn{{flex:1;min-width:110px;padding:11px 8px;border:none;border-radius:7px;
  font-size:.9em;font-weight:700;cursor:pointer;letter-spacing:.02em;transition:filter .2s}}
.btn:hover{{filter:brightness(1.15)}}
.btn-save{{background:#1d4ed8;color:#fff}}
.btn-reset{{background:#b45309;color:#fff}}
.btn-reboot{{background:#be123c;color:#fff}}
.btn-danger{{background:#991b1b;color:#fff}}
.btn-gpio{{background:#0f766e;color:#fff;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px;border:none;cursor:pointer;letter-spacing:.02em}}
.btn-gpio:hover{{filter:brightness(1.15)}}
.btn-logout{{background:#374151;color:#e2e8f0;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px}}
/* Divider */
hr{{border:none;border-top:1px solid #1e3a5f;margin:10px 0}}
</style></head><body>
<div class="page">
<h1>&#x1F30B; EEW Display Config</h1>
<div id="msg"></div>

<!-- Status -->
<div class="card">
  <div class="section-title">&#x1F4F6; Device Status</div>
  <div id="status-card" class="stat-grid">
    <span class="stat-key">Loading</span><span class="stat-val">...</span>
  </div>
</div>

<!-- Recent earthquakes (P2P地震情報); shown only when quake_enable = "true" -->
<div class="card" id="quake-box" style="display:none">
  <div class="section-title">&#x1F30B; Recent Earthquakes</div>
  <div id="eew-banner" style="display:none;background:#7f1d1d;color:#fff;padding:10px;border-radius:6px;margin-bottom:8px;font-weight:600"></div>
  <div id="quake-card" style="overflow-x:auto">Loading...</div>
  <div style="font-size:11px;color:#64748b;margin-top:6px">出典: 気象庁の情報を利用 / P2P地震情報 (<a href="https://www.p2pquake.net/" target="_blank" rel="noopener">p2pquake.net</a>) &mdash; Source: JMA via P2PQuake</div>
</div>

<form id="cfg">

<!-- Actions -->
<div class="card">
  <div class="actions">
    <button type="button" class="btn btn-save" onclick="saveConfig()">&#x1F4BE; Save to NVS</button>
    <button type="button" class="btn btn-reset" onclick="resetDefaults()">&#x21BA; Reset Defaults</button>
    <button type="button" class="btn btn-reboot" onclick="reboot()">&#x1F501; Reboot</button>
    <a href="/map" class="btn-gpio" style="background:#164e63">&#x1F4CD; Map</a>
    <a href="/logout" class="btn-logout">&#x1F6AA; Logout</a>
    <button type="button" class="btn" style="background:#0e7490" onclick="showBootLog()">&#x1F4DC; Boot Log</button>
  </div>
</div>

<!-- Boot Log Modal -->
<div id="boot-log-overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,.7);z-index:100;align-items:center;justify-content:center">
  <div style="background:#1e2d45;border:1px solid #243b55;border-radius:12px;padding:20px 24px;max-width:480px;width:90%;max-height:80vh;overflow-y:auto;box-shadow:0 8px 32px rgba(0,0,0,.6)">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:14px">
      <span style="color:#93c5fd;font-weight:700;font-size:.95em;text-transform:uppercase;letter-spacing:.08em">&#x1F4DC; Boot Log</span>
      <button onclick="closeBootLog()" style="background:none;border:none;color:#94a3b8;font-size:1.4em;cursor:pointer;line-height:1">&times;</button>
    </div>
    <div id="boot-log-body" style="font-size:.88em;color:#e2e8f0;line-height:1.7">
      <span style="color:#64748b">Loading...</span>
    </div>
  </div>
</div>

<!-- Audio (MAX98357A): test tone + voice test; shown only when audio_enable = "true" -->
<div class="card" id="tone-box" style="display:none">
  <div class="section-title">&#x1F50A; Audio</div>
  <div style="display:flex;align-items:center;gap:12px;flex-wrap:wrap">
    <button type="button" class="btn btn-save" id="tone-btn" style="flex:0 0 auto" onclick="toneToggle()">ON</button>
    <span class="stat-val" id="tone-hz" style="min-width:90px;font-size:1.1em">-- Hz</span>
    <input type="range" id="tone-range" min="0" max="1000" value="333" style="flex:1;min-width:160px" oninput="toneSlide()">
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.4em 0 0">Test tone: sine wave on the MAX98357A, 100 Hz &ndash; 15 kHz (logarithmic slider). Applies immediately.</p>
  <div style="display:flex;align-items:center;gap:12px;margin-top:12px">
    <span class="stat-key" style="min-width:60px">Volume</span>
    <input type="range" id="vol-range" min="0" max="100" value="70" style="flex:1" oninput="volSlide()" onchange="volCommit()">
    <span class="stat-val" id="vol-val" style="min-width:48px">70 %</span>
  </div>
  <div id="notify-row" style="display:none;margin-top:12px;gap:18px;flex-wrap:wrap;align-items:center">
    <span class="stat-key">Announce</span>
    <label style="display:flex;align-items:center;gap:6px;cursor:pointer"><input type="checkbox" id="notify-eew" onchange="notifySend()"> EEW (緊急地震速報)</label>
    <label style="display:flex;align-items:center;gap:6px;cursor:pointer"><input type="checkbox" id="notify-new" onchange="notifySend()"> New earthquake (地震情報)</label>
  </div>
  <div id="voice-row" style="display:none;margin-top:10px;gap:8px;flex-wrap:wrap">
    <button type="button" class="btn" style="background:#b91c1c;flex:0 0 auto" onclick="toneSend('speak=eew')">&#x1F5E3; Test EEW voice</button>
    <button type="button" class="btn" style="background:#b45309;flex:0 0 auto" onclick="toneSend('speak=new')">&#x1F5E3; Test new-quake voice</button>
  </div>
</div>

<!-- WiFi -->
<div class="card">
  <div class="section-title">&#x1F4F6; WiFi</div>
  <div class="grid">
    <div class="field"><label>SSID</label>
      <input type="text" name="wifi_ssid" value="{wifi_ssid}" maxlength="32" autocomplete="off"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="wifi_psk" value="{wifi_psk}" maxlength="64" autocomplete="off"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">WPS (PBC)</div>
  <div class="toggle-group">
    <label><input type="radio" name="wps_enable" value="true" {wps_true}><span>Enable</span></label>
    <label><input type="radio" name="wps_enable" value="false" {wps_false}><span>Disable</span></label>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.25em 0 0;">When WPS is enabled and SSID is empty, WPS PBC is triggered on next boot.</p>
</div>

<!-- Network -->
<div class="card">
  <div class="section-title">&#x1F310; Network</div>
  <div class="toggle-group">
    <label><input type="radio" name="ip_mode" value="dhcp" {dhcp_chk} onchange="ipModeChange()"><span>DHCP</span></label>
    <label><input type="radio" name="ip_mode" value="static" {static_chk} onchange="ipModeChange()"><span>Static IP</span></label>
  </div>
  <div class="indent" id="static-fields">
    <div class="grid">
      <div class="field"><label>IP Address</label>
        <input type="text" name="ip_address" value="{ip_address}" placeholder="192.168.1.100"></div>
      <div class="field"><label>Subnet Mask</label>
        <input type="text" name="subnet_mask" value="{subnet_mask}" placeholder="255.255.255.0"></div>
      <div class="field"><label>Gateway</label>
        <input type="text" name="gateway" value="{gateway}" placeholder="192.168.1.1"></div>
      <div class="field"><label>DNS Server</label>
        <input type="text" name="dns" value="{dns}" placeholder="192.168.1.1"></div>
    </div>
  </div>
</div>

<!-- Syslog -->
<div class="card">
  <div class="section-title">&#x1F4CB; Syslog</div>
  <div class="grid">
    <div class="field" style="grid-column:1/-1"><label>Server (host:port)</label>
      <input type="text" name="syslog_server" value="{syslog_server}" placeholder="192.168.1.1:514"></div>
    <div class="field"><label>Host Name</label>
      <input type="text" name="syslog_hostname" value="{syslog_host_name}" placeholder="esp32"></div>
    <div class="field"><label>App Name</label>
      <input type="text" name="syslog_app_name" value="{syslog_app_name}" placeholder="app"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">Enable Syslog</div>
  <div class="toggle-group">
    <label><input type="radio" name="syslog_enable" value="true" {syslog_true}><span>Enabled</span></label>
    <label><input type="radio" name="syslog_enable" value="false" {syslog_false}><span>Disabled</span></label>
  </div>
</div>

<!-- Auth -->
<div class="card">
  <div class="section-title">&#x1F510; Authentication (Web UI)</div>
  <div class="grid">
    <div class="field"><label>Username</label>
      <input type="text" name="web_user" value="{web_user}" maxlength="64" autocomplete="username"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="web_password" value="{web_password}" maxlength="64" autocomplete="new-password"></div>
  </div>
</div>

<!-- Display -->
<div class="card">
  <div class="section-title">&#x1F4FA; Display</div>
  <div class="grid">
    <div class="field"><label>Screen off after idle (seconds, 0 = always on)</label>
      <input type="number" name="screen_timeout" value="{screen_timeout}" min="0" max="86400"></div>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.4em 0 0">The backlight goes off after this long without a button press, and comes back on at a button press or when an EEW / new earthquake arrives. Applies immediately on Save.</p>
</div>

<!-- NTP Servers -->
<div class="card">
  <div class="section-title">&#x1F552; NTP Servers</div>
  <div class="grid">
    <div class="field"><label>NTP Server 1 (Primary)</label>
      <input type="text" name="ntp_server1" value="{ntp_server1}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 2</label>
      <input type="text" name="ntp_server2" value="{ntp_server2}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 3</label>
      <input type="text" name="ntp_server3" value="{ntp_server3}" placeholder="time.cloudflare.com"></div>
    <div class="field"><label>NTP Server 4</label>
      <input type="text" name="ntp_server4" value="{ntp_server4}" placeholder="ntp.nict.jp"></div>
  </div>
</div>

<!-- Map Settings -->
<div class="card">
  <div class="section-title">&#x1F5FA; Map Settings</div>
  <div class="grid">
    <div class="field"><label>Map Type</label>
      <select name="map_type">
        <option value="satellite" {mt_sat}>Satellite (aerial photo)</option>
        <option value="roadmap"   {mt_road}>Roadmap</option>
      </select></div>
    <div class="field"><label>Google Maps API Key</label>
      <input type="password" name="maps_api_key" value="{maps_api_key}" maxlength="128" autocomplete="off" placeholder="Maps Static API key"></div>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.4em 0 0">Needs the <b>Maps Static API</b> enabled on the key. Applies immediately on Save. Leaving it empty uses the cfg.toml default. The Web UI map pages load images from Google directly in your browser, so a logged-in browser receives this key.</p>
</div>

</form>
</div>
<script>
// Show/hide static IP fields based on ip_mode radio
function ipModeChange() {{
  var isStatic = document.querySelector('input[name=ip_mode]:checked').value === 'static';
  document.getElementById('static-fields').style.display = isStatic ? 'block' : 'none';
}}
// Apply on load
ipModeChange();
function showMsg(text, ok) {{
  var d = document.getElementById('msg');
  d.textContent = text;
  d.className = ok ? 'ok' : 'err';
  d.style.display = 'block';
  d.scrollIntoView({{behavior:'smooth'}});
}}
function saveConfig() {{
  var fd = new FormData(document.getElementById('cfg'));
  var body = new URLSearchParams(fd).toString();
  fetch('/api/config', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/x-www-form-urlencoded'}},
    body: body
  }})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Save failed: ' + e, false));
}}
function reboot() {{
  if (!confirm('Reboot the device?')) return;
  fetch('/api/reboot', {{method: 'POST'}})
  .then(() => {{
    showMsg('Rebooting... redirecting to login in 3 seconds.', true);
    setTimeout(() => {{ location.href = '/login'; }}, 3000);
  }})
  .catch(e => showMsg('Reboot request failed: ' + e, false));
}}
function resetDefaults() {{
  if (!confirm('Reset all settings to compile-time defaults and reload the page?')) return;
  fetch('/api/reset', {{method: 'POST'}})
  .then(r => r.text())
  .then(() => {{ location.reload(); }})
  .catch(e => showMsg('Reset failed: ' + e, false));
}}
function rssiBar(rssi) {{
  if (rssi >= -55) return '<span class="badge badge-ok">Excellent (' + rssi + ' dBm)</span>';
  if (rssi >= -70) return '<span class="badge badge-ok">Good (' + rssi + ' dBm)</span>';
  if (rssi >= -80) return '<span class="badge badge-warn">Fair (' + rssi + ' dBm)</span>';
  return '<span class="badge badge-err">Weak (' + rssi + ' dBm)</span>';
}}
function updateStatus() {{
  fetch('/api/status')
  .then(r => r.json())
  .then(d => {{
    document.getElementById('status-card').innerHTML =
      '<span class="stat-key">Firmware Version</span><span class="stat-val"><span class="badge badge-ok">' + d.version + '</span></span>' +
      '<span class="stat-key">Build Date</span><span class="stat-val">' + d.build + '</span>' +
      '<span class="stat-key">IP Address</span><span class="stat-val">' + d.ip + '</span>' +
      '<span class="stat-key">WiFi SSID</span><span class="stat-val">' + d.ssid + '</span>' +
      '<span class="stat-key">RSSI</span><span class="stat-val">' + rssiBar(d.rssi) + '</span>' +
      '<span class="stat-key">Battery Voltage</span><span class="stat-val"><span class="badge badge-ok">' + d.battery_voltage.toFixed(2) + ' V</span></span>' +
      '<span class="stat-key">Chip Temp</span><span class="stat-val"><span class="badge ' + (d.chip_temp < 70 ? 'badge-ok' : d.chip_temp < 85 ? 'badge-warn' : 'badge-err') + '">' + d.chip_temp.toFixed(1) + ' °C</span></span>' +
      '<span class="stat-key">Free Heap (Internal)</span><span class="stat-val"><span class="badge badge-ok">' + (d.heap_int_free/1024).toFixed(0) + ' KB</span> <small>min ' + (d.heap_int_min/1024).toFixed(0) + ' KB</small></span>' +
      '<span class="stat-key">Free Heap (PSRAM)</span><span class="stat-val"><span class="badge badge-ok">' + (d.heap_psram_free/1024).toFixed(0) + ' KB</span> <small>min ' + (d.heap_psram_min/1024).toFixed(0) + ' KB</small></span>';
  }})
  .catch(() => {{}});
}}
updateStatus();
setInterval(updateStatus, 3000);
function updateQuakes() {{
  fetch('/api/quakes')
  .then(r => r.json())
  .then(d => {{
    document.getElementById('quake-box').style.display = d.enabled ? '' : 'none';
    if (!d.enabled) return;
    var b = document.getElementById('eew-banner');
    if (d.eew) {{
      b.style.display = 'block';
      b.textContent = '\u26A0 \u7DCA\u6025\u5730\u9707\u901F\u5831 ' + d.eew.time + ' ' + d.eew.name +
        ' M' + (d.eew.mag >= 0 ? d.eew.mag.toFixed(1) : '?') + ' \u6700\u5927\u4E88\u6E2C\u9707\u5EA6' + d.eew.intensity +
        (d.eew.areas ? ' / ' + d.eew.areas : '');
    if (d.eew.view) {{
      var a = document.createElement('a');
      a.href = '/map?lat=' + d.eew.view.lat + '&lon=' + d.eew.view.lon + '&zoom=' + d.eew.view.zoom;
      a.textContent = ' \u25B6 MAP';
      a.style.color = '#fde047';
      b.appendChild(a);
    }}
    }} else {{ b.style.display = 'none'; }}
    if (!d.quakes.length) {{
      document.getElementById('quake-card').textContent = d.ready ? 'No events' : 'Waiting for data...';
      return;
    }}
    var h = '<table style="width:100%;border-collapse:collapse;font-size:13px">' +
      '<tr style="text-align:left;color:#64748b"><th>Time (JST)</th><th>Epicenter</th><th>M</th><th>Int.</th><th>Depth</th><th>Tsunami</th></tr>';
    d.quakes.forEach(function(q) {{
      var sev = q.scale >= 45 || q.mag >= 6 ? 'badge-err' : (q.scale >= 30 || q.mag >= 4.5 ? 'badge-warn' : 'badge-ok');
      var depth = q.depth < 0 ? '?' : (q.depth === 0 ? 'Very shallow' : q.depth + ' km');
      var link = '/map?lat=' + q.lat + '&lon=' + q.lon + '&zoom=' + (q.mag >= 6 ? 6 : 8);
      h += '<tr><td>' + q.time + '</td>' +
        '<td><a href="' + link + '">' + q.name + '</a></td>' +
        '<td><span class="badge ' + sev + '">' + (q.mag >= 0 ? q.mag.toFixed(1) : '?') + '</span></td>' +
        '<td>' + q.intensity + '</td><td>' + depth + '</td><td>' + q.tsunami + '</td></tr>';
    }});
    document.getElementById('quake-card').innerHTML = h + '</table>';
  }})
  .catch(() => {{}});
}}
updateQuakes();
setInterval(updateQuakes, 5000);
var toneOn = true, toneTimer = null;
function toneHzFromSlider() {{
  var x = parseInt(document.getElementById('tone-range').value);
  return Math.round(100 * Math.pow(150, x / 1000));
}}
function toneShow(d) {{
  document.getElementById('tone-box').style.display = d.running ? '' : 'none';
  document.getElementById('voice-row').style.display = d.voice ? 'flex' : 'none';
  if (!d.running) return;
  toneOn = d.on;
  document.getElementById('tone-hz').textContent = d.hz + ' Hz';
  var b = document.getElementById('tone-btn');
  b.textContent = d.on ? 'ON' : 'OFF';
  b.style.background = d.on ? '#1d4ed8' : '#374151';
}}
function toneSend(body) {{
  return fetch('/api/tone', {{method: 'POST',
    headers: {{'Content-Type': 'application/x-www-form-urlencoded'}}, body: body}})
  .then(r => r.json()).then(toneShow).catch(() => {{}});
}}
function toneSlide() {{
  var hz = toneHzFromSlider();
  document.getElementById('tone-hz').textContent = hz + ' Hz';
  if (toneTimer) clearTimeout(toneTimer);
  toneTimer = setTimeout(function() {{ toneSend('hz=' + hz); }}, 60);
}}
var volTimer = null;
function volSlide() {{
  var v = parseInt(document.getElementById('vol-range').value);
  document.getElementById('vol-val').textContent = v + ' %';
  if (volTimer) clearTimeout(volTimer);
  volTimer = setTimeout(function() {{ toneSend('volume=' + v); }}, 60);
}}
function volCommit() {{
  if (volTimer) clearTimeout(volTimer);
  toneSend('volume=' + parseInt(document.getElementById('vol-range').value) + '&save=1');
}}
function notifySend() {{
  toneSend('notify_eew=' + document.getElementById('notify-eew').checked +
           '&notify_new=' + document.getElementById('notify-new').checked);
}}
function toneToggle() {{ toneSend('on=' + (!toneOn)); }}
fetch('/api/tone').then(r => r.json()).then(function(d) {{
  toneShow(d);
  if (d.running) document.getElementById('tone-range').value =
    Math.round(1000 * Math.log(d.hz / 100) / Math.log(150));
  if (d.running) {{
    document.getElementById('notify-eew').checked = d.notify_eew;
    document.getElementById('notify-new').checked = d.notify_new;
    document.getElementById('notify-row').style.display = 'flex';
    document.getElementById('vol-range').value = d.volume;
    document.getElementById('vol-val').textContent = d.volume + ' %';
  }}
}}).catch(() => {{}});
function showBootLog() {{
  var overlay = document.getElementById('boot-log-overlay');
  overlay.style.display = 'flex';
  document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">Loading...</span>';
  fetch('/api/boot_log')
  .then(r => r.json())
  .then(d => {{
    if (!d.entries || d.entries.length === 0) {{
      document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">No entries yet.</span>';
      return;
    }}
    var reasons = {{
      'PowerOn':     ['badge-ok',   '&#x1F50C; PowerOn'],
      'Software':    ['badge-ok',   '&#x1F501; Software'],
      'Panic/Crash': ['badge-err',  '&#x1F4A5; Panic/Crash'],
      'IntWatchdog': ['badge-err',  '&#x231B; IntWatchdog'],
      'TaskWatchdog':['badge-warn', '&#x231B; TaskWatchdog'],
      'Watchdog':    ['badge-warn', '&#x231B; Watchdog'],
      'Brownout':    ['badge-err',  '&#x26A1; Brownout'],
      'DeepSleep':   ['badge-ok',   '&#x1F4A4; DeepSleep'],
      'ExtReset':    ['badge-ok',   '&#x1F504; ExtReset'],
      'SDIO':        ['badge-warn', '&#x1F4BE; SDIO'],
    }};
    var html = '<table style="width:100%;border-collapse:collapse">' +
      '<tr><th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55">#</th>' +
      '<th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55;padding-left:10px">Reset Reason</th></tr>';
    d.entries.forEach(function(e) {{
      var m = e.match(/^#(\d+)\s+(.+)$/);
      if (!m) return;
      var num = m[1], reason = m[2].trim();
      var info = reasons[reason] || ['badge-warn', reason];
      html += '<tr><td style="padding:5px 0;color:#64748b;font-size:.85em;vertical-align:top">' + num + '</td>' +
        '<td style="padding:5px 0 5px 10px"><span class="badge ' + info[0] + '">' + info[1] + '</span></td></tr>';
    }});
    html += '</table>';
    document.getElementById('boot-log-body').innerHTML = html;
  }})
  .catch(e => {{
    document.getElementById('boot-log-body').innerHTML = '<span style="color:#fca5a5">Failed to load: ' + e + '</span>';
  }});
}}
function closeBootLog() {{
  document.getElementById('boot-log-overlay').style.display = 'none';
}}
document.getElementById('boot-log-overlay').addEventListener('click', function(e) {{
  if (e.target === this) closeBootLog();
}});
</script>
</body></html>"#,
        wifi_ssid     = esc(&cfg.wifi_ssid),
        wifi_psk      = esc(&cfg.wifi_psk),
        wps_true      = chk(&cfg.wps_enable, "true"),
        wps_false     = chk(&cfg.wps_enable, "false"),
        dhcp_chk      = chk(&cfg.ip_mode, "dhcp"),
        static_chk    = chk(&cfg.ip_mode, "static"),
        ip_address    = esc(&cfg.ip_address),
        subnet_mask   = esc(&cfg.subnet_mask),
        gateway       = esc(&cfg.gateway),
        dns           = esc(&cfg.dns),
        syslog_server = esc(&cfg.syslog_server),
        syslog_true   = chk(&cfg.syslog_enable, "true"),
        syslog_false  = chk(&cfg.syslog_enable, "false"),
        syslog_host_name = esc(&cfg.syslog_host_name),
        syslog_app_name  = esc(&cfg.syslog_app_name),
        web_user      = esc(&cfg.web_user),
        web_password  = esc(&cfg.web_password),
        screen_timeout = esc(&cfg.screen_timeout),
        ntp_server1   = esc(&cfg.ntp_server1),
        ntp_server2   = esc(&cfg.ntp_server2),
        ntp_server3   = esc(&cfg.ntp_server3),
        ntp_server4   = esc(&cfg.ntp_server4),
        maps_api_key  = esc(&cfg.maps_api_key),
        mt_sat        = if cfg.map_type == "satellite" { "selected" } else { "" },
        mt_road       = if cfg.map_type == "roadmap"   { "selected" } else { "" },
    )
}

// ─── HTTP server ──────────────────────────────────────────────────────────────

// ─── HTTP server ──────────────────────────────────────────────────────────────

pub fn start_http_server(state: ConfigState) -> anyhow::Result<EspHttpServer<'static>> {
    // 32 is esp-idf-svc's default and the handlers below were already close to it.
    let config = Configuration { max_uri_handlers: 40, ..Default::default() };
    let mut server = EspHttpServer::new(&config)?;

    // ── GET /login ──────────────────────────────────────────────────────────
    server.fn_handler("/login", Method::Get, |request| {
        let mut response = request.into_ok_response()?;
        response.write_all(LOGIN_HTML.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /login ─────────────────────────────────────────────────────────
    let state_login = state.clone();
    server.fn_handler("/login", Method::Post, move |mut request| {
        let len = (request.content_len().unwrap_or(0) as usize).min(512);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let cfg = state_login.config.lock().unwrap();
        let valid = form.get("username").map(String::as_str) == Some(cfg.web_user.as_str())
            && form.get("password").map(String::as_str) == Some(cfg.web_password.as_str());
        drop(cfg);

        if valid {
            let token = generate_token();
            *state_login.session.lock().unwrap() = Some(token.clone());
            let cookie = format!(
                "session={}; Path=/; HttpOnly; SameSite=Strict",
                token
            );
            let headers = [("Set-Cookie", cookie.as_str()), ("Location", "/")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        } else {
            info!("HTTP: failed login attempt");
            let headers = [("Location", "/login?error=1")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /logout ─────────────────────────────────────────────────────────
    let state_logout = state.clone();
    server.fn_handler("/logout", Method::Get, move |request| {
        *state_logout.session.lock().unwrap() = None;
        let headers = [
            ("Set-Cookie", "session=; Path=/; Max-Age=0"),
            ("Location", "/login"),
        ];
        request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET / — config page (requires auth) ─────────────────────────────────
    let state_main = state.clone();
    server.fn_handler("/", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_main.session) {
            let headers = [("Location", "/login")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let cfg = state_main.config.lock().unwrap().clone();
        let html = build_config_html(&cfg);
        let headers = [("Content-Type", "text/html; charset=UTF-8")];
        let mut response = request.into_response(200, Some("OK"), &headers)?;
        response.write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/config — save config to NVS (requires auth) ───────────────
    let state_cfg = state.clone();
    server.fn_handler("/api/config", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_cfg.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(4096);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let mut cfg = state_cfg.config.lock().unwrap();
        macro_rules! update {
            ($field:ident, $key:expr) => {
                if let Some(v) = form.get($key) {
                    cfg.$field = v.clone();
                }
            };
        }
        update!(wifi_ssid,     "wifi_ssid");
        update!(wifi_psk,      "wifi_psk");
        update!(ip_mode,       "ip_mode");
        update!(ip_address,    "ip_address");
        update!(subnet_mask,   "subnet_mask");
        update!(gateway,       "gateway");
        update!(dns,           "dns");
        update!(syslog_server, "syslog_server");
        update!(syslog_enable, "syslog_enable");
        update!(syslog_host_name, "syslog_hostname");
        update!(syslog_app_name, "syslog_app_name");
        update!(web_user,      "web_user");
        update!(web_password,  "web_password");
        update!(wps_enable,    "wps_enable");
        update!(screen_timeout, "screen_timeout");
        update!(ntp_server1,   "ntp_server1");
        update!(ntp_server2,   "ntp_server2");
        update!(ntp_server3,   "ntp_server3");
        update!(ntp_server4,   "ntp_server4");
        update!(map_type,      "map_type");
        update!(maps_api_key,  "maps_api_key");
        cfg.maps_api_key = cfg.maps_api_key.trim().to_string();

        // Apply map type change immediately (no reboot needed)
        crate::map_display::set_map_type_satellite(cfg.map_type == "satellite");

        // Apply Maps API key change immediately (no reboot needed), and
        // refetch the map with it.
        if crate::map_display::set_api_key(&cfg.maps_api_key) {
            crate::map_display::reset_last_pos();
        }

        // Apply the screen-saver timeout immediately
        crate::display::set_idle_timeout(cfg.screen_timeout.trim().parse().unwrap_or(0));

        // Apply syslog hostname/app_name changes immediately (no reboot needed)
        if let Err(e) = crate::syslogger::update_logger_config(&cfg.syslog_host_name, &cfg.syslog_app_name) {
            warn!("Failed to update syslog config: {}", e);
        }

        let ok = nvs_write_all(&cfg);
        drop(cfg);

        if ok {
            info!("HTTP: config saved to NVS");
            let headers = [("Content-Type", "text/plain; charset=UTF-8")];
            request.into_response(200, Some("OK"), &headers)?
                .write_all("Settings saved. Reboot to apply.".as_bytes())?;
        } else {
            warn!("HTTP: failed to save config to NVS");
            request.into_status_response(500)?
                .write_all(b"NVS write error")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/status — device/wifi/usb status (requires auth) ────────────
    let state_stat = state.clone();
    server.fn_handler("/api/status", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_stat.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let ip   = state_stat.status.ip_address.lock().unwrap().clone();
        let ssid = state_stat.status.ssid.lock().unwrap().clone();
        let rssi = *state_stat.status.rssi.lock().unwrap();
        let battery_voltage = *state_stat.status.battery_voltage.lock().unwrap();
        let chip_temp = *state_stat.status.chip_temp.lock().unwrap();
        let mem = mem_info();
        let json = format!(
            r#"{{"ip":"{ip}","ssid":"{ssid}","rssi":{rssi},"battery_voltage":{battery_voltage:.2},"chip_temp":{chip_temp:.1},"heap_int_free":{hif},"heap_int_min":{him},"heap_psram_free":{hpf},"heap_psram_min":{hpm},"version":"{version}","build":"{build}"}}"#,
            version = esc(FW_VERSION), build = esc(FW_BUILD),
            hif = mem.int_free, him = mem.int_min, hpf = mem.psram_free, hpm = mem.psram_min,
            ip              = esc(&ip),
            ssid            = esc(&ssid),
            rssi            = rssi,
            battery_voltage = battery_voltage,
            chip_temp       = chip_temp,
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reset — erase NVS and restore defaults (requires auth) ────
    let state_rst = state.clone();
    server.fn_handler("/api/reset", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rst.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        nvs_erase_namespace();
        let defaults = (*state_rst.defaults).clone();
        *state_rst.config.lock().unwrap() = defaults;
        info!("HTTP: config reset to defaults");
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"Reset to defaults. NVS erased.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reboot (requires auth) ────────────────────────────────────
    let state_rb = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rb.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        // Clear session so the login page is shown after reconnect
        *state_rb.session.lock().unwrap() = None;
        request.into_ok_response()?.write_all(b"Rebooting...")?;
        // Use Builder with explicit stack size — bare std::thread::spawn can
        // silently fail on ESP-IDF when default stack is too small.
        let spawned = std::thread::Builder::new()
            .name("reboot".into())
            .stack_size(2048)
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe { esp_restart(); }
            });
        if spawned.is_err() {
            // Fallback: reboot directly if thread spawn failed
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe { esp_restart(); }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET/POST /api/tone — audio test tone frequency / on-off (requires auth) ──
    fn tone_json() -> String {
        format!(r#"{{"running":{},"hz":{},"on":{},"min":{},"max":{},"voice":{},"volume":{},"notify_eew":{},"notify_new":{}}}"#,
            crate::audio::running(), crate::audio::freq_hz(), crate::audio::enabled(),
            crate::audio::FREQ_MIN_HZ, crate::audio::FREQ_MAX_HZ, crate::voice::available(),
            crate::audio::volume(), crate::voice::notify_eew(), crate::voice::notify_new())
    }
    let state_tg = state.clone();
    server.fn_handler("/api/tone", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tg.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?.write_all(tone_json().as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;
    let state_tp = state.clone();
    server.fn_handler("/api/tone", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tp.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(256);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let form = parse_form(&String::from_utf8_lossy(&buf));
        if let Some(hz) = form.get("hz").and_then(|v| v.trim().parse::<u32>().ok()) {
            crate::audio::set_freq_hz(hz);
        }
        if let Some(on) = form.get("on") {
            crate::audio::set_enabled(on == "true");
        }
        if let Some(kind) = form.get("speak") {
            crate::voice::speak_sample(kind);
        }
        // Announcement switches: applied at once and stored in NVS.
        for (key, is_eew) in [("notify_eew", true), ("notify_new", false)] {
            if let Some(v) = form.get(key) {
                let on = v == "true";
                if is_eew { crate::voice::set_notify_eew(on); } else { crate::voice::set_notify_new(on); }
                let mut cfg = state_tp.config.lock().unwrap();
                if is_eew { cfg.voice_eew = on.to_string(); } else { cfg.voice_new = on.to_string(); }
                nvs_write_all(&cfg);
            }
        }
        // Voice volume (percent): applied at once; `save=1` also stores it in NVS.
        if let Some(vol) = form.get("volume").and_then(|v| v.trim().parse::<u32>().ok()) {
            crate::audio::set_volume(vol);
            if form.get("save").map(String::as_str) == Some("1") {
                let mut cfg = state_tp.config.lock().unwrap();
                cfg.audio_volume = vol.min(100).to_string();
                nvs_write_all(&cfg);
            }
        }
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?.write_all(tone_json().as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/boot_log — boot history from NVS (requires auth) ───────────
    let state_bl = state.clone();
    server.fn_handler("/api/boot_log", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_bl.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let log = crate::boot_log::read_log();
        let entries: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
        let json_entries = entries.iter()
            .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!("{{\"entries\":[{}]}}", json_entries);
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/mapinfo — map metadata (requires auth) ─────────────
    let state_mi = state.clone();
    server.fn_handler("/api/mapinfo", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_mi.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        // The view the device is showing, or its default overview before the first fetch.
        let (clat, clon) = {
            let (la, lo) = crate::map_display::get_map_center();
            if la.is_nan() || lo.is_nan() {
                let (dl, dn, _) = crate::map_display::DEFAULT_VIEW;
                (dl, dn)
            } else { (la, lo) }
        };
        let zoom    = crate::map_display::get_zoom();
        let has_map = crate::map_display::has_map();
        let ver     = crate::map_display::version();
        let json = format!(
            r#"{{"has_map":{hm},"version":{ver},"center_lat":{clat:.6},"center_lon":{clon:.6},"center_valid":true,"zoom":{zoom}}}"#,
            hm = has_map, ver = ver, clat = clat, clon = clon, zoom = zoom,
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/map/pixels — raw RGB565 image data (requires auth) ──────────
    let state_px = state.clone();
    server.fn_handler("/api/map/pixels", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_px.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let mut bytes: Vec<u8> = Vec::new();
        crate::map_display::with_pixels(|px| {
            bytes.reserve(px.len() * 2);
            for &v in px {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        });
        if bytes.is_empty() {
            request.into_status_response(204)?.write_all(b"")?;
        } else {
            let headers = [("Content-Type", "application/octet-stream")];
            let mut resp = request.into_response(200, Some("OK"), &headers)?;
            // Send in 4 KB chunks to avoid ENOBUFS on the TCP send buffer
            for chunk in bytes.chunks(4096) {
                resp.write_all(chunk)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/mapkey — Google Maps API key + map type for browser use ──────
    let state_mk = state.clone();
    server.fn_handler("/api/mapkey", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_mk.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let api_key  = crate::map_display::api_key();
        let map_type = state_mk.config.lock().unwrap().map_type.clone();
        let json = format!("{{{:?}:{:?},{:?}:{:?}}}", "api_key", api_key, "map_type", map_type);
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /map — map view page (requires auth) ──────────────────────────────
    let state_map = state.clone();
    server.fn_handler("/map", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_map.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_map_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/zoomin — zoom in one level (requires auth) ─────────────
    let state_zi = state.clone();
    server.fn_handler("/api/map/zoomin", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_zi.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        crate::map_display::zoom_in();
        let zoom = crate::map_display::get_zoom();
        let key = "zoom";
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{}}}", key, zoom);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/zoomout — zoom out one level (requires auth) ───────────
    let state_zo = state.clone();
    server.fn_handler("/api/map/zoomout", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_zo.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        crate::map_display::zoom_out();
        let zoom = crate::map_display::get_zoom();
        let key = "zoom";
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{}}}", key, zoom);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/toggletype — toggle satellite/roadmap (requires auth) ──
    let state_tt = state.clone();
    server.fn_handler("/api/map/toggletype", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tt.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let is_sat = crate::map_display::toggle_map_type();
        crate::map_display::reset_last_pos();
        let map_type = if is_sat { "satellite" } else { "roadmap" };
        // Persist to NVS and config mutex
        {
            let mut cfg = state_tt.config.lock().unwrap();
            cfg.map_type = map_type.to_string();
            nvs_write_all(&cfg);
        }
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{:?}}}", "map_type", map_type);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/quakes — recent earthquakes from P2P地震情報 (requires auth) ─
    let state_qk = state.clone();
    server.fn_handler("/api/quakes", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_qk.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let items: Vec<String> = crate::quake::get().iter().map(|q| format!(
            r#"{{"time":{time},"name":{name},"lat":{lat:.3},"lon":{lon:.3},"depth":{depth},"mag":{mag:.1},"scale":{scale},"intensity":{inten},"tsunami":{tsu}}}"#,
            time  = serde_json::to_string(&q.time).unwrap_or_default(),
            name  = serde_json::to_string(&q.name).unwrap_or_default(),
            lat = q.lat, lon = q.lon, depth = q.depth, mag = q.mag, scale = q.scale,
            inten = serde_json::to_string(q.intensity_jp()).unwrap_or_default(),
            tsu   = serde_json::to_string(&q.tsunami).unwrap_or_default(),
        )).collect();
        let eew = match crate::quake::eew() {
            Some(e) => {
                // Map framing of the epicenter (same as the TFT)
                let (vl, vo, vz) = crate::quake::eew_view(&e);
                let view = if vl.is_nan() { "null".to_string() } else { format!(
                    r#"{{"lat":{:.4},"lon":{:.4},"zoom":{}}}"#, vl, vo, vz) };
                format!(
                r#"{{"time":{t},"name":{n},"mag":{m:.1},"depth":{d},"intensity":{i},"areas":{a},"view":{v}}}"#,
                t = serde_json::to_string(&e.time).unwrap_or_default(),
                n = serde_json::to_string(&e.name).unwrap_or_default(),
                m = e.mag, d = e.depth,
                i = serde_json::to_string(e.intensity_jp()).unwrap_or_default(),
                a = serde_json::to_string(&e.areas).unwrap_or_default(),
                v = view)
            }
            None => "null".to_string(),
        };
        let json = format!(r#"{{"enabled":{},"ready":{},"eew":{},"quakes":[{}]}}"#,
            crate::quake::enabled(), crate::quake::last_ok_secs() != 0, eew, items.join(","));
        let headers = [("Content-Type", "application/json; charset=utf-8"), ("Cache-Control", "no-store")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    info!("HTTP config server started on port 80");
    Ok(server)
}

// ─── Map View HTML page ───────────────────────────────────────────────────────

fn build_map_html() -> String {
r##"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Map View</title>
<style>*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;display:flex;flex-direction:column;min-height:100vh}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0;flex-wrap:wrap}
.tb h1{font-size:1em;color:#93c5fd;margin-right:auto}
.bb{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.bdg{padding:2px 10px;border-radius:20px;font-size:.78em;font-weight:700}
.ct{display:flex;flex-direction:column;align-items:center;padding:12px;gap:8px;flex:1}
.mw{position:relative;display:block;line-height:0;max-width:640px;width:100%;min-height:240px;background:#0a1628;border-radius:6px;overflow:hidden}
.mw img{display:block;width:100%;height:auto}
#ov{position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none}
#nm{position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);color:#64748b;font-size:.9em;text-align:center;line-height:1.6;pointer-events:none}
.il{font-size:.78em;color:#64748b;text-align:center;padding:0 8px}
.zb{background:#0f172a;border:1px solid #2d4a6a;color:#e2e8f0;padding:5px 14px;border-radius:6px;font-size:1.1em;cursor:pointer;font-weight:700;line-height:1}
.zb:hover{background:#1e3a5f;border-color:#60a5fa}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0;flex-wrap:wrap}
.zb.sat{border-color:#f59e0b;color:#fcd34d}
.zb.road{border-color:#6ee7b7;color:#6ee7b7}
</style></head><body>
<div class="tb">
  <h1>&#x1F4CD; Map View</h1>
  <button class="zb" onclick="doZoom(-1)">&#x2212;</button>
  <span id="fix" class="bdg" style="background:#1e3a5f;color:#93c5fd">Zoom --</span>
  <button class="zb" onclick="doZoom(1)">+</button>
  <button class="zb" id="tbt" onclick="doToggleType()">&#x1F6F0; Sat</button>
  <a href="/map" id="back" class="bb" style="display:none">&#x1F4CD; Overview</a>
  <a href="/" class="bb">&#x2190; Config</a>
</div>
<div class="ct">
  <div class="mw">
    <img id="mi" alt="">
    <canvas id="ov" width="640" height="480"></canvas>
    <div id="nm">Initializing...</div>
  </div>
  <div id="il" class="il">Loading...</div>
</div>
<script>
const MAP_W=640,MAP_H=480;
let apiKey='',mapType='satellite',mi={},lastUrl='',mapCenter=null;
// Optional focus (?lat=&lon=&zoom=): view an arbitrary point, e.g. an earthquake
// epicenter. Browser-side only — the device's own map/zoom state is untouched.
const qp=new URLSearchParams(location.search);
let focus=null;
if(qp.has('lat')&&qp.has('lon')){
  const fl=parseFloat(qp.get('lat')),fo=parseFloat(qp.get('lon'));
  if(isFinite(fl)&&isFinite(fo))focus={lat:fl,lon:fo,zoom:Math.min(16,Math.max(2,parseInt(qp.get('zoom'))||8))};
}
let quakes=[],eew=null,qTick=0,topKey=null,newQuake=null,newUntil=0;
const qKey=q=>q.time+'|'+q.lat+'|'+q.lon;
const img=document.getElementById('mi');
const oc=document.getElementById('ov'),octx=oc.getContext('2d');
const nm=document.getElementById('nm');

async function init(){
  try{
    const d=await fetch('/api/mapkey').then(r=>r.json());
    apiKey=d.api_key||'';mapType=d.map_type||'satellite';
    updateTypeBtn();
  }catch(e){}
  refresh();setInterval(refresh,3000);
}

function updateTypeBtn(){
  const b=document.getElementById('tbt');
  if(mapType==='satellite'){b.textContent='\u{1F6F0} Sat';b.className='zb sat';}
  else{b.textContent='\u{1F5FA} Road';b.className='zb road';}
}

// Epicenter view (?lat=&lon=&zoom=) or the view the device itself is showing.
function getMapCenter(){
  if(focus)return{lat:focus.lat,lon:focus.lon,zoom:focus.zoom};
  if(mi.center_valid)return{lat:mi.center_lat,lon:mi.center_lon,zoom:mi.zoom};
  return null;
}

function mapUrl(){
  const c=getMapCenter();
  if(!apiKey||!c)return null;
  return'https://maps.googleapis.com/maps/api/staticmap'+
    '?center='+c.lat.toFixed(6)+','+c.lon.toFixed(6)+
    '&zoom='+c.zoom+'&size='+MAP_W+'x'+MAP_H+
    '&format=jpg&maptype='+mapType+'&key='+apiKey;
}

function toPx(lat,lon){
  if(!mapCenter)return null;
  const w=256*Math.pow(2,mapCenter.zoom);
  const lr=lat*Math.PI/180,cr=mapCenter.lat*Math.PI/180;
  const mp=Math.log(Math.tan(lr/2+Math.PI/4));
  const cm=Math.log(Math.tan(cr/2+Math.PI/4));
  const dx=(lon-mapCenter.lon)*w/360;
  const dy=-(mp-cm)*w/(2*Math.PI);
  const px=Math.round(MAP_W/2+dx),py=Math.round(MAP_H/2+dy);
  if(px<-20||px>MAP_W+20||py<-20||py>MAP_H+20)return null;
  return[px,py];
}

function drawOv(){
  octx.clearRect(0,0,oc.width,oc.height);
  // Earthquake epicenters (P2P地震情報) and an active EEW
  if(mapCenter){
    const SC={10:'#fde047',20:'#fde047',30:'#fb923c',40:'#fb923c',45:'#ef4444',50:'#ef4444',55:'#e879f9',60:'#e879f9',70:'#e879f9'};
    octx.font='bold 11px monospace';
    for(const q of quakes){
      const p=toPx(q.lat,q.lon);
      if(!p)continue;
      const m=q.mag>0?q.mag:1;
      const r=Math.min(14,Math.max(4,2+m*1.6));
      const col=SC[q.scale]||(m>=5?'#ef4444':m>=3?'#fb923c':'#fde047');
      octx.lineWidth=2.5;octx.strokeStyle=col;
      octx.beginPath();octx.arc(p[0],p[1],r,0,2*Math.PI);octx.stroke();
      if(focus&&Math.abs(q.lat-focus.lat)<0.0005&&Math.abs(q.lon-focus.lon)<0.0005){
        octx.strokeStyle='#ffffff';octx.lineWidth=1.5;
        octx.beginPath();octx.arc(p[0],p[1],r+4,0,2*Math.PI);octx.stroke();
      }
      if(newQuake&&Date.now()<newUntil&&qKey(q)===qKey(newQuake)){
        octx.strokeStyle='#ffffff';octx.lineWidth=1.5;
        octx.beginPath();octx.arc(p[0],p[1],r+8,0,2*Math.PI);octx.stroke();
      }
      const lab=(q.mag>0?'M'+q.mag.toFixed(1):'')+(q.scale>0?' I'+q.intensity:'');
      if(lab){
        octx.lineWidth=3;octx.strokeStyle='#000000';octx.strokeText(lab,p[0]+r+3,p[1]+4);
        octx.fillStyle='#ffffff';octx.fillText(lab,p[0]+r+3,p[1]+4);
      }
    }
  }
}

img.onload=function(){
  nm.style.display='none';
  drawOv();
};
img.onerror=function(){
  nm.textContent='Map image load failed\n(check API key or network)';
  nm.style.display='block';
};

async function loadQuakes(){
  try{
    const q=await fetch('/api/quakes').then(r=>r.json());
    quakes=q.quakes||[];eew=q.eew||null;
    // A quake we haven't seen before: highlight it on the map for a minute
    // (not on the first load, which is just the existing list).
    if(quakes.length){
      const k=qKey(quakes[0]);
      if(topKey!==null&&k!==topKey){newQuake=quakes[0];newUntil=Date.now()+60000;}
      topKey=k;
    }
    drawOv();
  }catch(e){}
}

async function refresh(){
  if(qTick++%4===0)loadQuakes();
  try{
    const d=await fetch('/api/mapinfo').then(r=>r.json());
    mi=d;
    const url=mapUrl();
    if(url){
      if(url!==lastUrl){
        mapCenter=getMapCenter();
        img.src=url;lastUrl=url;
      }
      drawOv(); // always redraw overlay (quakes may have changed)
    }else{
      nm.style.display='block';
      nm.textContent=!apiKey?'Maps API key not set (Config page > Map Settings)':'Fetching map...';
    }
    const fb=document.getElementById('fix');
    fb.textContent=d.center_valid?'Zoom '+d.zoom:'No Map';
    const nq=(newQuake&&Date.now()<newUntil)?
      'NEW: '+newQuake.time.slice(5,16)+' M'+(newQuake.mag>0?newQuake.mag.toFixed(1):'?')+' I'+newQuake.intensity+' '+newQuake.name+'  |  ':'';
    document.getElementById('il').textContent=nq+(focus?
      'Epicenter view: '+focus.lat.toFixed(3)+'N  '+focus.lon.toFixed(3)+'E  |  Zoom: '+focus.zoom+
        '  |  Quakes: '+quakes.length+(eew?'  |  EEW ACTIVE':'')+'  |  ':
      'Overview  |  Zoom: '+d.zoom+'  |  Quakes: '+quakes.length+(eew?'  |  EEW ACTIVE':''));
    if(focus){
      const a=document.getElementById('back');
      if(a)a.style.display='inline-flex';
      fb.textContent='Zoom '+focus.zoom;
    }
  }catch(e){}
}

async function doZoom(dir){
  if(focus){
    focus.zoom=Math.min(16,Math.max(2,focus.zoom+dir));
    lastUrl='';document.getElementById('fix').textContent='Zoom '+focus.zoom;
    refresh();return;
  }
  try{
    const r=await fetch(dir>0?'/api/map/zoomin':'/api/map/zoomout',{method:'POST'});
    const d=await r.json();
    mi.zoom=d.zoom;lastUrl='';
    document.getElementById('fix').textContent='Zoom '+d.zoom;
    refresh();
  }catch(e){}
}

async function doToggleType(){
  try{
    const r=await fetch('/api/map/toggletype',{method:'POST'});
    const d=await r.json();
    mapType=d.map_type;lastUrl='';
    updateTypeBtn();
    refresh();
  }catch(e){}
}

init();
</script></body></html>"##.to_string()
}

/// Firmware version (Cargo.toml `version`) and build date/time (set by build.rs).
pub const FW_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FW_BUILD: &str = env!("BUILD_TIMESTAMP");

/// Heap usage snapshot: (free, min-ever-free) in bytes for internal RAM and PSRAM.
#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub int_free: u32,
    pub int_min:  u32,
    pub psram_free: u32,
    pub psram_min:  u32,
}

pub fn mem_info() -> MemInfo {
    use esp_idf_sys::{heap_caps_get_free_size, heap_caps_get_minimum_free_size,
                      MALLOC_CAP_INTERNAL, MALLOC_CAP_SPIRAM};
    unsafe {
        MemInfo {
            int_free:   heap_caps_get_free_size(MALLOC_CAP_INTERNAL) as u32,
            int_min:    heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) as u32,
            psram_free: heap_caps_get_free_size(MALLOC_CAP_SPIRAM) as u32,
            psram_min:  heap_caps_get_minimum_free_size(MALLOC_CAP_SPIRAM) as u32,
        }
    }
}
