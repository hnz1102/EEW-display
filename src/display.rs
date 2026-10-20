// ILI9488 480x320 landscape SPI TFT: driver, RAM framebuffer and the display thread.
//
// The pages in `display_pages` draw into a 480x320 RGB565 framebuffer (PSRAM).
// After each pass only the bounding box of what was actually drawn is sent to
// the panel, converted to the 18-bit RGB666 format the ILI9488 requires over
// SPI (3 bytes per pixel).
//
// Wiring (ESP32-S3 → TFT module):
//   GPIO10 → CS      GPIO9  → RST     GPIO46 → D/C
//   GPIO3  → SDI     (the panel's data input = ESP32 MOSI)
//   GPIO8  → SCK     GPIO18 → BL
//   GPIO17 → SDO     (the panel's data output = MISO; the panel is write-only
//                     here, so the pin is left unconfigured)
//
// SPDX-License-Identifier: MIT

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use esp_idf_hal::{gpio::*, spi, delay::FreeRtos};
use esp_idf_hal::spi::config::MODE_0;
use esp_idf_hal::spi::{Dma, SpiDeviceDriver, SpiDriver};
use esp_idf_hal::units::FromValueType;
use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::{raw::RawU16, Rgb565},
    prelude::*,
    primitives::Rectangle,
};

pub use crate::display_pages::*;

pub const LCD_W: usize = SCREEN_W as usize;
pub const LCD_H: usize = SCREEN_H as usize;

/// SPI clock. The driver rounds this down to what the 80 MHz APB divides to
/// (26.67 MHz). The ILI9488 is only specified to ~15 MHz on paper, but modules
/// run reliably at 27 MHz; raise to 40 MHz at your own risk, lower it if the
/// picture shows noise.
const SPI_HZ: u32 = 27_000_000;

/// Memory access control (0x36): MV | BGR = landscape (480x320). Use 0xE8
/// (MY | MX | MV | BGR) to turn the picture 180 degrees, and drop the 0x08 bit
/// if red and blue come out swapped.
const MADCTL: u8 = 0x28;

/// Order in which the R, G, B channels (indices 0, 1, 2) are sent for each
/// pixel. [0, 1, 2] is normal; change it only if hues come out rotated.
const CH_ORDER: [usize; 3] = [0, 1, 2];

/// Panel inversion command (INVON/INVOFF). This IPS module is correct with it off.
const INVERT: bool = false;

/// Invert every colour byte in software, for a panel that shows the complement
/// of what it is sent. This module does not need it.
const SW_INVERT: bool = false;

/// Bytes sent to the panel per SPI transaction (a multiple of 3, ≤ 4096 so the
/// buffer stays in internal RAM where DMA can reach it).
const CHUNK_BYTES: usize = 4092;

// ─── ILI9488 + framebuffer ────────────────────────────────────────────────

struct Ili9488 {
    spi: SpiDeviceDriver<'static, SpiDriver<'static>>,
    dc: PinDriver<'static, AnyOutputPin, Output>,
    /// RGB565, row-major, `LCD_W` × `LCD_H`.
    fb: Vec<u16>,
    /// Bounding box (x0, y0, x1, y1, inclusive) of what changed since the last flush.
    dirty: Option<(usize, usize, usize, usize)>,
    chunk: Box<[u8; CHUNK_BYTES]>,
}

impl Ili9488 {
    fn command(&mut self, cmd: u8, data: &[u8]) {
        let _ = self.dc.set_low();
        let _ = self.spi.write(&[cmd]);
        if !data.is_empty() {
            let _ = self.dc.set_high();
            let _ = self.spi.write(data);
        }
    }

    fn init(&mut self, rst: &mut PinDriver<'static, AnyOutputPin, Output>) {
        // Hardware reset
        let _ = rst.set_high();
        FreeRtos::delay_ms(10);
        let _ = rst.set_low();
        FreeRtos::delay_ms(20);
        let _ = rst.set_high();
        FreeRtos::delay_ms(150);

        self.command(0x01, &[]);                      // software reset
        FreeRtos::delay_ms(150);
        self.command(0xE0, &[0x00, 0x03, 0x09, 0x08, 0x16, 0x0A, 0x3F, 0x78,
                             0x4C, 0x09, 0x0A, 0x08, 0x16, 0x1A, 0x0F]); // positive gamma
        self.command(0xE1, &[0x00, 0x16, 0x19, 0x03, 0x0F, 0x05, 0x32, 0x45,
                             0x46, 0x04, 0x0E, 0x0D, 0x35, 0x37, 0x0F]); // negative gamma
        self.command(0xC0, &[0x17, 0x15]);            // power control 1
        self.command(0xC1, &[0x41]);                  // power control 2
        self.command(0xC5, &[0x00, 0x12, 0x80]);      // VCOM
        self.command(0x36, &[MADCTL]);                // memory access control
        self.command(0x3A, &[0x66]);                  // 18 bits per pixel
        self.command(0xB0, &[0x00]);                  // interface mode
        self.command(0xB1, &[0xA0]);                  // frame rate 60 Hz
        self.command(0xB4, &[0x02]);                  // display inversion control: 2-dot
        self.command(0xB6, &[0x02, 0x02]);            // display function control
        self.command(0xE9, &[0x00]);                  // set image function: 24-bit off
        self.command(0xF7, &[0xA9, 0x51, 0x2C, 0x82]);// adjust control 3
        self.command(if INVERT { 0x21 } else { 0x20 }, &[]);
        self.command(0x11, &[]);                      // sleep out
        FreeRtos::delay_ms(150);
        self.command(0x29, &[]);                      // display on
        FreeRtos::delay_ms(20);
    }

