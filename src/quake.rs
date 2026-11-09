// Recent earthquakes via the P2P地震情報 JSON API v2 (https://www.p2pquake.net)
// EEW (code 556) arrives over the P2P WebSocket (wss://.../v2/ws), which pushes
// every event as it is registered. Code-551 pushes are merged into the
// recent-earthquake list directly; REST loads the initial list and is polled
// every POLL_SECS only while the WebSocket is down (EEW likewise).
// The TFT quake page, the MAP overlay and the Web UI read the cached state.
// SPDX-License-Identifier: MIT

use log::*;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use esp_idf_svc::ws::client::{EspWebSocketClient, EspWebSocketClientConfig, WebSocketEventType};
use esp_idf_sys::{heap_caps_get_free_size, MALLOC_CAP_INTERNAL};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use embedded_svc::http::client::Client;
use embedded_svc::http::Method;
use embedded_svc::io::Read;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use serde::Deserialize;
use crate::health;

/// How many recent events are kept / shown.
pub const QUAKE_MAX: usize = 20;
const POLL_SECS: u64 = 60;
const EEW_POLL_SECS: u64 = 10;
/// Safety re-sync of the list while the WebSocket is up (pushes normally keep it current).
const QUAKE_RESYNC_SECS: u64 = 30 * 60;
/// EEW older than this (by its issue time) is history, not an alert.
const EEW_FRESH_SECS: i64 = 180;
/// How long an EEW overlay / Web UI banner stays up.
const EEW_SHOW_SECS: u64 = 120;
/// Skip a poll when internal RAM is this low: TLS setup would starve the
/// WiFi/lwIP stack (internal heap is the scarce resource on this board).
const MIN_INTERNAL_FREE: u32 = 36 * 1024;
/// Upper bound on one response body (QUAKE_MAX events incl. per-station points;
/// lives in PSRAM). One DetailScale of a large event is ~80 KB (M5.7 of
/// 2026-11-08 / M6.1 of 2026-11-06), and the history holds several reports of
/// it, so the old 320 KB limit made every re-sync fail for hours afterwards.
const BODY_MAX: usize = 1536 * 1024;

// `--features quake-sandbox` points at the P2P sandbox (replays old 551/556
// data, e.g. a 2023 Hokkaido EEW) and skips the EEW freshness check, so the
// whole alert path can be tested without waiting for a real earthquake.
#[cfg(feature = "quake-sandbox")]
macro_rules! api { ($p:literal) => { concat!("https://api-v2-sandbox.p2pquake.net", $p) }; }
#[cfg(not(feature = "quake-sandbox"))]
macro_rules! api { ($p:literal) => { concat!("https://api.p2pquake.net", $p) }; }
const URL_QUAKE: &str = api!("/v2/history?codes=551&limit=20");
#[cfg(feature = "quake-sandbox")]
const URL_WS: &str = "wss://api-realtime-sandbox.p2pquake.net/v2/ws";
#[cfg(not(feature = "quake-sandbox"))]
const URL_WS: &str = "wss://api.p2pquake.net/v2/ws";
/// WebSocket receive buffer. >4 KB lands in PSRAM (SPIRAM_MALLOC_ALWAYSINTERNAL),
/// and a large EEW (100+ areas) is ~16 KB, so keep it comfortably above that.
const WS_BUF: usize = 32 * 1024;
/// Max size of one reassembled WebSocket message we keep (in PSRAM). A
/// DetailScale for a large event lists ~1000 stations: the M6.1 of 2026-11-06
/// was 77 KB, which the old 64 KB limit silently dropped.
const WS_MSG_MAX: usize = 256 * 1024;
/// Free internal RAM needed before (re)creating the WebSocket client.
const WS_MIN_INTERNAL_FREE: u32 = 44 * 1024;
/// Restart the chip when no WebSocket has been connected for this long.
const WS_MISSING_RESTART_SECS: u64 = 30 * 60;
const URL_EEW:   &str = api!("/v2/history?codes=556&limit=1");

#[derive(Clone, Debug)]
pub struct Quake {
    pub time:     String, // "2026/10/10 10:20:00" (JST)
    pub name:     String, // Japanese hypocenter name (Web UI only; TFT has no kanji font)
    pub lat:      f64,
    pub lon:      f64,
    pub depth:    i32,    // km; 0 = very shallow, -1 = unknown
    pub mag:      f32,    // -1 = unknown
    pub scale:    i32,    // P2P maxScale code; -1 = unknown
    pub tsunami:  String, // domesticTsunami: None/Unknown/Checking/NonEffective/Watch/Warning
}

impl Quake {
    /// Max seismic intensity as short ASCII ("1".."4", "5-", "5+", "6-", "6+", "7").
    pub fn intensity(&self) -> &'static str { scale_str(self.scale, false) }
    /// Same, in the JMA notation for the Web UI ("5弱" etc.).
    pub fn intensity_jp(&self) -> &'static str { scale_str(self.scale, true) }
}

fn scale_str(scale: i32, jp: bool) -> &'static str {
    match (scale, jp) {
        (10, _) => "1", (20, _) => "2", (30, _) => "3", (40, _) => "4",
        (45, false) => "5-", (45, true) => "5弱",
        (50, false) => "5+", (50, true) => "5強",
        (55, false) => "6-", (55, true) => "6弱",
        (60, false) => "6+", (60, true) => "6強",
        (70, _) => "7",
        _ => "?",
    }
}

static QUAKES: LazyLock<Mutex<Vec<Quake>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Bumped whenever the list content changes (redraw trigger).
static VERSION: AtomicU32 = AtomicU32::new(0);
/// Highlighted row on the quake page.
static SEL: AtomicUsize = AtomicUsize::new(0);
/// Epicenter the MAP page is currently centred on (set from the quake page).
static FOCUS: Mutex<Option<(f64, f64)>> = Mutex::new(None);
/// Unix seconds of the last successful fetch (0 = never).
static LAST_OK_SECS: AtomicU32 = AtomicU32::new(0);

