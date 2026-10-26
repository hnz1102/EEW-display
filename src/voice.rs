// Spoken announcements for EEW and newly received earthquakes (Japanese).
//
// Each announcement starts with a chime made in firmware (EEW: "ツー、リー、ツー、
// リー", repeated with the sentence; new earthquake: "ピンポン", once). The speech itself is generated once on a PC (tools/voice/gen_voice.py, Google
// Cloud Text-to-Speech) as ~340 short parts — fixed phrases, digits, seismic
// intensities, parts of region names — stored as IMA-ADPCM in the `voice` flash
// partition. This module memory-maps that partition, finds the parts by key and
// strings them into a sentence for `audio` to play, e.g.
//
//   緊急地震速報。 震源は 福島県 沖 です。 マグニチュード 6 点 1。 最大 震度6弱 と予測されます。 強い揺れに警戒してください。
//
// No network and no API key is needed at run time. If the partition does not
// hold a voice bank (never flashed), announcements are simply skipped.
//
// SPDX-License-Identifier: MIT

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
use log::{info, warn};

/// One spoken part: IMA-ADPCM data in the memory-mapped partition.
#[derive(Clone, Copy)]
pub struct Clip {
    pub data: &'static [u8],
    pub nsamples: u32,
}

/// The index is ONE sorted Vec of (hash of key, offset, nsamples): a single
/// allocation of ~5 KB, which goes to PSRAM. (A HashMap<String, _> of ~340 small
/// entries is ~27 KB of tiny allocations, all of them in scarce internal RAM.)
struct Bank {
    blob: &'static [u8],
    index: Vec<(u64, u32, u32)>,
    /// Longest region-name part, in characters (bounds the longest-match search).
    max_tok: usize,
}

