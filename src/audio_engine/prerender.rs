//! Prerendered playback: a render thread walks the MIDI file ahead of the
//! audio device and fills a PCM buffer up to a configurable length; the cpal
//! callback plays from it. The song position comes from the frames actually
//! played, so the player follows the audio rather than the wall clock.
//!
//! Rendering continues while paused, so resuming plays the kept buffer.
//! A forward seek inside the buffer just skips ahead in it; seeking back
//! always re-renders (played audio is not kept).
//! Playback never waits for rendering: if the buffer runs dry the position
//! keeps advancing over silence, and rendering skips ahead to catch up
//! (dropping the notes it missed) instead of holding the song back. Only the
//! first chunk after a (re)start is waited for, so a seek does not clip its
//! opening notes.
//!
//! Anything that changes what lies ahead (seek, speed, new file, shrinking
//! the buffer) bumps a generation: buffered audio is dropped and
//! rendering restarts from the current position.
//!
//! The buffered audio itself is stored Opus-compressed at the rates Opus
//! supports (see [`AudioQueue`]), which cuts its memory by an order of
//! magnitude; other rates keep raw PCM.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};

use crate::kazumidiparser_cursor::{
    TrackCursor, cursor_next, ns_to_tick, seek_track_cursor, tick_to_ns,
};

/// Frames rendered per step. Events carry their offset within the step.
pub const CHUNK_FRAMES: usize = 256;

pub const DEFAULT_MAX_BUFFER_SECS: u32 = 10;
pub const MIN_MAX_BUFFER_SECS: u32 = 1;
pub const MAX_MAX_BUFFER_SECS: u32 = 600;

/// Bytes one buffered frame takes (stereo `f32`).
pub const BYTES_PER_FRAME: u64 = std::mem::size_of::<(f32, f32)>() as u64;

/// KazuLimiter settings, applied to every prerender engine's output.
/// S16 Clipping skips it; overflowing samples wrap around instead,
/// reproducing the 16-bit output's fuzzing noise.
const LIMITER_THRESHOLD_DB: f32 = 0.0;
const LIMITER_RELEASE_MS: f32 = 100.0;
const LIMITER_LOOKAHEAD_MS: f32 = 50.0;

/// Schedules the `(frame offset, message)` pairs, then renders one chunk.
/// Returns the voices sounding at its end.
pub type RenderFn = Box<dyn FnMut(&[(u32, u32)], &mut [f32], &mut [f32]) -> u32 + Send>;

type MidiData = Arc<kazumidiparser::MIDIPlayerData>;

/// Windows thread priorities (`SetThreadPriority`).
const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
const THREAD_PRIORITY_NORMAL: i32 = 0;
const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

fn set_thread_priority(priority: i32) {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        // Safety: the pseudo-handle refers to the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), priority);
        }
    }
    #[cfg(not(windows))]
    let _ = priority;
}

/// Run the calling thread below normal priority (`low`) or at normal.
fn set_current_thread_priority(low: bool) {
    set_thread_priority(if low {
        THREAD_PRIORITY_BELOW_NORMAL
    } else {
        THREAD_PRIORITY_NORMAL
    });
}

/// Render and synth pool threads yield to the UI and piano roll while the
/// buffer is comfortably full, and compete normally when it runs low. Always
/// below normal would let a busy UI starve rendering outright.
fn set_render_priority(low: bool) {
    set_current_thread_priority(low);
    synth_pool().broadcast(|_| set_current_thread_priority(low));
}

/// Below this fraction of the maximum, rendering runs at normal priority...
const URGENT_BELOW: f64 = 0.25;
/// ...until the buffer is back above this fraction.
const RELAXED_ABOVE: f64 = 0.5;

/// Worker pool for synths that render in parallel: all logical cores but
/// two, which stay free for the UI and the audio callback even while the
/// pool runs flat out refilling the buffer. Separate from rayon's global
/// pool; its priority follows the render thread's (see
/// [`set_render_priority`]).
pub fn synth_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(cores.saturating_sub(2).max(1))
            .thread_name(|i| format!("synth-{i}"))
            .build()
            .expect("failed to start synth thread pool")
    })
}

/// Song position reached after `frames` output frames from `base_ns`.
fn song_ns(base_ns: u64, frames: u64, speed: f32, rate: u32) -> u64 {
    base_ns.saturating_add((frames as f64 * 1e9 * speed as f64 / rate as f64) as u64)
}

/// Output frame (from the generation start) at which song time `ns` sounds.
fn frame_at(base_ns: u64, ns: u64, speed: f32, rate: u32) -> u64 {
    (ns.saturating_sub(base_ns) as f64 * rate as f64 / (1e9 * speed as f64)).round() as u64
}

/// Frames kept in a buffer of `secs` seconds.
fn frames_for_secs(secs: u32, rate: u32) -> usize {
    secs as usize * rate as usize
}

struct State {
    generation: u64,
    data: Option<MidiData>,
    playing: bool,
    /// Song position the current generation started at.
    base_ns: u64,
    speed: f32,
    max_frames: usize,
    /// Rendered, not yet played.
    ring: AudioQueue,
    /// Frames of this generation the device has played, including silence
    /// output on underrun (playback does not wait for rendering).
    played: u64,
    /// The first chunk of this generation has been buffered. Until then an
    /// empty buffer holds the position (so a seek starts cleanly).
    started: bool,
    /// Device output latency: frames handed over but not yet audible.
    latency_frames: u64,
    /// Frames the audio callback has taken into its own stash but not yet
    /// output (see [`Stash`]); counted in `played`, not yet audible.
    stash_frames: u64,
    /// Bumped whenever buffered audio stops being what comes next (restart,
    /// skip), so the callback drops its stash.
    epoch: u64,
    /// Highest position reported in this generation. The latency estimate
    /// jitters between callbacks, which alone must not move playback back.
    heard_floor: u64,
    /// Pitch Bend Override: forced bend range in semitones.
    bend_override: Option<u8>,
    /// Position the kept buffer resumes from, as last reported to the player
    /// while stopped. Compared exactly on play: recomputing it would drift
    /// with the device latency and needlessly re-render.
    resume_at: Option<u64>,
    /// `(first frame, voices)` per buffered chunk, oldest first, so the
    /// polyphony shown follows what is heard rather than what is rendered.
    voice_marks: VecDeque<(u64, u32)>,
    /// Highest note-on velocity rendering currently drops (0: none).
    drop_velocity: u32,
    /// S16 Clipping: skip the output limiter; overflowing samples wrap
    /// around to the other end of the 16-bit range.
    limiter_bypassed: bool,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the render thread.
    wake: Condvar,
    rate: u32,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Song position being heard, i.e. handed over minus the device latency.
/// Never decreases within a generation (see `heard_floor`).
fn heard_ns(s: &mut State, rate: u32) -> u64 {
    let pending = s.latency_frames + s.stash_frames;
    let heard = s.played - pending.min(s.played);
    let position = song_ns(s.base_ns, heard, s.speed, rate).max(s.heard_floor);
    s.heard_floor = position;
    position
}

impl State {
    /// Skip ahead to frame `target` of this generation inside the buffer.
    /// `false` if it is behind playback or past the buffered audio.
    fn skip_to(&mut self, target: u64) -> bool {
        let Some(ahead) = target.checked_sub(self.played) else {
            return false;
        };
        if ahead as usize >= self.ring.frames() {
            return false;
        }
        self.ring.skip_frames(ahead as usize);
        self.played = target;
        self.epoch += 1;
        self.stash_frames = 0;
        true
    }
}

/// Delay of the Opus round trip, in frames, at each rate Opus supports.
/// Measured by encoding an impulse after a second of silence and locating
/// it in the decoded stream; the values were identical at 128 and
/// 256 kbit/s. The decoded stream is the input shifted by this many
/// frames, so the queue discards that many frames at the start of a
/// stream and decoded frames line up with the frames that were pushed.
/// (The last frames of a stream stay inside the codec until that much
/// more audio is pushed, which rendering ahead always does.)
/// `opus_roundtrip_is_aligned` in the tests re-measures this.
fn codec_delay(rate: u32) -> usize {
    match rate {
        8_000 => 52,
        12_000 => 78,
        16_000 => 104,
        24_000 => 60,
        _ => 120, // 48_000
    }
}

/// Bit rate the buffered audio is encoded at.
const OPUS_BITRATE_BPS: i32 = 256_000;

/// Debug builds: scores the Opus round-trip quality of the stream
/// being encoded with ViSQOL (audio mode, resampled to 48 kHz
/// internally), producing one MOS-LQO score (1.0 to 5.0) per
/// 1-second window scored every half second. A shadow decoder decodes
/// every packet as it is encoded; the decoded samples are downmixed
/// to mono alongside the encoder input, discarding the codec delay
/// like the playback path, and completed windows wait in a small
/// queue for the scoring worker (see `visqol_worker`).
#[cfg(debug_assertions)]
struct VisqolTracker {
    rate: u32,
    decoder: OpusDecoder,
    delay_left: usize,
    orig: Vec<f32>,
    decoded: Vec<f32>,
    scratch: Vec<f32>,
    /// Mono samples of the sliding window being filled (original, decoded).
    ref_seg: Vec<f64>,
    deg_seg: Vec<f64>,
    /// Completed windows waiting to be scored (ref, deg, bitrate_kbps), oldest first.
    pending: std::collections::VecDeque<(Vec<f64>, Vec<f64>, u32)>,
    series: Vec<(f32, u32)>,
    window_frames: usize,
    hop_frames: usize,
}

#[cfg(debug_assertions)]
impl VisqolTracker {
    fn new(rate: u32) -> Option<Self> {
        let decoder = OpusDecoder::new(rate as i32, 2).ok()?;
        let window_frames = rate as usize;
        let hop_frames = (rate / 2) as usize;
        Some(Self {
            rate,
            decoder,
            delay_left: codec_delay(rate),
            orig: Vec::new(),
            decoded: Vec::new(),
            scratch: vec![0.0; (rate / 50) as usize * 2],
            ref_seg: Vec::with_capacity(window_frames),
            deg_seg: Vec::with_capacity(window_frames),
            pending: std::collections::VecDeque::new(),
            series: Vec::new(),
            window_frames,
            hop_frames,
        })
    }

