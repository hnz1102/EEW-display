//! Host stub of `map_display`. Same pure logic as the firmware (zoom); instead
//! of fetching Google tiles, `set_view` paints a synthetic map.
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

pub const MAP_W: usize = 480;
pub const MAP_H: usize = 266;
pub const DEFAULT_VIEW: (f64, f64, u8) = (36.5, 137.5, 4);

static MAP_PIXELS: LazyLock<Mutex<Option<Vec<u16>>>> = LazyLock::new(|| Mutex::new(None));
static MAP_VERSION: AtomicU32 = AtomicU32::new(0);
static MAP_BUSY: AtomicBool = AtomicBool::new(false);
static MAP_ZOOM: AtomicU8 = AtomicU8::new(DEFAULT_VIEW.2);
static MAP_TILE_ZOOM: AtomicU8 = AtomicU8::new(DEFAULT_VIEW.2);
static MAP_CENTER: Mutex<(f64, f64)> = Mutex::new((f64::NAN, f64::NAN));
static MAP_TYPE_SAT: AtomicBool = AtomicBool::new(false);
static HAS_KEY: AtomicBool = AtomicBool::new(true);

pub fn version() -> u32 { MAP_VERSION.load(Ordering::Relaxed) }
pub fn has_map() -> bool { MAP_PIXELS.lock().unwrap().is_some() }
pub fn is_busy() -> bool { MAP_BUSY.load(Ordering::Relaxed) }
pub fn has_api_key() -> bool { HAS_KEY.load(Ordering::Relaxed) }
pub fn get_zoom() -> u8 { MAP_ZOOM.load(Ordering::Relaxed) }
pub fn get_tile_zoom() -> u8 { MAP_TILE_ZOOM.load(Ordering::Relaxed) }
pub fn reset_last_pos() {}
/// The firmware kicks off a tile fetch here; the host paints in `set_view`.
pub fn trigger(_lat: f64, _lon: f64) {}
pub fn get_map_center() -> (f64, f64) { *MAP_CENTER.lock().unwrap() }

pub fn set_zoom(z: u8) { MAP_ZOOM.store(z.clamp(1, 20), Ordering::Relaxed); }

pub fn with_pixels<F: FnOnce(&[u16])>(f: F) {
    if let Some(ref px) = *MAP_PIXELS.lock().unwrap() { f(px.as_slice()); }
}

// ── host-only ────────────────────────────────────────────────────────────────

pub fn set_map_type_satellite(sat: bool) { MAP_TYPE_SAT.store(sat, Ordering::Relaxed); }
pub fn set_has_api_key(on: bool) { HAS_KEY.store(on, Ordering::Relaxed); }

/// Pretend a tile fetch just completed for this view: the real Google Static
/// Maps tile (same request as the firmware) if a key is available, else the
/// schematic fallback.
pub fn set_view(clat: f64, clon: f64, zoom: u8) {
    MAP_ZOOM.store(zoom, Ordering::Relaxed);
    MAP_TILE_ZOOM.store(zoom, Ordering::Relaxed);
    *MAP_CENTER.lock().unwrap() = (clat, clon);
    let px = fetch_tile(clat, clon, zoom).unwrap_or_else(|| paint(clat, clon, zoom));
    *MAP_PIXELS.lock().unwrap() = Some(px);
    MAP_VERSION.fetch_add(1, Ordering::Relaxed);
}

fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

/// A schematic map (land, water, blocks, roads), deterministic per view. Not a
/// real map: the sample images are about the UI, not about geography.
fn paint(clat: f64, clon: f64, zoom: u8) -> Vec<u16> {
    let land = rgb565(222, 218, 206);
    let water = rgb565(140, 180, 222);
    let park = rgb565(190, 215, 180);
    let block = rgb565(204, 200, 188);
    let road = rgb565(240, 240, 236);
    let main = rgb565(240, 204, 110);
    let world = 256.0 * (1u64 << zoom) as f64;
    let mut px = vec![land; MAP_W * MAP_H];
    // World pixel of the tile origin.
    let wx0 = (clon + 180.0) / 360.0 * world - MAP_W as f64 / 2.0;
    let my = |lat: f64| {
        let l = lat.clamp(-85.0, 85.0).to_radians();
        (1.0 - (l.tan() + 1.0 / l.cos()).ln() / core::f64::consts::PI) / 2.0 * world
    };
    let wy0 = my(clat) - MAP_H as f64 / 2.0;
    if zoom < 12 { return paint_regional(wx0, wy0, world); }
    let hash = |x: i64, y: i64| -> u32 {
        let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
        h ^= h >> 15; h = h.wrapping_mul(0x2C1B_3C6D); h ^= h >> 12; h
    };
    // Block size shrinks with coarser zoom so something is visible at any scale.
    let cell = if zoom >= 15 { 64.0 } else if zoom >= 12 { 40.0 } else { 24.0 };
    for y in 0..MAP_H {
        for x in 0..MAP_W {
            let wx = wx0 + x as f64;
            let wy = wy0 + y as f64;
            let (cx, cy) = ((wx / cell).floor() as i64, (wy / cell).floor() as i64);
            let (fx, fy) = (wx - cx as f64 * cell, wy - cy as f64 * cell);
            let h = hash(cx, cy);
            let mut c = if h % 7 == 0 { park } else { block };
            // streets between blocks, an avenue every 4th row/column
            let street = 5.0;
            let on_x = fx < street || fx > cell - 1.0;
            let on_y = fy < street || fy > cell - 1.0;
            if on_x || on_y {
                let avenue = cx.rem_euclid(4) == 0 && on_x || cy.rem_euclid(4) == 0 && on_y;
                c = if avenue { main } else { road };
            }
            // a diagonal river
            let d = (wx * 0.6 + wy * 0.8 + (wx / 90.0).sin() * 18.0) % 700.0;
            if (d - 350.0).abs() < 22.0 { c = water; }
            px[y * MAP_W + x] = c;
        }
    }
    px
}

