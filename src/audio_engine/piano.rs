use super::synths::piano::{PianoPrerender, PianoStream, THREADS_SETTING, VOICE_LIMIT_SETTING};
use super::{AudioEngine, EngineSettingDef, EngineSettingValue, RenderMode};

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

    fn polyphony(&self) -> Option<u32> {
        Some(PianoPrerender::polyphony(self))
    }

    const SETTINGS: &'static [EngineSettingDef] = &[VOICE_LIMIT_SETTING, THREADS_SETTING];

    fn set_setting(&mut self, key: &str, value: EngineSettingValue) {
        if key == VOICE_LIMIT_SETTING.key
            && let EngineSettingValue::U32(limit) = value
        {
            PianoPrerender::set_voice_limit(self, limit);
        }
        if key == THREADS_SETTING.key
            && let EngineSettingValue::U32(threads) = value
        {
            super::prerender::set_synth_threads(threads);
        }
    }
}

impl AudioEngine for PianoStream {
    const LABEL: &'static str = "Piano";

    fn open() -> Result<Self, String> {
        PianoStream::spawn()
            .recv()
            .map_err(|_| "Piano output thread ended".to_string())?
    }

    fn send_midi_event(&mut self, data: u32) -> u32 {
        self.send(data);
        0
    }

    /// Dropping the stream stops audio; nothing else to release.
    fn close(&mut self) {}

    const SETTINGS: &'static [EngineSettingDef] = &[VOICE_LIMIT_SETTING, THREADS_SETTING];

    fn set_setting(&mut self, key: &str, value: EngineSettingValue) {
        if key == VOICE_LIMIT_SETTING.key
            && let EngineSettingValue::U32(limit) = value
        {
            PianoStream::set_voice_limit(self, limit);
        }
        if key == THREADS_SETTING.key
            && let EngineSettingValue::U32(threads) = value
        {
            super::prerender::set_synth_threads(threads);
        }
    }

    fn set_limiter_bypassed(&mut self, bypassed: bool) {
        PianoStream::set_limiter_bypassed(self, bypassed);
    }

    fn polyphony(&self) -> Option<u32> {
        Some(PianoStream::polyphony(self))
    }

    fn render_load(&self) -> Option<f32> {
        Some(PianoStream::render_load(self))
    }
}
