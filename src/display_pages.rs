//! Page rendering for the TFT (ILI9488, 480x320 landscape, RGB565 framebuffer).
//! Everything here is generic over `DrawTarget`, with no ESP-IDF dependency, so
//! `tools/screenshots` can compile this file on the host and render each page
//! to a PNG.
//!
//! Pages: EARTHQUAKE (recent list) → MAP (epicenters / EEW) → INFO → ...
use std::sync::Mutex;
use core::sync::atomic::{AtomicU8, Ordering};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X12, ascii::FONT_8X13, ascii::FONT_10X20, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::Rgb565,
    text::Text,
    geometry::{Point, Size},
    primitives::{Circle, Line, Rectangle, PrimitiveStyle, PrimitiveStyleBuilder},
    prelude::*,
};

/// Screen size in pixels.
pub(crate) const SCREEN_W: i32 = 480;
pub(crate) const SCREEN_H: i32 = 320;
/// Header height in pixels.
pub(crate) const HEADER_H: i32 = 28;
/// The button legend row at the bottom of every page.
pub(crate) const LEGEND_Y: i32 = SCREEN_H - 24;
/// Info rows use FONT_8X13 on this pitch.
const INFO_ROW_H: i32 = 18;
const INFO_CHAR_W: i32 = 8;

// ─── Display page management ─────────────────────────────────────────────
// Page order: QUAKE(0) → MAP(1) → INFO(2) → QUAKE ...
pub const PAGE_QUAKE: u8 = 0;
pub const PAGE_MAP:   u8 = 1;
pub const PAGE_INFO:  u8 = 2;
pub(crate) const PAGE_COUNT: u8 = 3;

pub(crate) static CURRENT_PAGE: AtomicU8 = AtomicU8::new(PAGE_MAP);

pub fn next_page() {
    let cur = CURRENT_PAGE.load(Ordering::Relaxed);
    CURRENT_PAGE.store((cur + 1) % PAGE_COUNT, Ordering::SeqCst);
}

pub fn set_page(page: u8) {
    if page < PAGE_COUNT {
        CURRENT_PAGE.store(page, Ordering::SeqCst);
    }
}

pub fn current_page() -> u8 {
    CURRENT_PAGE.load(Ordering::Relaxed)
}

// ─── System message overlay (for important notifications) ────────────────
pub(crate) static SYSTEM_MESSAGE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Display a system message overlay (title + text).
pub fn show_system_message(title: &str, text: &str) {
    *SYSTEM_MESSAGE.lock().unwrap() = Some((title.to_string(), text.to_string()));
}

/// Clear the system message overlay.
pub fn clear_system_message() {
    *SYSTEM_MESSAGE.lock().unwrap() = None;
}

// ─── Shared info for Info page (set from main after init) ────────────────
pub struct DisplayAux {
    pub status: crate::httpserver::StatusInfo,
}

pub(crate) static DISPLAY_AUX: Mutex<Option<DisplayAux>> = Mutex::new(None);

pub fn set_display_aux(aux: DisplayAux) {
    *DISPLAY_AUX.lock().unwrap() = Some(aux);
}

// ─── Small helpers ───────────────────────────────────────────────────────

const DIM: Rgb565 = Rgb565::new(10, 21, 10);
const CYAN: Rgb565 = Rgb565::new(0, 50, 31);
const SEL_BG: Rgb565 = Rgb565::new(1, 10, 14);

fn fill<D: DrawTarget<Color = Rgb565>>(display: &mut D, x: i32, y: i32, w: i32, h: i32, c: Rgb565) {
    let _ = Rectangle::new(Point::new(x, y), Size::new(w.max(0) as u32, h.max(0) as u32))
        .into_styled(PrimitiveStyle::with_fill(c))
        .draw(display);
}

fn text<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, s: &str, x: i32, baseline: i32, font: &'static embedded_graphics::mono_font::MonoFont<'static>, c: Rgb565,
) {
    let _ = Text::new(s, Point::new(x, baseline), MonoTextStyle::new(font, c)).draw(display);
}

/// "35.70N 140.85E"
fn fmt_latlon(lat: f64, lon: f64) -> String {
    format!("{:.2}{} {:.2}{}",
        lat.abs(), if lat >= 0.0 { 'N' } else { 'S' },
        lon.abs(), if lon >= 0.0 { 'E' } else { 'W' })
}

fn fmt_mag(mag: f32) -> String {
    if mag >= 0.0 { format!("M{:.1}", mag) } else { "M?".into() }
}

fn fmt_depth(depth: i32) -> String {
    match depth { d if d < 0 => "?km".to_string(), 0 => "0km".to_string(), d => format!("{}km", d) }
}

/// Epicenter marker / list colour by max seismic intensity (falls back to magnitude).
fn quake_color(scale: i32, mag: f32) -> Rgb565 {
    let m = if mag > 0.0 { mag } else { 1.0 };
    if scale >= 55 { Rgb565::new(31, 0, 31) }              // 6- and up: magenta
    else if scale >= 45 || m >= 5.0 { Rgb565::RED }        // 5- / 5+
    else if scale >= 30 || m >= 3.0 { Rgb565::CSS_ORANGE }
    else { Rgb565::YELLOW }
}

// ─── Battery icon ────────────────────────────────────────────────────────