/// Coarse-zoom view: land/sea blobs and a graticule instead of street blocks.
fn paint_regional(wx0: f64, wy0: f64, world: f64) -> Vec<u16> {
    let land = rgb565(206, 214, 190);
    let water = rgb565(140, 180, 222);
    let grid = rgb565(190, 196, 180);
    let mut px = vec![land; MAP_W * MAP_H];
    let k = 2.0 * core::f64::consts::PI / world * 4096.0;
    let coast = |wx: f64, wy: f64| wx * 0.8 + wy * 0.6 + 140.0 * (wy / 260.0).sin() + 60.0 * (wx / 130.0).cos();
    // Sea starts a little east/south-east of the view centre, whatever the view.
    let base = coast((wx0 + MAP_W as f64 / 2.0) * k, (wy0 + MAP_H as f64 / 2.0) * k);
    for y in 0..MAP_H {
        for x in 0..MAP_W {
            let (wx, wy) = ((wx0 + x as f64) * k, (wy0 + y as f64) * k);
            let mut c = if coast(wx, wy) - base > 90.0 { water } else { land };
            if ((wx0 + x as f64) % 80.0 < 1.0 || (wy0 + y as f64) % 80.0 < 1.0) && c == land { c = grid; }
            px[y * MAP_W + x] = c;
        }
    }
    px
}

// ── real tiles ───────────────────────────────────────────────────────────────

/// `GOOGLE_MAPS_API_KEY`, else `maps_api_key` from the firmware's cfg.toml.
fn api_key() -> Option<String> {
    if let Ok(k) = std::env::var("GOOGLE_MAPS_API_KEY") {
        if !k.trim().is_empty() { return Some(k.trim().to_string()); }
    }
    let cfg = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cfg.toml")).ok()?;
    cfg.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        if k.trim() != "maps_api_key" { return None; }
        let v = v.trim().trim_matches('"');
        if v.is_empty() { None } else { Some(v.to_string()) }
    })
}

/// Downloads (curl) and decodes the tile; JPEGs are cached in `.tile-cache/` so
/// re-rendering doesn't hit the API. The key is never printed or stored.
fn fetch_tile(lat: f64, lon: f64, zoom: u8) -> Option<Vec<u16>> {
    // SCREENSHOTS_OFFLINE=1: never call the Maps API, always use the schematic map.
    if std::env::var("SCREENSHOTS_OFFLINE").map(|v| v == "1").unwrap_or(false) { return None; }
    let sat = MAP_TYPE_SAT.load(Ordering::Relaxed);
    let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.tile-cache"));
    let file = dir.join(format!("{:.6}_{:.6}_{}{}.jpg", lat, lon, zoom, if sat { "_sat" } else { "" }));
    let jpeg = match std::fs::read(&file) {
        Ok(b) => b,
        Err(_) => {
            let key = api_key()?;
            let url = format!(
                "https://maps.googleapis.com/maps/api/staticmap?center={:.6},{:.6}&zoom={}&size={}x{}&format=jpg&maptype={}&key={}",
                lat, lon, zoom, MAP_W, MAP_H, if sat { "satellite" } else { "roadmap" }, key);
            let out = std::process::Command::new("curl").args(["-sSf", "--max-time", "20", &url]).output().ok()?;
            if !out.status.success() || out.stdout.len() < 1000 {
                eprintln!("map tile fetch failed (z{zoom}); using schematic map");
                return None;
            }
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&file, &out.stdout);
            out.stdout
        }
    };
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpeg));
    let rgb = dec.decode().ok()?;
    let info = dec.info()?;
    let (sw, sh) = (info.width as usize, info.height as usize);
    if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 { return None; }
    let mut out = Vec::with_capacity(MAP_W * MAP_H);
    for y in 0..MAP_H {
        for x in 0..MAP_W {
            out.push(if x < sw && y < sh {
                let i = (y * sw + x) * 3;
                rgb565(rgb[i], rgb[i + 1], rgb[i + 2])
            } else { 0 });
        }
    }
    Some(out)
}
