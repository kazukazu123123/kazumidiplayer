//! Additive physical-ish piano. Every note is a bank of damped rotating
//! phasors (one complex multiply per sample per lane) shaped by formulas for
//! the main features of a real grand:
//!
//! - **Stiff strings**: partial k sits at `k f0 sqrt(1 + B k^2) / sqrt(1 + B)`
//!   with the inharmonicity B rising from ~1e-4 in the bass to ~1e-2 at C8.
//! - **Hammer**: struck at 1/8 of the string, so partials near multiples of
//!   8 are weak (`|sin(pi k / 8)|`); the felt acts as a low-pass whose corner
//!   rises with velocity, so harder hits are brighter, not just louder.
//! - **Two-stage decay**: the lower partials each get a pair of slightly
//!   detuned lanes, a loud fast-decaying "prompt sound" and a quiet slow
//!   "aftersound", which gives the beating and the long tail of real
//!   unison strings. Losses grow with frequency, and the fundamental's
//!   decay time shrinks from ~35 s at A0 to ~3 s at C8.
//! - **Soundboard**: weak radiation below ~90 Hz, plus a short low-passed
//!   noise "knock" at the strike.
//! - **Dampers**: note-off adds strong damping (none above F#6, like a real
//!   piano); the sustain pedal (CC64) defers it.
//! - **Stereo**: notes are spread low-left to high-right (player's view).
//!
//! Per-channel pitch bend (range set by RPN 0, default 2 semitones) changes
//! pitch sample-accurately, held notes included. Unlimited polyphony; voices
//! render in parallel on every logical core.

use crate::audio_engine::stream_stats::StreamStats;

/// Overall level before polyphonic summing. Calibrated so a 48-note full-velocity
/// chord sits near the prerender limiter's -0.5 dBFS ceiling without heavy gain reduction.
const VOICE_GAIN: f32 = 0.1;

/// The one setting this synth publishes to the settings UI.
pub const VOICE_LIMIT_SETTING: crate::audio_engine::EngineSettingDef =
    crate::audio_engine::EngineSettingDef {
        key: "voice_limit",
        label: "Voice Limit (0 = unlimited)",
        help: "Most notes the Piano engine plays at once (0 = unlimited). Over the limit, the quietest notes are faded out first, not the oldest, so loud and long-ringing notes stay. Lower uses less CPU on dense MIDIs.",
        kind: crate::audio_engine::EngineSettingKind::U32Range { min: 0, max: 100_000 },
        default: crate::audio_engine::EngineSettingValue::U32(0),
    };
/// The thread-count setting this synth publishes to the settings UI.
pub const THREADS_SETTING: crate::audio_engine::EngineSettingDef =
    crate::audio_engine::EngineSettingDef {
        key: "threads",
        label: "Threads (0 = auto)",
        help: "Worker threads the Piano engine renders with. 0 is auto: all cores but two, which stay free for the UI and the audio callback. Changing it rebuilds the worker pool; renders in flight finish on the old one.",
        kind: crate::audio_engine::EngineSettingKind::U32Range { min: 0, max: 32 },
        default: crate::audio_engine::EngineSettingValue::U32(0),
    };
/// SIMD-friendly lane count per voice.
const LANES: usize = 32;
/// Partials that get a prompt/aftersound lane pair; the rest get one lane.
const PAIRED: usize = 8;
/// Upper bound of partial frequencies.
const MAX_PARTIAL_HZ: f64 = 14_000.0;
/// Hammer strike point, as a fraction of the speaking length.
const STRIKE_POINT: f64 = 1.0 / 8.0;
/// Aftersound share of a paired partial, and how much faster prompt decays.
const AFTERSOUND: f32 = 0.32;
const PROMPT_DECAY_RATIO: f64 = 3.0;
/// Highest damped key (F#6); keys above ring until they die away.
const LAST_DAMPED: usize = 90;
/// Strike knock: length, and level relative to the tone.
const KNOCK_SECS: f64 = 0.025;
const KNOCK_LEVEL: f32 = 0.25;
const BEND_CENTER: u16 = 8192;
const DEFAULT_BEND_RANGE: f32 = 2.0;

/// A voice whose lanes have all decayed below this is dropped.
const SILENT: f32 = 1e-4;

/// Frames per entry of a cached note's loudness table (see `CachedNote::loud`).
const LOUD_BLOCK: usize = 256;
/// A voice cut by the voice limit fades out over this long instead of
/// stopping dead, which would click.
const KILL_FADE_SECS: f64 = 0.002;

/// Voices grouped into each reusable layer.
const VOICES_PER_LAYER: usize = 128;

fn note_hz(note: usize) -> f64 {
    440.0 * 2f64.powf((note as f64 - 69.0) / 12.0)
}

/// String inharmonicity coefficient: lowest around E2, rising both ways.
fn inharmonicity(note: usize) -> f64 {
    let n = note as f64;
    10f64.powf(-4.0 + (n - 40.0).max(0.0) * 0.028) * (1.0 + (40.0 - n).max(0.0) * 0.05)
}

/// 60 dB decay time of a note's aftersound fundamental, in seconds.
fn decay_secs(note: usize) -> f64 {
    let x = (note as f64 - 21.0).clamp(0.0, 87.0) / 87.0;
    35.0 * (3.0f64 / 35.0).powf(x)
}

/// Felt low-pass of the hammer at velocity `v` (0-1) for a partial.
fn hammer_filter(hz: f32, f0: f32, v: f32) -> f32 {
    let corner = (500.0 + 5_500.0 * v * v).max(2.0 * f0);
    1.0 / (1.0 + (hz / corner).powi(2))
}

/// Per-note synthesis constants, computed once per sample rate.
#[derive(Clone, Copy)]
struct NoteSpec {
    /// Phase advance per frame of each lane, in radians.
    omegas: [f64; LANES],
    /// Per-frame amplitude multiplier of each lane while undamped.
    decays: [f32; LANES],
    /// Velocity-independent amplitude of each lane.
    amps: [f32; LANES],
    /// Frequency of each lane, in Hz (for the hammer filter).
    hz: [f32; LANES],
    /// Lanes in use.
    count: usize,
    f0: f32,
    /// Extra per-frame multiplier once the damper is down (1 = no damper).
    damper: f32,
    /// Per-frame multiplier of a voice being cut by the voice limit: down
    /// 60 dB over `KILL_FADE_SECS`.
    kill_fade: f32,
    /// Knock: one-pole low-pass coefficient, per-frame decay, length.
    knock_lp: f32,
    knock_decay: f32,
    knock_frames: u32,
    /// Output gains.
    left: f32,
    right: f32,
}

impl NoteSpec {
    fn new(note: usize, rate: u32) -> Self {
        let sr = rate as f64;
        let f0 = note_hz(note);
        let b = inharmonicity(note);
        let sigma0 = 6.9 / decay_secs(note);
        let mut spec = Self {
            omegas: [0.0; LANES],
            decays: [0.0; LANES],
            amps: [0.0; LANES],
            hz: [0.0; LANES],
            count: 0,
            f0: f0 as f32,
            damper: 1.0,
            kill_fade: (-6.9 / (KILL_FADE_SECS * sr)).exp() as f32,
            knock_lp: 0.0,
            knock_decay: 0.0,
            knock_frames: (KNOCK_SECS * sr) as u32,
            left: 0.0,
            right: 0.0,
        };
        let push = |spec: &mut Self, hz: f64, sigma: f64, amp: f64| {
            let i = spec.count;
            spec.omegas[i] = std::f64::consts::TAU * hz / sr;
            spec.decays[i] = (-sigma / sr).exp() as f32;
            spec.amps[i] = amp as f32;
            spec.hz[i] = hz as f32;
            spec.count += 1;
        };
        let limit = MAX_PARTIAL_HZ.min(sr * 0.45);
        for k in 1.. {
            let kf = k as f64;
            let hz = kf * f0 * ((1.0 + b * kf * kf) / (1.0 + b)).sqrt();
            let lanes = if k <= PAIRED { 2 } else { 1 };
            if hz >= limit || spec.count + lanes > LANES {
                break;
            }
            // The hammer excites each string mode with the signed spatial
            // shape at its strike point. Keeping the sign preserves the
            // phase reversals between partials that a real strike creates.
            let comb = (std::f64::consts::PI * kf * STRIKE_POINT).sin();
            let radiation = hz * hz / (hz * hz + 90.0 * 90.0);
            // Treble partials sit sharp of the harmonic series (stiff short
            // strings) and read as harsh when they ring at full strength, so
            // the spectral rolloff steepens toward the top of the keyboard.
            // The fundamental (k = 1) is untouched at every register.
            let tilt = 1.35 + 0.5 * ((note as f64 - 60.0) / 48.0).clamp(0.0, 1.0);
            let amp = comb / kf.powf(tilt) * radiation;
            // Bass strings retain their low modes longer; treble loses upper
            // modes quickly through hammer and soundboard absorption.
            let loss = sigma0 * (1.0 + 0.11 * (kf - 1.0)) + 3e-7 * hz * hz;
            let sigma = loss;
            if lanes == 2 {
                let register = ((note as f64 - 21.0) / 87.0).clamp(0.0, 1.0);
                let detune_cents = 0.35 + register * 1.1 + kf * 0.035;
                let detune = 2f64.powf(detune_cents / 1200.0);
                push(
                    &mut spec,
                    hz,
                    sigma * PROMPT_DECAY_RATIO,
                    amp * (1.0 - AFTERSOUND as f64),
                );
                push(&mut spec, hz * detune, sigma, amp * AFTERSOUND as f64);
            } else {
                push(&mut spec, hz, sigma * PROMPT_DECAY_RATIO * 0.5, amp);
            }
        }
        if note <= LAST_DAMPED {
            // Dampers stop bass strings more slowly.
            let damp_secs = 0.08 + 0.4 * (1.0 - (note as f64 - 21.0).max(0.0) / 69.0);
            spec.damper = (-6.9 / (damp_secs * sr)).exp() as f32;
        }
        let knock_hz = 250.0 + 8.0 * f0.min(1_500.0);
        spec.knock_lp = (1.0 - (-std::f64::consts::TAU * knock_hz / sr).exp()) as f32;
        spec.knock_decay = (-5.0 / (KNOCK_SECS * sr)).exp() as f32;
        // Constant-power pan, low keys left.
        let pan = ((note as f64 - 21.0) / 87.0).clamp(0.0, 1.0) * 0.6 + 0.2;
        let angle = pan * std::f64::consts::FRAC_PI_2;
        spec.left = angle.cos() as f32;
        spec.right = angle.sin() as f32;
        spec
    }