/// FNV-1a, 64 bit.
fn hash_key(k: &str) -> u64 {
    k.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

impl Bank {
    fn get(&self, key: &str) -> Option<Clip> {
        let h = hash_key(key);
        let i = self.index.binary_search_by_key(&h, |e| e.0).ok()?;
        let (_, off, ns) = self.index[i];
        let (off, ns) = (off as usize, ns);
        let end = (off + 4 + ns as usize / 2 + 1).min(self.blob.len());
        Some(Clip { data: &self.blob[off..end], nsamples: ns })
    }
}

static BANK: OnceLock<Bank> = OnceLock::new();

/// Announce EEWs / new earthquakes (chime + speech). Both on by default; set from
/// the saved configuration at boot and from the Web UI at run time.
static NOTIFY_EEW: AtomicBool = AtomicBool::new(true);
static NOTIFY_NEW: AtomicBool = AtomicBool::new(true);
pub fn notify_eew() -> bool { NOTIFY_EEW.load(Ordering::Relaxed) }
pub fn notify_new() -> bool { NOTIFY_NEW.load(Ordering::Relaxed) }
pub fn set_notify_eew(on: bool) { NOTIFY_EEW.store(on, Ordering::Relaxed); }
pub fn set_notify_new(on: bool) { NOTIFY_NEW.store(on, Ordering::Relaxed); }

// ─── Quiet hours ─────────────────────────────────────────────────────────────
// A daily window (JST) in which NEW-EARTHQUAKE announcements are not played.
// EEWs are never affected: they always sound.

static QUIET_ON: AtomicBool = AtomicBool::new(false);
/// Start / end of the window in minutes after midnight (JST).
static QUIET_START: AtomicU32 = AtomicU32::new(23 * 60);
static QUIET_END: AtomicU32 = AtomicU32::new(7 * 60);

pub fn quiet_enabled() -> bool { QUIET_ON.load(Ordering::Relaxed) }
pub fn quiet_start_min() -> u32 { QUIET_START.load(Ordering::Relaxed) }
pub fn quiet_end_min() -> u32 { QUIET_END.load(Ordering::Relaxed) }
pub fn set_quiet(on: bool, start_min: u32, end_min: u32) {
    QUIET_START.store(start_min.min(1439), Ordering::Relaxed);
    QUIET_END.store(end_min.min(1439), Ordering::Relaxed);
    QUIET_ON.store(on, Ordering::Relaxed);
}

/// "HH:MM" → minutes after midnight (None if malformed / out of range).
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Minutes → "HH:MM".
pub fn fmt_hhmm(min: u32) -> String { format!("{:02}:{:02}", min / 60 % 24, min % 60) }

/// Is `minute` (of the day) inside the window [start, end)? A window that runs
/// past midnight (start > end, e.g. 23:00–07:00) is handled; start == end is an
/// empty window.
fn in_window(minute: u32, start: u32, end: u32) -> bool {
    if start < end { minute >= start && minute < end }
    else if start > end { minute >= start || minute < end }
    else { false }
}

/// true if quiet hours are on and the time of day (JST) is inside the window.
/// Before the clock has synced (NTP) the time is unknown, so it is NOT quiet:
/// better an announcement at the wrong hour than a missed one.
pub fn quiet_now() -> bool {
    if !quiet_enabled() { return false; }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    if secs < 1_704_067_200 { return false; } // 2024-01-01: the clock is not set yet
    let minute = ((secs + 9 * 3600) % 86_400 / 60) as u32;
    in_window(minute, quiet_start_min(), quiet_end_min())
}

/// true if a voice bank was found and loaded.
pub fn available() -> bool { BANK.get().is_some() }

/// Map the `voice` partition and read its index. Call once at boot.
pub fn init() -> bool {
    match load() {
        Some(b) => {
            info!("[VOICE] bank loaded: {} parts", b.index.len());
            let _ = BANK.set(b);
            true
        }
        None => false,
    }
}

fn load() -> Option<Bank> {
    use esp_idf_sys::*;
    let label = CString::new("voice").ok()?;
    let blob: &'static [u8] = unsafe {
        let part = esp_partition_find_first(
            esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
            esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED,
            label.as_ptr(),
        );
        if part.is_null() {
            warn!("[VOICE] no `voice` partition (old partition table?) — announcements off");
            return None;
        }
        let size = (*part).size as usize;
        let mut ptr: *const core::ffi::c_void = core::ptr::null();
        let mut handle: esp_partition_mmap_handle_t = 0;
        let r = esp_partition_mmap(
            part, 0, size,
            esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
            &mut ptr, &mut handle,
        );
        if r != ESP_OK as esp_err_t || ptr.is_null() {
            warn!("[VOICE] mmap of the voice partition failed ({}) — announcements off", r);
            return None;
        }
        // Never unmapped: lives for the whole run.
        core::slice::from_raw_parts(ptr as *const u8, size)
    };
    if blob.len() < 12 || &blob[..4] != b"VOX1" {
        warn!("[VOICE] voice partition is empty — flash voice.bin (see tools/voice) — announcements off");
        return None;
    }
    let count = u32::from_le_bytes(blob[4..8].try_into().ok()?) as usize;
    let rate = u32::from_le_bytes(blob[8..12].try_into().ok()?);
    if rate != 16_000 || count == 0 || count > 4096 {
        warn!("[VOICE] unsupported voice bank (rate {}, {} parts)", rate, count);
        return None;
    }
    let mut index: Vec<(u64, u32, u32)> = Vec::with_capacity(count);
    let mut max_tok = 1;
    let mut p = 12usize;
    for _ in 0..count {
        let kl = *blob.get(p)? as usize;
        let key = core::str::from_utf8(blob.get(p + 1..p + 1 + kl)?).ok()?.to_string();
        let off = u32::from_le_bytes(blob.get(p + 1 + kl..p + 5 + kl)?.try_into().ok()?) as usize;
        let ns = u32::from_le_bytes(blob.get(p + 5 + kl..p + 9 + kl)?.try_into().ok()?);
        p += 9 + kl;
        if off + 4 > blob.len() { return None; }
        if let Some(t) = key.strip_prefix("t:") { max_tok = max_tok.max(t.chars().count()); }
        index.push((hash_key(&key), off as u32, ns));
    }
    index.sort_unstable_by_key(|e| e.0);
    Some(Bank { blob, index, max_tok })
}

// ─── Sentence building ───────────────────────────────────────────────────────

fn push(v: &mut Vec<Clip>, key: &str) {
    if let Some(c) = BANK.get().and_then(|b| b.get(key)) { v.push(c); }
}