    fn mark_all_dirty(&mut self) { self.mark_dirty(0, 0, LCD_W - 1, LCD_H - 1); }

    fn mark_dirty(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        self.dirty = Some(match self.dirty {
            None => (x0, y0, x1, y1),
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
        });
    }

    /// Send everything drawn since the last flush to the panel.
    fn flush(&mut self) {
        let Some((x0, y0, x1, y1)) = self.dirty.take() else { return };
        self.command(0x2A, &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]);
        self.command(0x2B, &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]);
        self.command(0x2C, &[]);
        let _ = self.dc.set_high();

        let w = x1 - x0 + 1;
        let mut n = 0usize;
        for y in y0..=y1 {
            let row = &self.fb[y * LCD_W + x0..y * LCD_W + x0 + w];
            for &p in row {
                // RGB565 → 8-bit channels; the panel keeps the top 6 bits of each.
                let r = ((p >> 11) & 0x1F) as u8;
                let g = ((p >> 5) & 0x3F) as u8;
                let b = (p & 0x1F) as u8;
                let c = [(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)];
                let x = if SW_INVERT { 0xFF } else { 0x00 };
                self.chunk[n]     = c[CH_ORDER[0]] ^ x;
                self.chunk[n + 1] = c[CH_ORDER[1]] ^ x;
                self.chunk[n + 2] = c[CH_ORDER[2]] ^ x;
                n += 3;
                if n == CHUNK_BYTES {
                    let _ = self.spi.write(&self.chunk[..n]);
                    n = 0;
                }
            }
        }
        if n > 0 {
            let _ = self.spi.write(&self.chunk[..n]);
        }
    }
}

impl OriginDimensions for Ili9488 {
    fn size(&self) -> Size { Size::new(LCD_W as u32, LCD_H as u32) }
}

