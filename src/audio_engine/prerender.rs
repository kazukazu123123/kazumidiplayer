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

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::kazumidiparser_cursor::{
    TrackCursor, cursor_next, ns_to_tick, seek_track_cursor, tick_to_ns,
};

/// Frames rendered per step. Events carry their offset within the step.
pub const CHUNK_FRAMES: usize = 256;

pub const DEFAULT_MAX_BUFFER_SECS: u32 = 10;
pub const MIN_MAX_BUFFER_SECS: u32 = 1;
pub const MAX_MAX_BUFFER_SECS: u32 = 60;

/// KazuLimiter settings, applied to every prerender engine's output.
const LIMITER_THRESHOLD_DB: f32 = 0.0;
const LIMITER_RELEASE_MS: f32 = 100.0;
const LIMITER_LOOKAHEAD_MS: f32 = 50.0;

/// Schedules the `(frame offset, message)` pairs, then renders one chunk.
pub type RenderFn = Box<dyn FnMut(&[(u32, u32)], &mut [f32], &mut [f32]) + Send>;

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
    /// Rendered, not yet played (left, right).
    ring: VecDeque<(f32, f32)>,
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
        if ahead as usize >= self.ring.len() {
            return false;
        }
        self.ring.drain(..ahead as usize);
        self.played = target;
        self.epoch += 1;
        self.stash_frames = 0;
        true
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
                ring: VecDeque::new(),
                played: 0,
                started: false,
                latency_frames: 0,
                stash_frames: 0,
                epoch: 0,
                heard_floor: 0,
                bend_override: None,
                resume_at: Some(0),
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
            if s.ring.len() > s.max_frames {
                s.base_ns = heard_ns(s, rate);
                restart(s);
            }
        });
    }

    /// Song position of the audio being heard.
    pub fn position_ns(&self) -> u64 {
        heard_ns(&mut self.shared.lock(), self.shared.rate)
    }

    /// Rendered audio waiting to be played, in wall-clock nanoseconds.
    pub fn buffered_ns(&self) -> u64 {
        let frames = self.shared.lock().ring.len() as u64;
        frames * 1_000_000_000 / self.shared.rate as u64
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
    /// in (tick, track) order.
    fn collect_until(&mut self, end_ns: u64, out: &mut Vec<(u64, u32)>) {
        let raw = unsafe { &*self.data.as_ptr() };
        let end_tick = ns_to_tick(raw, end_ns);
        let mut events = Vec::new();
        for (index, cursor) in self.cursors.iter_mut().enumerate() {
            while cursor.has_event
                && cursor.abs_tick <= end_tick
                && tick_to_ns(raw, cursor.abs_tick) < end_ns
            {
                if !cursor.is_tempo {
                    let msg = cursor.status as u32
                        | (cursor.param1 as u32) << 8
                        | (cursor.param2 as u32) << 16;
                    events.push((cursor.abs_tick, index, msg));
                }
                if !unsafe { cursor_next(cursor) } {
                    cursor.has_event = false;
                }
            }
        }
        events.sort_by_key(|&(tick, index, _)| (tick, index));
        out.extend(
            events
                .into_iter()
                .map(|(tick, _, msg)| (tick_to_ns(raw, tick), msg)),
        );
    }
}

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

    fn filter(&mut self, msg: u32, skipping: bool) -> Option<u32> {
        let status = msg & 0xFF;
        let key = (status as usize & 0x0F) * 128 + ((msg >> 8) & 0x7F) as usize;
        let velocity = (msg >> 16) & 0x7F;
        match status & 0xF0 {
            0x90 if velocity > 0 && skipping => {
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
                    msgs = reset_messages()
                        .into_iter()
                        .chain(bend_override.initial())
                        .map(|m| (0, m))
                        .collect();
                }
                // Also prefill while paused, so play starts from a full buffer.
                if tracks.is_some() && s.ring.len() < s.max_frames {
                    break;
                }
                s = shared.wake.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            let fill = s.ring.len() as f64 / s.max_frames.max(1) as f64;
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
            let skip_ns = song_ns(base_ns, played, speed, rate);
            if let Some(tracks) = tracks.as_mut() {
                tracks.collect_until(skip_ns, &mut pending);
            }
            for &(_, msg) in &pending {
                if let Some(msg) = skip.filter(msg, true) {
                    msgs.push((0, bend_override.rewrite(msg)));
                }
            }
            pending.clear();
            rendered = played;
        }

        let end_ns = song_ns(base_ns, rendered + CHUNK_FRAMES as u64, speed, rate);
        if let Some(tracks) = tracks.as_mut() {
            tracks.collect_until(end_ns, &mut pending);
        }
        for &(ns, msg) in &pending {
            let Some(msg) = skip.filter(msg, false) else {
                continue;
            };
            let frame = frame_at(base_ns, ns, speed, rate)
                .saturating_sub(rendered)
                .min(CHUNK_FRAMES as u64 - 1);
            msgs.push((frame as u32, bend_override.rewrite(msg)));
        }
        pending.clear();
        render(&msgs, &mut left, &mut right);
        msgs.clear();
        interleaved.clear();
        interleaved.extend(left.iter().zip(&right).flat_map(|(&l, &r)| [l, r]));
        limiter.process(&mut interleaved);
        let chunk_start = rendered;
        rendered += CHUNK_FRAMES as u64;

        let mut s = shared.lock();
        if s.generation == generation {
            // Frames playback already passed while this chunk rendered.
            let late = (s.played.saturating_sub(chunk_start) as usize).min(CHUNK_FRAMES);
            s.ring.extend(
                interleaved[late * 2..]
                    .chunks_exact(2)
                    .map(|lr| (lr[0], lr[1])),
            );
            s.started = true;
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
            let moved = want.min(s.ring.len());
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
            ring: VecDeque::from([(0.0, 0.0); 4]),
            played: 7,
            started: true,
            latency_frames: 0,
            stash_frames: 0,
            epoch: 0,
            heard_floor: 0,
            bend_override: None,
            resume_at: None,
            shutdown: false,
        };
        restart(&mut state);
        assert_eq!(state.generation, 4);
        assert!(state.ring.is_empty());
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
            ring: (played..played + ring as u64)
                .map(|n| (n as f32, n as f32))
                .collect(),
            played,
            started: true,
            latency_frames: 0,
            stash_frames: 0,
            epoch: 0,
            heard_floor: 0,
            bend_override: None,
            resume_at: None,
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
        assert_eq!(f.filter(on(60), false), Some(on(60)));
        // Pressed and released inside the skip: both dropped.
        assert_eq!(f.filter(on(62), true), None);
        assert_eq!(f.filter(off(62), true), None);
        // Pressed in the skip, released after it: the note-off is dropped too.
        assert_eq!(f.filter(on(64), true), None);
        assert_eq!(f.filter(off(64), false), None);
        // Velocity-0 note-on counts as a note-off.
        assert_eq!(f.filter(on(65), true), None);
        assert_eq!(f.filter(0x91 | 65 << 8, false), None);
        // Controllers and bends always pass; so does the held note's release.
        assert_eq!(f.filter(0xE1 | 64 << 16, true), Some(0xE1 | 64 << 16));
        assert_eq!(f.filter(off(60), true), Some(off(60)));
        assert_eq!(f.filter(off(64), false), Some(off(64)));
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
    fn forward_seek_skips_inside_buffer() {
        let mut s = numbered(50, 40);
        assert!(s.skip_to(70));
        assert_eq!(s.ring.front(), Some(&(70.0, 70.0)));
        assert_eq!(s.played, 70);
    }

    #[test]
    fn backward_or_outside_seek_rerenders() {
        let mut s = numbered(50, 40);
        assert!(!s.skip_to(49));
        assert!(!s.skip_to(90));
        assert_eq!((s.played, s.ring.len()), (50, 40));
    }
}
