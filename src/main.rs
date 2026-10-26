use log::{info, warn};
use esp_idf_svc::sntp::{EspSntp, SyncStatus, SntpConf, OperatingMode, SyncMode};
use chrono::{DateTime, Utc};
use esp_idf_hal::prelude::*;
use esp_idf_hal::adc::attenuation::DB_11;
use esp_idf_hal::adc::oneshot::{AdcDriver, AdcChannelDriver};
use esp_idf_hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_hal::temp_sensor::{TempSensorConfig, TempSensorDriver};
use std::time::{Duration, SystemTime};
use std::thread;

mod wifi;
mod syslogger;
mod httpserver;
pub mod display_pages;
pub mod display;
mod btn_ctrl;
mod boot_log;
mod map_display;
mod quake;
mod place;
mod audio;
mod voice;

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("dhcp")]
    ip_mode: &'static str,
    #[default("")]
    ip_address: &'static str,
    #[default("255.255.255.0")]
    subnet_mask: &'static str,
    #[default("")]
    gateway: &'static str,
    #[default("")]
    dns: &'static str,
    #[default("")]
    syslog_server: &'static str,
    #[default("false")]
    syslog_enable: &'static str,
    #[default("esp32")]
    syslog_host_name: &'static str,
    #[default("app")]
    syslog_app_name: &'static str,
    #[default("admin")]
    web_user: &'static str,
    #[default("esp32")]
    web_password: &'static str,
    #[default("false")]
    wps_enable: &'static str,
    #[default("true")]
    display_enable: &'static str,
    #[default("0")]
    screen_timeout: &'static str,
    #[default("time.aws.com")]
    ntp_server1: &'static str,
    #[default("time.google.com")]
    ntp_server2: &'static str,
    #[default("time.cloudflare.com")]
    ntp_server3: &'static str,
    #[default("ntp.nict.jp")]
    ntp_server4: &'static str,
    #[default("80.098")]
    adc_conversion_factor: &'static str,
    #[default("")]
    maps_api_key: &'static str,
    #[default("true")]
    quake_enable: &'static str,
    #[default("false")]
    audio_enable: &'static str,
    #[default("70")]
    audio_volume: &'static str,
    #[default("true")]
    voice_eew: &'static str,
    #[default("true")]
    voice_new: &'static str,
    #[default("false")]
    quiet_enable: &'static str,
    #[default("23:00")]
    quiet_start: &'static str,
    #[default("07:00")]
    quiet_end: &'static str,
}