/// Speak a JMA hypocenter name as far as its parts match (left to right,
/// longest first). Returns true if anything was added.
fn region(v: &mut Vec<Clip>, name: &str) -> bool {
    let Some(b) = BANK.get() else { return false };
    let chars: Vec<char> = name.chars().collect();
    let before = v.len();
    let mut pos = 0;
    while pos < chars.len() {
        if chars[pos] == '・' || chars[pos] == ' ' { pos += 1; continue; }
        let mut hit = false;
        for len in (1..=b.max_tok.min(chars.len() - pos)).rev() {
            let cand: String = chars[pos..pos + len].iter().collect();
            if let Some(c) = b.get(&format!("t:{}", cand)) {
                v.push(c);
                pos += len;
                hit = true;
                break;
            }
        }
        if !hit { break; } // unknown part: stop rather than say something wrong
    }
    v.len() > before
}

fn magnitude(v: &mut Vec<Clip>, mag: f32) {
    if !(0.0..10.0).contains(&mag) { return; }
    let tenths = (mag * 10.0).round() as i32;
    push(v, "p:mag");
    push(v, &format!("n:{}", tenths / 10));
    push(v, "p:point");
    push(v, &format!("n:{}", tenths % 10));
}

fn intensity(v: &mut Vec<Clip>, scale: i32) -> bool {
    let k = match scale {
        10 => "1", 20 => "2", 30 => "3", 40 => "4", 45 => "5-", 50 => "5+",
        55 => "6-", 60 => "6+", 70 => "7", _ => return false,
    };
    push(v, "p:max");
    push(v, &format!("i:{}", k));
    true
}

fn depth(v: &mut Vec<Clip>, d: i32) {
    if d < 0 { return; }
    push(v, "p:depth");
    if d == 0 { push(v, "p:shallow"); return; }
    if d >= 100 {
        push(v, &format!("h:{}", (d / 100).min(7) * 100));
        if d % 100 != 0 { push(v, &format!("n:{}", d % 100)); }
    } else {
        push(v, &format!("n:{}", d));
    }
    push(v, "p:km");
}

fn eew_clips(name: &str, mag: f32, max_scale: i32) -> Vec<Clip> {
    let mut v = Vec::new();
    push(&mut v, "p:eew");
    if !name.is_empty() {
        let mark = v.len();
        push(&mut v, "p:hypo");
        if region(&mut v, name) { push(&mut v, "p:desu"); } else { v.truncate(mark); }
    }
    magnitude(&mut v, mag);
    if intensity(&mut v, max_scale) { push(&mut v, "p:forecast"); }
    push(&mut v, "p:warn");
    v
}

fn new_clips(name: &str, mag: f32, scale: i32, d: i32, tsunami: &str) -> Vec<Clip> {
    let mut v = Vec::new();
    push(&mut v, "p:info");
    if !name.is_empty() {
        let mark = v.len();
        push(&mut v, "p:hypo");
        if region(&mut v, name) { push(&mut v, "p:desu"); } else { v.truncate(mark); }
    }
    magnitude(&mut v, mag);
    if intensity(&mut v, scale) { /* "最大 震度N。" ends the sentence */ }
    depth(&mut v, d);
    match tsunami {
        "None" => push(&mut v, "p:no_tsunami"),
        "NonEffective" => push(&mut v, "p:minor_tsunami"),
        "Watch" | "Warning" => push(&mut v, "p:tsunami"),
        _ => {}
    }
    v
}

// ─── Alert sounds ────────────────────────────────────────────────────────────

use crate::audio::{Note, Seg};

/// How many times the EEW chime + announcement is played.
const EEW_REPEATS: usize = 2;