    /// Per-frame rotation of each lane at pitch `ratio`, times `damp`.
    fn rotation(&self, ratio: f32, damp: f32) -> ([f32; LANES], [f32; LANES]) {
        let (mut cos, mut sin) = ([0.0; LANES], [0.0; LANES]);
        for k in 0..self.count {
            let w = self.omegas[k] * ratio as f64;
            let r = self.decays[k] as f64 * damp as f64;
            cos[k] = (r * w.cos()) as f32;
            sin[k] = (r * w.sin()) as f32;
        }
        (cos, sin)
    }
}

/// Lane amplitudes for a velocity (0-127). The tone is normalized at full
/// velocity, so softer hits are both quieter and darker, matching the SF2
/// engine's squared velocity response.
fn lane_amps(velocity: u8, spec: &NoteSpec) -> [f32; LANES] {
    let v = velocity as f32 / 127.0;
    let energy = |v: f32| -> f32 {
        (0..spec.count)
            .map(|k| (spec.amps[k] * hammer_filter(spec.hz[k], spec.f0, v)).powi(2))
            .sum::<f32>()
            .sqrt()
    };
    let gain = VOICE_GAIN * v * v / energy(v).max(f32::EPSILON);
    std::array::from_fn(|k| spec.amps[k] * hammer_filter(spec.hz[k], spec.f0, v) * gain)
}

/// Advance every lane one frame; returns the sum of the sine parts.
#[inline(always)]
fn step(
    re: &mut [f32; LANES],
    im: &mut [f32; LANES],
    cos: &[f32; LANES],
    sin: &[f32; LANES],
    count: usize,
) -> f32 {
    let mut sum = 0.0;
    for k in 0..count {
        let (a, b) = (re[k], im[k]);
        re[k] = a * cos[k] - b * sin[k];
        im[k] = a * sin[k] + b * cos[k];
        sum += im[k];
    }
    sum
}

/// xorshift32 step mapped to [-1, 1].
#[inline(always)]
fn next_noise(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Per-voice state that changes every frame.
#[derive(Clone, Copy)]
struct Osc {
    /// Lane phasors: amplitude and phase as a complex number; the output is
    /// the imaginary part, so strings start at rest and move like sines.
    re: [f32; LANES],
    im: [f32; LANES],
    /// Frames since note-on.
    age: u32,
    rng: u32,
    /// Knock level and low-pass state.
    knock: f32,
    knock_y: f32,
}

impl Osc {
    #[inline(always)]
    fn tick(&mut self, spec: &NoteSpec, cos: &[f32; LANES], sin: &[f32; LANES]) -> f32 {
        let mut s = step(&mut self.re, &mut self.im, cos, sin, spec.count);
        if self.age < spec.knock_frames {
            let x = next_noise(&mut self.rng) * self.knock;
            self.knock_y += spec.knock_lp * (x - self.knock_y);
            s += self.knock_y;
            self.knock *= spec.knock_decay;
            self.age += 1;
        }
        s
    }

    fn audible(&self, spec: &NoteSpec) -> bool {
        (0..spec.count).any(|k| self.re[k] * self.re[k] + self.im[k] * self.im[k] > SILENT * SILENT)
    }
}

/// Length of each cached note. Long enough that even a bass fundamental
/// (T60 ~35 s, audible for ~25 s) rings out inside the cache instead of
/// being cut off by the end-of-cache fade.
const CACHE_SECS: f64 = 30.0;
/// Fade at the end of notes still sounding when the cache runs out.
const CACHE_FADE_SECS: f64 = 0.5;

/// One key's prerendered full-velocity strike: undamped, at rest pitch,
/// mono, stored as i16 scaled by `scale`. Trailing silence is trimmed.
struct CachedNote {
    samples: Vec<i16>,
    /// Value of i16 full scale.
    scale: f32,
    /// Peak |sample| (in i16 units) of each `LOUD_BLOCK` frames: how loud the
    /// strike is at any point of its life. Every voice plays this same
    /// recording, so a voice's current amplitude is one lookup here times its
    /// own gains -- which is what makes the amplitude-based voice limit cheap.
    loud: Vec<f32>,
    /// Suffix maximum of `loud`: the loudest the strike still gets from a
    /// block onward. Once that, times a voice's own gains, is inaudible, the
    /// voice can never be heard again and is dropped without rendering.
    loud_suffix: Vec<f32>,
}

/// All 128 keys at one sample rate.
struct Cache {
    notes: Vec<CachedNote>,
}

impl Cache {
    /// The cache for `rate`, rendered once per process (all keys in
    /// parallel) and shared by every engine at that rate.
    fn get(rate: u32) -> std::sync::Arc<Self> {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex, OnceLock};
        static CACHES: OnceLock<Mutex<HashMap<u32, Arc<Cache>>>> = OnceLock::new();
        let mut caches = CACHES.get_or_init(Default::default).lock().unwrap();
        caches
            .entry(rate)
            .or_insert_with(|| {
                use rayon::prelude::*;
                let notes = crate::audio_engine::prerender::synth_pool().install(|| {
                    (0..128)
                        .into_par_iter()
                        .map(|n| render_note(n, rate))
                        .collect()
                });
                Arc::new(Self { notes })
            })
            .clone()
    }
}

/// Render key `note` struck at full velocity for `CACHE_SECS`.
fn render_note(note: usize, rate: u32) -> CachedNote {
    let spec = NoteSpec::new(note, rate);
    let len = (CACHE_SECS * rate as f64) as usize;
    let mut osc = Osc {
        re: lane_amps(127, &spec),
        im: [0.0; LANES],
        age: 0,
        rng: 0x9E37_79B9 ^ (note as u32).wrapping_mul(0x85EB_CA6B) | 1,
        knock: KNOCK_LEVEL * VOICE_GAIN,
        knock_y: 0.0,
    };
    let (cos, sin) = spec.rotation(1.0, 1.0);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(osc.tick(&spec, &cos, &sin));
        // Stop once every lane is inaudible (after the knock).
        if osc.age >= spec.knock_frames && out.len() % 1024 == 0 && !osc.audible(&spec) {
            break;
        }
    }
    let last = out
        .iter()
        .rposition(|s| s.abs() > SILENT)
        .map_or(0, |i| i + 1);
    out.truncate(last);
    if out.len() == len {
        let fade = ((CACHE_FADE_SECS * rate as f64) as usize).min(len);
        for (i, s) in out[len - fade..].iter_mut().enumerate() {
            let x = i as f32 / fade as f32;
            *s *= 0.5 + 0.5 * (std::f32::consts::PI * x).cos();
        }
    }
    let peak = out
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()))
        .max(f32::EPSILON);
    let to_i16 = i16::MAX as f32 / peak;
    let samples: Vec<i16> = out.iter().map(|s| (s * to_i16).round() as i16).collect();
    let loud: Vec<f32> = samples
        .chunks(LOUD_BLOCK)
        .map(|block| block.iter().map(|&s| (s as i32).abs()).max().unwrap_or(0) as f32)
        .collect();
    let mut loud_suffix = vec![0.0f32; loud.len()];
    let mut rest = 0.0f32;
    for (i, &level) in loud.iter().enumerate().rev() {
        rest = rest.max(level);
        loud_suffix[i] = rest;
    }
    CachedNote {
        samples,
        scale: peak / i16::MAX as f32,
        loud,
        loud_suffix,
    }
}

