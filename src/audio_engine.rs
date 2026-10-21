//! Pluggable MIDI output backends.
//!
//! Adding an engine: implement [`AudioEngine`] in `audio_engine/<name>.rs`, then
//! list it in [`ENGINES`] with [`register`]. Opening off the UI thread,
//! queueing, discarding output for a previous file, ordering and serializing
//! calls are handled here; the engine never sees two calls at once.

mod kdmapi;
mod pending;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

/// One MIDI output backend.
pub trait AudioEngine: Send + 'static {
    /// Name shown in the settings combo box.
    const LABEL: &'static str;

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
}

/// Type-erased registration record; build with [`register`].
pub struct EngineDef {
    label: &'static str,
    open: fn() -> Result<SharedEngine, String>,
}

type SharedEngine = Arc<Mutex<dyn ErasedEngine>>;

pub const fn register<E: AudioEngine>() -> EngineDef {
    EngineDef {
        label: E::LABEL,
        open: || E::open().map(|e| Arc::new(Mutex::new(e)) as SharedEngine),
    }
}

/// Object-safe view of an [`AudioEngine`].
trait ErasedEngine: Send {
    fn send_midi_event(&mut self, data: u32) -> u32;
    fn close(&mut self);
}

impl<E: AudioEngine> ErasedEngine for E {
    fn send_midi_event(&mut self, data: u32) -> u32 {
        AudioEngine::send_midi_event(self, data)
    }

    fn close(&mut self) {
        AudioEngine::close(self)
    }
}

/// Every selectable engine, in combo box order. Index 0 is the default.
/// Append new engines: the saved setting is an index into this list.
pub const ENGINES: &[EngineDef] = &[
    register::<crate::kdmapi::KDMAPIStream>(),
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
        if index < ENGINE_COUNT { Self(index) } else { Self::default() }
    }

    pub fn label(self) -> &'static str {
        ENGINES[self.0].label
    }

    pub fn start(self) -> Receiver<Result<OutputStream, String>> {
        OutputStream::open(ENGINES[self.0].open)
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
}

impl OutputStream {
    fn open(open: fn() -> Result<SharedEngine, String>) -> Receiver<Result<OutputStream, String>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let engine = match open() {
                Ok(engine) => engine,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let (queue, input) = pending::channel();
            let stream = OutputStream {
                queue,
                engine: engine.clone(),
                queued_since_flush: AtomicBool::new(false),
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
        Self::open(register::<E>().open)
    }

    /// Queue a message without blocking the caller.
    pub fn send_direct_data(&self, data: u32) -> u32 {
        self.queued_since_flush.store(true, Ordering::Relaxed);
        self.queue.send(data);
        0
    }

    /// Send on the caller's thread, in order with earlier queued messages.
    pub fn send_direct_data_blocking(&mut self, data: u32) -> u32 {
        if *self.queued_since_flush.get_mut() {
            if !self.queue.flush() {
                return 0;
            }
            *self.queued_since_flush.get_mut() = false;
        }
        lock(&self.engine).send_midi_event(data)
    }

    /// Make output already queued for the previous file obsolete.
    pub fn invalidate_pending(&self) {
        self.queue.invalidate();
    }
}