    fn record(&mut self, orig: &[f32], packet: &[u8], frames: usize, bitrate_kbps: u32) {
        self.orig.extend_from_slice(&orig[..frames * 2]);
        let got = self
            .decoder
            .decode(packet, frames, &mut self.scratch)
            .unwrap_or(0)
            .min(frames);
        self.decoded.extend_from_slice(&self.scratch[..got * 2]);
        // A short decode pads with silence so counts stay exact.
        for _ in got..frames {
            self.decoded.extend_from_slice(&[0.0, 0.0]);
        }
        if self.delay_left > 0 {
            let drop = (self.delay_left * 2).min(self.decoded.len());
            self.decoded.drain(..drop);
            self.delay_left -= drop / 2;
        }
        // Append the aligned samples as mono downmixes; whenever a
        // full window is ready, queue it and advance by one hop.
        let avail = (self.orig.len() / 2).min(self.decoded.len() / 2);
        for i in 0..avail {
            let j = i * 2;
            self.ref_seg
                .push((self.orig[j] + self.orig[j + 1]) as f64 * 0.5);
            self.deg_seg
                .push((self.decoded[j] + self.decoded[j + 1]) as f64 * 0.5);
        }
        self.orig.drain(..avail * 2);
        self.decoded.drain(..avail * 2);
        while self.ref_seg.len() >= self.window_frames {
            let ref_win = self.ref_seg[..self.window_frames].to_vec();
            let deg_win = self.deg_seg[..self.window_frames].to_vec();
            if self.pending.len() >= 4 {
                self.pending.pop_front();
            }
            self.pending.push_back((ref_win, deg_win, bitrate_kbps));
            self.ref_seg.drain(..self.hop_frames);
            self.deg_seg.drain(..self.hop_frames);
        }
    }

    /// Take the oldest unscored window, if any.
    fn take_work(&mut self) -> Option<VisqolWork> {
        let (ref_seg, deg_seg, bitrate_kbps) = self.pending.pop_front()?;
        Some(VisqolWork {
            rate: self.rate,
            ref_seg,
            deg_seg,
            bitrate_kbps,
        })
    }

    /// Store a score computed by the scoring worker, with the
    /// bitrate the scored window was encoded at.
    fn push_score(&mut self, score: f32, bitrate_kbps: u32) {
        self.series.push((score, bitrate_kbps));
    }

    fn snapshot(&self) -> Vec<(f32, u32)> {
        self.series.clone()
    }
}

/// Debug builds: one window of original/decoded mono audio waiting
/// to be scored at the stream's sample rate.
#[cfg(debug_assertions)]
struct VisqolWork {
    rate: u32,
    ref_seg: Vec<f64>,
    deg_seg: Vec<f64>,
    bitrate_kbps: u32,
}

#[cfg(debug_assertions)]
impl VisqolWork {
    fn score(&self) -> Option<f32> {
        use audio_samples_qoe::AudioSamplesQoE;

        let rate = std::num::NonZeroU32::new(self.rate)?;
        let reference = audio_samples::AudioSamples::new_mono(
            ndarray::Array1::from_vec(self.ref_seg.clone()),
            rate,
        )
        .ok()?;
        let degraded = audio_samples::AudioSamples::new_mono(
            ndarray::Array1::from_vec(self.deg_seg.clone()),
            rate,
        )
        .ok()?;
        reference.visqol(&degraded).ok().map(|score| score as f32)
    }
}

/// Debug builds: the process-wide ViSQOL scoring worker. Scoring a
/// segment takes a noticeable fraction of a second, so it must not
/// run on the player loop or behind the session lock (the audio
/// callback needs that lock every period); segments arrive here
/// with the session they belong to, and scores are pushed back
/// under a brief lock. If the session is gone by then, the score
/// lands in an orphaned state and is simply never read.
#[cfg(debug_assertions)]
fn visqol_worker(
) -> &'static std::sync::Mutex<std::sync::mpsc::Sender<(Arc<Shared>, VisqolWork)>> {
    static WORKER: std::sync::OnceLock<
        std::sync::Mutex<std::sync::mpsc::Sender<(Arc<Shared>, VisqolWork)>>,
    > = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(Arc<Shared>, VisqolWork)>();
        std::thread::spawn(move || {
            while let Ok((shared, work)) = rx.recv() {
                if let Some(score) = work.score() {
                    shared.lock().ring.push_visqol_score(score, work.bitrate_kbps);
                }
            }
        });
        std::sync::Mutex::new(tx)
    })
}

/// One encoded packet and the PCM frames it decodes to.
struct Packet {
    data: Vec<u8>,
    frames: usize,
}

/// Opus-backed storage: encoded packets plus one decoded packet.
struct OpusStore {
    rate: u32,
    encoder: OpusEncoder,
    decoder: OpusDecoder,
    /// Frames per packet (20 ms).
    packet_frames: usize,
    /// Pushed PCM not yet filling a whole packet.
    pending: Vec<(f32, f32)>,
    packets: VecDeque<Packet>,
    /// Total frames and bytes held in `packets`.
    packet_frames_total: usize,
    packet_bytes: usize,
    /// Decoded frames not yet consumed, oldest first.
    window: VecDeque<(f32, f32)>,
    /// Stream-start frames still to discard (the codec delay).
    delay_left: usize,
    /// Frames a skip already accounted for that have not emerged from
    /// the codec delay yet; dropped in `fill_window` as they emerge.
    drop_ahead: usize,
    /// Scratch buffers, reused so decoding allocates nothing.
    encode_in: Vec<f32>,
    packet_out: Vec<u8>,
    decode_out: Vec<f32>,
    /// Current encoder bit rate; kept so `reset` re-applies it.
    bitrate_bps: i32,
    /// Debug builds: round-trip quality measurement.
    #[cfg(debug_assertions)]
    visqol: Option<VisqolTracker>,
}

impl OpusStore {
    fn new(rate: u32) -> Option<Self> {
        let mut encoder = OpusEncoder::new(rate as i32, 2, Application::Audio).ok()?;
        encoder.bitrate_bps = OPUS_BITRATE_BPS;
        let decoder = OpusDecoder::new(rate as i32, 2).ok()?;
        let packet_frames = (rate / 50) as usize;
        Some(Self {
            rate,
            encoder,
            decoder,
            packet_frames,
            pending: Vec::with_capacity(packet_frames * 2),
            packets: VecDeque::new(),
            packet_frames_total: 0,
            packet_bytes: 0,
            window: VecDeque::new(),
            delay_left: codec_delay(rate),
            drop_ahead: 0,
            encode_in: vec![0.0; packet_frames * 2],
            packet_out: vec![0u8; 16_384],
            decode_out: vec![0.0; packet_frames * 2],
            bitrate_bps: OPUS_BITRATE_BPS,
            #[cfg(debug_assertions)]
            visqol: VisqolTracker::new(rate),
        })
    }

    /// Drop everything and restart the codec stream.
    fn reset(&mut self) {
        if let Ok(mut encoder) = OpusEncoder::new(self.rate as i32, 2, Application::Audio) {
            encoder.bitrate_bps = self.bitrate_bps;
            self.encoder = encoder;
        }
        if let Ok(decoder) = OpusDecoder::new(self.rate as i32, 2) {
            self.decoder = decoder;
        }
        self.pending.clear();
        self.packets.clear();
        self.packet_frames_total = 0;
        self.packet_bytes = 0;
        self.window.clear();
        self.delay_left = codec_delay(self.rate);
        self.drop_ahead = 0;
        #[cfg(debug_assertions)]
        {
            let prev_series = self
                .visqol
                .take()
                .map(|t| t.series)
                .unwrap_or_default();
            self.visqol = VisqolTracker::new(self.rate).map(|mut t| {
                t.series = prev_series;
                t
            });
        }
    }

    /// Set the encoder bit rate (kbps, clamped to the selectable
    /// range) for the rest of this stream.
    fn set_bitrate(&mut self, kbps: u32) {
        self.bitrate_bps = (kbps.clamp(6, 510) * 1_000) as i32;
        self.encoder.bitrate_bps = self.bitrate_bps;
    }

    /// Debug builds: take the oldest unscored ViSQOL window.
    #[cfg(debug_assertions)]
    fn take_visqol_work(&mut self) -> Option<VisqolWork> {
        self.visqol.as_mut().and_then(|tracker| tracker.take_work())
    }

