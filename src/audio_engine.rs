//! Pluggable MIDI output backends.
//!
//! Adding an engine: put a built-in synth in `audio_engine/synths/<name>.rs`,
//! implement [`AudioEngine`] in `audio_engine/<name>.rs`, then
//! list it in [`ENGINES`] with [`register`]. Opening off the UI thread,
//! queueing, discarding output for a previous file, ordering and serializing
//! calls are handled here; the engine never sees two calls at once.

mod csynth;
mod kdmapi;
mod pending;
pub mod prerender;
mod sine;
pub mod synths;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

/// How an engine produces audio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderMode {
    /// Plays each message as it is sent.
    #[default]
    Realtime,
    /// Renders the file ahead of playback into a buffer ([`prerender`]).
    /// Such engines implement [`AudioEngine::render`] and, for sample-accurate
    /// timing, [`AudioEngine::send_midi_event_at`].
    Prerender,
}

/// One MIDI output backend.
pub trait AudioEngine: Send + 'static {
    /// Name shown in the settings combo box.
    const LABEL: &'static str;

    /// How this engine renders. Defaults to [`RenderMode::Realtime`].
    const RENDER_MODE: RenderMode = RenderMode::Realtime;

    /// Open the engine. Runs on a background thread, so it may block
    /// (e.g. while samples download).
    fn open() -> Result<Self, String>
    where
        Self: Sized;

    /// Play one packed MIDI short message (`status | p1 << 8 | p2 << 16`).
    /// Usually called on the engine's worker thread; in legacy (blocking)
    /// mode on the player thread, after all earlier messages.
    fn send_midi_event(&mut self, data: u32) -> u32;

    /// Release the engine. Called once on the worker thread after the last
    /// message, once the output is no longer selected.
    fn close(&mut self);

    /// Schedule a message `frame` frames into the next `render` call.
    /// Prerender engines only; the default applies it at the start of it.
    fn send_midi_event_at(&mut self, frame: u32, data: u32) {
        let _ = frame;
        self.send_midi_event(data);
    }

    /// Output rate of [`render`](AudioEngine::render). Prerender engines only.
    fn sample_rate(&self) -> u32 {
        48_000
    }

    /// Render the next `left.len()` frames (always a multiple of
    /// [`prerender::CHUNK_FRAMES`]). Prerender engines only.
    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        left.fill(0.0);
        right.fill(0.0);
    }

    /// Voices sounding at the end of the last `render`, shown as the
    /// polyphony counter. Prerender engines only.
    fn polyphony(&self) -> u32 {
        0
    }
}

/// Type-erased registration record; build with [`register`].
pub struct EngineDef {
    label: &'static str,
    render_mode: RenderMode,
    open: fn() -> Result<SharedEngine, String>,
}

type SharedEngine = Arc<Mutex<dyn ErasedEngine>>;

pub const fn register<E: AudioEngine>() -> EngineDef {
    EngineDef {
        label: E::LABEL,
        render_mode: E::RENDER_MODE,
        open: || E::open().map(|e| Arc::new(Mutex::new(e)) as SharedEngine),
    }
}

/// Object-safe view of an [`AudioEngine`].
trait ErasedEngine: Send {
    fn send_midi_event(&mut self, data: u32) -> u32;
    fn close(&mut self);
    fn sample_rate(&self) -> u32;
    fn send_midi_event_at(&mut self, frame: u32, data: u32);
    fn render(&mut self, left: &mut [f32], right: &mut [f32]);
    fn polyphony(&self) -> u32;
}

impl<E: AudioEngine> ErasedEngine for E {
    fn send_midi_event(&mut self, data: u32) -> u32 {
        AudioEngine::send_midi_event(self, data)
    }

    fn close(&mut self) {
        AudioEngine::close(self)
    }

    fn sample_rate(&self) -> u32 {
        AudioEngine::sample_rate(self)
    }

    fn send_midi_event_at(&mut self, frame: u32, data: u32) {
        AudioEngine::send_midi_event_at(self, frame, data)
    }

    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        AudioEngine::render(self, left, right)
    }

    fn polyphony(&self) -> u32 {
        AudioEngine::polyphony(self)
    }
}

/// Every selectable engine, in combo box order. Index 0 is the default.
/// Append new engines: the saved setting is an index into this list.
pub const ENGINES: &[EngineDef] = &[
    register::<crate::kdmapi::KDMAPIStream>(),
    register::<synths::csynth::CSynthStream>(),
    register::<synths::csynth::CSynthPrerender>(),
    register::<synths::sine::SinePrerender>(),
];

const ENGINE_COUNT: usize = ENGINES.len();

/// Selected engine, as an index into [`ENGINES`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineSelection(usize);

