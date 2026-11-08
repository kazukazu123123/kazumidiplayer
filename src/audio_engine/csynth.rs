use super::AudioEngine;
use super::synths::csynth::CSynthStream;

impl AudioEngine for CSynthStream {
    const LABEL: &'static str = "CSynth (MPP Piano)";

    /// Loads the samples, possibly downloading them first.
    fn open() -> Result<Self, String> {
        CSynthStream::spawn()
            .recv()
            .map_err(|_| "CSynth output thread ended".to_string())?
    }

    fn send_midi_event(&mut self, data: u32) -> u32 {
        self.send(data);
        0
    }

    /// Dropping the stream stops audio; nothing else to release.
    fn close(&mut self) {}
}

use super::RenderMode;
use super::prerender::CHUNK_FRAMES;
use super::synths::csynth::CSynthPrerender;

const _: () = assert!(CHUNK_FRAMES % csynth::QUANTUM == 0);

impl AudioEngine for CSynthPrerender {
    const LABEL: &'static str = "CSynth (MPP Piano, Prerender)";
    const RENDER_MODE: RenderMode = RenderMode::Prerender;

    fn open() -> Result<Self, String> {
        CSynthPrerender::load()
    }

    fn send_midi_event(&mut self, data: u32) -> u32 {
        self.send(data);
        0
    }

    fn close(&mut self) {}

    fn send_midi_event_at(&mut self, frame: u32, data: u32) {
        self.send_at(frame, data);
    }

    fn sample_rate(&self) -> u32 {
        self.rate()
    }

    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        CSynthPrerender::render(self, left, right);
    }

    fn polyphony(&self) -> u32 {
        CSynthPrerender::polyphony(self)
    }

    fn set_limiter_bypassed(&mut self, bypassed: bool) {
        CSynthPrerender::set_limiter_bypassed(self, bypassed);
    }
}