    /// Debug builds: store a score computed by the scoring worker.
    #[cfg(debug_assertions)]
    fn push_visqol_score(&mut self, score: f32, bitrate_kbps: u32) {
        if let Some(tracker) = &mut self.visqol {
            tracker.push_score(score, bitrate_kbps);
        }
    }

    /// Debug builds: the ViSQOL series so far ((MOS-LQO, bitrate_kbps) per window).
    #[cfg(debug_assertions)]
    fn visqol_snapshot(&self) -> Vec<(f32, u32)> {
        self.visqol
            .as_ref()
            .map_or_else(Vec::new, |tracker| tracker.snapshot())
    }

    fn push_interleaved(&mut self, samples: &[f32]) {
        self.pending
            .extend(samples.chunks_exact(2).map(|lr| (lr[0], lr[1])));
        while self.pending.len() >= self.packet_frames {
            for (i, (l, r)) in self.pending.drain(..self.packet_frames).enumerate() {
                self.encode_in[i * 2] = l;
                self.encode_in[i * 2 + 1] = r;
            }
            let frames = self.packet_frames;
            match self.encoder.encode(&self.encode_in, frames, &mut self.packet_out) {
                Ok(len) => {
                    let data = self.packet_out[..len].to_vec();
                    self.packet_bytes += data.len();
                    self.packet_frames_total += frames;
                    #[cfg(debug_assertions)]
                    if let Some(tracker) = &mut self.visqol {
                        tracker.record(
                            &self.encode_in,
                            &data,
                            frames,
                            (self.bitrate_bps / 1_000) as u32,
                        );
                    }
                    self.packets.push_back(Packet { data, frames });
                }
                Err(e) => {
                    // Not expected (the arguments are fixed and the
                    // output buffer is generous): put the frames back
                    // and retry on the next push, so counts stay exact.
                    eprintln!("[WARN] Opus encode failed: {e}");
                    let back: Vec<(f32, f32)> = self.encode_in
                        .chunks_exact(2)
                        .map(|lr| (lr[0], lr[1]))
                        .collect();
                    self.pending.splice(0..0, back);
                    break;
                }
            }
        }
    }

    /// Decode the oldest packet into the window (unless it already holds
    /// frames or no packet is left), discarding the codec delay at the
    /// start of a stream.
    fn fill_window(&mut self) {
        if !self.window.is_empty() {
            return;
        }
        let Some(packet) = self.packets.pop_front() else {
            return;
        };
        self.packet_bytes -= packet.data.len();
        self.packet_frames_total -= packet.frames;
        let got = self
            .decoder
            .decode(&packet.data, packet.frames, &mut self.decode_out)
            .unwrap_or(0)
            .min(packet.frames);
        let skip = self.delay_left.min(got);
        self.delay_left -= skip;
        self.window.extend(
            self.decode_out[skip * 2..got * 2]
                .chunks_exact(2)
                .map(|lr| (lr[0], lr[1])),
        );
        // A short decode (a corrupt packet, which self-encoded data
        // never is) pads with silence so frame counts stay exact.
        for _ in got..packet.frames {
            self.window.push_back((0.0, 0.0));
        }
        // Frames an earlier skip already accounted for can still be
        // inside this packet (they emerge late, out of the codec
        // delay); drop them here so they are never played.
        if self.drop_ahead > 0 {
            let drop = self.drop_ahead.min(self.window.len());
            self.window.drain(..drop);
            self.drop_ahead -= drop;
        }
    }

    fn pop_front(&mut self) -> Option<(f32, f32)> {
        self.fill_window();
        self.window.pop_front()
    }

    fn ready_frames(&self) -> usize {
        self.window.len() + self.packet_frames_total
    }

    /// Drop `n` frames from the front (a seek inside the buffer).
    fn skip_frames(&mut self, mut n: usize) {
        let from_window = n.min(self.window.len());
        self.window.drain(..from_window);
        n -= from_window;
        // Whole packets go without decoding; the last one dropped warms
        // the decoder for whatever is decoded next, so its first frames
        // are not decoded cold.
        let mut last_dropped: Option<Packet> = None;
        // While an earlier skip still owes frames, packets must go
        // through the decoder so the owed frames are dropped inside
        // fill_window instead of being counted twice here.
        while n > 0 && self.drop_ahead == 0 {
            let Some(front) = self.packets.front() else {
                break;
            };
            if front.frames > n {
                break;
            }
            let packet = self.packets.pop_front().unwrap();
            self.packet_bytes -= packet.data.len();
            self.packet_frames_total -= packet.frames;
            n -= packet.frames;
            last_dropped = Some(packet);
        }
        if let Some(warm) = last_dropped {
            // Mid-stream now: the stream-start delay no longer applies.
            self.delay_left = 0;
            let _ = self
                .decoder
                .decode(&warm.data, warm.frames, &mut self.decode_out);
        }
        // Decode packets until the skip is covered. One fill is not
        // always enough: at the start of a stream the first fill loses
        // the codec delay, so it can yield fewer frames than the skip
        // still needs.
        while n > 0 && !self.packets.is_empty() {
            self.fill_window();
            let from_window = n.min(self.window.len());
            self.window.drain(..from_window);
            n -= from_window;
            if from_window == 0 {
                break;
            }
        }
        if n > 0 {
            // The skip lands in the not-yet-encoded tail; dropping raw
            // PCM is exact.
            let from_pending = n.min(self.pending.len());
            self.pending.drain(..from_pending);
            n -= from_pending;
        }
        if n > 0 {
            // The skip ran past everything decoded so far. The
            // remaining frames are still inside the codec delay (the
            // store delivers only pushed - delay frames until more
            // audio is encoded), so drop them as they emerge.
            self.drop_ahead += n;
        }
    }

    fn memory_bytes(&self) -> u64 {
        self.packet_bytes as u64
            + (self.window.len() + self.pending.len()) as u64 * BYTES_PER_FRAME
    }
}

/// The prerender buffer's audio storage. At rates Opus supports the
/// audio is kept as Opus packets (see [`OpusStore`]) and decoded a
/// packet at a time as the callback consumes it; at other rates it is
/// raw PCM. Either way it behaves as a frame queue: the render thread
/// pushes rendered chunks, the callback pops frames, and seeks skip
/// forward inside it.
struct AudioQueue {
    store: Store,
    /// Buffered frames (pushed, not yet popped or skipped).
    frames: usize,
    /// Generation frame index of the oldest buffered frame, and of the
    /// frame one past the newest. Playback can run ahead of both over
    /// silence (see `drop_before`).
    head: u64,
    tail: u64,
}

enum Store {
    Pcm(VecDeque<(f32, f32)>),
    Opus(Box<OpusStore>),
}

impl AudioQueue {
    fn new(rate: u32) -> Self {
        let store = match OpusStore::new(rate) {
            Some(store) => Store::Opus(Box::new(store)),
            None => Store::Pcm(VecDeque::new()),
        };
        Self {
            store,
            frames: 0,
            head: 0,
            tail: 0,
        }
    }

    #[cfg(test)]
    fn from_pcm(frames: &[(f32, f32)]) -> Self {
        let mut queue = Self {
            store: Store::Pcm(VecDeque::new()),
            frames: 0,
            head: 0,
            tail: 0,
        };
        let interleaved: Vec<f32> = frames.iter().flat_map(|&(l, r)| [l, r]).collect();
        queue.push_interleaved(&interleaved);
        queue
    }

    fn frames(&self) -> usize {
        self.frames
    }

    /// Frames the callback can take right now (everything except the
    /// not-yet-encoded tail of an Opus stream).
    fn ready_frames(&self) -> usize {
        match &self.store {
            Store::Pcm(_) => self.frames,
            Store::Opus(store) => store.ready_frames(),
        }
    }

    fn push_interleaved(&mut self, samples: &[f32]) {
        let pushed = samples.len() / 2;
        match &mut self.store {
            Store::Pcm(deque) => {
                deque.extend(samples.chunks_exact(2).map(|lr| (lr[0], lr[1])))
            }
            Store::Opus(store) => store.push_interleaved(samples),
        }
        self.frames += pushed;
        self.tail += pushed as u64;
    }

    fn pop_front(&mut self) -> Option<(f32, f32)> {
        let frame = match &mut self.store {
            Store::Pcm(deque) => deque.pop_front(),
            Store::Opus(store) => store.pop_front(),
        };
        if frame.is_some() {
            self.frames -= 1;
            self.head += 1;
        }
        frame
    }

    /// Drop `n` frames from the front (a seek inside the buffer).
    fn skip_frames(&mut self, n: usize) {
        debug_assert!(n <= self.frames);
        match &mut self.store {
            Store::Pcm(deque) => {
                deque.drain(..n);
            }
            Store::Opus(store) => store.skip_frames(n),
        }
        self.frames -= n;
        self.head += n as u64;
    }

    /// Playback advanced to `played` (over silence, on underrun) while
    /// the render thread was busy: drop the buffered frames it passed.
    /// With raw PCM the callback has always drained the queue before
    /// counting silence, so this is a no-op there; the not-yet-encoded
    /// tail of an Opus stream is what makes it necessary.
    fn drop_before(&mut self, played: u64) {
        if played <= self.head {
            return;
        }
        if played >= self.tail {
            self.clear();
            self.head = played;
            self.tail = played;
            return;
        }
        self.skip_frames((played - self.head) as usize);
    }