impl EngineSelection {
    /// Display names matching `ENGINES`.
    pub const LABELS: [&'static str; ENGINE_COUNT] = {
        let mut labels = [""; ENGINE_COUNT];
        let mut i = 0;
        while i < ENGINE_COUNT {
            labels[i] = ENGINES[i].label;
            i += 1;
        }
        labels
    };

    pub fn from_index(index: usize) -> Self {
        if index < ENGINE_COUNT {
            Self(index)
        } else {
            Self::default()
        }
    }

    pub fn label(self) -> &'static str {
        ENGINES[self.0].label
    }

    pub fn render_mode(self) -> RenderMode {
        ENGINES[self.0].render_mode
    }

    pub fn start(self) -> Receiver<Result<OutputStream, String>> {
        OutputStream::open(ENGINES[self.0].open, ENGINES[self.0].render_mode)
    }
}

fn lock<'a>(
    engine: &'a Mutex<dyn ErasedEngine + 'static>,
) -> std::sync::MutexGuard<'a, dyn ErasedEngine + 'static> {
    engine.lock().unwrap_or_else(|e| e.into_inner())
}

/// An open engine the player sends packed MIDI short messages to.
/// Dropping it never waits, even if the engine is stuck in a call.
pub struct OutputStream {
    queue: pending::MidiSender,
    engine: SharedEngine,
    queued_since_flush: AtomicBool,
    /// Present for [`RenderMode::Prerender`] engines. MIDI sent through the
    /// `send_*` methods is then ignored: the session renders the file itself.
    prerender: Option<prerender::Session>,
}

impl OutputStream {
    fn open(
        open: fn() -> Result<SharedEngine, String>,
        mode: RenderMode,
    ) -> Receiver<Result<OutputStream, String>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let engine = match open() {
                Ok(engine) => engine,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let prerender = match mode {
                RenderMode::Realtime => None,
                RenderMode::Prerender => {
                    let rate = lock(&engine).sample_rate();
                    let render_engine = engine.clone();
                    let render: prerender::RenderFn = Box::new(move |msgs, left, right| {
                        let mut engine = lock(&render_engine);
                        for &(frame, msg) in msgs {
                            engine.send_midi_event_at(frame, msg);
                        }
                        engine.render(left, right);
                        engine.polyphony()
                    });
                    match prerender::Session::start(rate, render) {
                        Ok(session) => Some(session),
                        Err(e) => {
                            lock(&engine).close();
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
            };
            let (queue, input) = pending::channel();
            let stream = OutputStream {
                queue,
                engine: engine.clone(),
                queued_since_flush: AtomicBool::new(false),
                prerender,
            };
            if tx.send(Ok(stream)).is_err() {
                lock(&engine).close();
                return;
            }
            // This thread becomes the worker. It holds the lock across a burst
            // of queued messages instead of taking it per message, and lets go
            // when the queue runs dry or a blocking send is waiting.
            let mut guard = None;
            loop {
                match input.next(guard.is_none()) {
                    pending::Next::Data(data) => {
                        guard
                            .get_or_insert_with(|| lock(&engine))
                            .send_midi_event(data);
                    }
                    pending::Next::Flush(done) => {
                        guard = None;
                        let _ = done.send(());
                    }
                    pending::Next::Empty => guard = None,
                    pending::Next::Closed => break,
                }
            }
            drop(guard);
            lock(&engine).close();
        });
        rx
    }

    #[cfg(test)]
    pub(crate) fn open_engine<E: AudioEngine>() -> Receiver<Result<OutputStream, String>> {
        Self::open(register::<E>().open, E::RENDER_MODE)
    }

    /// Queue a message without blocking the caller.
    pub fn send_direct_data(&self, data: u32) -> u32 {
        if self.prerender.is_some() {
            return 0;
        }
        self.queued_since_flush.store(true, Ordering::Relaxed);
        self.queue.send(data);
        0
    }

    /// Send on the caller's thread, in order with earlier queued messages.
    pub fn send_direct_data_blocking(&mut self, data: u32) -> u32 {
        if self.prerender.is_some() {
            return 0;
        }
        if *self.queued_since_flush.get_mut() {
            if !self.queue.flush() {
                return 0;
            }
            *self.queued_since_flush.get_mut() = false;
        }
        lock(&self.engine).send_midi_event(data)
    }

    /// The prerender session, for [`RenderMode::Prerender`] engines.
    pub fn prerender(&self) -> Option<&prerender::Session> {
        self.prerender.as_ref()
    }

    /// Make output already queued for the previous file obsolete.
    pub fn invalidate_pending(&self) {
        self.queue.invalidate();
    }
}
