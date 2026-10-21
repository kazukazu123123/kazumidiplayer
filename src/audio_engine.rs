use crate::kdmapi::{KDMAPIStream, get_kdmapi};

/// MIDI output backend the player sends events to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioEngine {
    #[default]
    Kdmapi,
}

impl AudioEngine {
    /// Every selectable engine, in the order shown in the settings combo box.
    pub const ALL: [AudioEngine; 1] = [AudioEngine::Kdmapi];
    /// Display names matching `ALL`.
    pub const LABELS: [&'static str; 1] = ["KDMAPI (OmniMIDI)"];

    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or_default()
    }

    /// Open an output stream, or `None` when the engine is unavailable.
    pub fn open(self) -> Option<KDMAPIStream> {
        match self {
            AudioEngine::Kdmapi => get_kdmapi()
                .filter(|k| k.is_kdmapi_available())
                .map(|k| k.open_stream()),
        }
    }
}
