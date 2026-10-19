// Button control thread
//
// GPIO0 (BOOT button, active-low, internal pull-up):
//   Short press  (< 3 s)  : cycle display page
//   Long press   (>= 10 s) : factory reset to cfg.toml defaults
//
// GPIO38/GPIO39 (active-low, internal pull-up) are context-sensitive:
//   On the quake page : move the list highlight (GPIO38 = up, GPIO39 = down)
//   Otherwise         : MAP zoom in / out
//   Buttons debounced at 50 ms, with 400 ms initial delay then 300 ms repeat.
//
// GPIO40 (active-low, internal pull-up) is also context-sensitive:
//   On the quake page : show the highlighted epicenter on the MAP page
//   Otherwise         : MAP type toggle (satellite ↔ roadmap)

use std::thread;
use std::time::{Duration, Instant};
use log::info;
use esp_idf_hal::gpio::{Gpio0, Gpio38, Gpio39, Gpio40, PinDriver, Pull};

const FACTORY_RESET_PRESS_MS: u64 = 10000;
const ZOOM_REPEAT_DELAY_MS: u64 = 400;
const ZOOM_REPEAT_MS: u64 = 300;

/// GPIO38 (UP, +) action: move the list highlight up, or zoom in —
/// whichever applies to the currently displayed page.
fn step_plus() {
    if crate::display::current_page() == crate::display::PAGE_QUAKE {
        crate::quake::move_sel(-1);
    } else {
        crate::map_display::zoom_in();
    }
}

/// GPIO39 action: mirror of [`step_plus`] in the opposite direction.
fn step_minus() {
    if crate::display::current_page() == crate::display::PAGE_QUAKE {
        crate::quake::move_sel(1);
    } else {
        crate::map_display::zoom_out();
    }
}

pub fn start_button_thread(btn_pin: Gpio0, zoom_in_pin: Gpio38, zoom_out_pin: Gpio39, map_type_pin: Gpio40) {
    thread::Builder::new()
        .name("btn_ctrl".into())
        .stack_size(4096)
        .spawn(move || button_task(btn_pin, zoom_in_pin, zoom_out_pin, map_type_pin))
        .expect("spawn btn_ctrl thread");
}

fn button_task(btn_pin: Gpio0, zoom_in_pin: Gpio38, zoom_out_pin: Gpio39, map_type_pin: Gpio40) {
    let mut btn = PinDriver::input(btn_pin).unwrap();
    btn.set_pull(Pull::Up).unwrap();
    let mut btn_zin = PinDriver::input(zoom_in_pin).unwrap();
    btn_zin.set_pull(Pull::Up).unwrap();
    let mut btn_zout = PinDriver::input(zoom_out_pin).unwrap();
    btn_zout.set_pull(Pull::Up).unwrap();
    let mut btn_type = PinDriver::input(map_type_pin).unwrap();
    btn_type.set_pull(Pull::Up).unwrap();

    let mut btn_last = true;
    let mut press_start: Option<Instant> = None;
    let mut factory_reset_triggered = false;
    let mut display_updated = false;

    let mut zin_last  = true;
    let mut zout_last = true;
    let mut zin_next_repeat:  Option<Instant> = None;
    let mut zout_next_repeat: Option<Instant> = None;
    let mut type_last = true;
    // Press that only woke the screen: its action is swallowed.
    let mut btn_swallow = false;

    loop {
        // GPIO0: page cycle / factory reset
        let btn_now = btn.is_high();
        if !btn_now && btn_last {
            press_start = Some(Instant::now());
            factory_reset_triggered = false;
            display_updated = false;
            btn_swallow = crate::display::wake_if_off();
        } else if !btn_now {
            crate::display::wake(); // holding the button counts as activity
            if let Some(start) = press_start {
                let elapsed = start.elapsed().as_millis() as u64;
                if elapsed >= 6000 && !display_updated {
                    let rem = ((FACTORY_RESET_PRESS_MS - elapsed.min(FACTORY_RESET_PRESS_MS)) / 1000) as i32;
                    if rem > 0 {
                        crate::display::show_system_message("FACTORY RESET",
                            &format!("Resetting to defaults\nin {} seconds...", rem));
                        display_updated = true;
                    }
                }
                if !factory_reset_triggered && elapsed >= FACTORY_RESET_PRESS_MS {
                    info!("GPIO0 long press -- factory reset");
                    crate::display::show_system_message("FACTORY RESET", "Erasing NVS...\nPlease wait...");
                    thread::sleep(Duration::from_millis(500));
                    crate::httpserver::factory_reset();
                }
            }
        } else if btn_now && !btn_last {
            if !factory_reset_triggered && !btn_swallow {
                if let Some(start) = press_start.take() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    info!("GPIO0 short press ({}ms) -- next display page", elapsed);
                    crate::quake::clear_focus();
                    crate::display::next_page();
                }
            }
            if display_updated { crate::display::clear_system_message(); }
            press_start = None;
            factory_reset_triggered = false;
            display_updated = false;
            btn_swallow = false;
        }
        btn_last = btn_now;

        // GPIO38: zoom in / list-select up
        let zin_now = btn_zin.is_high();
        if !zin_now && zin_last {
            let woke_only = crate::display::wake_if_off();
            if !woke_only { step_plus(); }
            // A press that only woke the screen does not auto-repeat either.
            zin_next_repeat = (!woke_only).then(|| Instant::now() + Duration::from_millis(ZOOM_REPEAT_DELAY_MS));
        } else if !zin_now {
            if zin_next_repeat.map(|t| Instant::now() >= t).unwrap_or(false) {
                crate::display::wake();
                step_plus();
                zin_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_MS));
            }
        } else {
            zin_next_repeat = None;
        }
        zin_last = zin_now;

        // GPIO39: zoom out / list-select down
        let zout_now = btn_zout.is_high();
        if !zout_now && zout_last {
            let woke_only = crate::display::wake_if_off();
            if !woke_only { step_minus(); }
            zout_next_repeat = (!woke_only).then(|| Instant::now() + Duration::from_millis(ZOOM_REPEAT_DELAY_MS));
        } else if !zout_now {
            if zout_next_repeat.map(|t| Instant::now() >= t).unwrap_or(false) {
                crate::display::wake();
                step_minus();
                zout_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_MS));
            }
        } else {
            zout_next_repeat = None;
        }
        zout_last = zout_now;

        // GPIO40: on the quake page, show the highlighted epicenter on the
        // MAP page; otherwise map type toggle (satellite ↔ roadmap).
        let type_now = btn_type.is_high();
        if !type_now && type_last {
            if crate::display::wake_if_off() {
                // only woke the screen
            } else if crate::display::current_page() == crate::display::PAGE_QUAKE {
                if crate::quake::focus_selected() {
                    info!("GPIO40 press -- showing selected epicenter on MAP");
                    crate::display::set_page(crate::display::PAGE_MAP);
                }
            } else {
                let is_sat = crate::map_display::toggle_map_type();
                info!("GPIO40 press -- map type toggled to {}",
                      if is_sat { "satellite" } else { "roadmap" });
                // Force a new map fetch by resetting the cached position
                crate::map_display::reset_last_pos();
            }
        }
        type_last = type_now;

        thread::sleep(Duration::from_millis(50));
    }
}
