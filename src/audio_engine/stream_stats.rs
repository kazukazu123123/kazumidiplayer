//! Stats a realtime engine's audio callback publishes for its handle.

use std::sync::atomic::{AtomicU32, Ordering};

/// Voices sounding after the last rendered block, and the smoothed
/// share of one block's playback time its rendering took
/// (1.0 = the whole block, i.e. 100%). Written by the audio callback
/// and read by the engine handle; both values are plain atomics, so
/// neither side ever locks.
#[derive(Default)]
pub struct StreamStats {
    polyphony: AtomicU32,
    load: AtomicU32,
}

impl StreamStats {
    /// Stores the latest figures (the audio callback side).
    pub fn publish(&self, polyphony: u32, load: f32) {
        self.polyphony.store(polyphony, Ordering::Relaxed);
        self.load.store(load.to_bits(), Ordering::Relaxed);
    }

    /// Voices sounding after the last rendered block.
    pub fn polyphony(&self) -> u32 {
        self.polyphony.load(Ordering::Relaxed)
    }

    /// Smoothed share of one block's playback time spent rendering it.
    pub fn load(&self) -> f32 {
        f32::from_bits(self.load.load(Ordering::Relaxed))
    }
}