/// Whether the feed is switched on (`quake_enable` in cfg.toml). Off by default:
/// the feed holds a TLS connection and ~30 KB of scarce internal RAM.
static ENABLED: AtomicBool = AtomicBool::new(false);
pub fn set_enabled(on: bool) { ENABLED.store(on, Ordering::Relaxed); }
pub fn enabled() -> bool { ENABLED.load(Ordering::Relaxed) }

pub fn get() -> Vec<Quake> { QUAKES.lock().unwrap().clone() }
pub fn version() -> u32 { VERSION.load(Ordering::Relaxed) }
pub fn last_ok_secs() -> u32 { LAST_OK_SECS.load(Ordering::Relaxed) }

pub fn selection() -> usize { SEL.load(Ordering::Relaxed) }

/// Move the highlight by `delta` rows, clamped to the list bounds.
pub fn move_sel(delta: i32) {
    let len = QUAKES.lock().unwrap().len();
    let cur = SEL.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).clamp(0, len.saturating_sub(1) as i32);
    SEL.store(next as usize, Ordering::Relaxed);
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Centre the MAP page on the highlighted epicenter. Returns false if none.
pub fn focus_selected() -> bool {
    let q = QUAKES.lock().unwrap().get(SEL.load(Ordering::Relaxed)).cloned();
    match q {
        Some(q) => {
            AUTO_FOCUS.store(false, Ordering::Relaxed);
            *FOCUS.lock().unwrap() = Some((q.lat, q.lon));
            // M7-class quakes need a regional view; keep small ones closer.
            crate::map_display::set_zoom(fit_view(q.lat, q.lon, q.mag).2);
            crate::map_display::reset_last_pos();
            true
        }
        None => false,
    }
}

pub fn focus() -> Option<(f64, f64)> { *FOCUS.lock().unwrap() }

pub fn clear_focus() {
    AUTO_FOCUS.store(false, Ordering::Relaxed);
    clear_new_quake();
    let mut f = FOCUS.lock().unwrap();
    if f.is_some() {
        *f = None;
        // Back to the Japan overview.
        crate::map_display::set_zoom(crate::map_display::DEFAULT_VIEW.2);
        crate::map_display::reset_last_pos();
    }
}

/// True while the MAP focus was set by an EEW (not picked on the quake page).
static AUTO_FOCUS: AtomicBool = AtomicBool::new(false);

/// Map view for an EEW: centred on the forecast epicenter at a regional zoom
/// (wider for a big event). Returns (centre lat, centre lon, zoom); the centre
/// is NaN when the report carries no coordinates.
pub fn eew_view(e: &Eew) -> (f64, f64, u8) {
    fit_view(e.lat, e.lon, e.mag)
}

/// Same framing for any epicenter (`lat` NaN = unknown).
pub fn fit_view(lat: f64, lon: f64, mag: f32) -> (f64, f64, u8) {
    (lat, lon, if mag >= 6.0 { 6 } else { 8 })
}

// ─── Newly received earthquake, shown on the MAP page ────────────────────────

/// How long the "new earthquake" view stays up.
const NEWQ_SHOW_SECS: u64 = 60;

struct NewQuake { q: Quake, expires: u64 }
static NEWQ: Mutex<Option<NewQuake>> = Mutex::new(None);
static NEWQ_VERSION: AtomicU32 = AtomicU32::new(0);

/// The new earthquake being highlighted on the MAP page, if any.
pub fn new_quake() -> Option<Quake> {
    let g = NEWQ.lock().unwrap();
    let n = g.as_ref()?;
    if now_secs() < n.expires { Some(n.q.clone()) } else { None }
}
pub fn newq_version() -> u32 { NEWQ_VERSION.load(Ordering::Relaxed) }

