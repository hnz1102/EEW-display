// MAX98357A I2S class-D amplifier output.
//
// Wiring (ESP32-S3 → MAX98357A):
//   GPIO7 → DIN     GPIO6 → BCLK     GPIO5 → LRCLK (WS)
//   GPIO4 → SD_MODE (high = amplifier on, low = shutdown)
//
// One thread owns the I2S output (48 kHz, 16-bit, stereo, Philips) and mixes
// two sources:
//   * spoken announcements (`voice`): 16 kHz mono IMA-ADPCM parts, decoded on
//     the fly, upsampled ×3 and sent to both channels; they take priority;
//   * a sine test tone, 100 Hz – 15 kHz, switched on/off and tuned from the Web
//     UI (starts OFF). Its phase carries across frequency changes (no clicks).
// SD_MODE is high only while something is playing; the clocks run all the time.
//
// SPDX-License-Identifier: MIT

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use log::{info, warn};
use esp_idf_hal::gpio::{AnyIOPin, Gpio4, Gpio5, Gpio6, Gpio7, PinDriver};
use esp_idf_hal::i2s::{self, config::{Config, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig, StdSlotConfig}, I2sDriver};
use esp_idf_hal::delay::BLOCK;
use esp_idf_hal::cpu::Core;
use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
use crate::voice::Clip;

const SAMPLE_RATE_HZ: u32 = 48_000;
pub const FREQ_MIN_HZ: u32 = 100;
pub const FREQ_MAX_HZ: u32 = 15_000;
/// Peak amplitude of the test tone out of 32767. ~25 % keeps the default 9 dB gain comfortable.
const AMPLITUDE: f32 = 8_000.0;
/// Frames per write (10 ms). A multiple of 3 (the voice upsampling factor).
const FRAMES: usize = 480;
/// Sine table length (power of two); the phase accumulator's top bits index it.
const TABLE_BITS: u32 = 10;
/// Silence before the first part (lets the amplifier wake up) and after the last, in 16 kHz samples.
const LEAD_SAMPLES: u32 = 960;
const TAIL_SAMPLES: u32 = 1600;

static RUNNING: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);
static FREQ_HZ: AtomicU32 = AtomicU32::new(1_000);
/// Voice volume, percent of full scale (0–100).
static VOLUME: AtomicU32 = AtomicU32::new(70);

/// True once the audio thread has started the I2S output.
pub fn running() -> bool { RUNNING.load(Ordering::Relaxed) }
/// Current tone frequency in Hz.
pub fn freq_hz() -> u32 { FREQ_HZ.load(Ordering::Relaxed) }
/// Test tone on, or muted.
pub fn enabled() -> bool { ENABLED.load(Ordering::Relaxed) }
pub fn set_enabled(on: bool) { ENABLED.store(on, Ordering::Relaxed); }
/// Set the tone frequency, clamped to 100 Hz … 15 kHz. Returns the value used.
pub fn set_freq_hz(hz: u32) -> u32 {
    let hz = hz.clamp(FREQ_MIN_HZ, FREQ_MAX_HZ);
    FREQ_HZ.store(hz, Ordering::Relaxed);
    hz
}
/// Voice volume in percent (clamped to 0–100).
pub fn set_volume(pct: u32) { VOLUME.store(pct.min(100), Ordering::Relaxed); }
/// Current voice volume in percent.
pub fn volume() -> u32 { VOLUME.load(Ordering::Relaxed) }

// ─── Playback queue ──────────────────────────────────────────────────────────

