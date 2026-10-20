//! Renders each TFT page (480x320) to a PNG on the host, using the firmware's
//! own drawing code (`src/display_pages.rs`) with stubbed data sources.
//!
//!   cargo run --release -- [OUT_DIR] [SCALE]     (defaults: ../../docs/images/tft, 1)
//!
//! The sample data is synthetic (made-up quakes); the map is a Google Static
//! Maps tile if a key is configured (see `map_display.rs`), else a schematic
//! stand-in. Set SCREENSHOTS_OFFLINE=1 to never call the Maps API.
#![allow(dead_code)]

#[path = "../../../src/display_pages.rs"]
mod display_pages;
#[path = "../../../src/place.rs"]
mod place;
mod httpserver;
mod map_display;
mod quake;
mod wifi;

use std::convert::Infallible;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use embedded_graphics::{
    mono_font::{ascii::FONT_6X12, ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::Rectangle,
    text::Text,
};

/// Caption band (device pixels) added below the image to mark the synthetic data.
const CAP_H: u32 = 18;
/// Note printed in that band on the pages that show earthquake data.
const SAMPLE_NOTE: &str = "Sample earthquake data (not live) - for layout only";

use display_pages::*;

const W: u32 = 480;
const H: u32 = 320;

/// In-memory 480x320 RGB565 framebuffer standing in for the ILI9488, with room
/// for a caption band of `CAP_H` rows below it (outside what the pages can draw
/// on). The band is only part of the saved image when `caption` is set.
struct Fb(Vec<Rgb565>, bool);

/// Draws into the caption band: (0, 0) is the band's top-left corner.
struct Band<'a>(&'a mut Fb);

impl OriginDimensions for Band<'_> {
    fn size(&self) -> Size { Size::new(W, CAP_H) }
}

impl DrawTarget for Band<'_> {
    type Color = Rgb565;
    type Error = Infallible;
    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && (p.x as u32) < W && (p.y as u32) < CAP_H {
                self.0 .0[((H + p.y as u32) * W + p.x as u32) as usize] = c;
            }
        }
        Ok(())
    }
}

impl Fb {
    fn new() -> Self { Fb(vec![Rgb565::BLACK; (W * (H + CAP_H)) as usize], false) }

    /// Mark the picture as showing sample data: adds a caption band below it.
    fn with_sample_note(mut self) -> Self {
        let mut band = Band(&mut self);
        let _ = band.clear(Rgb565::new(2, 4, 2));
        let style = MonoTextStyle::new(&FONT_6X12, Rgb565::CSS_ORANGE);
        let x = ((W as i32) - SAMPLE_NOTE.len() as i32 * 6) / 2;
        let _ = Text::new(SAMPLE_NOTE, Point::new(x.max(2), 13), style).draw(&mut band);
        self.1 = true;
        self
    }

    fn save(&self, path: &PathBuf, scale: u32) {
        let (w, h) = (W * scale, (H + if self.1 { CAP_H } else { 0 }) * scale);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let p = self.0[((y / scale) * W + x / scale) as usize];
                // Expand 5/6/5 to 8 bits with bit replication (full-range white).
                let (r, g, b) = (p.r(), p.g(), p.b());
                rgb.push((r << 3) | (r >> 2));
                rgb.push((g << 2) | (g >> 4));
                rgb.push((b << 3) | (b >> 2));
            }
        }
        let mut enc = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        println!("wrote {} ({}x{})", path.display(), w, h);
    }
}

impl OriginDimensions for Fb {
    fn size(&self) -> Size { Size::new(W, H) }
}

impl DrawTarget for Fb {
    type Color = Rgb565;
    type Error = Infallible;
    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && (p.x as u32) < W && (p.y as u32) < H {
                self.0[(p.y as u32 * W + p.x as u32) as usize] = c;
            }
        }
        Ok(())
    }
    fn fill_solid(&mut self, area: &Rectangle, c: Rgb565) -> Result<(), Infallible> {
        self.fill_contiguous(area, core::iter::repeat(c))
    }
}

fn header_style() -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_GREEN)
}

/// What `display_task` does on a full redraw: clear, header, page, battery icon.
fn render(page: u8, batt: u8, body: impl FnOnce(&mut Fb)) -> Fb {
    let mut fb = Fb::new();
    if page != PAGE_MAP { draw_header(&mut fb, page, &header_style()); }
    body(&mut fb);
    draw_battery_icon(&mut fb, batt, page == PAGE_MAP);
    let (w, s) = net_status();
    draw_status_icons(&mut fb, w, s, page == PAGE_MAP);
    fb
}

// ─── Sample data ─────────────────────────────────────────────────────────────