struct Voice {
    /// Creation order; `voices` stays sorted by it.
    seq: u64,
    channel: u8,
    note: u8,
    /// Read position in the cached note, in frames.
    pos: f32,
    /// Velocity loudness, times the note's i16 scale.
    gain: f32,
    /// Velocity brightness: one-pole low-pass coefficient (1 = bypass).
    lp: f32,
    lp_y: f32,
    /// Damper envelope.
    env: f32,
    /// Damper down (note released, pedal up).
    damped: bool,
    /// Key released while the sustain pedal was down.
    pedal_held: bool,
    /// Cut by the voice limit: fading out over `KILL_FADE_SECS`.
    fading: bool,
    dead: bool,
    // Within the current render, set while applying its events. Recording
    // them up front lets every voice render the whole chunk on its own.
    /// First frame it sounds (its note-on frame, else 0).
    start: usize,
    /// Frame the damper falls.
    release_at: Option<usize>,
    /// Frame it is cut off (player reset).
    kill_at: Option<usize>,
}

/// Add `s[index..index + (to - from)]`, scaled by the constant gains
/// `gl`/`gr`, into `left`/`right` at `from..to`; returns the advanced
/// index. A plain multiply-accumulate over slices, in the shape the
/// compiler vectorizes.
fn mix_flat(
    left: &mut [f32],
    right: &mut [f32],
    s: &[i16],
    index: usize,
    from: usize,
    to: usize,
    gl: f32,
    gr: f32,
) -> usize {
    let n = to - from;
    if n == 0 {
        // Nothing to add. The read position may sit past the end of the
        // strike (a pitch-bend skip can leave it there), which is out of
        // range even for an empty slice.
        return index;
    }
    let src = &s[index..index + n];
    for ((l, r), &v) in left[from..to].iter_mut().zip(&mut right[from..to]).zip(src) {
        let x = v as f32;
        *l += x * gl;
        *r += x * gr;
    }
    index + n
}

impl Voice {
    fn releasing(&self) -> bool {
        self.damped || self.release_at.is_some()
    }

    fn alive(&self) -> bool {
        self.kill_at.is_none()
    }

    /// Add this voice's part of the render into `left`/`right`, then fold
    /// the render's events into its state for the next one. `bends` is its
    /// channel's `(start frame, pitch ratio)` segments for this render.
    fn render(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        spec: &NoteSpec,
        cached: &CachedNote,
        bends: &[(usize, f32)],
        channel_gain: f32,
        channel_pan: f32,
    ) {
        let len = left.len();
        let end = self.kill_at.unwrap_or(len).min(len);
        let start = self.start.min(end);
        let damp_from = if self.damped {
            start
        } else {
            self.release_at.unwrap_or(end).clamp(start, end)
        };
        let s = &cached.samples;
        let (pan_left, pan_right) = {
            let angle = channel_pan.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
            (angle.cos(), angle.sin())
        };
        let (gl, gr) = (
            spec.left * self.gain * channel_gain * pan_left * 1.4142,
            spec.right * self.gain * channel_gain * pan_right * 1.4142,
        );
        let (mut pos, mut y, mut env) = (self.pos, self.lp_y, self.env);
        let lp = self.lp;
        let mut ended = false;
        // A voice cut by the voice limit fades on top of whatever else it does.
        let fade = if self.fading { spec.kill_fade } else { 1.0 };
        // Peak output this voice can still produce at its own gains: the
        // strike's remaining loudness times velocity gain, damper envelope
        // and stereo spread. Channel gains are left out of the death test
        // (they can change later, and a dropped voice never comes back) but
        // included in the per-chunk skip test, which every render redoes.
        let own_peak = self.gain * env * spec.left.max(spec.right) * 1.4142;
        let block = pos as usize / LOUD_BLOCK;
        if cached.loud_suffix.get(block).copied().unwrap_or(0.0) * own_peak <= SILENT {
            // The strike has decayed past audibility for this voice: drop
            // it without rendering; it will never be heard again.
            self.dead = true;
            self.start = 0;
            self.release_at = None;
            self.kill_at = None;
            return;
        }
        if bends.len() == 1 && bends[0].1 == 1.0 && pos.fract() == 0.0 {
            let mut index = pos as usize;
            let available = s.len().saturating_sub(index.saturating_add(1));
            let frames = (end - start).min(available);
            let sample_end = start + frames;
            let damp_start = damp_from.clamp(start, sample_end);
            // Skip the span when even its loudest block is inaudible at
            // this render's full gains: advance the position and the
            // envelopes and write nothing.
            let last_block =
                ((index + frames) / LOUD_BLOCK).min(cached.loud.len().saturating_sub(1));
            let span_peak = cached
                .loud
                .get(block..=last_block)
                .map(|b| b.iter().fold(0.0f32, |m, &v| m.max(v)))
                .unwrap_or(0.0);
            if frames > 0 && span_peak * own_peak * channel_gain <= SILENT {
                env *= fade.powi((damp_start - start) as i32)
                    * (spec.damper * fade).powi((sample_end - damp_start) as i32);
                y *= (1.0 - lp).powi(frames as i32);
                index += frames;
            } else if lp == 1.0 {
                // Full-velocity strike: the low-pass state is the sample
                // itself, so a span whose envelope is constant is a plain
                // multiply-accumulate (see mix_flat).
                if fade == 1.0 {
                    index = mix_flat(
                        left,
                        right,
                        s,
                        index,
                        start,
                        damp_start,
                        gl * env,
                        gr * env,
                    );
                } else {
                    for i in start..damp_start {
                        let x = s[index] as f32;
                        left[i] += x * gl * env;
                        right[i] += x * gr * env;
                        env *= fade;
                        index += 1;
                    }
                }
                let damp = spec.damper * fade;
                if damp == 1.0 {
                    index = mix_flat(
                        left,
                        right,
                        s,
                        index,
                        damp_start,
                        sample_end,
                        gl * env,
                        gr * env,
                    );
                } else {
                    for i in damp_start..sample_end {
                        let x = s[index] as f32;
                        left[i] += x * gl * env;
                        right[i] += x * gr * env;
                        env *= damp;
                        index += 1;
                    }
                }
                if frames > 0 {
                    y = s[index - 1] as f32;
                }
            } else {
                if self.fading {
                    for i in start..damp_start {
                        y += lp * (s[index] as f32 - y);
                        left[i] += y * gl * env;
                        right[i] += y * gr * env;
                        env *= fade;
                        index += 1;
                    }
                } else {
                    for i in start..damp_start {
                        y += lp * (s[index] as f32 - y);
                        left[i] += y * gl * env;
                        right[i] += y * gr * env;
                        index += 1;
                    }
                }
                let damp = spec.damper * fade;
                for i in damp_start..sample_end {
                    y += lp * (s[index] as f32 - y);
                    left[i] += y * gl * env;
                    right[i] += y * gr * env;
                    env *= damp;
                    index += 1;
                }
            }
            ended = sample_end < end;
            pos = index as f32;
        } else {
            'segments: for (si, &(from, ratio)) in bends.iter().enumerate() {
                let to = bends.get(si + 1).map_or(len, |b| b.0);
                let (lo, hi) = (from.max(start), to.min(end));
                if lo >= hi {
                    continue;
                }
                let mid = damp_from.clamp(lo, hi);
                for (range, damp) in [(lo..mid, fade), (mid..hi, spec.damper * fade)] {
                    let n = range.end - range.start;
                    if n == 0 {
                        continue;
                    }
                    // Skip a span whose loudest covered block is inaudible
                    // at this render's gains, as in the unbent path. The
                    // block under the read position is checked first: if it
                    // alone is audible, the span cannot be skipped and the
                    // full span maximum is never computed.
                    let first = pos as usize / LOUD_BLOCK;
                    if cached.loud.get(first).copied().unwrap_or(0.0) * own_peak * channel_gain
                        <= SILENT
                    {
                        let last = ((pos + ratio * n as f32) as usize / LOUD_BLOCK)
                            .min(cached.loud.len().saturating_sub(1));
                        let span_peak = cached
                            .loud
                            .get(first..=last)
                            .map(|b| b.iter().fold(0.0f32, |m, &v| m.max(v)))
                            .unwrap_or(0.0);
                        if span_peak * own_peak * channel_gain <= SILENT {
                            env *= damp.powi(n as i32);
                            y *= (1.0 - lp).powi(n as i32);
                            pos += ratio * n as f32;
                            continue;
                        }
                    }
                    // Step the read position as an integer index plus a
                    // fractional part instead of one float position: the
                    // index stays exact however far into the cache the
                    // voice gets, and the per-sample work loses its
                    // float-to-int conversion.
                    let mut idx = pos as usize;
                    let mut frac = pos - idx as f32;
                    let step_int = ratio as usize;
                    let step_frac = ratio - step_int as f32;
                    for i in range {
                        if idx + 1 >= s.len() {
                            ended = true;
                            break 'segments;
                        }
                        let (a, b) = (s[idx] as f32, s[idx + 1] as f32);
                        y += lp * (a + (b - a) * frac - y);
                        left[i] += y * gl * env;
                        right[i] += y * gr * env;
                        env *= damp;
                        frac += step_frac;
                        let carry = (frac >= 1.0) as usize;
                        idx += step_int + carry;
                        frac -= carry as f32;
                    }
                    pos = idx as f32 + frac;
                }
            }
        }
        (self.pos, self.lp_y, self.env) = (pos, y, env);
        self.damped |= self.release_at.is_some();
        let silent = env * self.gain * (i16::MAX as f32) < SILENT;
        // A faded voice is gone once it is 60 dB down, quiet strike or not.
        let faded = self.fading && env < 1e-3;
        self.dead = self.kill_at.is_some() || ended || silent || faded;
        self.start = 0;
        self.release_at = None;
        self.kill_at = None;
    }
}