fn clear_new_quake() {
    if NEWQ.lock().unwrap().take().is_some() {
        NEWQ_VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// A new earthquake just arrived. If the TFT is on the MAP page (and no EEW
/// owns the map), frame its epicenter and show its details.
fn announce_new(q: &Quake) {
    if crate::display::current_page() != crate::display::PAGE_MAP { return; }
    if EEW.lock().unwrap().is_some() { return; }
    let (clat, clon, zoom) = fit_view(q.lat, q.lon, q.mag);
    if clat.is_nan() { return; }
    *FOCUS.lock().unwrap() = Some((clat, clon));
    AUTO_FOCUS.store(true, Ordering::Relaxed);
    crate::map_display::set_zoom(zoom);
    crate::map_display::reset_last_pos();
    *NEWQ.lock().unwrap() = Some(NewQuake { q: q.clone(), expires: now_secs() + NEWQ_SHOW_SECS });
    NEWQ_VERSION.fetch_add(1, Ordering::Relaxed);
    info!("[QUAKE] showing new earthquake on MAP: M{:.1} {}", q.mag, q.name);
}

/// Switch the TFT to the MAP page, framing the EEW epicenter.
fn focus_eew(e: &Eew) {
    let (clat, clon, zoom) = eew_view(e);
    if clat.is_nan() { return; }
    *FOCUS.lock().unwrap() = Some((clat, clon));
    AUTO_FOCUS.store(true, Ordering::Relaxed);
    crate::map_display::set_zoom(zoom);
    crate::map_display::reset_last_pos();
    crate::display::set_page(crate::display::PAGE_MAP);
}

// ─── Active EEW (緊急地震速報・警報) ──────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Eew {
    pub time:      String,  // issue time (JST)
    pub name:      String,  // hypocenter name (Japanese)
    pub lat:       f64,     // NaN when the report carries no coordinates
    pub lon:       f64,
    pub depth:     i32,
    pub mag:       f32,
    pub max_scale: i32,     // largest forecast intensity code over the areas, -1 unknown
    pub areas:     String,  // forecast areas (Japanese), comma separated, truncated
    expires:       u64,     // unix secs
}

impl Eew {
    pub fn intensity(&self) -> &'static str { scale_str(self.max_scale, false) }
    pub fn intensity_jp(&self) -> &'static str { scale_str(self.max_scale, true) }
}

static EEW: Mutex<Option<Eew>> = Mutex::new(None);
static EEW_VERSION: AtomicU32 = AtomicU32::new(0);

/// The EEW to show, if one is active (not cancelled / expired).
pub fn eew() -> Option<Eew> {
    let e = EEW.lock().unwrap().clone()?;
    if now_secs() < e.expires { Some(e) } else { None }
}
/// True while the WebSocket is connected (for diagnostics).
pub fn ws_connected() -> bool { WS_CONNECTED.load(Ordering::Relaxed) }
pub fn eew_version() -> u32 { EEW_VERSION.load(Ordering::Relaxed) }

// ─── P2P JSON types (only the fields we use; the rest, e.g. `points`, is skipped) ──

#[derive(Deserialize)]
struct Hypocenter {
    name:      Option<String>,
    latitude:  Option<f64>,
    longitude: Option<f64>,
    depth:     Option<i32>,
    magnitude: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Earthquake {
    time:             Option<String>,
    hypocenter:       Option<Hypocenter>,
    max_scale:        Option<i32>,
    domestic_tsunami: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    earthquake: Option<Earthquake>,
}

#[derive(Deserialize)]
struct EewEq {
    hypocenter: Option<Hypocenter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EewArea {
    name:       Option<String>,
    scale_from: Option<i32>,
}

#[derive(Deserialize)]
struct EewItem {
    id:         Option<String>,
    time:       Option<String>,
    test:       Option<bool>,
    cancelled:  Option<bool>,
    earthquake: Option<EewEq>,
    areas:      Option<Vec<EewArea>>,
}

fn item_to_quake(it: Item) -> Option<Quake> {
    let eq = it.earthquake?;
    let h = eq.hypocenter?;
    // Intensity-only reports carry no epicenter (lat/lon missing or -200).
    let (lat, lon) = (h.latitude?, h.longitude?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) { return None; }
    Some(Quake {
        time:    eq.time.unwrap_or_default(),
        name:    h.name.unwrap_or_default(),
        lat, lon,
        depth:   h.depth.unwrap_or(-1),
        mag:     h.magnitude.map(|m| m as f32).unwrap_or(-1.0),
        scale:   eq.max_scale.unwrap_or(-1),
        tsunami: eq.domestic_tsunami.unwrap_or_default(),
    })
}

/// REST response: array of events, newest first.
fn parse(body: &[u8]) -> Option<Vec<Quake>> {
    let items: Vec<Item> = serde_json::from_slice(body)
        .map_err(|e| info!("[QUAKE] json: {:?}", e))
        .ok()?;
    Some(items.into_iter().filter_map(item_to_quake).take(QUAKE_MAX).collect())
}

/// Merge one event pushed over the WebSocket into the list (newest first).
/// A re-issue of the same event (same origin time + epicenter) replaces it.
fn merge_pushed(body: &[u8]) {
    let q = match serde_json::from_slice::<Item>(body) {
        Ok(it) => match item_to_quake(it) { Some(q) => q, None => return },
        Err(e) => { info!("[QUAKE] pushed json: {:?}", e); return; }
    };
    let mut cur = QUAKES.lock().unwrap();
    let is_new = !cur.iter().any(|o| o.time == q.time && o.lat == q.lat && o.lon == q.lon);
    cur.retain(|o| !(o.time == q.time && o.lat == q.lat && o.lon == q.lon));
    info!("[QUAKE] pushed: {} M{:.1} {}", q.time, q.mag, q.name);
    if PROMPT.lock().unwrap().as_ref().map(|p| p.time == q.time).unwrap_or(false) {
        clear_prompt("epicenter report arrived");
    }
    cur.insert(0, q.clone());
    cur.truncate(QUAKE_MAX);
    VERSION.fetch_add(1, Ordering::Relaxed);
    LAST_OK_SECS.store(now_secs() as u32, Ordering::Relaxed);
    drop(cur);
    // A re-issue of a known event (correction) only updates the list.
    if is_new {
        crate::voice::speak_new(&q);
        announce_new(&q);
    }
}

/// P2P timestamps ("2026/10/10 10:23:10.944") are JST; returns unix seconds.
fn jst_to_unix(t: &str) -> Option<i64> {
    let t = t.get(..19)?;
    let dt = chrono::NaiveDateTime::parse_from_str(t, "%Y/%m/%d %H:%M:%S").ok()?;
    Some(dt.and_utc().timestamp() - 9 * 3600)
}

/// Handle the newest code-556 entry. `last_id` is the last one already acted on.
fn handle_eew(body: &[u8], last_id: &mut String) {
    // REST returns an array, the WebSocket a single object.
    let it = match serde_json::from_slice::<Vec<EewItem>>(body) {
        Ok(items) => match items.into_iter().next() { Some(it) => it, None => return },
        Err(_) => match serde_json::from_slice::<EewItem>(body) {
            Ok(it) => it,
            Err(e) => { info!("[EEW] json parse failed: {:?}", e); return; }
        },
    };
    let id = it.id.clone().unwrap_or_default();
    if id.is_empty() || id == *last_id { return; }
    *last_id = id.clone();

    if it.test.unwrap_or(false) { info!("[EEW] test message ignored"); return; }
    if it.cancelled.unwrap_or(false) {
        info!("[EEW] cancelled");
        clear_eew();
        return;
    }
    let time = it.time.clone().unwrap_or_default();
    let fresh = cfg!(feature = "quake-sandbox") || jst_to_unix(&time)
        .map(|t| (now_secs() as i64 - t).abs() <= EEW_FRESH_SECS)
        .unwrap_or(false);
    if !fresh { info!("[EEW] stale report ({}) ignored", time); return; }

    let h = it.earthquake.and_then(|e| e.hypocenter);
    let areas = it.areas.unwrap_or_default();
    let max_scale = areas.iter().filter_map(|a| a.scale_from).max().unwrap_or(-1);
    let mut names: Vec<String> = Vec::new();
    for a in &areas {
        if let Some(n) = &a.name { if !names.contains(n) { names.push(n.clone()); } }
    }
    let shown = names.len().min(8);
    let mut area_txt = names[..shown].join("、");
    if names.len() > shown { area_txt.push_str("…"); }

    let eew = Eew {
        time,
        name:  h.as_ref().and_then(|h| h.name.clone()).unwrap_or_default(),
        lat:   h.as_ref().and_then(|h| h.latitude).unwrap_or(f64::NAN),
        lon:   h.as_ref().and_then(|h| h.longitude).unwrap_or(f64::NAN),
        depth: h.as_ref().and_then(|h| h.depth).unwrap_or(-1),
        mag:   h.as_ref().and_then(|h| h.magnitude).map(|m| m as f32).unwrap_or(-1.0),
        max_scale,
        areas: area_txt,
        expires: now_secs() + EEW_SHOW_SECS,
    };
    warn!("[EEW] ALERT M{:.1} maxI={} ({})", eew.mag, eew.intensity(), eew.name);

    // Speak it: the first report of an event, and later reports only if the
    // forecast maximum intensity went up (an event sends several updates).
    let prev_scale = EEW.lock().unwrap().as_ref()
        .filter(|e| now_secs() < e.expires).map(|e| e.max_scale);
    if prev_scale.map(|p| eew.max_scale > p).unwrap_or(true) {
        crate::voice::speak_eew(&eew);
    }

    // Show it on the TFT map, framed on the forecast epicenter.
    focus_eew(&eew);
    *EEW.lock().unwrap() = Some(eew);
    EEW_VERSION.fetch_add(1, Ordering::Relaxed);
}

// ─── 震度速報 (ScalePrompt) ─────────────────────────────────────────────────────
//
// JMA's first report, ~1.5-2.5 min after the shaking: maximum intensity and the
// areas that felt it, but no epicenter yet (lat/lon -200) — so it does not go
// into the list. It is announced and shown as a banner until the report with
// the epicenter (Destination / DetailScale) for the same event arrives, which
// is announced as usual. (2026-11-09 13:52 M5.9: ScalePrompt 13:54:31, but the
// first announcement only came with the DetailScale at 13:59.)

/// A banner lasts at most this long if no epicenter report follows.
const PROMPT_SHOW_SECS: u64 = 15 * 60;
/// Ignore 震度速報 for events older than this (e.g. a delayed delivery).
const PROMPT_FRESH_SECS: i64 = 20 * 60;
/// How many max-intensity areas are spoken / listed.
pub const PROMPT_AREAS_MAX: usize = 4;

#[derive(Clone, Debug)]
pub struct Prompt {
    pub time:      String,      // origin time, JST "YYYY/MM/DD HH:MM:SS"
    pub max_scale: i32,
    pub areas:     Vec<String>, // areas at the maximum intensity, in report order
    pub n_areas:   usize,       // all areas in the report
    expires: u64,
}
impl Prompt {
    pub fn intensity(&self) -> &'static str { scale_str(self.max_scale, false) }
    pub fn intensity_jp(&self) -> &'static str { scale_str(self.max_scale, true) }
}
static PROMPT: Mutex<Option<Prompt>> = Mutex::new(None);

/// The 震度速報 waiting for its epicenter report, if any.
pub fn prompt() -> Option<Prompt> {
    let g = PROMPT.lock().unwrap();
    let p = g.as_ref()?;
    if now_secs() < p.expires { Some(p.clone()) } else { None }
}

fn clear_prompt(reason: &str) {
    if let Some(p) = PROMPT.lock().unwrap().take() {
        info!("[QUAKE] 震度速報 {} cleared ({})", p.time, reason);
        NEWQ_VERSION.fetch_add(1, Ordering::Relaxed); // redraw the screens
    }
}

#[derive(Deserialize)]
struct PromptPoint {
    addr:  Option<String>,
    scale: Option<i32>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptEq {
    time:             Option<String>,
    max_scale:        Option<i32>,
}
#[derive(Deserialize)]
struct PromptItem {
    earthquake: Option<PromptEq>,
    points:     Option<Vec<PromptPoint>>,
}

fn handle_prompt(body: &[u8]) {
    let it = match serde_json::from_slice::<PromptItem>(body) {
        Ok(it) => it,
        Err(e) => { info!("[QUAKE] 震度速報 json: {:?}", e); return; }
    };
    let Some(eq) = it.earthquake else { return };
    let time = eq.time.unwrap_or_default();
    let fresh = cfg!(feature = "quake-sandbox") || jst_to_unix(&time)
        .map(|t| (now_secs() as i64 - t).abs() <= PROMPT_FRESH_SECS)
        .unwrap_or(false);
    if !fresh { info!("[QUAKE] stale 震度速報 ({}) ignored", time); return; }
    // The epicenter report for this event already came: nothing to add.
    if QUAKES.lock().unwrap().iter().any(|q| q.time == time) { return; }

    let points = it.points.unwrap_or_default();
    let max_scale = eq.max_scale.filter(|&s| s > 0)
        .or_else(|| points.iter().filter_map(|p| p.scale).max())
        .unwrap_or(-1);
    let mut areas: Vec<String> = Vec::new();
    for p in &points {
        if p.scale == Some(max_scale) {
            if let Some(a) = &p.addr { if !areas.contains(a) { areas.push(a.clone()); } }
        }
    }
    let prompt = Prompt {
        time, max_scale, areas, n_areas: points.len(),
        expires: now_secs() + PROMPT_SHOW_SECS,
    };
    info!("[QUAKE] 震度速報: {} max {} at {} ({} areas)",
          prompt.time, prompt.intensity_jp(), prompt.areas.join("、"), prompt.n_areas);

    // A follow-up 震度速報 (more areas) only speaks again if the maximum went up.
    let prev = PROMPT.lock().unwrap().as_ref()
        .filter(|p| p.time == prompt.time && now_secs() < p.expires)
        .map(|p| p.max_scale);
    let speak = prev.map(|s| prompt.max_scale > s).unwrap_or(true);
    *PROMPT.lock().unwrap() = Some(prompt.clone());
    NEWQ_VERSION.fetch_add(1, Ordering::Relaxed); // wakes and redraws the TFT
    crate::display::wake();
    if speak { crate::voice::speak_prompt(&prompt); }
}

fn clear_eew() {
    *EEW.lock().unwrap() = None;
    EEW_VERSION.fetch_add(1, Ordering::Relaxed);
    // Hand the map back to the Japan overview (the page stays as is).
    if AUTO_FOCUS.load(Ordering::Relaxed) {
        clear_focus();
    }
}

// ─── HTTP ─────────────────────────────────────────────────────────────────────

type HttpClient = Client<EspHttpConnection>;

fn new_client() -> Option<HttpClient> {
    let config = Configuration {
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        use_global_ca_store: false,
        buffer_size:    Some(2048),
        buffer_size_tx: Some(512),
        ..Default::default()
    };
    EspHttpConnection::new(&config)
        .map_err(|e| info!("[QUAKE] connection: {:?}", e))
        .ok()
        .map(Client::wrap)
}

/// GET `url` over the shared kept-alive client (created on demand, dropped on
/// any error so the next call starts a fresh connection).
fn http_get(slot: &mut Option<HttpClient>, url: &str) -> Option<Vec<u8>> {
    if slot.is_none() { *slot = new_client(); }
    let client = slot.as_mut()?;
    let headers = [("User-Agent", "satellite-viewer/1.0"), ("Accept", "application/json")];

    let result = (|| {
        let request = client.request(Method::Get, url, &headers)
            .map_err(|e| info!("[QUAKE] request: {:?}", e)).ok()?;
        let mut response = request.submit()
            .map_err(|e| {
                // 0x7007 = ESP_ERR_HTTP_EAGAIN: no response within the timeout.
                let hint = if e.0.code().abs() == esp_idf_sys::ESP_ERR_HTTP_EAGAIN { " (timeout, no response)" } else { "" };
                info!("[QUAKE] submit: {:?}{}", e, hint)
            }).ok()?;
        if response.status() != 200 {
            info!("[QUAKE] HTTP {}", response.status());
            return None;
        }
        // Read to the end even when oversized, so the connection stays reusable.
        let mut body: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut chunk = [0u8; 1024];
        let mut too_big = false;
        loop {
            match Read::read(&mut response, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if body.len() + n > BODY_MAX { too_big = true; } else { body.extend_from_slice(&chunk[..n]); }
                }
                Err(_) => return None,
            }
        }
        if too_big { info!("[QUAKE] response too large, dropped"); return None; }
        Some(body)
    })();

    if result.is_none() { *slot = None; }
    result
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Smallest amount of this thread's stack that has ever been unused (bytes).
fn stack_free() -> u32 {
    unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) as u32 }
}

fn internal_free() -> u32 {
    unsafe { heap_caps_get_free_size(MALLOC_CAP_INTERNAL) as u32 }
}

// ─── WebSocket ────────────────────────────────────────────────────────────────

/// Bytes of the WebSocket message being reassembled, filled by `ws_raw_data`
/// (static so that no callback captures anything — see `drop_ws`).
static WS_RX: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// The message being received is over WS_MSG_MAX: drop the rest of it.
static WS_RX_SKIP: AtomicBool = AtomicBool::new(false);
static WS_CONNECTED: AtomicBool = AtomicBool::new(false);
/// True while a WebSocket client object exists (connected or reconnecting).
static WS_ALIVE: AtomicBool = AtomicBool::new(false);
/// Unix secs of the most recent map fetch start (the socket is not re-created
/// while the user is still zooming/panning: each fetch would just drop it again).
static LAST_MAP_FETCH_SECS: AtomicU32 = AtomicU32::new(0);
/// Quiet time after the last map fetch before the WebSocket comes back.
const WS_RESUME_AFTER_MAP_SECS: u32 = 12;
/// Asked by the map fetch worker: drop the WebSocket now to free internal RAM.
static WS_RELEASE: AtomicBool = AtomicBool::new(false);

/// Called by the map fetch worker before it starts a (TLS + JPEG) fetch.
///
/// The WebSocket's TLS session holds ~30 KB of internal RAM for as long as it
/// lives, and a map fetch needs ~45 KB on top of the ~50 KB that is otherwise
/// free — together they dipped the internal heap to 3 KB. So the WebSocket is
/// dropped for the duration of the fetch (blocks up to 3 s until it is gone)
/// and the quake thread re-creates it afterwards. An EEW issued meanwhile is
/// caught by the REST EEW check that runs whenever the socket is down.
pub fn release_ws_for_map() {
    LAST_MAP_FETCH_SECS.store(now_secs() as u32, Ordering::Relaxed);
    if !WS_ALIVE.load(Ordering::Relaxed) { return; }
    WS_RELEASE.store(true, Ordering::Relaxed);
    for _ in 0..60 {
        if !WS_ALIVE.load(Ordering::Relaxed) { return; }
        thread::sleep(Duration::from_millis(50));
    }
    warn!("[WS] release for map fetch timed out");
}
/// Complete code-551/556 messages received over the WebSocket, oldest first,
/// waiting for the quake thread (the WS task has a small stack, so it only
/// hands them over). An event comes as a burst (ScalePrompt, Destination,
/// DetailScale, EEW updates), so keep them all, not just the newest.
static WS_PENDING: Mutex<VecDeque<(u16, String)>> = Mutex::new(VecDeque::new());
const WS_PENDING_MAX: usize = 8;

/// Messages received over the WebSocket (all codes) and when the last one came
/// (uptime s) — shown in the [DIAG] line, to tell a silent socket from a quiet day.
static WS_RX_COUNT: AtomicU32 = AtomicU32::new(0);
static WS_RX_LAST: AtomicU32 = AtomicU32::new(0);
pub fn ws_rx_stats() -> (u32, u32) {
    (WS_RX_COUNT.load(Ordering::Relaxed), WS_RX_LAST.load(Ordering::Relaxed))
}

/// Value of the first `"key":` in a JSON message, as raw text (no parsing:
/// runs on the small-stack WS task).
fn json_peek<'a>(msg: &'a [u8], key: &[u8]) -> Option<&'a str> {
    let i = msg.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &msg[i..];
    let rest = rest.strip_prefix(b"\"").unwrap_or(rest);
    let end = rest.iter().position(|&b| matches!(b, b'"' | b',' | b'}')).unwrap_or(rest.len());
    core::str::from_utf8(&rest[..end.min(40)]).ok()
}

