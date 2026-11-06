use super::synths::piano::PianoPrerender;
use super::{AudioEngine, RenderMode};

impl AudioEngine for PianoPrerender {
    const LABEL: &'static str = "Piano (Prerender)";
    const RENDER_MODE: RenderMode = RenderMode::Prerender;

    fn open() -> Result<Self, String> {
        PianoPrerender::open()
    }

    fn send_midi_event(&mut self, data: u32) -> u32 {
        self.send_at(0, data);
        0
    }

    fn send_midi_event_at(&mut self, frame: u32, data: u32) {
        self.send_at(frame, data);
    }

    fn close(&mut self) {}

    fn sample_rate(&self) -> u32 {
        self.rate()
    }

    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        PianoPrerender::render(self, left, right);
    }

    fn polyphony(&self) -> u32 {
        PianoPrerender::polyphony(self)
    }
}