/// Controller state of one MIDI channel.
#[derive(Clone, Copy)]
struct Channel {
    bend: u16,
    /// Semitones at full bend.
    range: f32,
    /// Selected RPN (MSB, LSB); 127/127 is none.
    rpn: (u8, u8),
    /// MIDI channel volume (CC7), normalized.
    volume: f32,
    /// MIDI channel expression (CC11), normalized.
    expression: f32,
    /// MIDI channel pan (CC10), normalized left to right.
    pan: f32,
    /// Soft pedal (CC67).
    soft: bool,
    sustain: bool,
}

impl Channel {
    const RESET: Self = Self {
        bend: BEND_CENTER,
        range: DEFAULT_BEND_RANGE,
        rpn: (127, 127),
        volume: 1.0,
        expression: 1.0,
        pan: 0.5,
        soft: false,
        sustain: false,
    };

    fn ratio(&self) -> f32 {
        let semitones = (self.bend as f32 - BEND_CENTER as f32) / BEND_CENTER as f32 * self.range;
        2f32.powf(semitones / 12.0)
    }
}

struct MixLayer {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl MixLayer {
    fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    fn resize(&mut self, len: usize) {
        self.left.resize(len, 0.0);
        self.right.resize(len, 0.0);
    }
}

pub struct PianoPrerender {
    rate: u32,
    notes: Box<[NoteSpec; 128]>,
    cache: std::sync::Arc<Cache>,
    voices: Vec<Voice>,
    /// `(frame offset, message)` for the next render, in send order.
    scheduled: Vec<(u32, u32)>,
    channels: [Channel; 16],
    /// Per (channel, key), serials of voices pressed and not yet released,
    /// oldest first. Entries of voices dropped meanwhile are skipped.
    held: Vec<std::collections::VecDeque<u64>>,
    next_seq: u64,
    /// Per channel, `(start frame, pitch ratio)` segments of this render.
    bends: [Vec<(usize, f32)>; 16],
    layers: Vec<MixLayer>,
    /// Most voices sounding at once; 0 = unlimited. Over it, the quietest go.
    voice_limit: u32,
    /// The voices counted against the limit, quietest on top: (amplitude as
    /// f32 bits -- order-preserving for non-negative floats -- voice index).
    /// Rebuilt each render from the voices left by the last one; indices stay
    /// valid within a render because voices are only appended until its end.
    limit_heap: std::collections::BinaryHeap<std::cmp::Reverse<(u32, u32)>>,
    /// Voices in `limit_heap`, i.e. sounding and not being cut.
    limit_active: usize,
}

/// Marks a `held` entry whose note-on the voice limit turned away: its
/// note-off consumes the entry and releases nothing.
const TURNED_AWAY: u64 = 1 << 63;

impl PianoPrerender {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            notes: Box::new(std::array::from_fn(|note| NoteSpec::new(note, rate))),
            cache: Cache::get(rate),
            voices: Vec::new(),
            scheduled: Vec::new(),
            channels: [Channel::RESET; 16],
            held: vec![std::collections::VecDeque::new(); 16 * 128],
            next_seq: 0,
            bends: std::array::from_fn(|_| Vec::new()),
            layers: Vec::new(),
            voice_limit: 0,
            limit_heap: std::collections::BinaryHeap::new(),
            limit_active: 0,
        }
    }

    /// Most voices sounding at once; 0 = unlimited.
    pub fn set_voice_limit(&mut self, limit: u32) {
        self.voice_limit = limit;
    }

    /// A voice's current amplitude. No analysis needed: every voice plays its
    /// key's cached strike, whose loudness over time is tabulated (`loud`), so
    /// it is that table at the voice's position times its velocity gain,
    /// damper envelope, channel volume/expression and stereo spread.
    fn amplitude(&self, note: u8, channel: u8, pos: f32, gain: f32, env: f32) -> f32 {
        let n = note as usize;
        let level = self.cache.notes[n]
            .loud
            .get(pos as usize / LOUD_BLOCK)
            .copied()
            .unwrap_or(0.0);
        let ch = &self.channels[channel as usize];
        let spec = &self.notes[n];
        level * gain * env * ch.volume * ch.expression * spec.left.max(spec.right)
    }

    /// Voice limit, amplitude based: the *quietest* voice goes first, not the
    /// oldest, so a loud long-ringing note outlasts a fresh pianissimo one.
    ///
    /// Every event of a render is known before any of it is rendered, so the
    /// limit is enforced at each note-on rather than after the fact: a new
    /// note quieter than the quietest sounding voice never becomes a voice at
    /// all, and a louder one takes that voice's place (cut with a fade of
    /// `KILL_FADE_SECS`). The quietest is the top of a min-heap, so a note-on
    /// costs O(log limit), and the synth never holds more than `limit` voices
    /// -- cutting after rendering them, as at first, let a dense chunk create
    /// tens of thousands of voices only to fade them out again.
    ///
    /// This sets up the heap for a render from the voices the last one left.
    fn prepare_limit(&mut self) {
        self.limit_heap.clear();
        self.limit_active = 0;
        let limit = self.voice_limit as usize;
        if limit == 0 {
            return;
        }
        let mut entries = Vec::with_capacity(self.voices.len());
        for (i, v) in self.voices.iter().enumerate() {
            // Already being cut: it dies within this render.
            if v.fading || !v.alive() {
                continue;
            }
            let amp = self.amplitude(v.note, v.channel, v.pos, v.gain, v.env);
            entries.push(std::cmp::Reverse((amp.to_bits(), i as u32)));
        }
        self.limit_active = entries.len();
        self.limit_heap = std::collections::BinaryHeap::from(entries);
        // The limit was lowered: cut back to it now.
        while self.limit_active > limit {
            if let Some(std::cmp::Reverse((_, i))) = self.limit_heap.pop() {
                self.voices[i as usize].fading = true;
            }
            self.limit_active -= 1;
        }
    }

    /// Start every channel's bend segments at its current pitch.
    fn begin_render(&mut self) {
        for (bends, channel) in self.bends.iter_mut().zip(&self.channels) {
            bends.clear();
            bends.push((0, channel.ratio()));
        }
    }

    /// The channel's pitch changed at `frame`: start a new segment.
    fn bend_changed(&mut self, channel: usize, frame: usize) {
        let ratio = self.channels[channel].ratio();
        let bends = &mut self.bends[channel];
        match bends.last_mut() {
            Some(last) if last.0 == frame => last.1 = ratio,
            _ => bends.push((frame, ratio)),
        }
    }

    /// Run at the default output's rate.
    pub fn open() -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let rate = cpal::default_host()
            .default_output_device()
            .ok_or("No audio output device found")?
            .default_output_config()
            .map_err(|e| format!("No usable output format: {e}"))?
            .sample_rate();
        Ok(Self::new(rate))
    }

    /// Voices sounding (releasing included) after the last render.
    pub fn polyphony(&self) -> u32 {
        self.voices.len() as u32
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Schedule a message `frame` frames into the next `render`.
    pub fn send_at(&mut self, frame: u32, data: u32) {
        self.scheduled.push((frame, data));
    }

    /// Drop the damper on `channel`'s keys released under the pedal.
    fn pedal_up(&mut self, channel: u8, frame: usize) {
        for v in &mut self.voices {
            if v.channel == channel && v.pedal_held {
                v.pedal_held = false;
                if v.alive() && !v.releasing() {
                    v.release_at = Some(frame);
                }
            }
        }
    }

    /// Record `msg` at `frame` of the coming render on the voices.
    fn apply(&mut self, frame: usize, msg: u32) {
        let status = (msg & 0xFF) as u8;
        let p1 = ((msg >> 8) & 0x7F) as u8;
        let p2 = ((msg >> 16) & 0x7F) as u8;
        let channel = status & 0x0F;
        match status & 0xF0 {
            0x90 if p2 > 0 => {
                let seq = self.next_seq;
                self.next_seq += 1;
                let spec = &self.notes[p1 as usize];
                let v = p2 as f32 / 127.0;
                // Softer hits: the felt corner falls, so the tone darkens.
                let lp = if p2 == 127 {
                    1.0
                } else {
                    let corner = (500.0 + 5_500.0 * v * v).max(2.0 * spec.f0);
                    1.0 - (-std::f32::consts::TAU * corner / self.rate as f32).exp()
                };
                let gain = v.powf(1.5)
                    * self.cache.notes[p1 as usize].scale
                    * if self.channels[channel as usize].soft {
                        0.7
                    } else {
                        1.0
                    };
                let held = channel as usize * 128 + p1 as usize;
                let limit = self.voice_limit as usize;
                if limit > 0 {
                    let amp = self.amplitude(p1, channel, 0.0, gain, 1.0);
                    if self.limit_active >= limit {
                        match self.limit_heap.peek() {
                            // Strictly louder than the quietest voice: it takes
                            // its place. (Strictly, so a stack of equal notes
                            // in one chunk does not keep replacing itself.)
                            Some(&std::cmp::Reverse((bits, i))) if f32::from_bits(bits) < amp => {
                                self.limit_heap.pop();
                                let victim = &mut self.voices[i as usize];
                                if victim.pos == 0.0 {
                                    // Started in this very render, so it has not
                                    // made a sound yet: drop it unheard rather
                                    // than render a fade of it. On a dense chunk
                                    // most replaced voices are of this kind.
                                    victim.kill_at = Some(victim.start);
                                } else {
                                    victim.fading = true;
                                }
                                self.limit_active -= 1;
                            }
                            // Quieter than everything sounding: never starts.
                            // Its note-off still has to find its own entry.
                            _ => {
                                self.held[held].push_back(seq | TURNED_AWAY);
                                return;
                            }
                        }
                    }
                    self.limit_heap
                        .push(std::cmp::Reverse((amp.to_bits(), self.voices.len() as u32)));
                    self.limit_active += 1;
                }
                self.held[held].push_back(seq);
                self.voices.push(Voice {
                    seq,
                    channel,
                    note: p1,
                    pos: 0.0,
                    gain,
                    lp,
                    lp_y: 0.0,
                    env: 1.0,
                    damped: false,
                    pedal_held: false,
                    fading: false,
                    dead: false,
                    start: frame,
                    release_at: None,
                    kill_at: None,
                });
            }
            0x80 | 0x90 => {
                // Release the oldest held voice for this key.
                let sustain = self.channels[channel as usize].sustain;
                let queue = &mut self.held[channel as usize * 128 + p1 as usize];
                while let Some(seq) = queue.pop_front() {
                    // The oldest press on this key is the one this note-off
                    // ends. If the voice limit turned it away, or cut it, or
                    // it died out, there is nothing left to release -- and
                    // releasing the next press instead would end a note that
                    // is still held, early.
                    if seq & TURNED_AWAY != 0 {
                        break;
                    }
                    let Ok(i) = self.voices.binary_search_by_key(&seq, |v| v.seq) else {
                        break;
                    };
                    let v = &mut self.voices[i];
                    // Its own press either way: release it if it still can be.
                    if !v.fading && v.alive() && !v.releasing() && !v.pedal_held {
                        if sustain {
                            v.pedal_held = true;
                        } else {
                            v.release_at = Some(frame);
                        }
                    }
                    break;
                }
            }
            0xE0 => {
                self.channels[channel as usize].bend = p1 as u16 | (p2 as u16) << 7;
                self.bend_changed(channel as usize, frame);
            }
            0xB0 if matches!(p1, 7 | 10 | 11 | 64 | 66 | 67) => match p1 {
                7 => self.channels[channel as usize].volume = p2 as f32 / 127.0,
                10 => self.channels[channel as usize].pan = p2 as f32 / 127.0,
                11 => self.channels[channel as usize].expression = p2 as f32 / 127.0,
                64 => {
                    let down = p2 >= 64;
                    let was = std::mem::replace(&mut self.channels[channel as usize].sustain, down);
                    if was && !down {
                        self.pedal_up(channel, frame);
                    }
                }
                66 => {}
                67 => self.channels[channel as usize].soft = p2 >= 64,
                _ => unreachable!(),
            },
            0xB0 if matches!(p1, 6 | 38 | 100 | 101 | 121) => {
                let ch = &mut self.channels[channel as usize];
                match p1 {
                    101 => ch.rpn.0 = p2,
                    100 => ch.rpn.1 = p2,
                    // RPN 0: pitch bend sensitivity (semitones, then cents).
                    6 if ch.rpn == (0, 0) => ch.range = p2 as f32 + ch.range.fract(),
                    38 if ch.rpn == (0, 0) => ch.range = ch.range.trunc() + p2 as f32 / 100.0,
                    // Reset all controllers: centre the wheel, deselect RPN,
                    // lift the pedal.
                    121 => {
                        ch.bend = BEND_CENTER;
                        ch.rpn = (127, 127);
                        ch.volume = 1.0;
                        ch.expression = 1.0;
                        ch.pan = 0.5;
                        ch.soft = false;
                        if std::mem::replace(&mut ch.sustain, false) {
                            self.pedal_up(channel, frame);
                        }
                    }
                    _ => return,
                }
                self.bend_changed(channel as usize, frame);
            }
            // All sound off / all notes off.
            0xB0 if p1 == 120 || p1 == 123 => {
                for v in &mut self.voices {
                    if v.alive() && !v.releasing() && v.channel == channel {
                        v.pedal_held = false;
                        v.release_at = Some(frame);
                    }
                }
                let keys = channel as usize * 128..(channel as usize + 1) * 128;
                self.held[keys].iter_mut().for_each(|q| q.clear());
            }
            0xF0 if status == 0xFF => {
                for v in self.voices.iter_mut().filter(|v| v.alive()) {
                    v.kill_at = Some(frame);
                }
                // Every voice is gone: the whole limit is free again.
                self.limit_heap.clear();
                self.limit_active = 0;
                self.channels = [Channel::RESET; 16];
                self.held.iter_mut().for_each(|q| q.clear());
                for ch in 0..16 {
                    self.bend_changed(ch, frame);
                }
            }
            _ => {}
        }
    }

    /// Voices render independently on the synth pool (one thread per logical
    /// core, below normal priority), each group into its own buffers; the
    /// buffers are summed.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let len = left.len();
        // Stable: same-frame events keep their send order.
        let mut scheduled = std::mem::take(&mut self.scheduled);
        scheduled.sort_by_key(|&(frame, _)| frame);
        self.begin_render();
        // The voice limit decides each note-on as it is applied.
        self.prepare_limit();
        for &(frame, msg) in &scheduled {
            // Offsets past the chunk (shouldn't happen) apply at its end.
            self.apply((frame as usize).min(len), msg);
        }
        scheduled.clear();
        self.scheduled = scheduled;
        let (bends, notes, cache, channels) =
            (&self.bends, &self.notes, &*self.cache, self.channels);

        let layer_count = self.voices.len().div_ceil(VOICES_PER_LAYER);
        left.fill(0.0);
        right.fill(0.0);
        if self.layers.len() < layer_count {
            self.layers.resize_with(layer_count, MixLayer::new);
        }
        for layer in self.layers.iter_mut().take(layer_count) {
            layer.resize(len);
            layer.left.fill(0.0);
            layer.right.fill(0.0);
        }
        if layer_count <= 1 {
            for v in &mut self.voices {
                let (n, b) = (v.note as usize, &bends[v.channel as usize]);
                let channel_state = channels[v.channel as usize];
                let channel_gain = channel_state.volume * channel_state.expression;
                let channel_pan = channel_state.pan;
                v.render(
                    left,
                    right,
                    &notes[n],
                    &cache.notes[n],
                    b,
                    channel_gain,
                    channel_pan,
                );
            }
        } else {
            use rayon::prelude::*;
            let layers = &mut self.layers[..layer_count];
            let voices = &mut self.voices;
            crate::audio_engine::prerender::synth_pool().install(|| {
                voices
                    .par_chunks_mut(VOICES_PER_LAYER)
                    .zip(layers.par_iter_mut())
                    .for_each(|(voices, layer)| {
                        for v in voices {
                            let (n, b) = (v.note as usize, &bends[v.channel as usize]);
                            let channel_state = channels[v.channel as usize];
                            let channel_gain = channel_state.volume * channel_state.expression;
                            let channel_pan = channel_state.pan;
                            v.render(
                                &mut layer.left,
                                &mut layer.right,
                                &notes[n],
                                &cache.notes[n],
                                b,
                                channel_gain,
                                channel_pan,
                            );
                        }
                    });
            });
            for layer in &self.layers[..layer_count] {
                left.iter_mut().zip(&layer.left).for_each(|(a, b)| *a += b);
                right
                    .iter_mut()
                    .zip(&layer.right)
                    .for_each(|(a, b)| *a += b);
            }
        }
        // Not clamped: peaks are left to the prerender limiter.
        self.voices.retain(|v| !v.dead);
    }
}