    fn clear(&mut self) {
        match &mut self.store {
            Store::Pcm(deque) => deque.clear(),
            Store::Opus(store) => store.reset(),
        }
        self.frames = 0;
        self.head = 0;
        self.tail = 0;
    }

    fn memory_bytes(&self) -> u64 {
        match &self.store {
            Store::Pcm(_) => self.frames as u64 * BYTES_PER_FRAME,
            Store::Opus(store) => store.memory_bytes(),
        }
    }

    fn is_compressed(&self) -> bool {
        matches!(self.store, Store::Opus(_))
    }

    /// Debug builds: take the oldest unscored ViSQOL segment.
    #[cfg(debug_assertions)]
    fn take_visqol_work(&mut self) -> Option<VisqolWork> {
        match &mut self.store {
            Store::Opus(store) => store.take_visqol_work(),
            Store::Pcm(_) => None,
        }
    }

    /// Debug builds: store a score computed by the scoring worker.
    #[cfg(debug_assertions)]
    fn push_visqol_score(&mut self, score: f32, bitrate_kbps: u32) {
        if let Store::Opus(store) = &mut self.store {
            store.push_visqol_score(score, bitrate_kbps);
        }
    }

    /// Debug builds: the ViSQOL series so far.
    #[cfg(debug_assertions)]
    fn visqol_snapshot(&self) -> Vec<(f32, u32)> {
        match &self.store {
            Store::Opus(store) => store.visqol_snapshot(),
            Store::Pcm(_) => Vec::new(),
        }
    }

    /// Set the Opus encoder bit rate (kbps).
    fn set_opus_bitrate(&mut self, kbps: u32) {
        if let Store::Opus(store) = &mut self.store {
            store.set_bitrate(kbps);
        }
    }

    /// Oldest buffered frame, decoding the head packet if needed.
    #[cfg(test)]
    fn front(&mut self) -> Option<(f32, f32)> {
        match &mut self.store {
            Store::Pcm(deque) => deque.front().copied(),
            Store::Opus(store) => {
                store.fill_window();
                store.window.front().copied()
            }
        }
    }
}

/// Restart rendering from the current state.
fn restart(state: &mut State) {
    state.generation += 1;
    state.ring.clear();
    state.played = 0;
    state.epoch += 1;
    state.stash_frames = 0;
    state.started = false;
    state.heard_floor = 0;
    state.resume_at = Some(state.base_ns);
    state.voice_marks.clear();
}

pub struct Session {
    shared: Arc<Shared>,
    /// Dropping this ends the thread owning the (non-Send) cpal stream.
    _stream_shutdown: std::sync::mpsc::Sender<()>,
}

impl Session {
    /// Open the default output at `rate` and start the render thread.
    pub fn start(rate: u32, render: RenderFn) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                generation: 0,
                data: None,
                playing: false,
                base_ns: 0,
                speed: 1.0,
                max_frames: frames_for_secs(DEFAULT_MAX_BUFFER_SECS, rate),
                ring: AudioQueue::new(rate),
                played: 0,
                started: false,
                latency_frames: 0,
                stash_frames: 0,
                epoch: 0,
                heard_floor: 0,
                bend_override: None,
                resume_at: Some(0),
                voice_marks: VecDeque::new(),
                drop_velocity: 0,
                limiter_bypassed: false,
                shutdown: false,
            }),
            wake: Condvar::new(),
            rate,
        });
        let (tx_shutdown, rx_shutdown) = std::sync::mpsc::channel::<()>();
        let (tx_ready, rx_ready) = std::sync::mpsc::channel();
        let device_shared = shared.clone();
        thread::spawn(move || {
            let stream = match open_device(device_shared) {
                Ok(stream) => {
                    let _ = tx_ready.send(Ok(()));
                    stream
                }
                Err(e) => {
                    let _ = tx_ready.send(Err(e));
                    return;
                }
            };
            let _ = rx_shutdown.recv();
            drop(stream);
        });
        rx_ready
            .recv()
            .map_err(|_| "Audio output thread ended".to_string())??;

        let render_shared = shared.clone();
        thread::spawn(move || render_loop(render_shared, render));

        Ok(Self {
            shared,
            _stream_shutdown: tx_shutdown,
        })
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.shared.lock());
        self.shared.wake.notify_all();
    }

    /// File to render; `None` while loading.
    pub fn set_data(&self, data: Option<MidiData>) {
        self.update(|s| {
            s.data = data;
            restart(s);
        });
    }

    /// Resume from `position_ns`. The kept buffer is used when it starts
    /// there (normally: the position [`pause`](Self::pause) returned).
    pub fn play(&self, position_ns: u64, speed: f32) {
        self.update(|s| {
            s.playing = true;
            if s.resume_at != Some(position_ns) || s.speed != speed {
                s.base_ns = position_ns;
                s.speed = speed;
                restart(s);
            }
        });
    }

    /// Stop consuming audio, keeping the buffer. Returns the paused position.
    pub fn pause(&self) -> u64 {
        let rate = self.shared.rate;
        let mut s = self.shared.lock();
        s.playing = false;
        let position = heard_ns(&mut s, rate);
        s.resume_at = Some(position);
        position
    }

    /// Jump to `position_ns`. A forward target inside the buffer just skips
    /// ahead; anything else re-renders. Returns the new position.
    pub fn seek(&self, position_ns: u64) -> u64 {
        let rate = self.shared.rate;
        let mut s = self.shared.lock();
        // The player seeks in heard time, which trails `played` by the
        // device latency; aim there so the requested moment is what plays.
        let target = frame_at(s.base_ns, position_ns, s.speed, rate) + s.latency_frames;
        if position_ns < s.base_ns || !s.skip_to(target) {
            s.base_ns = position_ns;
            restart(&mut s);
        }
        let position = heard_ns(&mut s, rate);
        s.resume_at = Some(position);
        drop(s);
        self.shared.wake.notify_all();
        position
    }

    pub fn set_speed(&self, speed: f32) {
        let rate = self.shared.rate;
        self.update(|s| {
            if s.speed == speed {
                return;
            }
            s.base_ns = heard_ns(s, rate);
            s.speed = speed;
            restart(s);
        });
    }

    /// Throw away what is buffered and render it again from what is being
    /// heard, so a change to the synth's own settings is heard right away.
    pub fn rerender(&self) {
        let rate = self.shared.rate;
        self.update(|s| {
            s.base_ns = heard_ns(s, rate);
            restart(s);
        });
    }

    /// Force every channel's pitch bend range to `semitones` (Pitch Bend
    /// Override), or `None` to follow the file. Re-renders what is buffered.
    pub fn set_bend_override(&self, semitones: Option<u8>) {
        let rate = self.shared.rate;
        self.update(|s| {
            if s.bend_override == semitones {
                return;
            }
            s.bend_override = semitones;
            s.base_ns = heard_ns(s, rate);
            restart(s);
        });
    }

    /// Takes effect immediately. Shrinking below what is already buffered
    /// re-renders, since the synth has already advanced past the cut.
    pub fn set_max_buffer_secs(&self, secs: u32) {
        let rate = self.shared.rate;
        let secs = secs.clamp(MIN_MAX_BUFFER_SECS, MAX_MAX_BUFFER_SECS);
        self.update(|s| {
            s.max_frames = frames_for_secs(secs, rate);
            if s.ring.frames() > s.max_frames {
                s.base_ns = heard_ns(s, rate);
                restart(s);
            }
        });
    }

    /// S16 Clipping: `true` skips the output limiter; samples past full
    /// scale wrap around. Takes effect on the next chunk.
    pub fn set_limiter_bypassed(&self, bypassed: bool) {
        self.update(|s| s.limiter_bypassed = bypassed);
    }

    /// Song position of the audio being heard.
    pub fn position_ns(&self) -> u64 {
        heard_ns(&mut self.shared.lock(), self.shared.rate)
    }

    /// Output rate in Hz.
    pub fn rate(&self) -> u32 {
        self.shared.rate
    }

    /// Memory the buffered audio takes, in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.shared.lock().ring.memory_bytes()
    }

    /// Whether the buffered audio is Opus-compressed (`false`: raw PCM,
    /// at a rate Opus does not support).
    pub fn compressed(&self) -> bool {
        self.shared.lock().ring.is_compressed()
    }

    /// Debug builds: ViSQOL MOS-LQO and bitrate (kbps) of the Opus
    /// round trip, one value per 1-second window scored every half
    /// second, for the debug graph window. Empty for PCM sessions.
    #[cfg(debug_assertions)]
    pub fn visqol_series(&self) -> Vec<(f32, u32)> {
        // Hand unscored segments to the scoring worker instead of
        // scoring here: the caller is the player loop, and a ViSQOL
        // run would stall it (and, through this lock, the audio
        // callback) for a noticeable fraction of a second per
        // segment, every segment.
        if let Some(work) = self.shared.lock().ring.take_visqol_work() {
            let tx = visqol_worker()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _ = tx.send((self.shared.clone(), work));
        }
        self.shared.lock().ring.visqol_snapshot()
    }

    /// Set the Opus encoder bit rate (kbps, 6 to 510).
    pub fn set_opus_bitrate(&self, kbps: u32) {
        self.shared.lock().ring.set_opus_bitrate(kbps);
    }

    /// Rendered audio waiting to be played, in wall-clock nanoseconds.
    pub fn buffered_ns(&self) -> u64 {
        let frames = self.shared.lock().ring.frames() as u64;
        frames * 1_000_000_000 / self.shared.rate as u64
    }
}