impl DrawTarget for Ili9488 {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Infallible>
    where I: IntoIterator<Item = Pixel<Rgb565>> {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for Pixel(p, c) in pixels {
            if p.x < 0 || p.y < 0 || p.x >= LCD_W as i32 || p.y >= LCD_H as i32 { continue; }
            let (x, y) = (p.x as usize, p.y as usize);
            self.fb[y * LCD_W + x] = RawU16::from(c).into_inner();
            x0 = x0.min(x); y0 = y0.min(y); x1 = x1.max(x); y1 = y1.max(y);
        }
        if x0 != usize::MAX { self.mark_dirty(x0, y0, x1, y1); }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Infallible>
    where I: IntoIterator<Item = Rgb565> {
        let clip = area.intersection(&Rectangle::new(Point::zero(), self.size()));
        if clip.size.width == 0 || clip.size.height == 0 { return Ok(()); }
        let mut it = colors.into_iter();
        if clip == *area {
            let (x, y) = (area.top_left.x as usize, area.top_left.y as usize);
            let (w, h) = (area.size.width as usize, area.size.height as usize);
            for row in 0..h {
                let base = (y + row) * LCD_W + x;
                for px in &mut self.fb[base..base + w] {
                    match it.next() {
                        Some(c) => *px = RawU16::from(c).into_inner(),
                        None => break,
                    }
                }
            }
        } else {
            for p in area.points() {
                let Some(c) = it.next() else { break };
                if p.x >= 0 && p.y >= 0 && (p.x as usize) < LCD_W && (p.y as usize) < LCD_H {
                    self.fb[p.y as usize * LCD_W + p.x as usize] = RawU16::from(c).into_inner();
                }
            }
        }
        let (x, y) = (clip.top_left.x as usize, clip.top_left.y as usize);
        self.mark_dirty(x, y, x + clip.size.width as usize - 1, y + clip.size.height as usize - 1);
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Rgb565) -> Result<(), Infallible> {
        let clip = area.intersection(&Rectangle::new(Point::zero(), self.size()));
        if clip.size.width == 0 || clip.size.height == 0 { return Ok(()); }
        let raw = RawU16::from(color).into_inner();
        let (x, y) = (clip.top_left.x as usize, clip.top_left.y as usize);
        let (w, h) = (clip.size.width as usize, clip.size.height as usize);
        for row in y..y + h {
            self.fb[row * LCD_W + x..row * LCD_W + x + w].fill(raw);
        }
        self.mark_dirty(x, y, x + w - 1, y + h - 1);
        Ok(())
    }

    fn clear(&mut self, color: Rgb565) -> Result<(), Infallible> {
        self.fb.fill(RawU16::from(color).into_inner());
        self.mark_dirty(0, 0, LCD_W - 1, LCD_H - 1);
        Ok(())
    }
}

// ─── Screen saver: backlight off after a period of inactivity ───────────────

/// Idle seconds before the backlight goes off; 0 = never.
static IDLE_TIMEOUT_SECS: AtomicU32 = AtomicU32::new(0);
/// Something happened (button, EEW, new earthquake): restart the idle timer / wake up.
static WAKE: AtomicBool = AtomicBool::new(false);
/// The backlight is currently off.
static SCREEN_OFF: AtomicBool = AtomicBool::new(false);

/// Set the idle timeout in seconds (0 = the screen stays on). Applies at once.
pub fn set_idle_timeout(secs: u32) { IDLE_TIMEOUT_SECS.store(secs, Ordering::Relaxed); }

/// Restart the idle timer, turning the screen back on if it is off.
pub fn wake() { WAKE.store(true, Ordering::Relaxed); }

/// Like [`wake`], but tells whether the screen was off. The buttons use this to
/// swallow the press that only wakes the screen.
pub fn wake_if_off() -> bool {
    let was_off = SCREEN_OFF.load(Ordering::Relaxed);
    wake();
    was_off
}

// ─── Display thread ───────────────────────────────────────────────────────

/// Start the display thread. Call once from main after peripherals init.
pub fn start_display_thread(
    spi_periph: spi::SPI3,
    sclk: Gpio8,
    mosi: Gpio3,
    cs: Gpio10,
    dc_pin: Gpio46,
    rst_pin: Gpio9,
    bl_pin: Gpio18,
) {
    thread::Builder::new()
        .name("display".into())
        .stack_size(24576)
        .spawn(move || display_task(spi_periph, sclk, mosi, cs, dc_pin, rst_pin, bl_pin))
        .expect("spawn display thread");
}

fn display_task(
    spi_periph: spi::SPI3,
    sclk: Gpio8,
    mosi: Gpio3,
    cs: Gpio10,
    dc_pin: Gpio46,
    rst_pin: Gpio9,
    bl_pin: Gpio18,
) {
    let dc = PinDriver::output(dc_pin.downgrade_output()).unwrap();
    let mut rst = PinDriver::output(rst_pin.downgrade_output()).unwrap();
    // Backlight stays off until the first (black) frame is on the panel.
    let mut bl = PinDriver::output(bl_pin.downgrade_output()).unwrap();
    let _ = bl.set_low();

    let spi_config = spi::SpiConfig::new().baudrate(SPI_HZ.Hz().into()).data_mode(MODE_0);
    let spi_driver_config = spi::SpiDriverConfig {
        dma: Dma::Auto(4096),
        ..Default::default()
    };
    let sdi_not_used: Option<Gpio17> = None;

    let spi_driver = SpiDriver::new(spi_periph, sclk, mosi, sdi_not_used, &spi_driver_config).unwrap();
    let spi_device = SpiDeviceDriver::new(spi_driver, Some(cs), &spi_config).unwrap();

    let mut display = Ili9488 {
        spi: spi_device,
        dc,
        fb: vec![0u16; LCD_W * LCD_H],
        dirty: None,
        chunk: Box::new([0u8; CHUNK_BYTES]),
    };
    display.init(&mut rst);
    display.clear(Rgb565::BLACK).unwrap();
    display.flush();
    let _ = bl.set_high();

    println!("Display initialized (ILI9488 {}x{}, {} Hz)", LCD_W, LCD_H, SPI_HZ);
    println!("Display config: MADCTL={:#04x} INVERT={} SW_INVERT={} CH_ORDER={:?}", MADCTL, INVERT, SW_INVERT, CH_ORDER);

    let header_style = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_GREEN);