fn ws_dispatch(msg: Vec<u8>) {
    WS_RX_COUNT.fetch_add(1, Ordering::Relaxed);
    WS_RX_LAST.store(health::uptime_s(), Ordering::Relaxed);
    // Keys are sorted alphabetically, so "code" comes after "areas" etc. — look
    // at the whole message, not just its start.
    let code: u16 = json_peek(&msg, b"\"code\":").and_then(|c| c.trim().parse().ok()).unwrap_or(0);
    // Earthquake-related reports are logged on arrival (also the ones that are
    // then not shown), so a missing one can be told apart from a missed one.
    if matches!(code, 551 | 552 | 556) {
        info!("[WS] rx code {} type {} {} B", code,
              json_peek(&msg, b"\"type\":").unwrap_or("-"), msg.len());
    }
    if code != 551 && code != 556 { return; }
    let msg = match String::from_utf8(msg) {
        Ok(m) => m,
        Err(_) => { warn!("[WS] code {} message is not valid UTF-8, dropped", code); return; }
    };
    let mut q = WS_PENDING.lock().unwrap();
    if q.len() >= WS_PENDING_MAX {
        q.pop_front();
        warn!("[WS] pending queue full, oldest message dropped");
    }
    q.push_back((code, msg));
}

/// Raw WEBSOCKET_EVENT_DATA handler, registered next to esp-idf-svc's own.
///
/// esp-idf-svc turns each received chunk into `&str` on its own, and a message
/// larger than what one read returns arrives as several chunks — when a chunk
/// boundary falls inside a multi-byte (Japanese) character that conversion
/// fails and the chunk was silently lost. So reassemble the raw bytes here,
/// using the frame's payload offset/length, and decode only the whole message.
unsafe extern "C" fn ws_raw_data(_arg: *mut core::ffi::c_void, _base: esp_idf_sys::esp_event_base_t,
                                 id: i32, data: *mut core::ffi::c_void) {
    if data.is_null() { return; }
    let d = &*(data as *const esp_idf_sys::esp_websocket_event_data_t);
    if id == esp_idf_sys::esp_websocket_event_id_t_WEBSOCKET_EVENT_ERROR {
        // esp-idf-svc hands errors to the Rust callback as a bare ESP_FAIL, so
        // a failing (re)connect only ever showed up as "[WS] disconnected".
        let e = &d.error_handle;
        // The fields are only filled in for some error types (0 = none: garbage).
        match e.error_type {
            esp_idf_sys::esp_websocket_error_type_t_WEBSOCKET_ERROR_TYPE_TCP_TRANSPORT =>
                warn!("[WS] error: transport, tls_err=0x{:x} tls_stack=-0x{:x} sock_errno={}",
                      e.esp_tls_last_esp_err, -e.esp_tls_stack_err, e.esp_transport_sock_errno),
            esp_idf_sys::esp_websocket_error_type_t_WEBSOCKET_ERROR_TYPE_HANDSHAKE =>
                warn!("[WS] error: handshake, HTTP {}", e.esp_ws_handshake_status_code),
            2 => warn!("[WS] error: pong timeout"),
            4 => warn!("[WS] error: closed by the server"),
            t => warn!("[WS] error (type {})", t),
        }
        return;
    }
    if id != esp_idf_sys::esp_websocket_event_id_t_WEBSOCKET_EVENT_DATA { return; }
    // 1 = text, 0 = continuation of a fragmented message; skip control frames.
    if d.op_code != 1 && d.op_code != 0 { return; }
    let len = d.data_len.max(0) as usize;
    let chunk: &[u8] = if len == 0 || d.data_ptr.is_null() { &[] }
                       else { core::slice::from_raw_parts(d.data_ptr as *const u8, len) };

    let mut rx = WS_RX.lock().unwrap();
    if d.op_code == 1 && d.payload_offset == 0 {
        // First chunk of a new message. >4 KB reservations land in PSRAM.
        rx.clear();
        WS_RX_SKIP.store(false, Ordering::Relaxed);
        let total = d.payload_len.max(0) as usize;
        if total > WS_MSG_MAX {
            WS_RX_SKIP.store(true, Ordering::Relaxed);
            warn!("[WS] message of {} B exceeds {} B, dropped", total, WS_MSG_MAX);
        } else {
            rx.reserve(total);
        }
    }
    if !WS_RX_SKIP.load(Ordering::Relaxed) {
        if rx.len() + chunk.len() > WS_MSG_MAX {
            WS_RX_SKIP.store(true, Ordering::Relaxed);
            *rx = Vec::new();
            warn!("[WS] fragmented message exceeds {} B, dropped", WS_MSG_MAX);
        } else {
            rx.extend_from_slice(chunk);
        }
    }
    let frame_done = d.payload_offset.max(0) as usize + len >= d.payload_len.max(0) as usize;
    if frame_done && d.fin {
        let msg = core::mem::take(&mut *rx); // also frees the (PSRAM) buffer
        drop(rx);
        if !WS_RX_SKIP.swap(false, Ordering::Relaxed) && !msg.is_empty() {
            ws_dispatch(msg);
        }
    }
}