impl Session {
    /// Highest note-on velocity being dropped to keep up (0: none).
    pub fn drop_velocity(&self) -> u32 {
        self.shared.lock().drop_velocity
    }

    /// Voices sounding in the audio being heard.
    pub fn polyphony(&self) -> u32 {
        let mut s = self.shared.lock();
        heard_polyphony(&mut s)
    }
}

/// Voice count of the chunk being heard; drops marks already passed.
fn heard_polyphony(s: &mut State) -> u32 {
    let pending = s.latency_frames + s.stash_frames;
    let heard = s.played - pending.min(s.played);
    while s
        .voice_marks
        .get(1)
        .is_some_and(|&(start, _)| start <= heard)
    {
        s.voice_marks.pop_front();
    }
    match s.voice_marks.front() {
        Some(&(start, voices)) if start <= heard => voices,
        _ => 0,
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.update(|s| s.shutdown = true);
    }
}

struct Tracks {
    data: MidiData,
    cursors: Vec<TrackCursor>,
}

impl Tracks {
    fn seek(data: MidiData, position_ns: u64) -> Self {
        let raw = unsafe { &*data.as_ptr() };
        let tick = ns_to_tick(raw, position_ns);
        let cursors = (0..data.header().tracks)
            .map(|i| unsafe { seek_track_cursor(raw, i, tick) })
            .collect();
        Self { data, cursors }
    }

    /// Append every channel message before `end_ns` as `(song ns, message)`,
    /// in (tick, track) order. Without `with_ns` the times are left 0.
    ///
    /// Black MIDI can pack millions of events into a second, so this is the
    /// render loop's main cost: tracks are walked in parallel, and the time
    /// of each event is not looked up just to test it against the end.
    fn collect_until(&mut self, end_ns: u64, with_ns: bool, out: &mut Vec<(u64, u32)>) {
        let raw = unsafe { &*self.data.as_ptr() };
        // Last tick sounding before `end_ns`, if any.
        let mut last = ns_to_tick(raw, end_ns);
        while last > 0 && tick_to_ns(raw, last) >= end_ns {
            last -= 1;
        }
        if tick_to_ns(raw, last) >= end_ns {
            return;
        }
        // Packed (tick, track) sort key, message.
        fn walk(index: usize, cursor: &mut TrackCursor, last: u32, events: &mut Vec<(u64, u32)>) {
            while cursor.has_event && cursor.abs_tick <= last {
                if !cursor.is_tempo {
                    let msg = cursor.status as u32
                        | (cursor.param1 as u32) << 8
                        | (cursor.param2 as u32) << 16;
                    events.push(((cursor.abs_tick as u64) << 32 | index as u64, msg));
                }
                if !unsafe { cursor_next(cursor) } {
                    cursor.has_event = false;
                }
            }
        }
        let mut events = Vec::new();
        if self.cursors.len() < PARALLEL_TRACKS {
            for (index, cursor) in self.cursors.iter_mut().enumerate() {
                walk(index, cursor, last, &mut events);
            }
        } else {
            use rayon::prelude::*;
            let parts: Vec<Vec<(u64, u32)>> = synth_pool().install(|| {
                self.cursors
                    .par_iter_mut()
                    .enumerate()
                    .fold(Vec::new, |mut part, (index, cursor)| {
                        walk(index, cursor, last, &mut part);
                        part
                    })
                    .collect()
            });
            events.reserve(parts.iter().map(Vec::len).sum());
            for part in parts {
                events.extend(part);
            }
        }
        events.sort_unstable_by_key(|&(key, _)| key);
        out.extend(events.into_iter().map(|(key, msg)| {
            let ns = if with_ns {
                tick_to_ns(raw, (key >> 32) as u32)
            } else {
                0
            };
            (ns, msg)
        }));
    }
}

/// Rendering more than this many frames behind playback jumps ahead instead
/// of reading the missed span (see `render_loop`): on files dense enough to
/// fall behind at all, reading even a short span costs more than it lasts.
const RESEEK_BEHIND_FRAMES: u64 = CHUNK_FRAMES as u64;

/// From this many tracks on, they are walked in parallel.
const PARALLEL_TRACKS: usize = 8;

/// Silence the synth and return controllers to neutral before a new run.
fn reset_messages() -> Vec<u32> {
    let mut msgs = Vec::with_capacity(64);
    for ch in 0..16u32 {
        msgs.push(0xB0 | ch | 120 << 8);
        msgs.push(0xB0 | ch | 123 << 8);
        msgs.push(0xB0 | ch | 121 << 8);
        msgs.push(0xE0 | ch | 64 << 16);
    }
    msgs
}

/// Pitch Bend Override for the file's messages, as the player does for
/// realtime engines: RPN 0 data entry is replaced by the forced range.
struct BendOverride {
    semitones: Option<u8>,
    /// Selected RPN (MSB, LSB) per channel.
    rpn: [(u8, u8); 16],
}

impl BendOverride {
    fn new(semitones: Option<u8>) -> Self {
        Self {
            semitones,
            rpn: [(127, 127); 16],
        }
    }

    /// Messages that set the forced range on every channel.
    fn initial(&self) -> Vec<u32> {
        let Some(range) = self.semitones else {
            return Vec::new();
        };
        let range = range.clamp(1, 127) as u32;
        (0..16u32)
            .flat_map(|ch| {
                let cc = |c: u32, v: u32| 0xB0 | ch | c << 8 | v << 16;
                [
                    cc(101, 0),
                    cc(100, 0),
                    cc(6, range),
                    cc(38, 0),
                    cc(101, 127),
                    cc(100, 127),
                ]
            })
            .collect()
    }

    fn rewrite(&mut self, msg: u32) -> u32 {
        let Some(range) = self.semitones else {
            return msg;
        };
        let status = msg & 0xFF;
        if status & 0xF0 != 0xB0 {
            return msg;
        }
        let ch = (status & 0x0F) as usize;
        let (p1, p2) = ((msg >> 8) & 0x7F, ((msg >> 16) & 0x7F) as u8);
        let value = match p1 {
            101 => {
                self.rpn[ch].0 = p2;
                return msg;
            }
            100 => {
                self.rpn[ch].1 = p2;
                return msg;
            }
            6 if self.rpn[ch] == (0, 0) => range.clamp(1, 127) as u32,
            38 if self.rpn[ch] == (0, 0) => 0,
            121 => {
                self.rpn[ch] = (127, 127);
                return msg;
            }
            _ => return msg,
        };
        status | p1 << 8 | value << 16
    }
}

/// Below this much buffered audio, quiet notes start being dropped...
const THIN_START_SECS: f64 = 2.0;
/// ...rising linearly to every note at this much.
const THIN_FULL_SECS: f64 = 0.1;

/// Highest note-on velocity to drop with `buffered_secs` of audio left:
/// 0 (none) above [`THIN_START_SECS`], 1 at it, 127 (all) at
/// [`THIN_FULL_SECS`] and below.
fn thin_velocity(buffered_secs: f64) -> u32 {
    if buffered_secs > THIN_START_SECS {
        return 0;
    }
    let t =
        ((THIN_START_SECS - buffered_secs) / (THIN_START_SECS - THIN_FULL_SECS)).clamp(0.0, 1.0);
    (1.0 + t * 126.0).round() as u32
}

/// Drops the note-ons of a span rendering skipped over, together with the
/// note-offs that would have ended them. Everything else still applies, so
/// controllers and pitch bend stay right.
struct SkipFilter {
    /// Dropped note-ons not yet matched by a note-off, per (channel, key).
    dropped: Vec<u16>,
}

impl SkipFilter {
    fn new() -> Self {
        Self {
            dropped: vec![0; 16 * 128],
        }
    }

    /// Drops note-ons of velocity up to `drop_velocity` (0: none, 127: all).
    fn filter(&mut self, msg: u32, drop_velocity: u32) -> Option<u32> {
        let status = msg & 0xFF;
        let key = (status as usize & 0x0F) * 128 + ((msg >> 8) & 0x7F) as usize;
        let velocity = (msg >> 16) & 0x7F;
        match status & 0xF0 {
            0x90 if velocity > 0 && velocity <= drop_velocity => {
                self.dropped[key] = self.dropped[key].saturating_add(1);
                None
            }
            0x80 | 0x90 if velocity == 0 || status & 0xF0 == 0x80 => {
                if self.dropped[key] > 0 {
                    self.dropped[key] -= 1;
                    None
                } else {
                    Some(msg)
                }
            }
            _ => Some(msg),
        }
    }
}