/// One piece of a sentence.
#[derive(Clone, Copy)]
pub enum Seg {
    /// A spoken part (IMA-ADPCM).
    Clip(Clip),
    /// A beep of up to three notes sounding together (a chord; unused slots are
    /// 0 Hz) and its length. `decay` = false: a sustained tone (soft attack, held
    /// level, slow release, a few harmonics); true: a struck bell (inharmonic
    /// partials that die away, the upper ones faster).
    Beep { hz: [u32; 3], ms: u32, decay: bool },
    /// Silence.
    Silence { ms: u32 },
    /// A chime built from timed notes (see `Note`): arpeggios and the like.
    Chime(&'static [Note]),
}

/// One note of a chime: a fundamental with three harmonics (1×, 2×, 3×) that
/// starts at `start_ms`, rings for `dur_ms`, fades with time constant `tau_ms`
/// and is cut with a short release at its end.
#[derive(Clone, Copy)]
pub struct Note { pub hz: f32, pub start_ms: u32, pub dur_ms: u32, pub tau_ms: u32, pub amp: f32 }

/// Harmonic amplitudes of a chime note.
const NOTE_PARTIALS: [f32; 3] = [1.0, 0.35, 0.18];
/// Level of the chimes relative to BEEP_AMP: the notes are quiet individually
/// (peak ≈ 15000 before this), so this brings them up to about the speech level.
const CHIME_GAIN: f32 = 1.6;
const NOTE_ATTACK: f32 = 96.0;   // 6 ms at 16 kHz
const NOTE_RELEASE: f32 = 480.0; // 30 ms

struct Playback { segs: Vec<Seg>, eew: bool }

static PLAY_TX: Mutex<Option<Sender<Playback>>> = Mutex::new(None);

/// Play a sentence made of beeps, silences and voice parts. A new sentence
/// replaces the current one, except that a non-EEW sentence never interrupts an
/// EEW one. Returns false if the audio output is not running.
pub fn play(segs: Vec<Seg>, eew: bool) -> bool {
    if segs.is_empty() { return false; }
    match PLAY_TX.lock().unwrap().as_ref() {
        Some(tx) => tx.send(Playback { segs, eew }).is_ok(),
        None => false,
    }
}

// ─── IMA-ADPCM decoder ───────────────────────────────────────────────────────

const STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73,
    80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494,
    544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499,
    2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487,
    12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX_ADJ: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// Decoder for one clip: { i16 predictor, u8 step index, u8 0, nibbles (low first) }.
struct Dec { data: &'static [u8], pred: i32, idx: i32, k: u32, n: u32, first: bool }

impl Dec {
    fn new(c: Clip) -> Option<Dec> {
        if c.nsamples == 0 || c.data.len() < 4 { return None; }
        Some(Dec {
            data: c.data,
            pred: i16::from_le_bytes([c.data[0], c.data[1]]) as i32,
            idx: (c.data[2] as i32).min(88),
            k: 0, n: c.nsamples, first: true,
        })
    }