/// Battery level: `BATT_NONE` = no reading, `BATT_USB` = externally powered
/// (voltage >= 4.5 V), 0..=5 = number of lit segments.
pub(crate) const BATT_NONE: u8 = 255;
pub(crate) const BATT_USB: u8 = 254;
pub(crate) const USB_ENTER_V: f32 = 4.50;
pub(crate) const USB_EXIT_V: f32 = 4.45;
pub(crate) const BATT_SEGS: u8 = 5;

pub(crate) fn battery_voltage() -> f32 {
    DISPLAY_AUX.lock().unwrap().as_ref()
        .map(|aux| *aux.status.battery_voltage.lock().unwrap())
        .unwrap_or(0.0)
}

/// Single-cell Li-ion thresholds with hysteresis (same steps as wakepcbox),
/// so the icon doesn't flicker when the voltage hovers near a boundary.
pub(crate) fn next_battery_level(prev: u8, v: f32) -> u8 {
    const UP:   [f32; 5] = [3.75, 3.85, 3.95, 4.00, 4.05]; // level k -> k+1
    const DOWN: [f32; 5] = [3.70, 3.80, 3.90, 3.95, 4.00]; // level k+1 -> k
    if v < 2.0 { return BATT_NONE; }          // no battery / not measured
    if prev == BATT_USB {
        if v >= USB_EXIT_V { return BATT_USB; }
        return UP.iter().filter(|&&t| v > t).count() as u8;
    }
    if v >= USB_ENTER_V { return BATT_USB; }
    if prev == BATT_NONE || prev > BATT_SEGS {
        // First reading: pick the level directly
        return UP.iter().filter(|&&t| v > t).count() as u8;
    }
    let mut lvl = prev;
    if (lvl as usize) < UP.len() && v > UP[lvl as usize] { lvl += 1; }
    else if lvl > 0 && v < DOWN[lvl as usize - 1] { lvl -= 1; }
    lvl
}

/// `compact`: draw into the one-line top bar of the MAP page (MAP_BAR_H high)
/// instead of the normal HEADER_H-high header.
pub(crate) fn draw_battery_icon<D: DrawTarget<Color = Rgb565>>(display: &mut D, level: u8, compact: bool) {
    const X: i32 = SCREEN_W - 34;
    let (bar_h, y0) = if compact { (MAP_BAR_H, 4) } else { (HEADER_H, 8) };
    const BODY_W: u32 = 26;
    const BODY_H: u32 = 12;
    // Clear the icon area (header background)
    fill(display, X - 2, 0, SCREEN_W - (X - 2), bar_h, Rgb565::BLACK);
    if level == BATT_NONE { return; }
    if level == BATT_USB {
        text(display, "USB", X + 4, y0 + 10, &FONT_6X12, Rgb565::CSS_CYAN);
        return;
    }
    let color = match level {
        0 | 1 => Rgb565::CSS_RED,
        2     => Rgb565::CSS_ORANGE,
        _     => Rgb565::CSS_LIGHT_GREEN,
    };
    // Body outline + terminal tip
    let _ = Rectangle::new(Point::new(X, y0), Size::new(BODY_W, BODY_H))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
        .draw(display);
    fill(display, X + BODY_W as i32, y0 + 3, 2, BODY_H as i32 - 6, Rgb565::WHITE);
    // Segments (4 px wide, 1 px gap)
    for i in 0..level.min(BATT_SEGS) as i32 {
        fill(display, X + 2 + i * 5, y0 + 2, 4, BODY_H as i32 - 4, color);
    }
}

// ─── System message overlay drawing ──────────────────────────────────────

pub(crate) fn draw_system_message<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    title: &str,
    text_body: &str,
    header_style: &MonoTextStyle<Rgb565>,
    force_clear: bool,
) {
    // Only full clear and redraw header on first display or when message changes
    if force_clear {
        let _ = display.clear(Rgb565::BLACK);
        let _ = Text::new(title, Point::new(4, 21), *header_style).draw(display);
        fill(display, 0, HEADER_H, SCREEN_W, 1, Rgb565::CSS_DARK_GREEN);
    }

    // Background colour overwrites the previous text without a clear.
    let msg_style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(Rgb565::YELLOW)
        .background_color(Rgb565::BLACK)
        .build();

    let y_start = 110;
    let lines: Vec<&str> = text_body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let y = y_start + (i as i32 * 28);
        // Pad to the full width (48 chars of 10 px) so a shorter line erases the old one.
        let padded = format!("{:<48}", line);
        let _ = Text::new(&padded, Point::new(0, y), msg_style).draw(display);
    }
    if !force_clear && lines.len() < 6 {
        for i in lines.len()..6 {
            let y = y_start + (i as i32 * 28);
            let _ = Text::new(&format!("{:<48}", ""), Point::new(0, y), msg_style).draw(display);
        }
    }
}

// ─── Header drawing ──────────────────────────────────────────────────────