/// Tear a client down without `EspWebSocketClient`'s own `Drop`.
///
/// That `Drop` does `esp_websocket_client_close(..).unwrap()`, which fails with
/// ESP_FAIL ("Websocket client is not connected") whenever the socket happens to
/// be down at that moment (server-side close, mid-reconnect) — and the unwrap
/// then aborts the whole firmware. So close/destroy the C handle here, ignoring
/// errors, and `forget` the wrapper (leaking only its 16-byte callback box).
fn drop_ws(ws: EspWebSocketClient<'static>) {
    use esp_idf_svc::handle::RawHandle;
    use esp_idf_svc::hal::delay::TickType;
    let h = ws.handle();
    let ticks = TickType::from(Duration::from_secs(2)).0;
    unsafe {
        let _ = esp_idf_sys::esp_websocket_client_close(h, ticks);
        let _ = esp_idf_sys::esp_websocket_client_destroy(h);
    }
    std::mem::forget(ws);
    *WS_RX.lock().unwrap() = Vec::new();
    WS_CONNECTED.store(false, Ordering::Relaxed);
}

fn new_ws() -> Option<EspWebSocketClient<'static>> {
    let cfg = EspWebSocketClientConfig {
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        buffer_size: WS_BUF,
        task_stack: 6144,
        ping_interval_sec: Duration::from_secs(30),
        // The API answers 429 (Retry-After: 10) to rapid reconnects; stay well clear.
        reconnect_timeout_ms: Duration::from_secs(30),
        network_timeout_ms: Duration::from_secs(10),
        ..Default::default()
    };
    // Message data is taken by `ws_raw_data`; this only tracks the connection.
    let cb = move |ev: &Result<esp_idf_svc::ws::client::WebSocketEvent<'_>, esp_idf_svc::io::EspIOError>| {
        let Ok(ev) = ev else { return };
        match &ev.event_type {
            WebSocketEventType::Connected => {
                WS_CONNECTED.store(true, Ordering::Relaxed);
                info!("[WS] connected, internal heap {} B", internal_free());
            }
            WebSocketEventType::Disconnected | WebSocketEventType::Closed | WebSocketEventType::Close(_) => {
                WS_CONNECTED.store(false, Ordering::Relaxed);
                *WS_RX.lock().unwrap() = Vec::new();
                info!("[WS] disconnected");
            }
            _ => {}
        }
    };
    let ws = EspWebSocketClient::new(URL_WS, &cfg, Duration::from_secs(10), cb)
        .map_err(|e| warn!("[WS] create failed: {:?}", e))
        .ok()?;
    {
        use esp_idf_svc::handle::RawHandle;
        let r = unsafe {
            let h = ws.handle();
            // Errors are diagnostics only: a failure to register them is not fatal.
            let _ = esp_idf_sys::esp_websocket_register_events(
                h, esp_idf_sys::esp_websocket_event_id_t_WEBSOCKET_EVENT_ERROR,
                Some(ws_raw_data), core::ptr::null_mut());
            esp_idf_sys::esp_websocket_register_events(
                h, esp_idf_sys::esp_websocket_event_id_t_WEBSOCKET_EVENT_DATA,
                Some(ws_raw_data), core::ptr::null_mut())
        };
        if r != esp_idf_sys::ESP_OK {
            // Without it no message would ever be seen: let REST take over.
            warn!("[WS] registering the data handler failed ({}), not using the WebSocket", r);
            drop_ws(ws);
            return None;
        }
    }
    Some(ws)
}