/// Frames rendered per audio-callback block of the realtime stream.
/// 256 frames at 48 kHz quantizes note onsets to about 5 ms, the same
/// chunk size the prerender path renders in.
const STREAM_BLOCK: usize = 256;

/// Messages from the realtime engine's handle to its audio callback.
enum StreamMsg {
    Midi(u32),
    VoiceLimit(u32),
    LimiterBypassed(bool),
}

/// Lives in the audio callback: applies queued messages at each block
/// boundary, renders one block of the prerender synth, and limits it with
/// the same settings the prerender path uses (that limiter adds no delay,
/// it only smooths a peak envelope, so realtime latency stays one block).
struct StreamRenderer {
    piano: PianoPrerender,
    rx: std::sync::mpsc::Receiver<StreamMsg>,
    stats: std::sync::Arc<StreamStats>,
    /// Smoothed render load, published into `stats` after every block.
    load: f32,
    limiter: crate::audio_limiter::AudioLimiter,
    limiter_bypassed: bool,
    left: [f32; STREAM_BLOCK],
    right: [f32; STREAM_BLOCK],
    interleaved: Vec<f32>,
    pos: usize,
}

impl StreamRenderer {
    fn render_block(&mut self) {
        for msg in self.rx.try_iter() {
            match msg {
                StreamMsg::Midi(data) => self.piano.send_at(0, data),
                StreamMsg::VoiceLimit(limit) => self.piano.set_voice_limit(limit),
                StreamMsg::LimiterBypassed(bypassed) => self.limiter_bypassed = bypassed,
            }
        }
        let started = std::time::Instant::now();
        self.piano.render(&mut self.left, &mut self.right);
        let took = started.elapsed().as_secs_f32();
        let budget = STREAM_BLOCK as f32 / self.piano.rate() as f32;
        self.load += (took / budget - self.load) * 0.05;
        self.stats.publish(self.piano.polyphony(), self.load);
        self.interleaved.clear();
        for i in 0..STREAM_BLOCK {
            self.interleaved.push(self.left[i]);
            self.interleaved.push(self.right[i]);
        }
        if !self.limiter_bypassed {
            self.limiter.process(&mut self.interleaved);
        }
        self.pos = 0;
    }

    fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        for frame in out.chunks_mut(channels) {
            if self.pos == STREAM_BLOCK {
                self.render_block();
            }
            let l = self.interleaved[self.pos * 2];
            let r = self.interleaved[self.pos * 2 + 1];
            self.pos += 1;
            if channels == 1 {
                frame[0] = T::from_sample((l + r) * 0.5);
                continue;
            }
            for (i, sample) in frame.iter_mut().enumerate() {
                let v = match i {
                    0 => l,
                    1 => r,
                    _ => 0.0,
                };
                *sample = T::from_sample(v);
            }
        }
    }
}

fn build_piano_stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut renderer: StreamRenderer,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let channels = config.channels as usize;
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| renderer.fill(out, channels),
            |err| eprintln!("[WARN] Piano output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to open audio output: {e}"))
}

/// The piano synth as a realtime engine: the same voices and cached strikes
/// as [`PianoPrerender`], rendered block by block in the audio callback
/// instead of ahead of playback into a buffer.
pub struct PianoStream {
    /// Messages for the audio callback, applied at the next block.
    tx: std::sync::mpsc::Sender<StreamMsg>,
    /// Stats the audio callback publishes (see [`StreamStats`]).
    stats: std::sync::Arc<StreamStats>,
    /// Dropping this ends the thread that owns the (non-Send) cpal stream.
    _shutdown: std::sync::mpsc::Sender<()>,
}

impl PianoStream {
    /// Build the note cache and open the default output on a background
    /// thread. The result arrives on the returned receiver.
    pub fn spawn() -> std::sync::mpsc::Receiver<Result<PianoStream, String>> {
        let (tx_result, rx_result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (tx_shutdown, rx_shutdown) = std::sync::mpsc::channel::<()>();
            let stream = match open_piano_stream(tx_shutdown) {
                Ok((stream, handle)) => {
                    if tx_result.send(Ok(handle)).is_err() {
                        return;
                    }
                    stream
                }
                Err(e) => {
                    let _ = tx_result.send(Err(e));
                    return;
                }
            };
            // Returns once the handle (and its shutdown sender) is dropped.
            let _ = rx_shutdown.recv();
            drop(stream);
        });
        rx_result
    }

    pub fn send(&self, data: u32) {
        let _ = self.tx.send(StreamMsg::Midi(data));
    }

    pub fn set_voice_limit(&self, limit: u32) {
        let _ = self.tx.send(StreamMsg::VoiceLimit(limit));
    }

    pub fn set_limiter_bypassed(&self, bypassed: bool) {
        let _ = self.tx.send(StreamMsg::LimiterBypassed(bypassed));
    }

    /// Voices sounding after the last rendered block.
    pub fn polyphony(&self) -> u32 {
        self.stats.polyphony()
    }

    /// Smoothed share of one block's playback time spent rendering it
    /// (1.0 = the whole block, i.e. 100%).
    pub fn render_load(&self) -> f32 {
        self.stats.load()
    }
}