pub(crate) fn draw_header<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    page: u8,
    style: &MonoTextStyle<Rgb565>,
) {
    let page_num = page + 1;
    let title = match page {
        PAGE_QUAKE => format!("EARTHQUAKE    {}/{}", page_num, PAGE_COUNT),
        PAGE_MAP   => format!("MAP           {}/{}", page_num, PAGE_COUNT),
        PAGE_INFO  => format!("SYSTEM INFO   {}/{}", page_num, PAGE_COUNT),
        _          => format!("PAGE {}        {}/{}", page, page_num, PAGE_COUNT),
    };
    fill(display, 0, 0, SCREEN_W, HEADER_H, Rgb565::BLACK);
    let _ = Text::new(&title, Point::new(4, 21), *style).draw(display);
    fill(display, 0, HEADER_H, SCREEN_W, 1, Rgb565::CSS_DARK_GREEN);
}

// ─── Button legend ───────────────────────────────────────────────────────
//
// Same look on every page so the on-screen hint matches the physical buttons:
// B0 (page) = yellow round button, GPIO38 = blue "+UP", GPIO39 = blue "-DOWN",
// B6 = red round button. Round buttons carry a short label of what they do
// on the current page.

pub(crate) enum Legend {
    /// B0 — yellow round button (page switch / exit).
    Page(&'static str),
    /// GPIO38 — blue "+UP".
    Up,
    /// GPIO39 — blue "-DOWN".
    Down,
    /// B6 — red round button.
    Action(&'static str),
}

const LEGEND_BLUE: Rgb565 = Rgb565::new(8, 28, 31);

/// Draws `items` left to right starting at `x`; `baseline` is the text baseline
/// (FONT_8X13). Returns the x just past the last item.
pub(crate) fn draw_legend<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, x: i32, baseline: i32, items: &[Legend],
) -> i32 {
    let mut x = x;
    for item in items {
        let (color, label, round) = match item {
            Legend::Page(l)   => (Rgb565::YELLOW, *l, true),
            Legend::Action(l) => (Rgb565::RED, *l, true),
            Legend::Up        => (LEGEND_BLUE, "+UP", false),
            Legend::Down      => (LEGEND_BLUE, "-DOWN", false),
        };
        if round {
            let _ = Circle::new(Point::new(x, baseline - 10), 11)
                .into_styled(PrimitiveStyle::with_fill(color)).draw(display);
            x += 15;
        }
        text(display, label, x, baseline, &FONT_8X13, color);
        x += label.len() as i32 * INFO_CHAR_W + 12;
    }
    x
}

/// Clears the legend row and draws `items` on it.
fn draw_legend_row<D: DrawTarget<Color = Rgb565>>(display: &mut D, items: &[Legend], width: i32) {
    fill(display, 0, LEGEND_Y, width, SCREEN_H - LEGEND_Y, Rgb565::BLACK);
    draw_legend(display, 4, LEGEND_Y + 16, items);
}


// ─── Map page ─────────────────────────────────────────────────────────────
//
// A Google Maps Static API JPEG (480x266) between a one-line top bar (title,
// button legend, battery) and a two-column text band at the bottom, centred on
// the earthquake picked on the quake page, on an EEW epicenter, or on a
// Japan-wide overview, with the recent epicenters drawn on it. Nothing is
// drawn over the map except the markers and the scale bar.
// Requires a Maps API key (Web UI / cfg.toml) and WiFi connectivity.

/// One-line top bar of the MAP page: title, button legend, battery.
pub(crate) const MAP_BAR_H: i32 = 20;
const MAP_Y_TOP: i32 = MAP_BAR_H;
const MAP_Y_BOT: i32 = MAP_Y_TOP + crate::map_display::MAP_H as i32;
/// Two-column text band under the map (2 rows of 16 px).
const TEXT_Y: i32 = MAP_Y_BOT;
const TEXT_COL_X: [i32; 2] = [4, SCREEN_W / 2 + 4];

/// Web Mercator projection: (lat, lon) → screen pixel, for a map image of
/// `crate::map_display::MAP_W`×(y_bot−y_top) centred at (clat, clon) at the
/// given zoom. Returns `None` if `clat` is NaN (no map yet) or the point
/// falls outside the visible image.
pub(crate) fn project_to_map(clat: f64, clon: f64, zoom: u8, lat: f64, lon: f64, y_top: i32, y_bot: i32) -> Option<(i32, i32)> {
    if clat.is_nan() { return None; }
    let world = 256.0_f64 * ((1u64 << zoom) as f64);
    let lat_r  = lat.to_radians();
    let clat_r = clat.to_radians();
    let merc_pt  = (lat_r  / 2.0 + core::f64::consts::FRAC_PI_4).tan().ln();
    let merc_cen = (clat_r / 2.0 + core::f64::consts::FRAC_PI_4).tan().ln();
    let dx = (lon - clon) * world / 360.0;
    let dy = -(merc_pt - merc_cen) * world / (2.0 * core::f64::consts::PI);
    let px = (crate::map_display::MAP_W as f64 / 2.0 + dx).round() as i32;
    let py = y_top + ((y_bot - y_top) as f64 / 2.0 + dy).round() as i32;
    if px >= 0 && px < crate::map_display::MAP_W as i32 && py >= y_top && py < y_bot {
        Some((px, py))
    } else {
        None
    }
}

/// Redraws the map page when something changed. Returns true if it drew
/// (the caller then redraws the battery icon, which sits on top of the map).
pub(crate) fn draw_map_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_sig: &mut u32,
) -> bool {
    let quake_focus = crate::quake::focus();
    let eew = crate::quake::eew();
    let newq = crate::quake::new_quake();

    // Where the map should be centred: the focused epicenter, else the Japan overview.
    let (want_lat, want_lon) = match quake_focus {
        Some((la, lo)) => (la, lo),
        None => (crate::map_display::DEFAULT_VIEW.0, crate::map_display::DEFAULT_VIEW.1),
    };
    crate::map_display::trigger(want_lat, want_lon);

    let map_ver = crate::map_display::version();
    let has     = crate::map_display::has_map();
    let busy    = crate::map_display::is_busy();

    // ── Change detection ─────────────────────────────────────────────────────
    let map_sig = map_ver
        .wrapping_mul(31)
        .wrapping_add(if busy { 1 } else { 0 })
        .wrapping_add((crate::map_display::get_zoom() as u32).wrapping_mul(5))
        .wrapping_add(crate::quake::version().wrapping_mul(13))
        .wrapping_add(crate::quake::eew_version().wrapping_mul(17))
        .wrapping_add(crate::quake::newq_version().wrapping_mul(19))
        .wrapping_add(if quake_focus.is_some() { 23 } else { 0 });
    if !force && map_sig == *prev_sig { return false; }
    *prev_sig = map_sig;

    // ── Map background ───────────────────────────────────────────────────────
    let map_rect = Rectangle::new(
        Point::new(0, MAP_Y_TOP),
        Size::new(crate::map_display::MAP_W as u32, crate::map_display::MAP_H as u32),
    );
    if has {
        crate::map_display::with_pixels(|pixels| {
            let _ = display.fill_contiguous(
                &map_rect,
                pixels.iter().map(|&p| Rgb565::new(
                    ((p >> 11) & 0x1F) as u8,
                    ((p >> 5)  & 0x3F) as u8,
                    (p & 0x1F) as u8,
                )),
            );
        });
    } else {
        let _ = map_rect.into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
        let msg = if !crate::map_display::has_api_key() {
            "Set Maps API key in Web UI"
        } else if busy {
            "Fetching map..."
        } else {
            "Waiting for map..."
        };
        text(display, msg, 8, MAP_Y_TOP + 130, &FONT_8X13, Rgb565::new(0, 25, 12));
    }

    // ── Overlays ─────────────────────────────────────────────────────────────
    if has {
        let (clat, clon) = crate::map_display::get_map_center();
        // The zoom the displayed image was fetched at, not the current
        // setting — they differ while a refetch after a zoom change is in
        // flight, and everything below is drawn onto that image.
        let zoom = crate::map_display::get_tile_zoom();
        let to_px = |lat: f64, lon: f64| -> Option<(i32, i32)> {
            project_to_map(clat, clon, zoom, lat, lon, MAP_Y_TOP, MAP_Y_BOT)
        };

        // Recent earthquake epicenters that fall inside the visible image.
        let s_q = MonoTextStyleBuilder::new().font(&FONT_6X12)
            .text_color(Rgb565::WHITE).build();
        for q in crate::quake::get().iter() {
            if let Some((qx, qy)) = to_px(q.lat, q.lon) {
                let m = if q.mag > 0.0 { q.mag } else { 1.0 };
                let diam = (6.0 + m * 2.5).clamp(8.0, 30.0) as u32;
                let col = quake_color(q.scale, q.mag);
                let _ = Circle::with_center(Point::new(qx, qy), diam)
                    .into_styled(PrimitiveStyleBuilder::new()
                        .stroke_color(col).stroke_width(2).build())
                    .draw(display);
                // The newly received earthquake gets an extra white ring.
                if newq.as_ref().map(|n| n.time == q.time && n.lat == q.lat && n.lon == q.lon).unwrap_or(false) {
                    let _ = Circle::with_center(Point::new(qx, qy), diam + 12)
                        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
                        .draw(display);
                }
                // The epicenter picked on the quake page gets a thin cyan ring.
                if quake_focus.map(|(fl, fo)| (fl - q.lat).abs() < 1e-6 && (fo - q.lon).abs() < 1e-6).unwrap_or(false) {
                    let _ = Circle::with_center(Point::new(qx, qy), diam + 8)
                        .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_CYAN, 1))
                        .draw(display);
                }
                // Label: magnitude and max seismic intensity, e.g. "M3.5 I3" / "M5.2 I5-"
                let label = match (q.mag > 0.0, q.scale > 0) {
                    (true, true)  => format!("M{:.1} I{}", q.mag, q.intensity()),
                    (true, false) => format!("M{:.1}", q.mag),
                    (false, true) => format!("I{}", q.intensity()),
                    _ => String::new(),
                };
                if !label.is_empty() {
                    let lx = qx + diam as i32 / 2 + 3;
                    // Keep the label on screen near the right edge.
                    let lx = lx.min(SCREEN_W - label.len() as i32 * 6 - 1);
                    // Dark backing so the text reads over satellite imagery.
                    fill(display, lx - 1, qy - 7, label.len() as i32 * 6 + 2, 13, Rgb565::new(1, 2, 1));
                    let _ = Text::new(&label, Point::new(lx, qy + 3), s_q).draw(display);
                }
            }
        }

        // Active EEW: forecast epicenter drawn as a large red X.
        if let Some(e) = eew.as_ref() {
            if !e.lat.is_nan() {
                if let Some((ex, ey)) = to_px(e.lat, e.lon) {
                    let xs = PrimitiveStyle::with_stroke(Rgb565::RED, 3);
                    let _ = Line::new(Point::new(ex - 11, ey - 11), Point::new(ex + 11, ey + 11)).into_styled(xs).draw(display);
                    let _ = Line::new(Point::new(ex - 11, ey + 11), Point::new(ex + 11, ey - 11)).into_styled(xs).draw(display);
                    let lbl = format!("EEW I{}", e.intensity());
                    let lx = (ex + 14).min(SCREEN_W - lbl.len() as i32 * 6 - 1);
                    fill(display, lx - 1, ey - 7, lbl.len() as i32 * 6 + 2, 13, Rgb565::new(1, 2, 1));
                    text(display, &lbl, lx, ey + 3, &FONT_6X12, Rgb565::RED);
                }
            }
        }

        // ── Scale bar (bottom-left corner of the map) ─────────────────────
        // Web Mercator: metres-per-pixel at current centre latitude
        let lat_cos = if clat.is_nan() { 1.0f64 } else { clat.to_radians().cos() };
        let mpp = (2.0 * core::f64::consts::PI * 6_378_137.0 * lat_cos)
                  / (256.0 * (1u64 << zoom) as f64);

        // Nearest "nice" distance to a target of ~64 px
        const NICE_M: &[f64] = &[
            1.0, 2.0, 5.0, 10.0, 20.0, 50.0,
            100.0, 200.0, 500.0,
            1_000.0, 2_000.0, 5_000.0, 10_000.0,
            20_000.0, 50_000.0, 100_000.0, 200_000.0, 500_000.0, 1_000_000.0,
        ];
        let target_m = mpp * 64.0;
        let nice_m = NICE_M.iter().copied().min_by(|&a, &b| {
            (a - target_m).abs().partial_cmp(&(b - target_m).abs())
                .unwrap_or(core::cmp::Ordering::Equal)
        }).unwrap_or(1_000.0);
        let bar_px = ((nice_m / mpp).round() as i32).clamp(8, 240);

        // Human-readable label: "500 m" / "5 km"
        let label = if nice_m < 1_000.0 {
            format!("{} m", nice_m as u32)
        } else {
            format!("{} km", (nice_m / 1_000.0) as u32)
        };
        let label_px = label.len() as i32 * 6; // FONT_6X12 = 6 px wide

        let bar_y  = MAP_Y_BOT - 8;
        // Shifted right by 1/5 of the screen so it clears the Google logo / labels.
        let bar_x1 = 8 + SCREEN_W / 5;
        let bar_x2 = bar_x1 + bar_px;

        // Dark background box covering label + bar
        let bg_w = (bar_x2 - bar_x1 + 4).max(label_px + 4);
        fill(display, bar_x1 - 2, bar_y - 16, bg_w, 21, Rgb565::new(2, 4, 2));

        // Bar line + end ticks
        let wh = PrimitiveStyle::with_stroke(Rgb565::WHITE, 1);
        let _ = Line::new(Point::new(bar_x1, bar_y), Point::new(bar_x2, bar_y)).into_styled(wh).draw(display);
        let _ = Line::new(Point::new(bar_x1, bar_y - 3), Point::new(bar_x1, bar_y + 2)).into_styled(wh).draw(display);
        let _ = Line::new(Point::new(bar_x2, bar_y - 3), Point::new(bar_x2, bar_y + 2)).into_styled(wh).draw(display);

        // Centred label above the bar
        let label_x = (bar_x1 + (bar_px - label_px) / 2).max(0);
        text(display, &label, label_x, bar_y - 4, &FONT_6X12, Rgb565::WHITE);
    }

    // ── Text band under the map: two columns of two rows ─────────────────────
    fill(display, 0, TEXT_Y, SCREEN_W, SCREEN_H - TEXT_Y, Rgb565::BLACK);
    let (l1, l2, l3, l4, c1);
    let zoom = crate::map_display::get_zoom();
    if let Some(e) = eew.as_ref() {
        // Active EEW takes priority over everything else.
        c1 = Rgb565::RED;
        l1 = format!("EEW {} Imax {} {}", fmt_mag(e.mag), e.intensity(), fmt_depth(e.depth));
        l2 = crate::place::describe(e.lat, e.lon);
        l3 = if e.lat.is_nan() { "Epicenter unknown".to_string() } else { format!("{} Z{}", fmt_latlon(e.lat, e.lon), zoom) };
        l4 = format!("Issued {}", e.time.get(11..19).unwrap_or(&e.time));
    } else if let Some(q) = newq.as_ref() {
        c1 = Rgb565::YELLOW;
        l1 = format!("NEW {} Imax {} {}", fmt_mag(q.mag), q.intensity(), fmt_depth(q.depth));
        l2 = crate::place::describe(q.lat, q.lon);
        l3 = format!("{} Z{}", fmt_latlon(q.lat, q.lon), zoom);
        l4 = format!("{} {}", q.time.get(5..16).unwrap_or(&q.time), tsunami_short(&q.tsunami));
    } else if let Some(q) = quake_focus.and_then(|(fl, fo)| {
        crate::quake::get().into_iter().find(|q| (q.lat - fl).abs() < 1e-6 && (q.lon - fo).abs() < 1e-6)
    }) {
        c1 = Rgb565::CSS_CYAN;
        l1 = format!("{} Imax {} {}", fmt_mag(q.mag), q.intensity(), fmt_depth(q.depth));
        l2 = crate::place::describe(q.lat, q.lon);
        l3 = format!("{} Z{}", fmt_latlon(q.lat, q.lon), zoom);
        l4 = format!("{} {}", q.time.get(5..16).unwrap_or(&q.time), tsunami_short(&q.tsunami));
    } else {
        c1 = Rgb565::CSS_LIGHT_GREEN;
        let quakes = crate::quake::get();
        l1 = "Japan overview".to_string();
        l2 = format!("Zoom {}  Quakes {}", zoom, quakes.len());
        l3 = match quakes.first() {
            Some(q) => format!("Latest {} {} I{}", q.time.get(5..16).unwrap_or(&q.time), fmt_mag(q.mag), q.intensity()),
            None => if !crate::quake::enabled() { "Quake feed disabled".to_string() }
                    else if crate::quake::last_ok_secs() == 0 { "Waiting for quake data".to_string() }
                    else { "No events".to_string() },
        };
        l4 = String::new();
    }
    text(display, &l1, TEXT_COL_X[0], TEXT_Y + 14, &FONT_8X13, c1);
    text(display, &l2, TEXT_COL_X[0], TEXT_Y + 30, &FONT_8X13, Rgb565::WHITE);
    text(display, &l3, TEXT_COL_X[1], TEXT_Y + 14, &FONT_8X13, Rgb565::WHITE);
    text(display, &l4, TEXT_COL_X[1], TEXT_Y + 30, &FONT_8X13, Rgb565::WHITE);

    // ── One-line top bar: title, button legend (battery is drawn by the caller) ──
    fill(display, 0, 0, SCREEN_W, MAP_BAR_H, Rgb565::BLACK);
    let (title, tcol) = if let Some(e) = eew.as_ref() {
        (format!("!! EEW M{:.1} I{} !!", e.mag.max(0.0), e.intensity()), Rgb565::RED)
    } else if let Some(q) = newq.as_ref() {
        (format!("NEW QUAKE M{:.1} I{}", q.mag.max(0.0), q.intensity()), Rgb565::YELLOW)
    } else {
        (format!("MAP {}/{}", PAGE_MAP + 1, PAGE_COUNT), Rgb565::CSS_LIGHT_GREEN)
    };
    text(display, &title, 4, 15, &FONT_8X13, tcol);
    let legend: &[Legend] = if eew.is_some() || newq.is_some() {
        &[Legend::Page("EXIT"), Legend::Up, Legend::Down, Legend::Action("TYPE")]
    } else {
        &[Legend::Page("PAGE"), Legend::Up, Legend::Down, Legend::Action("TYPE")]
    };
    draw_legend(display, 190, 15, legend);
    true
}

