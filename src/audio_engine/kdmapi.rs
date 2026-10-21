use super::AudioEngine;
use crate::kdmapi::{KDMAPIStream, get_kdmapi};

impl AudioEngine for KDMAPIStream {
    const LABEL: &'static str = "KDMAPI (OmniMIDI)";

    fn open() -> Result<Self, String> {
        get_kdmapi()
            .filter(|k| k.is_kdmapi_available())
            .ok_or_else(|| "KDMAPI (OmniMIDI) not found".to_string())?
            .open_stream()
            .ok_or_else(|| "Failed to open KDMAPI stream".to_string())
    }

    fn send_midi_event(&mut self, data: u32) -> u32 {
        self.send(data)
    }

    fn close(&mut self) {
        KDMAPIStream::close(self);
    }
}