fn render_loop(shared: Arc<Shared>, mut render: RenderFn) {
    let mut low_priority = true;
    set_render_priority(low_priority);
    let rate = shared.rate;
    let mut generation = u64::MAX;
    let mut tracks: Option<Tracks> = None;
    let mut rendered = 0u64;
    let mut base_ns = 0;
    let mut speed = 1.0;
    let mut pending: Vec<(u64, u32)> = Vec::new();
    let mut msgs: Vec<(u32, u32)> = Vec::new();
    let mut bend_override = BendOverride::new(None);
    let mut skip = SkipFilter::new();
    let mut thin_armed = false;
    let mut drop_velocity;
    let mut limiter_bypassed;
    let mut left = vec![0.0f32; CHUNK_FRAMES];
    let mut right = vec![0.0f32; CHUNK_FRAMES];
    // Runs on interleaved stereo, so both channels share one gain (linked)
    // and the per-sample rate is doubled.
    let mut limiter = crate::audio_limiter::AudioLimiter::new(
        rate as f32 * 2.0,
        LIMITER_THRESHOLD_DB,
        LIMITER_RELEASE_MS,
        LIMITER_LOOKAHEAD_MS,
    );
    let mut interleaved = Vec::with_capacity(CHUNK_FRAMES * 2);
    loop {
        let played = {
            let mut s = shared.lock();
            loop {
                if s.shutdown {
                    return;
                }
                if s.generation != generation {
                    generation = s.generation;
                    tracks = s.data.clone().map(|d| Tracks::seek(d, s.base_ns));
                    rendered = 0;
                    base_ns = s.base_ns;
                    speed = s.speed;
                    pending.clear();
                    bend_override = BendOverride::new(s.bend_override);
                    skip = SkipFilter::new();
                    thin_armed = false;
                    msgs = reset_messages()
                        .into_iter()
                        .chain(bend_override.initial())
                        .map(|m| (0, m))
                        .collect();
                }
                // Also prefill while paused, so play starts from a full buffer.
                if tracks.is_some() && s.ring.frames() < s.max_frames {
                    break;
                }
                s = shared.wake.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            // Thinning arms once this generation has built up its buffer, so
            // the refill after a seek or start keeps every note, or once
            // playback outruns rendering (the buffer may never fill).
            let buffered = s.ring.frames();
            let arm_frames = s.max_frames.min((THIN_START_SECS * rate as f64) as usize);
            if buffered >= arm_frames {
                thin_armed = true;
            }
            drop_velocity = if thin_armed && s.playing {
                thin_velocity(buffered as f64 / rate as f64)
            } else {
                0
            };
            s.drop_velocity = drop_velocity;
            limiter_bypassed = s.limiter_bypassed;
            let fill = buffered as f64 / s.max_frames.max(1) as f64;
            if low_priority && fill < URGENT_BELOW || !low_priority && fill > RELAXED_ABOVE {
                low_priority = !low_priority;
                drop(s);
                set_render_priority(low_priority);
                shared.lock().played
            } else {
                s.played
            }
        };

        // Playback ran past what was rendered: jump there, applying the
        // missed span's events except the notes it no longer has time for.
        if played > rendered {
            thin_armed = true;
            let skip_ns = song_ns(base_ns, played, speed, rate);
            if played - rendered > RESEEK_BEHIND_FRAMES {
                // Far behind: reading the whole missed span could take longer
                // than it lasts and fall further behind each time. Jump there
                // and silence what was sounding instead.
                if let Some(t) = tracks.as_mut() {
                    *t = Tracks::seek(t.data.clone(), skip_ns);
                }
                skip = SkipFilter::new();
                msgs.extend((0..16u32).map(|ch| (0, 0xB0 | ch | 123 << 8)));
            } else {
                if let Some(tracks) = tracks.as_mut() {
                    tracks.collect_until(skip_ns, false, &mut pending);
                }
                for &(_, msg) in &pending {
                    if let Some(msg) = skip.filter(msg, 127) {
                        msgs.push((0, bend_override.rewrite(msg)));
                    }
                }
                pending.clear();
            }
            rendered = played;
        }

        let end_ns = song_ns(base_ns, rendered + CHUNK_FRAMES as u64, speed, rate);
        if let Some(tracks) = tracks.as_mut() {
            tracks.collect_until(end_ns, true, &mut pending);
        }
        for &(ns, msg) in &pending {
            let Some(msg) = skip.filter(msg, drop_velocity) else {
                continue;
            };
            let frame = frame_at(base_ns, ns, speed, rate)
                .saturating_sub(rendered)
                .min(CHUNK_FRAMES as u64 - 1);
            msgs.push((frame as u32, bend_override.rewrite(msg)));
        }
        pending.clear();
        let voices = render(&msgs, &mut left, &mut right);
        msgs.clear();
        interleaved.clear();
        interleaved.extend(left.iter().zip(&right).flat_map(|(&l, &r)| [l, r]));
        if limiter_bypassed {
            // S16 Clipping: no limiter. A sample past ±full scale wraps
            // around to the other end of the 16-bit range, the way s16
            // overflow did. A f32 output plays the same wrapped wave, so
            // the sound never depends on the device format.
            for sample in &mut interleaved {
                *sample = (*sample + 1.0).rem_euclid(2.0) - 1.0;
            }
        } else {
            limiter.process(&mut interleaved);
        }
        let chunk_start = rendered;
        rendered += CHUNK_FRAMES as u64;

        let mut s = shared.lock();
        if s.generation == generation {
            // Frames playback already passed while this chunk rendered
            // (it advances over silence on underrun): drop what it passed
            // inside the queue too, then append the rest of the chunk.
            let played = s.played;
            s.ring.drop_before(played);
            let late = (played.saturating_sub(chunk_start) as usize).min(CHUNK_FRAMES);
            s.ring.push_interleaved(&interleaved[late * 2..]);
            s.started |= s.ring.ready_frames() > 0;
            let start = chunk_start.max(s.played);
            s.voice_marks.push_back((start, voices));
        }
    }
}

fn open_device(shared: Arc<Shared>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("No audio output device found")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("No usable output format: {e}"))?;
    if supported.sample_rate() != shared.rate {
        return Err(format!(
            "Output rate changed ({} Hz, engine {} Hz)",
            supported.sample_rate(),
            shared.rate
        ));
    }
    let format = supported.sample_format();
    let config = supported.config();
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, shared)?,
        SampleFormat::I16 => build::<i16>(&device, config, shared)?,
        SampleFormat::U16 => build::<u16>(&device, config, shared)?,
        SampleFormat::I32 => build::<i32>(&device, config, shared)?,
        other => return Err(format!("Unsupported output sample format: {other}")),
    };
    stream
        .play()
        .map_err(|e| format!("Failed to start audio output: {e}"))?;
    Ok(stream)
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    let channels = config.channels as usize;
    let mut stash = Stash::default();
    // Reserved up front so the callback never grows it.
    stash.frames.reserve(STASH_FRAMES * 2);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let latency = ts.playback.duration_since(ts.callback);
                fill(&shared, &mut stash, out, channels, latency)
            },
            |err| eprintln!("[WARN] Prerender output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to open audio output: {e}"))
}

/// Frames the callback keeps topped up for itself.
const STASH_FRAMES: usize = 4096;

/// Audio owned by the callback, refilled whenever it gets the state lock.
/// The callback never waits for the lock: a render or player thread holding
/// it (possibly preempted, running below normal priority) would otherwise
/// stall the device and click even with plenty buffered.
#[derive(Default)]
struct Stash {
    frames: VecDeque<(f32, f32)>,
    /// `State::epoch` the frames belong to.
    epoch: u64,
    playing: bool,
    /// The callback thread has been raised to time-critical priority.
    boosted: bool,
}