fn tsunami_short(t: &str) -> &'static str {
    match t {
        "Watch" | "Warning" => "TSUNAMI!",
        "Checking" => "tsunami: checking",
        _ => "",
    }
}

// ─── Earthquake page ─────────────────────────────────────────────────────
//
// Recent earthquakes from the P2P地震情報 API (see quake.rs), one line per
// event (time/M/intensity/depth, epicenter coordinates, tsunami flag) and a
// details line for the highlighted one; the font is ASCII-only, so the
// Japanese hypocenter name is shown on the Web UI only. +UP/-DOWN move the
// highlight, the red button centres the MAP page on the highlighted epicenter
// — see btn_ctrl.rs.

const QUAKE_STATUS_Y: i32 = HEADER_H + 1;
const QUAKE_STATUS_H: i32 = 24;
const QUAKE_ROW_Y: i32 = QUAKE_STATUS_Y + QUAKE_STATUS_H;
const QUAKE_ROW_H: i32 = 28;
/// List rows visible at once; longer lists scroll to keep the highlight in view.
const QUAKE_ROWS: usize = 8;
const QUAKE_DETAIL_Y: i32 = QUAKE_ROW_Y + QUAKE_ROWS as i32 * QUAKE_ROW_H + 2;

/// Index of the first visible list row.
static QUAKE_TOP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub(crate) fn draw_quake_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_slots: &mut Vec<String>,
) {
    let quakes = crate::quake::get();
    let sel = crate::quake::selection().min(quakes.len().saturating_sub(1));

    // Scroll just enough to keep the highlighted row visible.
    let mut top = QUAKE_TOP.load(Ordering::Relaxed).min(quakes.len().saturating_sub(QUAKE_ROWS));
    if sel < top { top = sel; }
    if sel >= top + QUAKE_ROWS { top = sel + 1 - QUAKE_ROWS; }
    QUAKE_TOP.store(top, Ordering::Relaxed);

    // Each slot is redrawn only when its signature string changes:
    // [0] status line, [1..=QUAKE_ROWS] list rows, [QUAKE_ROWS+1] details line.
    let n_slots = QUAKE_ROWS + 2;
    let mut sigs: Vec<String> = Vec::with_capacity(n_slots);

    let feed = if !crate::quake::enabled() { "feed off" }
               else if crate::quake::ws_connected() { "live" }
               else if crate::quake::last_ok_secs() == 0 { "waiting" }
               else { "polling" };
    sigs.push(if quakes.is_empty() {
        format!("Recent quakes: 0  [{}]", feed)
    } else {
        format!("Recent quakes: {}  [{}]  #{}/{}", quakes.len(), feed, sel + 1, quakes.len())
    });

    for i in 0..QUAKE_ROWS {
        sigs.push(match quakes.get(top + i) {
            Some(q) => format!("{}|{}|{}|{}|{}|{}|{:.2}|{:.2}|{}", top + i == sel, top + i, q.time, q.mag, q.scale, q.depth, q.lat, q.lon, q.tsunami),
            None if i == 0 && quakes.is_empty() => {
                let msg = if !crate::quake::enabled() { "(disabled: set quake_enable in cfg.toml)" }
                          else if crate::quake::last_ok_secs() == 0 { "(waiting for data...)" } else { "(no events)" };
                format!("msg|{}", msg)
            }
            None => String::new(),
        });
    }
    sigs.push(match quakes.get(sel) {
        Some(q) => format!("{}|{}|{}|{}|{:.2}|{:.2}|{}", q.time, q.mag, q.scale, q.depth, q.lat, q.lon, q.tsunami),
        None => String::new(),
    });

    let changed = |i: usize| force || prev_slots.len() != sigs.len() || prev_slots[i] != sigs[i];

    // Status line
    if changed(0) {
        fill(display, 0, QUAKE_STATUS_Y, SCREEN_W, QUAKE_STATUS_H, Rgb565::BLACK);
        text(display, &sigs[0], 4, QUAKE_STATUS_Y + 17, &FONT_8X13, Rgb565::CSS_LIGHT_GREEN);
        text(display, "JMA via P2P", SCREEN_W - 11 * INFO_CHAR_W - 4, QUAKE_STATUS_Y + 17, &FONT_8X13, DIM);
    }

    // List rows
    for i in 0..QUAKE_ROWS {
        if !changed(1 + i) { continue; }
        let y = QUAKE_ROW_Y + i as i32 * QUAKE_ROW_H;
        let selected = top + i == sel && top + i < quakes.len();
        fill(display, 0, y, SCREEN_W, QUAKE_ROW_H, if selected { SEL_BG } else { Rgb565::BLACK });
        match quakes.get(top + i) {
            Some(q) => {
                let col = quake_color(q.scale, q.mag);
                if selected { fill(display, 0, y, 3, QUAKE_ROW_H, col); }
                let t = q.time.get(5..16).unwrap_or(&q.time); // "10/10 10:20"
                let l1 = format!("{} {} I{} {}", t, fmt_mag(q.mag), q.intensity(), fmt_depth(q.depth));
                text(display, &l1, 8, y + 21, &FONT_10X20, col);
                text(display, &fmt_latlon(q.lat, q.lon), 290, y + 19, &FONT_8X13, DIM);
                let tsu = tsunami_short(&q.tsunami);
                if !tsu.is_empty() {
                    text(display, "TSUNAMI!", SCREEN_W - 8 * INFO_CHAR_W - 4, y + 19, &FONT_8X13, Rgb565::CSS_ORANGE);
                }
            }
            None => {
                if let Some(msg) = sigs[1 + i].strip_prefix("msg|") {
                    text(display, msg, 8, y + 19, &FONT_8X13, DIM);
                }
            }
        }
    }

    // Details of the highlighted event
    if changed(n_slots - 1) {
        let h = LEGEND_Y - QUAKE_DETAIL_Y;
        fill(display, 0, QUAKE_DETAIL_Y, SCREEN_W, h, Rgb565::BLACK);
        fill(display, 0, QUAKE_DETAIL_Y - 1, SCREEN_W, 1, Rgb565::CSS_DARK_GREEN);
        if let Some(q) = quakes.get(sel) {
            let l = format!("{} Imax {} Depth {} {} Tsu:{}",
                fmt_mag(q.mag), q.intensity(), fmt_depth(q.depth), fmt_latlon(q.lat, q.lon), q.tsunami);
            text(display, &l, 4, QUAKE_DETAIL_Y + 14, &FONT_8X13, quake_color(q.scale, q.mag));
        }
    }
    *prev_slots = sigs;

    if force {
        draw_legend_row(display, &[Legend::Page("PAGE"), Legend::Up, Legend::Down, Legend::Action("MAP")], SCREEN_W);
    }
}