fn open_piano_stream(
    shutdown: std::sync::mpsc::Sender<()>,
) -> Result<(cpal::Stream, PianoStream), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No audio output device found")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("No usable output format: {e}"))?;
    let rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();

    let (tx, rx) = std::sync::mpsc::channel();
    let stats = std::sync::Arc::new(StreamStats::default());
    let renderer = StreamRenderer {
        piano: PianoPrerender::new(rate),
        rx,
        stats: stats.clone(),
        load: 0.0,
        // Same settings as the prerender path's limiter (see prerender.rs),
        // on interleaved stereo so both channels share one gain.
        limiter: crate::audio_limiter::AudioLimiter::new(rate as f32 * 2.0, 0.0, 100.0, 50.0),
        limiter_bypassed: false,
        left: [0.0; STREAM_BLOCK],
        right: [0.0; STREAM_BLOCK],
        interleaved: Vec::with_capacity(STREAM_BLOCK * 2),
        pos: STREAM_BLOCK,
    };
    let stream = match format {
        cpal::SampleFormat::F32 => build_piano_stream::<f32>(&device, config, renderer)?,
        cpal::SampleFormat::I16 => build_piano_stream::<i16>(&device, config, renderer)?,
        cpal::SampleFormat::U16 => build_piano_stream::<u16>(&device, config, renderer)?,
        cpal::SampleFormat::I32 => build_piano_stream::<i32>(&device, config, renderer)?,
        other => return Err(format!("Unsupported output sample format: {other}")),
    };
    stream
        .play()
        .map_err(|e| format!("Failed to start audio output: {e}"))?;
    Ok((
        stream,
        PianoStream {
            tx,
            stats,
            _shutdown: shutdown,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: u32 = 0x90;
    const OFF: u32 = 0x80;

    fn peak(l: &[f32]) -> f32 {
        l.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// Render `secs` seconds in 512-frame chunks; returns the left channel.
    fn run(e: &mut PianoPrerender, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let (mut l, mut r) = (vec![0.0; 512], vec![0.0; 512]);
        while out.len() < frames {
            e.render(&mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    }

    #[test]
    fn note_starts_at_its_frame_and_damper_stops_it() {
        let mut e = PianoPrerender::new(48_000);
        let (mut l, mut r) = (vec![0.0; 4_800], vec![0.0; 4_800]);
        e.send_at(10, ON | 60 << 8 | 100 << 16);
        e.send_at(2_400, OFF | 60 << 8);
        e.render(&mut l, &mut r);
        assert!(l[..10].iter().all(|&s| s == 0.0));
        assert!(peak(&l[11..2_400]) > 0.0);
        // Dampers kill middle C within about half a second.
        run(&mut e, 48_000);
        assert!(e.voices.is_empty());
    }

    #[test]
    fn treble_keys_have_no_damper() {
        let mut e = PianoPrerender::new(48_000);
        e.send_at(0, ON | 100 << 8 | 100 << 16);
        e.send_at(100, OFF | 100 << 8);
        // A damper would silence it within 0.1 s.
        run(&mut e, 12_000);
        assert_eq!(e.voices.len(), 1);
        assert_eq!(e.notes[100].damper, 1.0);
    }

    #[test]
    fn sustain_pedal_defers_the_damper() {
        let mut e = PianoPrerender::new(48_000);
        e.send_at(0, 0xB0 | 64 << 8 | 127 << 16);
        e.send_at(0, ON | 60 << 8 | 100 << 16);
        e.send_at(100, OFF | 60 << 8);
        run(&mut e, 48_000);
        assert_eq!(e.voices.len(), 1, "pedal should hold the note");
        e.send_at(0, 0xB0 | 64 << 8);
        run(&mut e, 48_000);
        assert!(e.voices.is_empty());
    }

    #[test]
    fn reset_cuts_voices_at_its_frame() {
        let mut e = PianoPrerender::new(1_000);
        let (mut l, mut r) = (vec![0.0; 100], vec![0.0; 100]);
        e.send_at(0, ON | 69 << 8 | 127 << 16);
        e.send_at(20, 0xFF);
        e.send_at(30, ON | 60 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        assert!(l[1..20].iter().any(|&s| s != 0.0));
        assert!(l[20..30].iter().all(|&s| s == 0.0));
        assert_eq!(e.voices.len(), 1);
    }

    fn many_notes(e: &mut PianoPrerender) {
        for i in 0..1_000u32 {
            e.send_at(i % 256, ON | (i % 128) << 8 | (1 + i % 127) << 16);
        }
        for i in 0..300u32 {
            e.send_at(100 + i % 100, OFF | (i % 128) << 8);
        }
    }

    #[test]
    #[ignore]
    fn benchmark_high_polyphony_render() {
        use std::time::Instant;

        let mut e = PianoPrerender::new(48_000);
        for i in 0..2_048u32 {
            let note = i % 128;
            e.send_at(0, ON | note << 8 | 100 << 16);
        }
        let (mut l, mut r) = (vec![0.0; 256], vec![0.0; 256]);
        e.render(&mut l, &mut r);
        let started = Instant::now();
        for _ in 0..200 {
            e.render(&mut l, &mut r);
        }
        eprintln!(
            "2048 voices x 256 frames x 200 chunks: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn threaded_mix_matches_serial() {
        let mut threaded = PianoPrerender::new(48_000);
        many_notes(&mut threaded);
        let (mut l, mut r) = (vec![0.0; 512], vec![0.0; 512]);
        threaded.render(&mut l, &mut r);

        let mut serial = PianoPrerender::new(48_000);
        many_notes(&mut serial);
        let mut scheduled = std::mem::take(&mut serial.scheduled);
        scheduled.sort_by_key(|&(frame, _)| frame);
        serial.begin_render();
        for &(frame, msg) in &scheduled {
            serial.apply(frame as usize, msg);
        }
        let (mut el, mut er) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        for v in &mut serial.voices {
            let n = v.note as usize;
            let b = &serial.bends[v.channel as usize];
            v.render(
                &mut el,
                &mut er,
                &serial.notes[n],
                &serial.cache.notes[n],
                b,
                serial.channels[v.channel as usize].volume
                    * serial.channels[v.channel as usize].expression,
                serial.channels[v.channel as usize].pan,
            );
        }
        for (a, b) in l.iter().zip(&el).chain(r.iter().zip(&er)) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn stream_renderer_plays_and_damps_notes() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut r = StreamRenderer {
            piano: PianoPrerender::new(48_000),
            rx,
            stats: std::sync::Arc::new(StreamStats::default()),
            load: 0.0,
            limiter: crate::audio_limiter::AudioLimiter::new(96_000.0, 0.0, 100.0, 50.0),
            limiter_bypassed: false,
            left: [0.0; STREAM_BLOCK],
            right: [0.0; STREAM_BLOCK],
            interleaved: Vec::new(),
            pos: STREAM_BLOCK,
        };
        tx.send(StreamMsg::Midi(ON | 60 << 8 | 100 << 16)).unwrap();
        let mut out = vec![0.0f32; STREAM_BLOCK * 2 * 4];
        r.fill(&mut out, 2);
        assert!(out.iter().any(|&s| s != 0.0));
        // The limiter keeps the realtime output within full scale.
        assert!(out.iter().all(|&s| s.abs() <= 1.0));
        tx.send(StreamMsg::Midi(OFF | 60 << 8)).unwrap();
        let mut out = vec![0.0f32; STREAM_BLOCK * 2];
        for _ in 0..200 {
            r.fill(&mut out, 2);
        }
        assert!(r.piano.voices.is_empty());
    }

    #[test]
    fn low_notes_lean_left() {
        let low = NoteSpec::new(30, 48_000);
        let high = NoteSpec::new(100, 48_000);
        assert!(low.left > low.right && high.right > high.left);
        assert!((low.left.powi(2) + low.right.powi(2) - 1.0).abs() < 1e-5);
    }

    /// Zero crossings, i.e. ~2 * Hz / rate for a lone sine-like note.
    fn crossings(l: &[f32]) -> usize {
        l.windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    #[test]
    fn pitch_bend_shifts_held_note_at_its_frame() {
        // At 8 kHz note 100 keeps only its first partial (as a lane pair).
        let mut e = PianoPrerender::new(8_000);
        assert_eq!(NoteSpec::new(100, 8_000).count, 2);
        let (mut l, mut r) = (vec![0.0; 1_600], vec![0.0; 1_600]);
        e.send_at(0, ON | 100 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        let before = crossings(&l[400..]);
        // Full up bend at default range: +2 semitones.
        e.send_at(0, 0xE0 | 127 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        let after = crossings(&l[..1_200]);
        let ratio = after as f32 / before as f32;
        assert!((ratio - 2f32.powf(2.0 / 12.0)).abs() < 0.02, "{ratio}");
    }

    #[test]
    fn rpn_sets_bend_range_and_reset_restores() {
        let mut e = PianoPrerender::new(48_000);
        for msg in [0xB0 | 101 << 8, 0xB0 | 100 << 8, 0xB0 | 6 << 8 | 12 << 16] {
            e.send_at(0, msg);
        }
        e.send_at(0, 0xE0 | 127 << 8 | 127 << 16);
        let (mut l, mut r) = (vec![0.0; 256], vec![0.0; 256]);
        e.render(&mut l, &mut r);
        // Full bend at 12 semitones: about an octave up.
        assert!((e.channels[0].ratio() - 2.0).abs() < 0.01);
        e.send_at(10, 0xB0 | 121 << 8);
        e.render(&mut l, &mut r);
        assert_eq!(e.channels[0].ratio(), 1.0);
        assert_eq!(e.bends[0], vec![(0, e.bends[0][0].1), (10, 1.0)]);
    }

    #[test]
    fn phasor_tracks_damped_sine() {
        let spec = NoteSpec::new(69, 48_000);
        let (cos, sin) = spec.rotation(1.0, 1.0);
        let (mut re, mut im) = ([0.0; LANES], [0.0; LANES]);
        re[0] = 1.0;
        let (w, r) = (spec.omegas[0], spec.decays[0] as f64);
        for n in 1..=48_000 {
            let got = step(&mut re, &mut im, &cos, &sin, spec.count) as f64;
            let exact = r.powi(n) * (w * n as f64).sin();
            assert!((got - exact).abs() < 2e-3, "frame {n}: {got} vs {exact}");
        }
    }

    #[test]
    fn partials_are_stretched_and_fundamental_in_tune() {
        let spec = NoteSpec::new(69, 48_000);
        assert!((spec.hz[0] / 440.0 - 1.0).abs() < 1e-6);
        // Lane 2 is partial 2's prompt lane; stiff strings run sharp.
        assert!(spec.hz[2] > 880.0);
        // Treble strings are stiffer than mid ones.
        assert!(inharmonicity(100) > 10.0 * inharmonicity(50));
    }

    #[test]
    fn strike_point_mutes_the_eighth_partial() {
        let spec = NoteSpec::new(40, 48_000);
        // Partials 1-8 are pairs (lanes 0-15); partial 8 is lanes 14/15.
        assert!(spec.amps[14] < 1e-6);
        assert!(spec.amps[0] > 0.0);
    }

    #[test]
    fn prompt_decays_faster_than_aftersound_and_treble_faster_than_bass() {
        let mid = NoteSpec::new(60, 48_000);
        assert!(mid.decays[0] < mid.decays[1]);
        assert!(NoteSpec::new(90, 48_000).decays[1] < mid.decays[1]);
    }

    #[test]
    fn velocity_sets_loudness_and_brightness() {
        let spec = NoteSpec::new(60, 48_000);
        let soft = lane_amps(32, &spec);
        let hard = lane_amps(127, &spec);
        let sum = |a: &[f32]| a.iter().sum::<f32>();
        // Match the SF2 engine's velocity^2 gain curve.
        assert!((sum(&soft) / sum(&hard) - (32.0f32 / 127.0).powi(2)).abs() < 0.02);
        // Upper partials fall away faster on soft hits.
        let k = 2 * PAIRED; // first single-lane partial
        assert!(hard[k].abs() / hard[0].abs() > 2.0 * soft[k].abs() / soft[0].abs());
    }

    #[test]
    fn treble_note_decays_and_is_dropped_while_held() {
        let mut e = PianoPrerender::new(8_000);
        e.send_at(0, ON | 100 << 8 | 127 << 16);
        let first = peak(&run(&mut e, 2_048)[1_000..]);
        let later = peak(&run(&mut e, 8_000));
        assert!(later < first * 0.8, "no decay while held");
        // Never released, but eventually silent and removed.
        run(&mut e, 8_000 * 60);
        assert!(e.voices.is_empty());
    }

    #[test]
    fn cache_is_shared_trimmed_and_capped_at_30s() {
        let a = Cache::get(8_000);
        assert!(std::sync::Arc::ptr_eq(&a, &Cache::get(8_000)));
        let max = (CACHE_SECS * 8_000.0) as usize;
        assert!(a.notes.iter().all(|n| n.samples.len() <= max));
        // Bass rings for many seconds; treble is trimmed short.
        assert!(a.notes[21].samples.len() > max * 2 / 3);
        assert!(a.notes[21].samples.last().unwrap().abs() < 100);
        assert!(a.notes[100].samples.len() < max / 4);
    }

    #[test]
    fn soft_hits_are_quieter_and_darker() {
        let mut e = PianoPrerender::new(48_000);
        e.send_at(0, ON | 60 << 8 | 127 << 16);
        e.send_at(0, ON | 60 << 8 | 40 << 16);
        let (mut l, mut r) = (vec![0.0; 16], vec![0.0; 16]);
        e.render(&mut l, &mut r);
        let (hard, soft) = (&e.voices[0], &e.voices[1]);
        assert!(soft.gain < hard.gain);
        assert_eq!(hard.lp, 1.0);
        assert!(soft.lp < 1.0);
    }

    #[test]
    fn voice_past_the_cached_strike_renders_silently() {
        // A pitch-bend skip can leave a voice's read position past the
        // end of the cached strike. Rendering it must not panic (the
        // flat mix sliced at that position even for an empty span) and
        // must retire the voice.
        let cached = CachedNote {
            samples: vec![1_000; 7_288],
            scale: 1.0,
            loud: vec![0.5; 29],
            loud_suffix: vec![0.5; 29],
        };
        let spec = NoteSpec::new(60, 48_000);
        let mut voice = Voice {
            seq: 0,
            channel: 0,
            note: 60,
            pos: 7_315.0,
            gain: 1.0,
            lp: 1.0,
            lp_y: 0.0,
            env: 1.0,
            damped: false,
            pedal_held: false,
            fading: false,
            dead: false,
            start: 0,
            release_at: None,
            kill_at: None,
        };
        let mut left = [0.0f32; 256];
        let mut right = [0.0f32; 256];
        voice.render(&mut left, &mut right, &spec, &cached, &[(0, 1.0)], 1.0, 0.5);
        assert!(voice.dead);
        assert!(left.iter().chain(&right).all(|&v| v == 0.0));
    }

    /// Measurement only: steady-state render cost at extreme polyphony.
    /// Run with: cargo test --release -- --ignored bench_polyphony_sweep --nocapture
    #[test]
    #[ignore]
    fn bench_polyphony_sweep() {
        use std::time::Instant;

        const CHUNK: usize = 256;
        // Audio duration of one chunk at 48 kHz, in milliseconds.
        const CHUNK_MS: f64 = CHUNK as f64 / 48_000.0 * 1_000.0;

        fn fill(e: &mut PianoPrerender, target: usize) {
            // Bass and mid keys ring for many seconds, so voices stay alive
            // for the whole measurement without any note-offs.
            let mut i = 0u32;
            while (i as usize) < target {
                let note = 21 + (i % 30);
                let channel = (i / 30) % 16;
                e.send_at(0, ON | channel | note << 8 | 127 << 16);
                i += 1;
            }
        }

        fn measure(e: &mut PianoPrerender, chunks: usize) -> (f64, f64, f64, f64) {
            let (mut l, mut r) = (vec![0.0; CHUNK], vec![0.0; CHUNK]);
            let mut times = Vec::with_capacity(chunks);
            let mut checksum = 0.0f64;
            for _ in 0..chunks {
                let t = Instant::now();
                e.render(&mut l, &mut r);
                times.push(t.elapsed().as_secs_f64() * 1_000.0);
                checksum += l.iter().map(|&x| x as f64).sum::<f64>();
                checksum += r.iter().map(|&x| x as f64).sum::<f64>();
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            let p95 = times[(times.len() as f64 * 0.95) as usize];
            (mean, p95, *times.last().unwrap(), checksum)
        }

        fn fill_mixed(e: &mut PianoPrerender, target: usize) {
            // Whole keyboard, mixed velocities: closer to a real dense song
            // than the single-velocity bass fill above.
            let mut i = 0u32;
            while (i as usize) < target {
                let note = 21 + (i % 88);
                let channel = (i / 88) % 16;
                let vel = 60 + (i % 68);
                e.send_at(0, ON | channel | note << 8 | vel << 16);
                i += 1;
            }
        }

        for target in [10_000usize, 50_000, 100_000, 200_000, 300_000] {
            let mut e = PianoPrerender::new(48_000);
            fill(&mut e, target);
            let (mut l, mut r) = (vec![0.0; CHUNK], vec![0.0; CHUNK]);
            let t = Instant::now();
            e.render(&mut l, &mut r);
            let burst_ms = t.elapsed().as_secs_f64() * 1_000.0;
            let alive = e.voices.len();
            let (mean, p95, max, sum) = measure(&mut e, 100);
            eprintln!(
                "polyphony {alive}: burst {burst_ms:.1} ms, chunk mean {mean:.3} ms p95 {p95:.3} ms max {max:.3} ms, realtime x{:.2}, checksum {sum:.4}",
                CHUNK_MS / mean
            );
        }

        // Aged mix: strikes all over the keyboard at mixed velocities,
        // rendered 2 s before measuring, so most voices sit in their decay
        // tails like a real dense song.
        {
            let mut e = PianoPrerender::new(48_000);
            fill_mixed(&mut e, 100_000);
            let (mut l, mut r) = (vec![0.0; CHUNK], vec![0.0; CHUNK]);
            e.render(&mut l, &mut r);
            for _ in 0..375 {
                e.render(&mut l, &mut r);
            }
            let (mean, p95, max, sum) = measure(&mut e, 100);
            eprintln!(
                "polyphony {} (aged mix): chunk mean {mean:.3} ms p95 {p95:.3} ms max {max:.3} ms, realtime x{:.2}, checksum {sum:.4}",
                e.voices.len(),
                CHUNK_MS / mean
            );
        }

        // Same, but with a pitch bend held on every channel: every voice
        // takes the interpolated path.
        {
            let mut e = PianoPrerender::new(48_000);
            fill(&mut e, 100_000);
            let (mut l, mut r) = (vec![0.0; CHUNK], vec![0.0; CHUNK]);
            e.render(&mut l, &mut r);
            for ch in 0..16u32 {
                // +1 semitone with the default 2-semitone range.
                e.send_at(0, 0xE0 | ch | 0 << 8 | 80 << 16);
            }
            e.render(&mut l, &mut r);
            let (mean, p95, max, sum) = measure(&mut e, 100);
            eprintln!(
                "polyphony {} (bend): chunk mean {mean:.3} ms p95 {p95:.3} ms max {max:.3} ms, realtime x{:.2}, checksum {sum:.4}",
                e.voices.len(),
                CHUNK_MS / mean
            );
        }

        // Same, but with the voice limit engaged at the current count:
        // isolates the per-render limit bookkeeping.
        {
            let mut e = PianoPrerender::new(48_000);
            fill(&mut e, 100_000);
            let (mut l, mut r) = (vec![0.0; CHUNK], vec![0.0; CHUNK]);
            e.render(&mut l, &mut r);
            e.set_voice_limit(100_000);
            let (mean, p95, max, sum) = measure(&mut e, 100);
            eprintln!(
                "polyphony {} (limit on): chunk mean {mean:.3} ms p95 {p95:.3} ms max {max:.3} ms, realtime x{:.2}, checksum {sum:.4}",
                e.voices.len(),
                CHUNK_MS / mean
            );
        }
    }
}