    fn next(&mut self) -> Option<i16> {
        if self.first { self.first = false; return Some(self.pred as i16); }
        if self.k + 1 >= self.n { return None; }
        let b = *self.data.get(4 + (self.k / 2) as usize)?;
        let code = if self.k & 1 == 1 { b >> 4 } else { b & 15 } as i32;
        self.k += 1;
        let step = STEP[self.idx as usize];
        let mut d = step >> 3;
        if code & 4 != 0 { d += step; }
        if code & 2 != 0 { d += step >> 1; }
        if code & 1 != 0 { d += step >> 2; }
        self.pred = (if code & 8 != 0 { self.pred - d } else { self.pred + d }).clamp(-32768, 32767);
        self.idx = (self.idx + INDEX_ADJ[(code & 7) as usize]).clamp(0, 88);
        Some(self.pred as i16)
    }
}

/// What the player is doing right now.
enum St {
    Idle,
    Speech(Dec),
    Beep { n: u32, len: u32, phase: [f32; 3], step: [f32; 3], bell: bool },
    Silence(u32),
    /// A chime: sample position, total length, one phase accumulator per note.
    Chime { n: u32, len: u32, notes: &'static [Note], phase: [u32; MAX_NOTES] },
}

/// Most notes a chime can hold.
const MAX_NOTES: usize = 8;

/// One full sine period, 1024 points, used by the chimes (phase accumulators
/// index it, so a harmonic is just the phase times an integer).
static SINE: std::sync::LazyLock<Vec<f32>> = std::sync::LazyLock::new(|| {
    (0..1024).map(|i| (core::f32::consts::TAU * i as f32 / 1024.0).sin()).collect()
});

/// One sample of a chime at position `n` (16 kHz); advances the phases.
fn chime_sample(n: u32, notes: &[Note], phase: &mut [u32; MAX_NOTES]) -> f32 {
    let mut v = 0.0;
    for (k, nt) in notes.iter().enumerate().take(MAX_NOTES) {
        let start = nt.start_ms * 16;
        let dur = nt.dur_ms * 16;
        if n < start || n >= start + dur { continue; }
        let t = (n - start) as f32;
        let env = (t / NOTE_ATTACK).min(1.0)
            * (-t / (nt.tau_ms as f32 * 16.0)).exp()
            * ((dur as f32 - t) / NOTE_RELEASE).min(1.0);
        let step = (nt.hz / 16_000.0 * 4_294_967_296.0) as u32;
        phase[k] = phase[k].wrapping_add(step);
        let p = phase[k];
        let mut s = 0.0;
        for (h, a) in NOTE_PARTIALS.iter().enumerate() {
            s += a * SINE[(p.wrapping_mul(h as u32 + 1) >> 22) as usize];
        }
        v += s * env * nt.amp;
    }
    v * BEEP_AMP * CHIME_GAIN
}

/// Beep amplitude out of 32767 (before the volume setting; the partials add up
/// to about 1.6 of this, so the peak stays below full scale).
const BEEP_AMP: f32 = 14_000.0;
/// Sustained tone: partials (ratio, amplitude). Attack / release in 16 kHz samples.
const TONE_PARTIALS: [(f32, f32); 3] = [(1.0, 1.0), (2.0, 0.35), (3.0, 0.15)];
const TONE_ATTACK: f32 = 160.0;   // 10 ms
const TONE_RELEASE: f32 = 2400.0; // 150 ms
/// Bell: partials (ratio, amplitude, relative decay speed).
const BELL_PARTIALS: [(f32, f32, f32); 4] = [(1.0, 1.0, 1.0), (2.0, 0.45, 1.6), (2.76, 0.30, 2.2), (5.4, 0.15, 3.5)];

/// One beep sample at position `n` of `len`; `step[k] == 0` marks an unused note.
fn beep_sample(n: u32, len: u32, phase: &[f32; 3], step: &[f32; 3], bell: bool) -> f32 {
    let attack = (n as f32 / if bell { 48.0 } else { TONE_ATTACK }).min(1.0);
    let notes = step.iter().filter(|&&s| s > 0.0).count().max(1);
    // Chord notes share the headroom (a single note keeps its full level).
    let gain = if notes == 1 { 1.0 } else { 1.2 / notes as f32 };
    let rel = ((len - n) as f32 / TONE_RELEASE.min(len as f32)).min(1.0);
    let mut v = 0.0;
    for k in 0..3 {
        if step[k] <= 0.0 { continue; }
        v += if bell {
            BELL_PARTIALS.iter()
                .map(|&(r, a, d)| a * (phase[k] * r).sin() * (-(n as f32) * 3.0 * d / len as f32).exp())
                .sum::<f32>()
        } else {
            // Held level, then a smooth release over the last 150 ms (or the
            // whole note if it is shorter).
            TONE_PARTIALS.iter().map(|&(r, a)| a * (phase[k] * r).sin()).sum::<f32>() * rel * rel
        };
    }
    v * gain * attack * BEEP_AMP
}

// ─── Reverb (Schroeder: 4 damped combs + 2 all-passes), at the 16 kHz voice rate ─

/// All delay lines live in ONE Vec (~10 KB): a single allocation that goes to
/// PSRAM instead of six small ones in scarce internal RAM.
struct Reverb {
    buf: Vec<f32>,
    /// (start, len) of each comb / all-pass delay line in `buf`, and its position.
    comb: [(usize, usize); 4],
    ap: [(usize, usize); 2],
    ci: [usize; 4],
    ai: [usize; 2],
    lp: [f32; 4],
}

/// Comb / all-pass delays in samples (mutually prime-ish, 30–45 ms and 2–6 ms).
const COMB_DELAYS: [usize; 4] = [479, 599, 661, 709];
const AP_DELAYS: [usize; 2] = [83, 29];
const COMB_FEEDBACK: f32 = 0.82;  // reverberation time ≈ 1.2 s
const COMB_DAMP: f32 = 0.35;      // high-frequency damping
/// Wet level relative to the dry beep.
const REVERB_WET: f32 = 0.45;

impl Reverb {
    fn new() -> Reverb {
        let mut at = 0;
        let mut comb = [(0, 0); 4];
        for (k, &d) in COMB_DELAYS.iter().enumerate() { comb[k] = (at, d); at += d; }
        let mut ap = [(0, 0); 2];
        for (k, &d) in AP_DELAYS.iter().enumerate() { ap[k] = (at, d); at += d; }
        Reverb { buf: vec![0.0; at], comb, ap, ci: [0; 4], ai: [0; 2], lp: [0.0; 4] }
    }
    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.ci = [0; 4];
        self.ai = [0; 2];
        self.lp = [0.0; 4];
    }
    /// Reverberation of the input (wet signal only).
    fn process(&mut self, x: f32) -> f32 {
        let mut sum = 0.0;
        for k in 0..4 {
            let (start, len) = self.comb[k];
            let p = start + self.ci[k];
            let y = self.buf[p];
            self.lp[k] = y * (1.0 - COMB_DAMP) + self.lp[k] * COMB_DAMP;
            self.buf[p] = x + self.lp[k] * COMB_FEEDBACK;
            self.ci[k] = (self.ci[k] + 1) % len;
            sum += y;
        }
        let mut y = sum * 0.25;
        for k in 0..2 {
            let (start, len) = self.ap[k];
            let p = start + self.ai[k];
            let b = self.buf[p];
            let out = -y + b;
            self.buf[p] = y + b * 0.5;
            self.ai[k] = (self.ai[k] + 1) % len;
            y = out;
        }
        y
    }
}

impl Seg {
    /// A single-note beep.
    pub fn tone(hz: u32, ms: u32, decay: bool) -> Seg { Seg::Beep { hz: [hz, 0, 0], ms, decay } }
}

/// A sentence being played: lead silence, the segments one after another, tail silence.
struct Player { segs: Vec<Seg>, si: usize, st: St, lead: u32, tail: u32, eew: bool }

impl Player {
    fn new(p: Playback) -> Player {
        Player { segs: p.segs, si: 0, st: St::Idle, lead: LEAD_SAMPLES, tail: TAIL_SAMPLES, eew: p.eew }
    }