    let mut force_full_redraw = true;
    let mut last_page = current_page();
    let mut prev_info_lines: Vec<String> = Vec::new();
    let mut prev_quake_slots: Vec<String> = Vec::new();
    let mut prev_map_sig: u32 = 0xFFFF_FFFF;
    // Track previous system message to detect changes
    let mut prev_system_message: Option<(String, String)> = None;
    let mut last_eew_ver = crate::quake::eew_version();
    let mut last_newq_ver = crate::quake::newq_version();
    let mut batt_level: u8 = BATT_NONE;
    let mut last_net: (u8, u8) = (255, 255);
    let mut last_activity = Instant::now();
    let mut screen_off = false;

    loop {
        // ── Page change detection ─────────────────────────────────────
        let page = current_page();
        if page != last_page {
            last_page = page;
            force_full_redraw = true;
        }

        // An EEW appearing/ending changes the header and the whole map overlay.
        let eew_ver = crate::quake::eew_version();
        let newq_ver = crate::quake::newq_version();
        if eew_ver != last_eew_ver || newq_ver != last_newq_ver {
            last_eew_ver = eew_ver;
            last_newq_ver = newq_ver;
            force_full_redraw = true;
            WAKE.store(true, Ordering::Relaxed); // an alert wakes the screen
        }
        if SYSTEM_MESSAGE.lock().unwrap().is_some() { WAKE.store(true, Ordering::Relaxed); }

        // ── Screen saver ──────────────────────────────────────────────
        if WAKE.swap(false, Ordering::Relaxed) { last_activity = Instant::now(); }
        let timeout = IDLE_TIMEOUT_SECS.load(Ordering::Relaxed);
        let want_off = timeout > 0 && last_activity.elapsed().as_secs() >= timeout as u64;
        if want_off != screen_off {
            screen_off = want_off;
            SCREEN_OFF.store(screen_off, Ordering::Relaxed);
            if screen_off {
                let _ = bl.set_low();
                println!("Screen off after {} s idle", timeout);
            } else {
                // Everything drawn while it was off was not sent: send it all now.
                display.mark_all_dirty();
                display.flush();
                let _ = bl.set_high();
            }
        }

        let mut map_redrawn = false;

        // ── Check for system message overlay ──────────────────────────
        let sys_msg = SYSTEM_MESSAGE.lock().unwrap().clone();

        if let Some((title, text)) = sys_msg.clone() {
            let msg_changed = match &prev_system_message {
                Some((prev_title, prev_text)) => prev_title != &title || prev_text != &text,
                None => true, // First time showing system message
            };
            // System message is active — draw it, force clear only if changed
            draw_system_message(&mut display, &title, &text, &header_style, msg_changed);
            prev_system_message = Some((title, text));
        } else {
            // If we just exited system message mode, force redraw
            if prev_system_message.is_some() {
                force_full_redraw = true;
                prev_system_message = None;
            }

            if force_full_redraw {
                display.clear(Rgb565::BLACK).unwrap();
                // The map page draws its own one-line top bar.
                if page != PAGE_MAP { draw_header(&mut display, page, &header_style); }
                prev_info_lines.clear();
                prev_quake_slots.clear();
                prev_map_sig = 0xFFFF_FFFF;
            }

            match page {
                PAGE_QUAKE => draw_quake_page(&mut display, force_full_redraw, &mut prev_quake_slots),
                PAGE_MAP   => map_redrawn = draw_map_page(&mut display, force_full_redraw, &mut prev_map_sig),
                PAGE_INFO  => draw_info_page(&mut display, force_full_redraw, &mut prev_info_lines),
                _ => {}
            }
        }

        // ── Battery icon (top-right of header) ─────────────────────────
        // Redrawn on a full redraw (the header was just cleared) or when the
        // level changes; skipped while a system message owns the header.
        let batt_v = battery_voltage();
        let new_level = next_battery_level(batt_level, batt_v);
        if sys_msg.is_none() && (force_full_redraw || map_redrawn || new_level != batt_level) {
            draw_battery_icon(&mut display, new_level, page == PAGE_MAP);
        }
        batt_level = new_level;

        // ── Wi-Fi / WebSocket icons (every page, left of the battery) ──
        // Redrawn when the page was repainted or when either state changed.
        let net = net_status();
        if sys_msg.is_none() && (force_full_redraw || map_redrawn || net != last_net) {
            draw_status_icons(&mut display, net.0, net.1, page == PAGE_MAP);
        }
        last_net = net;

        // While the backlight is off nothing is sent (the changes pile up in the
        // dirty rectangle and go out in one piece when the screen wakes).
        if !screen_off { display.flush(); }
        force_full_redraw = false;
        thread::sleep(Duration::from_millis(50));
    }
}