/// Output one device buffer. With the lock: refresh the stash from the ring
/// (or count silence on underrun, see `State::started`). Without it: play on
/// from the stash.
fn fill<T: SizedSample + FromSample<f32>>(
    shared: &Shared,
    stash: &mut Stash,
    out: &mut [T],
    channels: usize,
    latency: std::time::Duration,
) {
    // The device thread is cpal's, at normal priority: with the synth pool
    // busy on every other core it can miss its deadline and click however
    // much is buffered. It does almost no work, so it may run first.
    if !stash.boosted {
        set_thread_priority(THREAD_PRIORITY_TIME_CRITICAL);
        stash.boosted = true;
    }
    let frames = out.len() / channels.max(1);
    let locked = match shared.state.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    let mut took = false;
    if let Some(mut s) = locked {
        s.latency_frames = (latency.as_secs_f64() * shared.rate as f64) as u64;
        if s.epoch != stash.epoch {
            stash.frames.clear();
            stash.epoch = s.epoch;
        }
        stash.playing = s.playing;
        if s.playing {
            let want = STASH_FRAMES.max(frames).saturating_sub(stash.frames.len());
            let moved = want.min(s.ring.ready_frames());
            // Frame by frame: no allocation on the audio thread.
            for _ in 0..moved {
                if let Some(frame) = s.ring.pop_front() {
                    stash.frames.push_back(frame);
                }
            }
            // Underrun: the shortfall plays as silence and still counts.
            let silence = if s.started {
                frames.saturating_sub(stash.frames.len())
            } else {
                0
            };
            let advanced = (moved + silence) as u64;
            s.played += advanced;
            if advanced > 0 {
                s.resume_at = None;
            }
            took = moved > 0;
            s.stash_frames = stash.frames.len().saturating_sub(frames) as u64;
        }
    }
    if took {
        shared.wake.notify_all();
    }

    for frame in out.chunks_mut(channels) {
        let (l, r) = if stash.playing {
            stash.frames.pop_front().unwrap_or((0.0, 0.0))
        } else {
            (0.0, 0.0)
        };
        if channels == 1 {
            frame[0] = T::from_sample((l + r) * 0.5);
            continue;
        }
        for (i, sample) in frame.iter_mut().enumerate() {
            *sample = T::from_sample(match i {
                0 => l,
                1 => r,
                _ => 0.0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_position_scales_with_speed() {
        assert_eq!(song_ns(0, 48_000, 1.0, 48_000), 1_000_000_000);
        assert_eq!(song_ns(500, 24_000, 2.0, 48_000), 1_000_000_500);
        assert_eq!(song_ns(0, 0, 0.5, 44_100), 0);
    }

    #[test]
    fn event_frame_inverts_song_position() {
        assert_eq!(frame_at(0, 1_000_000_000, 1.0, 48_000), 48_000);
        assert_eq!(frame_at(500, 1_000_000_500, 2.0, 48_000), 24_000);
        // Events from before the start (seek point) sound at frame 0.
        assert_eq!(frame_at(1_000, 10, 1.0, 48_000), 0);
        for frames in [0, 1, 255, 12_345] {
            let ns = song_ns(7, frames, 1.5, 44_100);
            assert_eq!(frame_at(7, ns, 1.5, 44_100), frames);
        }
    }

    #[test]
    fn buffer_length_in_frames() {
        assert_eq!(frames_for_secs(10, 48_000), 480_000);
    }

    #[test]
    fn restart_drops_buffered_audio() {
        let mut state = State {
            generation: 3,
            data: None,
            playing: true,
            base_ns: 0,
            speed: 1.0,
            max_frames: 10,
            ring: AudioQueue::from_pcm(&[(0.0, 0.0); 4]),
            played: 7,
            started: true,
            latency_frames: 0,
            stash_frames: 0,
            epoch: 0,
            heard_floor: 0,
            bend_override: None,
            resume_at: None,
            voice_marks: VecDeque::new(),
            drop_velocity: 0,
            limiter_bypassed: false,
            shutdown: false,
        };
        restart(&mut state);
        assert_eq!(state.generation, 4);
        assert_eq!(state.ring.frames(), 0);
        assert_eq!(state.played, 0);
    }

    fn numbered(played: u64, ring: usize) -> State {
        // Frame n holds (n, n) so skips can be checked by content.
        State {
            generation: 0,
            data: None,
            playing: true,
            base_ns: 0,
            speed: 1.0,
            max_frames: 100,
            ring: AudioQueue::from_pcm(
                &(played..played + ring as u64)
                    .map(|n| (n as f32, n as f32))
                    .collect::<Vec<_>>(),
            ),
            played,
            started: true,
            latency_frames: 0,
            stash_frames: 0,
            epoch: 0,
            heard_floor: 0,
            bend_override: None,
            resume_at: None,
            voice_marks: VecDeque::new(),
            drop_velocity: 0,
            limiter_bypassed: false,
            shutdown: false,
        }
    }

    fn shared_with(state: State) -> Shared {
        Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            rate: 48_000,
        }
    }

    #[test]
    fn callback_plays_from_stash_when_lock_is_busy() {
        let shared = shared_with(numbered(0, 10_000));
        // Leave the test thread's priority alone.
        let mut stash = Stash {
            boosted: true,
            ..Stash::default()
        };
        let mut out = vec![0.0f32; 480 * 2];
        let latency = std::time::Duration::ZERO;
        fill(&shared, &mut stash, &mut out, 2, latency);
        assert_eq!((out[0], out[958]), (0.0, 479.0));
        {
            let s = shared.lock();
            assert_eq!(s.played, STASH_FRAMES as u64);
            assert_eq!(s.stash_frames, (STASH_FRAMES - 480) as u64);
        }
        // Another thread holds the lock: playback continues seamlessly.
        let guard = shared.lock();
        fill(&shared, &mut stash, &mut out, 2, latency);
        assert_eq!((out[0], out[958]), (480.0, 959.0));
        drop(guard);
        // A restart drops the stash on the next locked callback.
        restart(&mut shared.lock());
        fill(&shared, &mut stash, &mut out, 2, latency);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn stash_is_not_heard_yet() {
        let mut s = numbered(10_000, 0);
        s.stash_frames = 4_000;
        s.latency_frames = 1_000;
        assert_eq!(heard_ns(&mut s, 48_000), song_ns(0, 5_000, 1.0, 48_000));
    }

    #[test]
    fn heard_position_ignores_latency_jitter() {
        let mut s = numbered(48_000, 0);
        s.latency_frames = 480;
        let first = heard_ns(&mut s, 48_000);
        // Latency estimate grows by more than playback advanced.
        s.played += 100;
        s.latency_frames = 960;
        assert_eq!(heard_ns(&mut s, 48_000), first);
        s.played += 1_000;
        assert!(heard_ns(&mut s, 48_000) > first);
        // A restart may move back.
        restart(&mut s);
        assert_eq!(heard_ns(&mut s, 48_000), 0);
    }

    #[test]
    fn skipped_notes_drop_with_their_note_offs() {
        let on = |key: u32| 0x91 | key << 8 | 100 << 16;
        let off = |key: u32| 0x81 | key << 8;
        let mut f = SkipFilter::new();
        // Held before the skip: its note-off must still pass.
        assert_eq!(f.filter(on(60), 0), Some(on(60)));
        // Pressed and released inside the skip: both dropped.
        assert_eq!(f.filter(on(62), 127), None);
        assert_eq!(f.filter(off(62), 127), None);
        // Pressed in the skip, released after it: the note-off is dropped too.
        assert_eq!(f.filter(on(64), 127), None);
        assert_eq!(f.filter(off(64), 0), None);
        // Velocity-0 note-on counts as a note-off.
        assert_eq!(f.filter(on(65), 127), None);
        assert_eq!(f.filter(0x91 | 65 << 8, 0), None);
        // Controllers and bends always pass; so does the held note's release.
        assert_eq!(f.filter(0xE1 | 64 << 16, 127), Some(0xE1 | 64 << 16));
        assert_eq!(f.filter(off(60), 127), Some(off(60)));
        assert_eq!(f.filter(off(64), 0), Some(off(64)));
    }

    #[test]
    fn thinning_ramps_from_2s_to_100ms() {
        assert_eq!(thin_velocity(5.0), 0);
        assert_eq!(thin_velocity(2.0), 1);
        assert_eq!(thin_velocity(1.05), 64);
        assert_eq!(thin_velocity(0.1), 127);
        assert_eq!(thin_velocity(0.0), 127);
        let mut f = SkipFilter::new();
        assert_eq!(f.filter(0x90 | 60 << 8 | 40 << 16, 50), None);
        assert_eq!(f.filter(0x80 | 60 << 8, 0), None);
        assert!(f.filter(0x90 | 61 << 8 | 60 << 16, 50).is_some());
    }

    #[test]
    fn bend_override_replaces_rpn0_range_only() {
        let cc = |ch: u32, c: u32, v: u32| 0xB0 | ch | c << 8 | v << 16;
        let mut o = BendOverride::new(Some(24));
        assert_eq!(o.initial().len(), 16 * 6);
        assert!(o.initial().contains(&cc(15, 6, 24)));
        // RPN 0 data entry on channel 3: forced to 24 semitones, 0 cents.
        o.rewrite(cc(3, 101, 0));
        o.rewrite(cc(3, 100, 0));
        assert_eq!(o.rewrite(cc(3, 6, 2)), cc(3, 6, 24));
        assert_eq!(o.rewrite(cc(3, 38, 50)), cc(3, 38, 0));
        // Other channels, other RPNs and other messages pass through.
        assert_eq!(o.rewrite(cc(4, 6, 2)), cc(4, 6, 2));
        o.rewrite(cc(3, 100, 1));
        assert_eq!(o.rewrite(cc(3, 6, 64)), cc(3, 6, 64));
        assert_eq!(
            o.rewrite(0x93 | 60 << 8 | 100 << 16),
            0x93 | 60 << 8 | 100 << 16
        );

        let mut off = BendOverride::new(None);
        assert!(off.initial().is_empty());
        off.rewrite(cc(0, 101, 0));
        off.rewrite(cc(0, 100, 0));
        assert_eq!(off.rewrite(cc(0, 6, 2)), cc(0, 6, 2));
    }

    #[test]
    fn polyphony_follows_heard_chunk() {
        let mut s = numbered(0, 0);
        s.voice_marks = VecDeque::from([(0, 3), (256, 5), (512, 2)]);
        assert_eq!(heard_polyphony(&mut s), 3);
        s.played = 300;
        assert_eq!(heard_polyphony(&mut s), 5);
        assert_eq!(s.voice_marks.len(), 2);
        s.latency_frames = 200;
        // Heard position never moves the marks back, but early frames read 0.
        restart(&mut s);
        assert_eq!(heard_polyphony(&mut s), 0);
    }

    #[test]
    fn forward_seek_skips_inside_buffer() {
        let mut s = numbered(50, 40);
        assert!(s.skip_to(70));
        assert_eq!(s.ring.front(), Some((70.0, 70.0)));
        assert_eq!(s.played, 70);
    }

    #[test]
    fn backward_or_outside_seek_rerenders() {
        let mut s = numbered(50, 40);
        assert!(!s.skip_to(49));
        assert!(!s.skip_to(90));
        assert_eq!((s.played, s.ring.frames()), (50, 40));
    }

    #[test]
    fn opus_roundtrip_is_aligned() {
        // Impulses over a quiet sine bed, pushed in render-sized chunks:
        // each impulse must come back within a few frames of where it
        // was pushed, which pins the codec delay table end to end.
        for rate in [8_000u32, 12_000, 16_000, 24_000, 48_000] {
            let mut queue = AudioQueue::new(rate);
            let total = (rate * 2) as usize;
            let mut pcm = Vec::with_capacity(total * 2);
            for i in 0..total {
                let bed =
                    (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.05;
                let impulse = if i > 0 && i % 4096 == 0 { 0.8 } else { 0.0 };
                pcm.extend([bed + impulse, bed + impulse]);
            }
            for chunk in pcm.chunks(256 * 2) {
                queue.push_interleaved(chunk);
            }
            assert_eq!(queue.frames(), total);
            // One more packet of silence flushes the codec delay, so
            // every pushed frame can be popped.
            queue.push_interleaved(&vec![0.0; (rate as usize / 50) * 2]);
            let mut out = Vec::with_capacity(total);
            for _ in 0..total {
                out.push(queue.pop_front().unwrap().0);
            }
            for i in (4096..total).step_by(4096) {
                let (mut best, mut best_at) = (0.0f32, i);
                for (j, &v) in out.iter().enumerate().take(i + 64).skip(i - 64) {
                    if v.abs() > best {
                        best = v.abs();
                        best_at = j;
                    }
                }
                assert!(
                    best > 0.15 && best_at.abs_diff(i) <= 6,
                    "rate {rate}: impulse at {i}, peak {best} at {best_at}"
                );
            }
        }
    }

    #[test]
    fn opus_skip_lands_on_the_right_segment() {
        let mut queue = AudioQueue::new(48_000);
        let mut pcm = Vec::new();
        for i in 0..9_600 {
            let freq = if i < 4_800 { 440.0 } else { 880.0 };
            let v = (i as f32 * freq * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
            pcm.extend([v, v]);
        }
        for chunk in pcm.chunks(256 * 2) {
            queue.push_interleaved(chunk);
        }
        assert_eq!(queue.frames(), 9_600);
        queue.skip_frames(4_800);
        assert_eq!(queue.frames(), 4_800);
        // Skip the landing transient, then count zero crossings of what
        // follows: 20 ms of the 880 Hz segment crosses about twice as
        // often as the 440 Hz one would.
        for _ in 0..480 {
            queue.pop_front();
        }
        let mut prev = queue.pop_front().unwrap().0;
        let mut crossings = 0;
        for _ in 0..960 {
            let v = queue.pop_front().unwrap().0;
            if (prev < 0.0) != (v < 0.0) {
                crossings += 1;
            }
            prev = v;
        }
        assert!(
            crossings >= 26,
            "expected the 880 Hz segment, {crossings} crossings"
        );
    }

    #[test]
    fn opus_buffer_uses_far_less_memory() {
        let mut queue = AudioQueue::new(48_000);
        let total = 480_000; // 10 s
        let mut pcm = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f32 / 48_000.0;
            let v = ((t * 220.0 * std::f32::consts::TAU).sin()
                + (t * 331.0 * std::f32::consts::TAU).sin() * 0.6)
                * 0.2;
            pcm.extend([v, v]);
        }
        for chunk in pcm.chunks(256 * 2) {
            queue.push_interleaved(chunk);
        }
        assert_eq!(queue.frames(), total);
        let pcm_bytes = total as u64 * BYTES_PER_FRAME;
        assert!(
            queue.memory_bytes() < pcm_bytes / 6,
            "{} bytes vs {pcm_bytes} raw",
            queue.memory_bytes()
        );
    }

    #[test]
    fn opus_skip_before_first_decode_consumes_exactly() {
        // Regression: skipping into the first packet of a fresh stream,
        // before anything was decoded, used to fall short by up to the
        // codec delay (the first fill yields packet_frames - delay),
        // tripping the debug assertion at the end of skip_frames.
        let mut queue = AudioQueue::new(48_000);
        let total = 1_920; // two packets
        let mut pcm = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f32 / 48_000.0;
            let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            pcm.extend([v, v]);
        }
        queue.push_interleaved(&pcm);
        queue.skip_frames(928);
        assert_eq!(queue.frames(), total - 928);
        assert!(queue.pop_front().is_some());
        assert_eq!(queue.frames(), total - 929);
    }

    #[test]
    fn opus_skip_into_the_unemerged_delay_tail() {
        // The store delivers pushed - delay frames until more audio is
        // encoded, but the queue counts pushed frames. Skipping into
        // that not-yet-emerged tail used to trip the assertion at the
        // end of skip_frames; the tail frames must instead be dropped
        // when they emerge.
        let mut queue = AudioQueue::new(48_000);
        let mut pcm = Vec::new();
        for i in 0..1_920 {
            let t = i as f32 / 48_000.0;
            let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            pcm.extend([v, v]);
        }
        queue.push_interleaved(&pcm);
        // Drain everything decodable: 1920 pushed - 120 delay.
        let mut popped = 0;
        while queue.pop_front().is_some() {
            popped += 1;
        }
        assert_eq!(popped, 1_800);
        assert_eq!(queue.frames(), 120);
        // Skip the phantom tail: no panic, counts stay exact.
        queue.skip_frames(120);
        assert_eq!(queue.frames(), 0);
        // The next packet's decode first yields the skipped tail
        // (dropped inside fill_window), then the new audio.
        queue.push_interleaved(&pcm[..960 * 2]);
        assert_eq!(queue.frames(), 960);
        let mut popped = 0;
        while queue.pop_front().is_some() {
            popped += 1;
        }
        assert_eq!(popped, 840);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn opus_visqol_series_scores_a_clean_signal() {
        let mut queue = AudioQueue::new(48_000);
        // Four seconds of a harmonic stack (a little more music-like
        // than a lone sine): enough for several 1 s sliding windows.
        let total = 48_000 * 4;
        let mut pcm = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f32 / 48_000.0;
            let v = (t * 220.0 * std::f32::consts::TAU).sin() * 0.3
                + (t * 440.0 * std::f32::consts::TAU).sin() * 0.2
                + (t * 880.0 * std::f32::consts::TAU).sin() * 0.1;
            pcm.extend([v, v]);
        }
        queue.push_interleaved(&pcm);
        // Scoring normally runs on the worker thread; run it inline.
        let mut series = Vec::new();
        for _ in 0..10 {
            if let Some(work) = queue.take_visqol_work() {
                if let Some(score) = work.score() {
                    queue.push_visqol_score(score, work.bitrate_kbps);
                }
            }
            series = queue.visqol_snapshot();
            if !series.is_empty() {
                break;
            }
        }
        assert_eq!(series.len(), 1, "series: {series:?}");
        assert!(
            series[0].0 > 3.5,
            "clean Opus round trip scored {}",
            series[0].0
        );
    }

    #[test]
    fn pcm_fallback_at_unsupported_rate_is_exact() {
        let mut queue = AudioQueue::new(44_100);
        queue.push_interleaved(&[0.5, -0.5, 0.25, -0.25]);
        assert_eq!(queue.frames(), 2);
        assert_eq!(queue.ready_frames(), 2);
        assert_eq!(queue.pop_front(), Some((0.5, -0.5)));
        assert_eq!(queue.pop_front(), Some((0.25, -0.25)));
        assert_eq!(queue.pop_front(), None);
    }
}

#[cfg(test)]
mod stress {
    use std::time::{Duration, Instant};

    #[test]
    #[ignore]
    fn player_polling_never_stalls() {
        let path = std::env::var("STRESS_MIDI").unwrap();
        let data = std::sync::Arc::new(
            kazumidiparser::MIDIPlayerData::parse(&path, 8, None, None).unwrap(),
        );
        let stream = crate::audio_engine::OutputStream::open_engine::<
            crate::audio_engine::synths::piano::PianoPrerender,
        >()
        .recv()
        .unwrap()
        .unwrap();
        let p = stream.prerender().unwrap();
        p.set_data(Some(data));
        let from_ns: u64 =
            std::env::var("STRESS_FROM_SECS").map_or(0, |v| v.parse().unwrap()) * 1_000_000_000;
        let secs: u64 = std::env::var("STRESS_SECS").map_or(20, |v| v.parse().unwrap());
        eprintln!("parsed");
        p.play(from_ns, 1.0);
        let start = Instant::now();
        let mut worst = Duration::ZERO;
        let mut last_sec = 0;
        while start.elapsed() < Duration::from_secs(secs) {
            let t = Instant::now();
            let pos = p.position_ns();
            let buf = p.buffered_ns();
            let poly = p.polyphony();
            let vel = p.drop_velocity();
            let dt = t.elapsed();
            if dt > worst {
                worst = dt;
                eprintln!(
                    "{:>6.2}s stall {dt:?} pos {pos} buf {buf} poly {poly} vel {vel}",
                    start.elapsed().as_secs_f64()
                );
            }
            if start.elapsed().as_secs() != last_sec {
                last_sec = start.elapsed().as_secs();
                eprintln!(
                    "t{last_sec:>3} pos {:.2}s buf {:.2}s poly {poly} vel {vel} poll {dt:?}",
                    pos as f64 / 1e9,
                    buf as f64 / 1e9
                );
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}
