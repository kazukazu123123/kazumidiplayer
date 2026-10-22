//! Debug synth: a simple additive piano. Each note sums a few slightly
//! inharmonic partials that decay on their own (higher partials and higher
//! notes faster), with velocity controlling brightness, then a linear 100 ms
//! release. Per-channel pitch bend (range set by RPN 0, default 2
//! semitones) changes pitch sample-accurately, held notes included.
//! Unlimited polyphony; no samples to load, so it isolates the prerender
//! path. Voices render in parallel on every logical core.
//!
//! Each partial is a damped rotating phasor: one complex multiply per sample
//! advances its phase and applies its decay together. All partials of a
//! voice sit in fixed-width lanes, so the inner loop has no table lookups or
//! branches and vectorizes.

const RELEASE_SECS: f32 = 0.1;
const VOICE_GAIN: f32 = 0.1;

/// Partials per note (fewer near Nyquist).
const PARTIALS: usize = 6;
/// SIMD-friendly lane count; lanes past `PARTIALS` stay silent.
const LANES: usize = 8;
/// String stiffness: partial k sits at k * f * sqrt(1 + B k^2).
const INHARMONICITY: f64 = 0.0004;
/// 60 dB decay time of the fundamental at A0; halves every two octaves.
const DECAY_SECS_A0: f64 = 8.0;
const BEND_CENTER: u16 = 8192;
const DEFAULT_BEND_RANGE: f32 = 2.0;

/// A voice whose partials have all decayed below this is dropped.
const SILENT: f32 = 1e-4;

/// Below this many voices per thread, splitting costs more than it saves.
const MIN_VOICES_PER_THREAD: usize = 64;

/// Per-note synthesis constants, computed once per sample rate.
#[derive(Clone, Copy)]
struct NoteSpec {
    /// Phase advance per frame of each partial, in radians.
    omegas: [f64; PARTIALS],
    /// Per-frame amplitude multiplier of each partial.
    decays: [f32; PARTIALS],
    /// Partials below Nyquist.
    count: usize,
}

impl NoteSpec {
    fn new(note: usize, rate: u32) -> Self {
        let freq = 440.0 * 2f64.powf((note as f64 - 69.0) / 12.0);
        let decay_secs = DECAY_SECS_A0 * 2f64.powf(-(note as f64 - 21.0) / 24.0);
        let mut spec = Self {
            omegas: [0.0; PARTIALS],
            decays: [0.0; PARTIALS],
            count: 0,
        };
        for k in 1..=PARTIALS {
            let kf = k as f64;
            let partial = kf * freq * (1.0 + INHARMONICITY * kf * kf).sqrt();
            if partial >= rate as f64 * 0.45 {
                break;
            }
            let secs = decay_secs / (1.0 + 0.7 * (kf - 1.0));
            spec.omegas[k - 1] = std::f64::consts::TAU * partial / rate as f64;
            spec.decays[k - 1] = 10f64.powf(-3.0 / (secs * rate as f64)) as f32;
            spec.count = k;
        }
        spec
    }

    /// Per-frame rotation (decay included) of each lane at pitch `ratio`.
    fn rotation(&self, ratio: f32) -> ([f32; LANES], [f32; LANES]) {
        let (mut cos, mut sin) = ([0.0; LANES], [0.0; LANES]);
        for k in 0..self.count {
            let w = self.omegas[k] * ratio as f64;
            let r = self.decays[k] as f64;
            cos[k] = (r * w.cos()) as f32;
            sin[k] = (r * w.sin()) as f32;
        }
        (cos, sin)
    }
}