fn main() {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Peripherals Initialize
    let peripherals = Peripherals::take().unwrap();

    // ── TFT on ILI9488 (SPI3, 320x480) ─────────────────────────────────────
    // Display init is deferred until after NVS config is loaded (see below)

    // NVS flash init (required before WiFi for RF calibration data)
    unsafe { esp_idf_sys::nvs_flash_init(); }
    // Record reset reason and boot count to NVS before anything else
    let (boot_count, reset_reason) = boot_log::record_boot();
    println!("Boot #{}, reset reason: {}", boot_count, reset_reason);
    // Load config from NVS (overrides cfg.toml defaults where NVS keys are set)
    let cfg_defaults = httpserver::NvsConfig {
        wifi_ssid:     CONFIG.wifi_ssid.to_string(),
        wifi_psk:      CONFIG.wifi_psk.to_string(),
        ip_mode:       CONFIG.ip_mode.to_string(),
        ip_address:    CONFIG.ip_address.to_string(),
        subnet_mask:   CONFIG.subnet_mask.to_string(),
        gateway:       CONFIG.gateway.to_string(),
        dns:           CONFIG.dns.to_string(),
        syslog_server: CONFIG.syslog_server.to_string(),
        syslog_enable: CONFIG.syslog_enable.to_string(),
        syslog_host_name: CONFIG.syslog_host_name.to_string(),
        syslog_app_name: CONFIG.syslog_app_name.to_string(),
        web_user:      CONFIG.web_user.to_string(),
        web_password:  CONFIG.web_password.to_string(),
        wps_enable:    CONFIG.wps_enable.to_string(),
        display_enable: CONFIG.display_enable.to_string(),
        screen_timeout: CONFIG.screen_timeout.to_string(),
        audio_volume:  CONFIG.audio_volume.to_string(),
        voice_eew:     CONFIG.voice_eew.to_string(),
        voice_new:     CONFIG.voice_new.to_string(),
        quiet_enable:  CONFIG.quiet_enable.to_string(),
        quiet_start:   CONFIG.quiet_start.to_string(),
        quiet_end:     CONFIG.quiet_end.to_string(),
        ntp_server1:   CONFIG.ntp_server1.to_string(),
        ntp_server2:   CONFIG.ntp_server2.to_string(),
        ntp_server3:   CONFIG.ntp_server3.to_string(),
        ntp_server4:   CONFIG.ntp_server4.to_string(),
        map_type:      "satellite".to_string(),
        maps_api_key:  CONFIG.maps_api_key.to_string(),
        quake_enable:  CONFIG.quake_enable.to_string(),
    };
    let mut nvs_config = httpserver::load_config(cfg_defaults.clone());
    display::set_idle_timeout(nvs_config.screen_timeout.trim().parse().unwrap_or(0));
    crate::map_display::set_map_type_satellite(nvs_config.map_type == "satellite");
    crate::map_display::set_api_key(&nvs_config.maps_api_key);

    // Start the persistent map-tile fetch worker now, before anything else
    // (buttons, display, httpd) can call map_display::trigger() — this is
    // the point in boot with the most free internal DRAM available for its
    // one-time thread spawn.
    crate::map_display::start_fetch_worker();

    // Parse ADC conversion factor from config (not stored in NVS)
    let adc_conversion_factor: f32 = CONFIG.adc_conversion_factor.parse().unwrap_or(80.098);

    // ── Button control thread — GPIO0 / BOOT button ──────────────────────────
    // Short press (< 3s): cycle display page
    // Long press (>= 10s): factory reset to cfg.toml defaults
    // GPIO38: MAP zoom in / list up, GPIO39: MAP zoom out / list down,
    // GPIO40: MAP type toggle / show the highlighted quake on the MAP
    btn_ctrl::start_button_thread(peripherals.pins.gpio0, peripherals.pins.gpio38, peripherals.pins.gpio39, peripherals.pins.gpio40);

    // ── Display init (only when enabled) ──────────────────────────────────
    if nvs_config.display_enable == "true" {
        // CS=IO10 RST=IO9 D/C=IO46 SDI=IO3 SCK=IO8 BL=IO18 (SDO=IO17 unused)
        display::start_display_thread(
            peripherals.spi3,
            peripherals.pins.gpio8,
            peripherals.pins.gpio3,
            peripherals.pins.gpio10,
            peripherals.pins.gpio46,
            peripherals.pins.gpio9,
            peripherals.pins.gpio18,
        );
        // Start on the map
        display::set_page(display::PAGE_MAP);
        info!("Display enabled — default page: MAP");
    } else {
        info!("Display disabled");
    }

    // ── MAX98357A (I2S: DIN=IO7 BCLK=IO6 LRCLK=IO5, SD_MODE=IO4): spoken EEW /
    //    new-earthquake announcements + Web UI test tone ──
    if CONFIG.audio_enable == "true" {
        audio::set_volume(nvs_config.audio_volume.trim().parse().unwrap_or(70));
        voice::set_notify_eew(nvs_config.voice_eew != "false");
        voice::set_notify_new(nvs_config.voice_new != "false");
        voice::set_quiet(
            nvs_config.quiet_enable == "true",
            voice::parse_hhmm(&nvs_config.quiet_start).unwrap_or(23 * 60),
            voice::parse_hhmm(&nvs_config.quiet_end).unwrap_or(7 * 60),
        );
        voice::init();
        audio::start(
            peripherals.i2s0,
            peripherals.pins.gpio6,
            peripherals.pins.gpio5,
            peripherals.pins.gpio7,
            peripherals.pins.gpio4,
        );
    }

    // WiFi connect (using NVS config — falls back to cfg.toml if not set)
    // When wps_enable=true and SSID is empty, WPS PBC is used to obtain credentials.
    let wifi_dev = if nvs_config.wps_enable == "true" && nvs_config.wifi_ssid.is_empty() {
        println!("WPS enabled and SSID not configured — starting WPS PBC (press WPS button on router)...");
        match wifi::wifi_connect_wps(peripherals.modem) {
            Ok((w, ssid, pass)) => {
                println!("WPS success: SSID={}", ssid);
                httpserver::nvs_write_wifi_creds(&ssid, &pass);
                nvs_config.wifi_ssid = ssid;
                nvs_config.wifi_psk  = pass;
                Ok(w)
            }
            Err(e) => {
                println!("WPS failed: {:?}", e);
                Err(e)
            }
        }
    } else {
        println!("Connecting to WiFi: {}", nvs_config.wifi_ssid);
        wifi::wifi_connect(peripherals.modem, &nvs_config.wifi_ssid, &nvs_config.wifi_psk)
    };

    // Create shared status and populate IP/SSID after WiFi connects
    let dev_status = httpserver::StatusInfo::new();
    match &wifi_dev {
        Ok(w) => {
            // Apply static IP unconditionally (interface config, independent of connection state)
            if nvs_config.ip_mode == "static" {
                match wifi::configure_static_ip(w, &nvs_config.ip_address, &nvs_config.subnet_mask, &nvs_config.gateway, &nvs_config.dns) {
                    Ok(_) => println!("Static IP configured"),
                    Err(e) => println!("Failed to set static IP: {:?}, falling back to DHCP", e),
                }
            }
            if w.is_connected().unwrap_or(false) {
                println!("WiFi connected.");
                wifi::refresh_net_state();
                if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                    println!("IP address: {}", ip_info.ip);
                    dev_status.set_wifi(
                        &ip_info.ip.to_string(),
                        &nvs_config.wifi_ssid,
                        wifi::get_rssi(),
                    );
                }
            } else {
                println!("WiFi initialized (AP not reachable yet — will retry every 30 s).");
            }
        },
        Err(e) => {
            println!("WiFi init failed: {:?}", e);
        }
    }

    // Start WiFi monitor thread — reconnects every 30 s when not associated.
    // Start after a 15 s grace period so the initial connect attempt can settle.
    if wifi_dev.is_ok() {
        wifi::start_wifi_monitor_thread(15, 30);
        // Only after the WiFi driver is initialised: the quake thread queries WiFi state.
        if nvs_config.quake_enable == "true" {
            crate::quake::set_enabled(true);
            crate::quake::start();
        } else {
            info!("Earthquake/EEW feed disabled (quake_enable=false)");
        }
    }

    // Start HTTP config server (login protected, NVS-backed settings)
    let config_state = httpserver::ConfigState::new(nvs_config.clone(), cfg_defaults, dev_status.clone());
    let _http_server = match httpserver::start_http_server(config_state) {
        Ok(s)  => { println!("HTTP config server started on port 80"); Some(s) }
        Err(e) => { println!("HTTP config server failed to start: {:?}", e); None }
    };

    // Provide info-page data to the display thread (only when display is enabled)
    if nvs_config.display_enable == "true" {
        display::set_display_aux(display::DisplayAux {
            status: dev_status.clone(),
        });
    }

    // Syslog init (after wifi is up)
    if nvs_config.syslog_enable == "true" {
        println!("Initializing syslog to {} ...", nvs_config.syslog_server);
        thread::sleep(Duration::from_secs(5));
        match syslogger::init_logger(&nvs_config.syslog_server, &nvs_config.syslog_enable, &nvs_config.syslog_host_name, &nvs_config.syslog_app_name) {
            Ok(_) => {
                log::set_max_level(log::LevelFilter::Info);
                println!("Syslog logger initialized successfully");
                info!("Syslog logger initialized successfully");
                if let Ok(w) = wifi_dev.as_ref() {
                    if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                        info!("WiFi IP address: {}", ip_info.ip);
                    }
                }
            },
            Err(e) => {
                println!("Failed to initialize syslog: {:?}, falling back to ESP logger", e);
                let _ = esp_idf_svc::log::EspLogger::initialize_default();
                log::set_max_level(log::LevelFilter::Info);
            }
        }
    } else {
        esp_idf_svc::log::EspLogger::initialize_default();
        log::set_max_level(log::LevelFilter::Info);
        info!("Using default ESP console logger (syslog disabled)");
    }

    // NTP Server
    let ntp1: &'static str = Box::leak(nvs_config.ntp_server1.clone().into_boxed_str());
    let ntp2: &'static str = Box::leak(nvs_config.ntp_server2.clone().into_boxed_str());
    let ntp3: &'static str = Box::leak(nvs_config.ntp_server3.clone().into_boxed_str());
    let ntp4: &'static str = Box::leak(nvs_config.ntp_server4.clone().into_boxed_str());
    let sntp_conf = SntpConf {
        servers: [ntp1, ntp2, ntp3, ntp4],
        operating_mode: OperatingMode::Poll,
        sync_mode: SyncMode::Immediate,
    };
    let ntp = EspSntp::new(&sntp_conf).unwrap();

    // NTP Sync
    info!("NTP Sync Start..");

    // wait for sync
    let mut sync_count = 0;
    while ntp.get_sync_status() != SyncStatus::Completed {
        sync_count += 1;
        if sync_count > 1000 {
            info!("NTP Sync Timeout");
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let now = SystemTime::now();
    let dt_now : DateTime<Utc> = now.into();
    let formatted = format!("{}", dt_now.format("%Y-%m-%d %H:%M:%S"));
    info!("NTP Sync Completed: {}", formatted);

    // ── Battery voltage ADC init (GPIO11) ────────────────────────
    // GPIO11 only exists on the ESP32-S3's ADC2 hardware unit (ADC2_CH0), so the
    // peripheral handle below is still `adc2` even though it now reads
    // a single channel (battery voltage).
    let adc = AdcDriver::new(peripherals.adc2).unwrap();
    let adc_config = AdcChannelConfig {
        attenuation: DB_11,
        ..Default::default()
    };
    let mut battery_pin = AdcChannelDriver::new(&adc, peripherals.pins.gpio11, &adc_config).unwrap();

    // ADC conversion factor (from config) is applied in the main loop during voltage reading to convert raw ADC values to volts.
    info!("ADC conversion factor: {} (raw ADC value / this factor = voltage in volts)", adc_conversion_factor,);

    // ── Internal temperature sensor init ──────────────────────────────────
    let temp_cfg = TempSensorConfig::default();
    let mut temp_sensor = TempSensorDriver::new(&temp_cfg, peripherals.temp_sensor).unwrap();
    temp_sensor.enable().unwrap();

    let mut count = 0u32;
    // Internal-RAM low-water-mark tracking: logs what was running whenever the
    // all-time minimum free internal heap drops by 2 KB or more.
    let mut logged_min_internal = u32::MAX;
    loop {
        {
            use esp_idf_sys::{heap_caps_get_free_size, heap_caps_get_minimum_free_size,
                              heap_caps_get_largest_free_block, MALLOC_CAP_INTERNAL};
            let min = unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) } as u32;
            if min + 2048 <= logged_min_internal {
                logged_min_internal = min;
                let (free, largest) = unsafe {
                    (heap_caps_get_free_size(MALLOC_CAP_INTERNAL) as u32,
                     heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL) as u32)
                };
                warn!("[HEAP] internal min dropped to {} B (free {} B, largest block {} B) map_busy={} ws={} page={}",
                      min, free, largest, crate::map_display::is_busy(),
                      crate::quake::ws_connected(), crate::display::current_page());
            }
        }

        // Check connection state via EspWifi before calling get_rssi(),
        // because esp_wifi_sta_get_rssi crashes (LoadProhibited) when
        // WiFi is not connected.
        let is_connected = wifi_dev.as_ref()
            .map(|w| w.is_connected().unwrap_or(false))
            .unwrap_or(false);



        if count % 50 == 0 {

            let battery_raw = adc.read(&mut battery_pin).unwrap();
            let battery_voltage : f32 = battery_raw as f32 / adc_conversion_factor; // Conversion factor from cfg.toml
            dev_status.set_battery_voltage(battery_voltage);
            if let Ok(t) = temp_sensor.get_celsius() {
                dev_status.set_chip_temp(t);
            }
        }

        if !is_connected {
            // Reconnection is handled by the wifi_monitor background thread.
            // Nothing to do here except keep the status updated.
        } else {
            let rssi = wifi::get_rssi();
            dev_status.set_rssi(rssi);
            if count % 30 == 0 {
                if let Ok(w) = wifi_dev.as_ref() {
                    if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                        dev_status.set_wifi(&ip_info.ip.to_string(), &nvs_config.wifi_ssid, rssi);
                    }
                }
            }
        }
        count = count.wrapping_add(1);
        thread::sleep(Duration::from_millis(100));
    }
}