// ─── Info page ───────────────────────────────────────────────────────────

pub(crate) fn draw_info_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
) {
    let st = |c: Rgb565| MonoTextStyleBuilder::new().font(&FONT_8X13).text_color(c).build();
    let section_style = st(Rgb565::CSS_LIGHT_GREEN);
    let label_style = st(CYAN);
    let val_style = st(Rgb565::WHITE);
    let val_yellow = st(Rgb565::YELLOW);
    let val_on = st(Rgb565::GREEN);
    let val_off = st(Rgb565::new(21, 0, 0));  // dim red
    let val_disabled = st(DIM);

    // Collect data
    let (ip, ssid, rssi, battery, chip_temp) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            let ip = aux.status.ip_address.lock().unwrap().clone();
            let ssid = aux.status.ssid.lock().unwrap().clone();
            let rssi = *aux.status.rssi.lock().unwrap();
            let battery = *aux.status.battery_voltage.lock().unwrap();
            let chip_temp = *aux.status.chip_temp.lock().unwrap();
            (ip, ssid, rssi, battery, chip_temp)
        } else {
            ("--".into(), "--".into(), 0i32, 0.0f32, 0.0f32)
        }
    };

    // Each line: Vec<(text, style_index)>
    // style_index: 0=section, 1=label, 2=val, 3=yellow, 4=on, 5=off, 6=disabled
    type Seg = (String, u8);
    // Two columns: left = network / power / memory, right = quake feed / display / firmware.
    let mut left: Vec<Vec<Seg>> = Vec::new();
    let mut right: Vec<Vec<Seg>> = Vec::new();

    // ── WiFi / Network ───────────────────
    left.push(vec![("-- Network --".into(), 0)]);
    left.push(vec![("IP:   ".into(), 1), (ip.clone(), 3)]);
    left.push(vec![("WiFi: ".into(), 1), (ssid.chars().take(20).collect::<String>(), 2)]);
    left.push(vec![
        ("RSSI: ".into(), 1),
        (format!("{}dBm", rssi), if rssi > -60 { 4 } else if rssi > -80 { 3 } else { 5 }),
    ]);

    // ── Power / chip ─────────────────────
    left.push(vec![("-- Power --".into(), 0)]);
    left.push(vec![("Battery:  ".into(), 1), (format!("{:.2}V", battery), 3)]);
    left.push(vec![
        ("ChipTemp: ".into(), 1),
        (format!("{:.1}C", chip_temp),
         if chip_temp < 70.0 { 4 } else if chip_temp < 85.0 { 3 } else { 5 }),
    ]);

    // ── Memory ───────────────────────────
    let mem = crate::httpserver::mem_info();
    left.push(vec![("-- Memory --".into(), 0)]);
    left.push(vec![
        ("Int:   ".into(), 1), (format!("{}KB", mem.int_free / 1024), 4),
        (" min ".into(), 1), (format!("{}KB", mem.int_min / 1024), 2),
    ]);
    left.push(vec![
        ("PSRAM: ".into(), 1), (format!("{}KB", mem.psram_free / 1024), 4),
        (" min ".into(), 1), (format!("{}KB", mem.psram_min / 1024), 2),
    ]);

    // ── Earthquake feed ──────────────────
    right.push(vec![("-- Earthquake feed --".into(), 0)]);
    if crate::quake::enabled() {
        let ws = crate::quake::ws_connected();
        right.push(vec![
            ("P2P WebSocket: ".into(), 1),
            (if ws { "connected".into() } else { "polling".into() }, if ws { 4 } else { 3 }),
        ]);
        right.push(vec![("Events: ".into(), 1), (format!("{}", crate::quake::get().len()), 2)]);
        right.push(vec![
            ("Maps API key: ".into(), 1),
            if crate::map_display::has_api_key() { ("set".into(), 4) } else { ("NOT SET".into(), 5) },
        ]);
    } else {
        right.push(vec![("disabled (quake_enable)".into(), 6)]);
    }

    // ── Display ──────────────────────────
    right.push(vec![("-- Display --".into(), 0)]);
    right.push(vec![("ILI9488 ".into(), 1), ("480x320 SPI".into(), 2)]);

    // ── Firmware ─────────────────────────
    right.push(vec![("-- Firmware --".into(), 0)]);
    right.push(vec![("Ver:   ".into(), 1), (crate::httpserver::FW_VERSION.to_string(), 3)]);
    right.push(vec![("Build: ".into(), 1), (crate::httpserver::FW_BUILD.to_string(), 2)]);

    // ── Draw, skipping rows whose text did not change ────────────────────────
    const COL_X: [i32; 2] = [0, 216];
    let max_rows = ((LEGEND_Y - HEADER_H - 2) / INFO_ROW_H) as usize;
    let mut text_strs: Vec<String> = Vec::new();
    for (ci, rows) in [&left, &right].into_iter().enumerate() {
        for (i, segs) in rows.iter().enumerate() {
            let line: String = segs.iter().map(|(t, _)| t.as_str()).collect();
            let idx = text_strs.len();
            text_strs.push(line);
            if i >= max_rows { continue; }
            let changed = force || idx >= prev_lines.len() || prev_lines[idx] != text_strs[idx];
            if !changed { continue; }

            let y = HEADER_H + 2 + (i as i32) * INFO_ROW_H;
            fill(display, COL_X[ci], y, if ci == 0 { 216 } else { SCREEN_W - 216 }, INFO_ROW_H, Rgb565::BLACK);

            if segs.len() == 1 && segs[0].1 == 0 {
                fill(display, COL_X[ci], y, if ci == 0 { 212 } else { SCREEN_W - 220 }, 1, Rgb565::new(0, 15, 0));
            }

            let mut x = COL_X[ci] + 4;
            for (t, sty) in segs {
                let style = match sty {
                    0 => section_style,
                    1 => label_style,
                    3 => val_yellow,
                    4 => val_on,
                    5 => val_off,
                    6 => val_disabled,
                    _ => val_style,
                };
                let _ = Text::new(t, Point::new(x, y + 14), style).draw(display);
                x += t.len() as i32 * INFO_CHAR_W;
            }
        }
    }
    *prev_lines = text_strs;

    if force {
        draw_legend_row(display, &[Legend::Page("PAGE")], SCREEN_W);
    }
}