    /// Next 16 kHz sample as (voice, beep), or None when the sentence is finished.
    /// The two are kept apart so that only the beeps go through the reverb.
    fn next(&mut self) -> Option<(f32, f32)> {
        if self.lead > 0 { self.lead -= 1; return Some((0.0, 0.0)); }
        loop {
            match core::mem::replace(&mut self.st, St::Idle) {
                St::Speech(mut d) => {
                    if let Some(s) = d.next() { self.st = St::Speech(d); return Some((s as f32, 0.0)); }
                    self.si += 1;
                }
                St::Beep { n, len, mut phase, step, bell } => {
                    if n < len {
                        let s = beep_sample(n, len, &phase, &step, bell);
                        for k in 0..3 {
                            phase[k] += step[k];
                            if phase[k] > core::f32::consts::TAU { phase[k] -= core::f32::consts::TAU; }
                        }
                        self.st = St::Beep { n: n + 1, len, phase, step, bell };
                        return Some((0.0, s));
                    }
                    self.si += 1;
                }
                St::Silence(k) => {
                    if k > 0 { self.st = St::Silence(k - 1); return Some((0.0, 0.0)); }
                    self.si += 1;
                }
                St::Chime { n, len, notes, mut phase } => {
                    if n < len {
                        let s = chime_sample(n, notes, &mut phase);
                        self.st = St::Chime { n: n + 1, len, notes, phase };
                        return Some((0.0, s));
                    }
                    self.si += 1;
                }
                St::Idle => {
                    let Some(seg) = self.segs.get(self.si).copied() else {
                        if self.tail > 0 { self.tail -= 1; return Some((0.0, 0.0)); }
                        return None;
                    };
                    match seg {
                        Seg::Clip(c) => match Dec::new(c) {
                            Some(d) => self.st = St::Speech(d),
                            None => self.si += 1,
                        },
                        Seg::Beep { hz, ms, decay } => {
                            self.st = St::Beep {
                                n: 0, len: ms * 16, phase: [0.0; 3], bell: decay,
                                step: hz.map(|f| core::f32::consts::TAU * f as f32 / 16_000.0),
                            };
                        }
                        Seg::Silence { ms } => self.st = St::Silence(ms * 16),
                        Seg::Chime(notes) => {
                            let len = notes.iter().map(|n| (n.start_ms + n.dur_ms) * 16).max().unwrap_or(0);
                            self.st = St::Chime { n: 0, len, notes, phase: [0; MAX_NOTES] };
                        }
                    }
                }
            }
        }
    }
}

// ─── Thread ──────────────────────────────────────────────────────────────────

/// Start the audio thread. Call once from main.
///
/// The thread feeds the I2S DMA directly, so a gap longer than the DMA buffer
/// (DMA_BUFFERS × FRAMES / 48 kHz) empties it and the hardware repeats stale
/// samples — heard as a few seconds of buzzing. That used to happen whenever a
/// quake/EEW arrived: the announcement plays at the same moment the same event
/// forces a map refetch (TLS handshake + JPEG decode), and both ran at the
/// default priority 5 on either core. So the thread runs at a raised priority,
/// pinned to core 1 (WiFi lives on core 0), to preempt that CPU-bound work;
/// since it blocks on the DMA it still yields to everything whenever the buffer
/// is full. esp_pthread_set_cfg honours priority / core even though it ignores
/// the stack size (which Rust's thread::Builder always sets via the attr).
pub fn start(i2s: i2s::I2S0, bclk: Gpio6, ws: Gpio5, din: Gpio7, sd: Gpio4) {
    let (tx, rx) = mpsc::channel::<Playback>();
    // Priority 12: above the default-5 workers (map fetch / TLS crypto / JPEG
    // decode / display), below lwIP (18) and WiFi (23) so networking is unaffected.
    let prev = ThreadSpawnConfiguration::get();
    let _ = ThreadSpawnConfiguration {
        name: Some(b"audio\0"),
        priority: 12,
        pin_to_core: Some(Core::Core1),
        ..Default::default()
    }.set();
    let r = thread::Builder::new()
        .name("audio".into())
        .stack_size(8192)
        .spawn(move || {
            if let Err(e) = audio_task(i2s, bclk, ws, din, sd, rx) {
                RUNNING.store(false, Ordering::Relaxed);
                *PLAY_TX.lock().unwrap() = None;
                warn!("[AUDIO] stopped: {:?}", e);
            }
        });
    // Restore the default spawn config so other threads aren't affected.
    let _ = prev.unwrap_or_default().set();
    match r {
        Ok(_) => *PLAY_TX.lock().unwrap() = Some(tx),
        Err(e) => warn!("[AUDIO] failed to spawn audio thread: {:?}", e),
    }
}

fn audio_task(i2s: i2s::I2S0, bclk: Gpio6, ws: Gpio5, din: Gpio7, sd: Gpio4,
              rx: Receiver<Playback>) -> anyhow::Result<()> {
    // One full sine period, signed 16-bit.
    let table: Vec<i16> = (0..(1u32 << TABLE_BITS))
        .map(|i| {
            let p = 2.0 * core::f32::consts::PI * (i as f32) / ((1u32 << TABLE_BITS) as f32);
            (AMPLITUDE * p.sin()) as i16
        })
        .collect();

    // Philips, 16-bit stereo, but with a larger DMA ring than the default
    // 6×240 (= 30 ms): 8×360 ≈ 60 ms, so a short preemption can't empty it.
    let cfg = StdConfig::new(
        Config::default().dma_buffer_count(8).frames_per_buffer(360),
        StdClkConfig::from_sample_rate_hz(SAMPLE_RATE_HZ),
        StdSlotConfig::philips_slot_default(DataBitWidth::Bits16, SlotMode::Stereo),
        StdGpioConfig::default(),
    );
    let mut tx = I2sDriver::new_std_tx(i2s, &cfg, bclk, din, None::<AnyIOPin>, ws)?;
    let mut sd_pin = PinDriver::output(sd)?;

    let mut phase: u32 = 0; // 32-bit phase accumulator: one full turn = 2^32
    let mut buf = vec![0u8; FRAMES * 4];
    // Preload silence so the first buffers are queued when the clocks start.
    tx.preload_data(&buf)?;
    tx.tx_enable()?;
    RUNNING.store(true, Ordering::Relaxed);
    info!("[AUDIO] MAX98357A I2S running ({} Hz, 16-bit stereo)", SAMPLE_RATE_HZ);

    let mut reverb = Reverb::new();
    let mut player: Option<Player> = None;
    let mut prev: i32 = 0; // last voice sample (for the ×3 interpolation)
    let mut amp_on = false;
    loop {
        // New sentence? An EEW sentence is never cut off by a non-EEW one.
        while let Ok(pb) = rx.try_recv() {
            if player.as_ref().map(|p| p.eew).unwrap_or(false) && !pb.eew { continue; }
            player = Some(Player::new(pb));
            reverb.clear();
            prev = 0;
        }

        let tone_on = ENABLED.load(Ordering::Relaxed);
        let want_amp = tone_on || player.is_some();
        // SD_MODE high only while something plays (the clocks are always running,
        // and the sentence starts with a short silence).
        if want_amp != amp_on {
            if want_amp { sd_pin.set_high()?; } else { sd_pin.set_low()?; }
            amp_on = want_amp;
        }

        if let Some(p) = player.as_mut() {
            let vol = VOLUME.load(Ordering::Relaxed) as i32;
            let mut done = false;
            for n in 0..FRAMES / 3 {
                let (voice, beep) = match p.next() { Some(v) => v, None => { done = true; (0.0, 0.0) } };
                // Beeps get the reverb; the speech stays dry so it remains easy to understand.
                let wet = reverb.process(beep);
                let s = (voice + beep + wet * REVERB_WET).clamp(-32768.0, 32767.0) as i32;
                let cur = s * vol / 100;
                for j in 0..3i32 {
                    let v = (prev + (cur - prev) * (j + 1) / 3) as i16;
                    let b = v.to_le_bytes();
                    let o = (n * 3 + j as usize) * 4;
                    buf[o] = b[0]; buf[o + 1] = b[1]; buf[o + 2] = b[0]; buf[o + 3] = b[1];
                }
                prev = cur;
            }
            if done { player = None; prev = 0; }
        } else {
            let step = (((FREQ_HZ.load(Ordering::Relaxed) as u64) << 32) / SAMPLE_RATE_HZ as u64) as u32;
            for n in 0..FRAMES {
                let s = if tone_on { table[(phase >> (32 - TABLE_BITS)) as usize] } else { 0 };
                phase = phase.wrapping_add(step);
                let b = s.to_le_bytes();
                buf[n * 4] = b[0]; buf[n * 4 + 1] = b[1]; buf[n * 4 + 2] = b[0]; buf[n * 4 + 3] = b[1];
            }
        }
        tx.write_all(&buf, BLOCK)?;
    }
}