/// Partial amplitudes for a velocity (0-127): harder hits are brighter.
/// Normalized so every velocity peaks at about its loudness.
fn partial_amps(velocity: u8, count: usize) -> [f32; PARTIALS] {
    let v = velocity as f32 / 127.0;
    let brightness = 0.25 + 0.5 * v;
    let mut amps = [0.0; PARTIALS];
    let mut total = 0.0;
    for (k, amp) in amps.iter_mut().enumerate().take(count) {
        *amp = brightness.powi(k as i32) / (k + 1) as f32;
        total += *amp;
    }
    // Loudness follows the velocity squared (0.0-1.0 mapped to v^2).
    let gain = VOICE_GAIN * v * v / total.max(f32::EPSILON);
    for amp in &mut amps {
        *amp *= gain;
    }
    amps
}

/// Advance every lane one frame; returns the sum of the sine parts.
#[inline(always)]
fn step(
    re: &mut [f32; LANES],
    im: &mut [f32; LANES],
    cos: &[f32; LANES],
    sin: &[f32; LANES],
) -> f32 {
    let mut sum = 0.0;
    for k in 0..LANES {
        let (a, b) = (re[k], im[k]);
        re[k] = a * cos[k] - b * sin[k];
        im[k] = a * sin[k] + b * cos[k];
        sum += im[k];
    }
    sum
}

struct Voice {
    /// Creation order; `voices` stays sorted by it.
    seq: u64,
    channel: u8,
    spec: NoteSpec,
    /// Partial phasors: amplitude and phase as a complex number. The output
    /// is the imaginary part, so a phasor starting on the real axis is a sine
    /// starting at zero.
    re: [f32; LANES],
    im: [f32; LANES],
    /// Release envelope; falls from 1 to 0 after note-off.
    level: f32,
    released: bool,
    // Within the current render, set while applying its events. Recording
    // them up front lets every voice render the whole chunk on its own.
    /// First frame it sounds (its note-on frame, else 0).
    start: usize,
    /// Frame its release begins.
    release_at: Option<usize>,
    /// Frame it is cut off (player reset).
    kill_at: Option<usize>,
}

impl Voice {
    fn releasing(&self) -> bool {
        self.released || self.release_at.is_some()
    }

    fn alive(&self) -> bool {
        self.kill_at.is_none()
    }

    /// Add this voice's part of the render into `out`, then fold the
    /// render's events into its state for the next one. `bends` is its
    /// channel's `(start frame, pitch ratio)` segments for this render.
    fn render(&mut self, out: &mut [f32], release_step: f32, bends: &[(usize, f32)]) {
        let end = self.kill_at.unwrap_or(out.len()).min(out.len());
        let start = self.start.min(end);
        let release_from = if self.released {
            start
        } else {
            self.release_at.unwrap_or(end).clamp(start, end)
        };
        // Locals, so the hot loop keeps them in registers.
        let (mut re, mut im) = (self.re, self.im);
        let mut level = self.level;
        'segments: for (si, &(from, ratio)) in bends.iter().enumerate() {
            let to = bends.get(si + 1).map_or(out.len(), |b| b.0);
            let (lo, hi) = (from.max(start), to.min(end));
            if lo >= hi {
                continue;
            }
            let (cos, sin) = self.spec.rotation(ratio);
            let mid = release_from.clamp(lo, hi);
            // Held part: constant level.
            for o in &mut out[lo..mid] {
                *o += step(&mut re, &mut im, &cos, &sin) * level;
            }
            // Released part: linear fade.
            for o in &mut out[mid..hi] {
                if level <= 0.0 {
                    break 'segments;
                }
                level -= release_step;
                *o += step(&mut re, &mut im, &cos, &sin) * level.max(0.0);
            }
        }
        (self.re, self.im) = (re, im);
        self.released |= self.release_at.is_some();
        self.level = level;
        let audible = (0..LANES).any(|k| re[k] * re[k] + im[k] * im[k] > SILENT * SILENT);
        if self.kill_at.is_some() || !audible {
            self.level = 0.0;
        }
        self.start = 0;
        self.release_at = None;
        self.kill_at = None;
    }
}