fn sample_quakes() -> Vec<quake::Quake> {
    let q = |time: &str, lat, lon, depth, mag, scale, ts: &str| quake::Quake {
        time: time.into(), name: String::new(), lat, lon, depth, mag, scale, tsunami: ts.into(),
    };
    vec![
        q("2026/10/12 02:48:00", 35.70, 140.85, 40, 4.6, 30, "None"),
        q("2026/10/11 23:15:00", 37.50, 141.60, 50, 5.4, 45, "NonEffective"),
        q("2026/10/11 17:02:00", 33.20, 131.70, 10, 3.9, 20, "None"),
        q("2026/10/11 09:31:00", 36.30, 139.90, 60, 3.3, 20, "None"),
        q("2026/10/10 21:40:00", 43.00, 144.30, 90, 5.0, 40, "None"),
        q("2026/10/10 06:12:00", 34.90, 138.20, 20, 3.1, 10, "None"),
        q("2026/10/09 18:55:00", 38.20, 142.90, 30, 6.3, 55, "Watch"),
        q("2026/10/09 04:20:00", 32.10, 130.30, 0, 2.8, 10, "None"),
        q("2026/10/08 22:11:00", 39.00, 141.80, 70, 4.1, 30, "None"),
        q("2026/10/08 15:03:00", 34.40, 135.30, 10, 2.6, 10, "None"),
        q("2026/10/08 08:47:00", 36.00, 137.60, 5, 3.4, 20, "None"),
        q("2026/10/07 19:30:00", 42.80, 143.10, 60, 4.8, 40, "None"),
    ]
}

fn setup() {
    let status = httpserver::StatusInfo::new();
    *status.ip_address.lock().unwrap() = "192.168.1.42".into();
    *status.ssid.lock().unwrap() = "home-ap".into();
    *status.rssi.lock().unwrap() = -57;
    *status.battery_voltage.lock().unwrap() = 4.72;
    *status.chip_temp.lock().unwrap() = 41.5;
    set_display_aux(DisplayAux { status });
    httpserver::set_mem_info(httpserver::MemInfo {
        int_free: 118_000, int_min: 71_000, psram_free: 7_400_000, psram_min: 7_100_000,
    });
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out: PathBuf = args.next().map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/images/tft")));
    let scale: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    std::fs::create_dir_all(&out).unwrap();
    setup();
    // Pages that show (synthetic) earthquake data get a "sample data" caption band.
    const SAMPLE_PAGES: &[&str] = &["01-quake.png", "02-map-overview.png", "02b-map-overview-satellite.png",
        "03-map-quake-focus.png", "04-map-new-quake.png", "05-map-eew.png"];
    let save = |name: &str, fb: Fb| {
        let fb = if SAMPLE_PAGES.contains(&name) { fb.with_sample_note() } else { fb };
        fb.save(&out.join(name), scale)
    };
    let batt = 4; // 4 of 5 segments
    let (jp_lat, jp_lon, jp_zoom) = map_display::DEFAULT_VIEW;

    // 1. Earthquake list
    quake::set_quakes(sample_quakes(), 1);
    quake::set_selection(10);
    save("01-quake.png", render(PAGE_QUAKE, batt, |fb| {
        draw_quake_page(fb, true, &mut Vec::new());
    }));

    // 1b. Quake list with nothing received yet
    quake::set_quakes(Vec::new(), 0);
    save("01b-quake-waiting.png", render(PAGE_QUAKE, batt, |fb| {
        draw_quake_page(fb, true, &mut Vec::new());
    }));
    quake::set_quakes(sample_quakes(), 1);

    // 2. Map: Japan overview with the recent epicenters
    map_display::set_view(jp_lat, jp_lon, jp_zoom);
    save("02-map-overview.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF);
    }));

    // 2b. The same on satellite imagery (the firmware's map-type toggle)
    map_display::set_map_type_satellite(true);
    map_display::set_view(jp_lat, jp_lon, jp_zoom);
    save("02b-map-overview-satellite.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF);
    }));
    map_display::set_map_type_satellite(false);

    // 3. Map framed on a quake selected from the list
    let q = &sample_quakes()[1];
    let (clat, clon, zoom) = quake::fit_view(q.lat, q.lon, q.mag);
    quake::set_focus(Some((q.lat, q.lon)));
    map_display::set_view(clat, clon, zoom);
    save("03-map-quake-focus.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF);
    }));
    quake::set_focus(None);

    // 4. New earthquake announced on the map page
    quake::set_new_quake(Some(q.clone()));
    quake::set_focus(Some((q.lat, q.lon)));
    save("04-map-new-quake.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF);
    }));
    quake::set_new_quake(None);
    quake::set_focus(None);

    // 5. Earthquake early warning takes over the map page
    let eew = quake::Eew {
        time: "2026/10/12 03:22:10".into(), name: String::new(),
        lat: 35.30, lon: 139.95, depth: 30, mag: 6.1, max_scale: 55,
        areas: String::new(),
    };
    let (clat, clon, zoom) = quake::eew_view(&eew);
    quake::set_eew(Some(eew));
    map_display::set_view(clat, clon, zoom);
    save("05-map-eew.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF);
    }));
    quake::set_eew(None);

    // 6. System info
    save("06-info.png", render(PAGE_INFO, batt, |fb| {
        draw_info_page(fb, true, &mut Vec::new());
    }));

    // 7. System message overlay
    let mut fb = Fb::new();
    draw_system_message(&mut fb, "FACTORY RESET", "Erasing NVS...\nPlease wait...", &header_style(), true);
    save("07-system-message.png", fb);
}