/// EEW chime: a 1.5 s arpeggio on a chord of 587 / 784 / 988 Hz (D5 G5 B5): the original
/// G#-C#-F chord (415 / 554 / 698 Hz) raised by half an octave (×√2, six semitones).
///
/// 0.0–0.4 s: a low D4 under the chord, D5 → G5 → B5 entering 60 ms apart and
/// all cut at 0.4 s. At 0.4 s the pitch steps up and a second arpeggio, G5 → B5 →
/// D6, enters the same way and rings out until 1.5 s with a slower fade. Seven
/// notes with three harmonics each give about 20 frequency components that
/// come and go over time.
const EEW_ARPEGGIO: &[Note] = &[
    // first half
    Note { hz: 293.7, start_ms: 0,   dur_ms: 400, tau_ms: 260, amp: 0.30 },
    Note { hz: 587.3, start_ms: 0,   dur_ms: 400, tau_ms: 220, amp: 0.40 },
    Note { hz: 784.0, start_ms: 60,  dur_ms: 340, tau_ms: 220, amp: 0.40 },
    Note { hz: 987.8, start_ms: 120, dur_ms: 280, tau_ms: 220, amp: 0.40 },
    // second half: one step up
    Note { hz: 784.0, start_ms: 400, dur_ms: 1100, tau_ms: 480, amp: 0.40 },
    Note { hz: 987.8, start_ms: 460, dur_ms: 1040, tau_ms: 480, amp: 0.40 },
    Note { hz: 1174.6, start_ms: 520, dur_ms: 980,  tau_ms: 480, amp: 0.40 },
];

fn eew_chime(v: &mut Vec<Seg>) {
    v.push(Seg::Chime(EEW_ARPEGGIO));
    v.push(Seg::Silence { ms: 450 }); // the reverb tail rings out here
}

/// New-earthquake chime: "ピンポン" — a high then a lower struck-bell tone.
fn info_chime(v: &mut Vec<Seg>) {
    v.push(Seg::tone(1319, 450, true));
    v.push(Seg::Silence { ms: 10 });
    v.push(Seg::tone(988, 1100, true));
    v.push(Seg::Silence { ms: 300 });
}

fn speech(v: &mut Vec<Seg>, clips: Vec<Clip>) {
    v.extend(clips.into_iter().map(Seg::Clip));
}

fn eew_sentence(name: &str, mag: f32, max_scale: i32) -> Vec<Seg> {
    let mut v = Vec::new();
    for i in 0..EEW_REPEATS {
        if i > 0 { v.push(Seg::Silence { ms: 700 }); }
        eew_chime(&mut v);
        speech(&mut v, eew_clips(name, mag, max_scale));
    }
    v
}

fn new_sentence(name: &str, mag: f32, scale: i32, d: i32, tsunami: &str) -> Vec<Seg> {
    let mut v = Vec::new();
    info_chime(&mut v);
    speech(&mut v, new_clips(name, mag, scale, d, tsunami));
    v
}

// ─── Entry points ────────────────────────────────────────────────────────────
// The chime is generated in firmware, so it plays even without a voice bank;
// the spoken part is skipped when there is none.

/// Announce an emergency earthquake warning (interrupts anything else):
/// chime + sentence, repeated.
pub fn speak_eew(e: &crate::quake::Eew) {
    if !notify_eew() { return; }
    let v = eew_sentence(&e.name, e.mag, e.max_scale);
    info!("[VOICE] EEW announcement ({} segments, voice bank: {})", v.len(), available());
    crate::audio::play(v, true);
}

/// Announce a newly received earthquake: chime + sentence, once (dropped while
/// an EEW is being played, and during the quiet hours — EEWs ignore those).
pub fn speak_new(q: &crate::quake::Quake) {
    if !notify_new() { return; }
    if quiet_now() {
        info!("[VOICE] quiet hours ({}-{} JST): new-earthquake announcement skipped",
              fmt_hhmm(quiet_start_min()), fmt_hhmm(quiet_end_min()));
        return;
    }
    let v = new_sentence(&q.name, q.mag, q.scale, q.depth, &q.tsunami);
    info!("[VOICE] new-earthquake announcement ({} segments, voice bank: {})", v.len(), available());
    crate::audio::play(v, false);
}

/// Play a sample announcement (Web UI test buttons). `kind` = "eew" | "new".
pub fn speak_sample(kind: &str) -> bool {
    if kind == "eew" {
        crate::audio::play(eew_sentence("福島県沖", 6.1, 55), true)
    } else {
        crate::audio::play(new_sentence("茨城県南部", 4.6, 40, 50, "None"), false)
    }
}