/// Pitch bend state of one MIDI channel.
#[derive(Clone, Copy)]
struct Channel {
    bend: u16,
    /// Semitones at full bend.
    range: f32,
    /// Selected RPN (MSB, LSB); 127/127 is none.
    rpn: (u8, u8),
}

impl Channel {
    const RESET: Self = Self {
        bend: BEND_CENTER,
        range: DEFAULT_BEND_RANGE,
        rpn: (127, 127),
    };

    fn ratio(&self) -> f32 {
        let semitones = (self.bend as f32 - BEND_CENTER as f32) / BEND_CENTER as f32 * self.range;
        2f32.powf(semitones / 12.0)
    }
}

pub struct SinePrerender {
    rate: u32,
    notes: [NoteSpec; 128],
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
}

impl SinePrerender {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            notes: std::array::from_fn(|note| NoteSpec::new(note, rate)),
            voices: Vec::new(),
            scheduled: Vec::new(),
            channels: [Channel::RESET; 16],
            held: vec![std::collections::VecDeque::new(); 16 * 128],
            next_seq: 0,
            bends: std::array::from_fn(|_| Vec::new()),
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
                self.held[channel as usize * 128 + p1 as usize].push_back(seq);
                self.voices.push(Voice {
                    seq,
                    channel,
                    spec: self.notes[p1 as usize],
                    re: {
                        let amps = partial_amps(p2, self.notes[p1 as usize].count);
                        std::array::from_fn(|k| amps.get(k).copied().unwrap_or(0.0))
                    },
                    im: [0.0; LANES],
                    level: 1.0,
                    released: false,
                    start: frame,
                    release_at: None,
                    kill_at: None,
                });
            }
            0x80 | 0x90 => {
                // Release the oldest held voice for this key.
                let queue = &mut self.held[channel as usize * 128 + p1 as usize];
                while let Some(seq) = queue.pop_front() {
                    if let Ok(i) = self.voices.binary_search_by_key(&seq, |v| v.seq) {
                        let v = &mut self.voices[i];
                        if v.alive() && !v.releasing() {
                            v.release_at = Some(frame);
                            break;
                        }
                    }
                }
            }
            0xE0 => {
                self.channels[channel as usize].bend = p1 as u16 | (p2 as u16) << 7;
                self.bend_changed(channel as usize, frame);
            }
            0xB0 if matches!(p1, 6 | 38 | 100 | 101 | 121) => {
                let ch = &mut self.channels[channel as usize];
                match p1 {
                    101 => ch.rpn.0 = p2,
                    100 => ch.rpn.1 = p2,
                    // RPN 0: pitch bend sensitivity (semitones, then cents).
                    6 if ch.rpn == (0, 0) => ch.range = p2 as f32 + ch.range.fract(),
                    38 if ch.rpn == (0, 0) => ch.range = ch.range.trunc() + p2 as f32 / 100.0,
                    // Reset all controllers: centre the wheel, deselect RPN.
                    121 => {
                        ch.bend = BEND_CENTER;
                        ch.rpn = (127, 127);
                    }
                    _ => return,
                }
                self.bend_changed(channel as usize, frame);
            }
            // All sound off / all notes off.
            0xB0 if p1 == 120 || p1 == 123 => {
                for v in &mut self.voices {
                    if v.alive() && !v.releasing() && v.channel == channel {
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
    /// core, below normal priority), each group into its own buffer; the
    /// buffers are summed.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let len = left.len();
        // Stable: same-frame events keep their send order.
        let mut scheduled = std::mem::take(&mut self.scheduled);
        scheduled.sort_by_key(|&(frame, _)| frame);
        self.begin_render();
        for &(frame, msg) in &scheduled {
            // Offsets past the chunk (shouldn't happen) apply at its end.
            self.apply((frame as usize).min(len), msg);
        }
        scheduled.clear();
        self.scheduled = scheduled;
        let bends = &self.bends;

        let release_step = 1.0 / (RELEASE_SECS * self.rate as f32);
        let per_thread = self
            .voices
            .len()
            .div_ceil(crate::audio_engine::prerender::synth_pool().current_num_threads())
            .max(MIN_VOICES_PER_THREAD);
        left.fill(0.0);
        if self.voices.len() <= per_thread {
            for v in &mut self.voices {
                v.render(left, release_step, &bends[v.channel as usize]);
            }
        } else {
            use rayon::prelude::*;
            let voices = &mut self.voices;
            let mix = crate::audio_engine::prerender::synth_pool().install(|| {
                voices
                    .par_chunks_mut(per_thread)
                    .map(|voices| {
                        let mut out = vec![0.0f32; len];
                        for v in voices {
                            v.render(&mut out, release_step, &bends[v.channel as usize]);
                        }
                        out
                    })
                    .reduce_with(|mut a, b| {
                        a.iter_mut().zip(&b).for_each(|(a, b)| *a += b);
                        a
                    })
            });
            if let Some(mix) = mix {
                left.copy_from_slice(&mix);
            }
        }
        self.voices.retain(|v| v.level > 0.0);
        // Not clamped: peaks are left to the prerender limiter.
        right.copy_from_slice(left);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_starts_at_its_frame_and_releases_in_100ms() {
        let mut e = SinePrerender::new(1_000);
        let (mut l, mut r) = (vec![0.0; 300], vec![0.0; 300]);
        e.send_at(10, 0x90 | 69 << 8 | 127 << 16);
        e.send_at(50, 0x80 | 69 << 8);
        e.render(&mut l, &mut r);
        assert!(l[..10].iter().all(|&s| s == 0.0));
        assert!(l[11..50].iter().any(|&s| s != 0.0));
        // 100 ms at 1 kHz = 100 frames of release after frame 50.
        assert_eq!(e.voices.len(), 0);
        assert!(l[151..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn reset_cuts_voices_at_its_frame() {
        let mut e = SinePrerender::new(1_000);
        let (mut l, mut r) = (vec![0.0; 100], vec![0.0; 100]);
        e.send_at(0, 0x90 | 69 << 8 | 127 << 16);
        e.send_at(20, 0xFF);
        e.send_at(30, 0x90 | 60 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        assert!(l[1..20].iter().any(|&s| s != 0.0));
        assert!(l[20..30].iter().all(|&s| s == 0.0));
        assert_eq!(e.voices.len(), 1);
    }

    fn many_notes(e: &mut SinePrerender) {
        for i in 0..1_000u32 {
            e.send_at(i % 256, 0x90 | (i % 128) << 8 | (1 + i % 127) << 16);
        }
        for i in 0..300u32 {
            e.send_at(100 + i % 100, 0x80 | (i % 128) << 8);
        }
    }

    #[test]
    fn threaded_mix_matches_serial() {
        let mut threaded = SinePrerender::new(48_000);
        many_notes(&mut threaded);
        let (mut l, mut r) = (vec![0.0; 512], vec![0.0; 512]);
        threaded.render(&mut l, &mut r);

        let mut serial = SinePrerender::new(48_000);
        many_notes(&mut serial);
        let mut scheduled = std::mem::take(&mut serial.scheduled);
        scheduled.sort_by_key(|&(frame, _)| frame);
        serial.begin_render();
        for &(frame, msg) in &scheduled {
            serial.apply(frame as usize, msg);
        }
        let mut expected = vec![0.0f32; 512];
        let release_step = 1.0 / (RELEASE_SECS * 48_000.0);
        for v in &mut serial.voices {
            v.render(
                &mut expected,
                release_step,
                &serial.bends[v.channel as usize],
            );
        }

        for (a, b) in l.iter().zip(&expected) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        assert_eq!(l, r);
    }

    /// Zero crossings per frame of a lone sine-like note, i.e. ~2 * Hz / rate.
    fn crossings(l: &[f32]) -> usize {
        l.windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    #[test]
    fn pitch_bend_shifts_held_note_at_its_frame() {
        // At 8 kHz note 100 keeps a single partial, so crossings track pitch.
        let mut e = SinePrerender::new(8_000);
        assert_eq!(NoteSpec::new(100, 8_000).count, 1);
        let (mut l, mut r) = (vec![0.0; 4_096], vec![0.0; 4_096]);
        e.send_at(0, 0x90 | 100 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        let before = crossings(&l[100..]);
        // Full up bend at default range: +2 semitones.
        e.send_at(0, 0xE0 | 127 << 8 | 127 << 16);
        e.render(&mut l, &mut r);
        let after = crossings(&l[100..]);
        let ratio = after as f32 / before as f32;
        assert!((ratio - 2f32.powf(2.0 / 12.0)).abs() < 0.02, "{ratio}");
    }

    #[test]
    fn rpn_sets_bend_range_and_reset_restores() {
        let mut e = SinePrerender::new(48_000);
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
        let (cos, sin) = spec.rotation(1.0);
        let (mut re, mut im) = ([0.0; LANES], [0.0; LANES]);
        re[0] = 1.0;
        let (w, r) = (spec.omegas[0], spec.decays[0] as f64);
        for n in 1..=48_000 {
            let got = step(&mut re, &mut im, &cos, &sin) as f64;
            let exact = r.powi(n) * (w * n as f64).sin();
            assert!((got - exact).abs() < 2e-3, "frame {n}: {got} vs {exact}");
        }
    }

    #[test]
    fn fundamental_hits_pitch() {
        let hz = |note: usize, k: usize| {
            NoteSpec::new(note, 48_000).omegas[k] / std::f64::consts::TAU * 48_000.0
        };
        // Inharmonicity shifts the fundamental by only ~0.02%.
        assert!((hz(69, 0) / 440.0 - 1.0).abs() < 1e-3);
        assert!((hz(81, 0) / 880.0 - 1.0).abs() < 1e-3);
        // Upper partials run slightly sharp of exact multiples.
        assert!(hz(69, 3) > 4.0 * hz(69, 0));
    }

    #[test]
    fn partials_stop_below_nyquist_and_decay_faster_higher() {
        let high = NoteSpec::new(127, 48_000);
        assert_eq!(high.count, 1);
        let mid = NoteSpec::new(60, 48_000);
        assert_eq!(mid.count, PARTIALS);
        assert!(mid.decays.windows(2).all(|w| w[1] < w[0]));
        assert!(NoteSpec::new(90, 48_000).decays[0] < mid.decays[0]);
    }

    #[test]
    fn held_note_decays_and_is_dropped() {
        let mut e = SinePrerender::new(8_000);
        e.send_at(0, 0x90 | 100 << 8 | 127 << 16);
        let (mut l, mut r) = (vec![0.0; 256], vec![0.0; 256]);
        let peak = |l: &[f32]| l.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        e.render(&mut l, &mut r);
        let first = peak(&l);
        for _ in 0..30 {
            e.render(&mut l, &mut r);
        }
        assert!(peak(&l) < first * 0.8, "no decay while held");
        // Never released, but eventually silent and removed.
        for _ in 0..2_000 {
            e.render(&mut l, &mut r);
        }
        assert!(e.voices.is_empty());
    }

    #[test]
    fn velocity_sets_loudness_and_brightness() {
        let soft = partial_amps(32, PARTIALS);
        let hard = partial_amps(127, PARTIALS);
        let sum = |a: &[f32]| a.iter().sum::<f32>();
        assert!((sum(&hard) - VOICE_GAIN).abs() < 1e-6);
        assert!(sum(&soft) < sum(&hard));
        assert!(hard[3] / hard[0] > soft[3] / soft[0]);
        // Half velocity is a quarter as loud.
        let half = partial_amps(64, PARTIALS);
        let expected = VOICE_GAIN * (64.0f32 / 127.0).powi(2);
        assert!((sum(&half) - expected).abs() < 1e-6);
    }
}