/// Spawn the polling thread. It waits for WiFi and a synced clock (TLS cert
/// validity) before the first request.
pub fn start() {
    let r = thread::Builder::new()
        .name("quake".into())
        // Measured: at most ~5 KB of the old 20 KB was ever used (high-water mark
        // 15360 B free). This stack lives in scarce internal RAM, so keep it lean.
        .stack_size(10240)
        .spawn(|| {
            if cfg!(feature = "quake-sandbox") {
                warn!("[QUAKE] SANDBOX mode: using api-v2-sandbox.p2pquake.net, EEW freshness check disabled");
            }
            let mut client: Option<HttpClient> = None;
            let mut ws: Option<EspWebSocketClient<'static>> = None;
            let mut ws_since = 0u64;           // when the current WS client was created
            let mut ws_down_since: Option<u64> = None; // since when the existing client has been disconnected
            let mut next_quake = 0u64;
            let mut next_eew = 0u64;           // REST EEW poll (boot catch-up + WS fallback)
            let mut booted_eew = false;
            let mut last_eew_id = String::new();
            let mut last_low_warn = 0u64;
            let mut ws_missing_since: Option<u64> = None; // since when no connected WebSocket
            let hb = &health::QUAKE;
            loop {
                hb.mark(health::q::LOOP);
                // Map fetch worker asked for the memory: drop the WebSocket.
                if WS_RELEASE.swap(false, Ordering::Relaxed) && ws.is_some() {
                    hb.mark(health::q::WS_DROP);
                    if let Some(w) = ws.take() { drop_ws(w); }
                    WS_CONNECTED.store(false, Ordering::Relaxed);
                    WS_ALIVE.store(false, Ordering::Relaxed);
                    client = None;
                    // Re-create ~8 s after the fetch ends (avoids 429s from churn).
                    ws_since = now_secs().saturating_sub(30 - 8);
                    info!("[WS] released for map fetch, internal heap {} B", internal_free());
                }

                let now = now_secs();
                // 2024-01-01 00:00:00 UTC: clock is synced (NTP)
                let ready = now > 1_704_067_200 && crate::wifi::net_up();

                // Expire the EEW banner / TFT overlay.
                if EEW.lock().unwrap().as_ref().map(|e| now >= e.expires).unwrap_or(false) {
                    clear_eew();
                }
                // Expire a 震度速報 banner no epicenter report followed.
                if PROMPT.lock().unwrap().as_ref().map(|p| now >= p.expires).unwrap_or(false) {
                    clear_prompt("expired");
                }
                // Expire the "new earthquake" map view and give the map back.
                if NEWQ.lock().unwrap().as_ref().map(|n| now >= n.expires).unwrap_or(false) {
                    clear_new_quake();
                    if EEW.lock().unwrap().is_none() { clear_focus(); }
                }

                // Earthquake info / EEW pushed over the WebSocket, in arrival order.
                loop {
                    let Some((code, msg)) = WS_PENDING.lock().unwrap().pop_front() else { break };
                    if code == 556 {
                        hb.mark(health::q::WS_EEW);
                        handle_eew(msg.as_bytes(), &mut last_eew_id);
                    } else if msg.contains("\"type\":\"ScalePrompt\"") {
                        hb.mark(health::q::WS_PUSHED);
                        handle_prompt(msg.as_bytes());
                    } else {
                        hb.mark(health::q::WS_PUSHED);
                        merge_pushed(msg.as_bytes());
                    }
                }

                // Time without WiFi/clock does not count towards the WebSocket restart.
                if !ready { ws_missing_since = None; }
                if ready {
                    // (Re)create the WebSocket client; it reconnects by itself,
                    // so only replace it when it has been stuck for a while.
                    let ws_up = WS_CONNECTED.load(Ordering::Relaxed);
                    // Measure from when it went down — not from when the client was
                    // created: after a long-lived connection is closed by the server
                    // (e.g. a clean FIN after an hour) the client's own reconnect, 10 s
                    // later, usually works and must be given the chance to.
                    if ws.is_some() && !ws_up {
                        ws_down_since.get_or_insert(now);
                    } else {
                        ws_down_since = None;
                    }
                    if ws.is_some() && ws_down_since.map(|t| now.saturating_sub(t) > 180).unwrap_or(false) {
                        ws_down_since = None;
                        warn!("[WS] not connected for 180 s, recreating");
                        hb.mark(health::q::WS_DROP);
                        if let Some(w) = ws.take() { drop_ws(w); }
                        WS_ALIVE.store(false, Ordering::Relaxed);
                    }
                    if ws.is_none() && now.saturating_sub(ws_since) >= 30 && !crate::map_display::is_busy()
                        // Creating the client stalled the audio thread ~820 ms (audible dropout): wait for the sentence to end.
                        && !crate::audio::playing()
                        && (now as u32).saturating_sub(LAST_MAP_FETCH_SECS.load(Ordering::Relaxed)) >= WS_RESUME_AFTER_MAP_SECS {
                        ws_since = now;
                        // The REST client kept alive between the fallback polls holds a
                        // TLS session in internal RAM. Release it first, or it can keep
                        // the heap just under the threshold for good: the WebSocket then
                        // never starts, so REST keeps polling and keeps the session open
                        // (seen 2026-11-07 23:27 UTC until noticed hours later).
                        if internal_free() < WS_MIN_INTERNAL_FREE && client.is_some() {
                            client = None;
                            info!("[WS] released the REST client for the WebSocket, internal heap {} B", internal_free());
                        }
                        if internal_free() >= WS_MIN_INTERNAL_FREE {
                            let before = internal_free();
                            hb.mark(health::q::WS_CREATE);
                            ws = new_ws();
                            WS_ALIVE.store(ws.is_some(), Ordering::Relaxed);
                            // Catch up on any EEW issued while the socket was down.
                            booted_eew = false;
                            next_eew = 0;
                            info!("[WS] client created, internal heap {} -> {} B", before, internal_free());
                        } else {
                            warn!("[WS] internal heap low ({} B), not starting", internal_free());
                        }
                    }
                    // Last resort: no WebSocket for this long means pushes (and
                    // fast EEW) are lost; a clean boot gets the heap back.
                    if ws.is_some() && ws_up {
                        ws_missing_since = None;
                    } else if now.saturating_sub(*ws_missing_since.get_or_insert(now)) > WS_MISSING_RESTART_SECS
                        && !crate::audio::playing() && EEW.lock().unwrap().is_none() {
                        health::restart(&format!("WebSocket down for {} s (internal heap {} B)",
                                                 WS_MISSING_RESTART_SECS, internal_free()));
                    }

                    if internal_free() < MIN_INTERNAL_FREE {
                        if now.saturating_sub(last_low_warn) >= 30 {
                            last_low_warn = now;
                            warn!("[QUAKE] internal heap low ({} B), skipping REST polls", internal_free());
                        }
                        client = None; // also frees the TLS context
                    } else {
                        // EEW over REST: once at boot (an event may be in progress),
                        // then only while the WebSocket is down.
                        // Not while the map is being used either: the socket is down on
                        // purpose then, and the catch-up after it returns covers that gap.
                        let map_quiet = (now as u32).saturating_sub(LAST_MAP_FETCH_SECS.load(Ordering::Relaxed)) >= WS_RESUME_AFTER_MAP_SECS;
                        if now >= next_eew && (!booted_eew || !ws_up) && !crate::map_display::is_busy() && map_quiet {
                            booted_eew = true;
                            next_eew = now + EEW_POLL_SECS;
                            hb.mark(health::q::REST_EEW);
                            if let Some(body) = http_get(&mut client, URL_EEW) {
                                handle_eew(&body, &mut last_eew_id);
                                info!("[EEW] REST check done, stack high-water {} B free", stack_free());
                            }
                        }
                        // The list is pushed over the WebSocket, so REST is only the initial
                        // load, a slow safety re-sync, and the fallback while the socket is
                        // down. Never while a map tile fetch (TLS + JPEG) is in flight: both
                        // need internal RAM at the same time.
                        if now >= next_quake && !crate::map_display::is_busy() {
                            next_quake = now + if ws_up { QUAKE_RESYNC_SECS } else { POLL_SECS };
                            let before = internal_free();
                            hb.mark(health::q::REST_QUAKE);
                            match http_get(&mut client, URL_QUAKE).and_then(|b| parse(&b)) {
                                Some(list) => {
                                    let mut cur = QUAKES.lock().unwrap();
                                    let changed = cur.len() != list.len()
                                        || cur.iter().zip(list.iter()).any(|(a, b)| a.time != b.time || a.lat != b.lat || a.lon != b.lon);
                                    if changed {
                                        info!("[QUAKE] {} event(s), latest: {}",
                                              list.len(), list.first().map(|q| q.time.as_str()).unwrap_or("-"));
                                        *cur = list;
                                        VERSION.fetch_add(1, Ordering::Relaxed);
                                    }
                                    LAST_OK_SECS.store(now_secs() as u32, Ordering::Relaxed);
                                }
                                None => info!("[QUAKE] fetch failed, retry in {}s", POLL_SECS),
                            }
                            info!("[QUAKE] internal heap {} -> {} B, stack high-water {} B free", before, internal_free(), stack_free());
                        }
                        // With the WebSocket carrying EEW, don't hold a second TLS
                        // session open between the (rare) REST polls.
                        if ws_up { client = None; }
                    }
                }
                hb.mark(health::q::SLEEP);
                thread::sleep(Duration::from_millis(if ready { 100 } else { 5000 }));
            }
        });
    if r.is_err() {
        warn!("[QUAKE] thread spawn failed");
    }
}
