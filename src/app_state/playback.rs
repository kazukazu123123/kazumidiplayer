use crate::midi_parser_worker::ParseProgress;
use crate::player::PlayerState;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::ParseToast;
/// The playback session: the loaded song, position and tempo
/// bookkeeping, and the parse progress shown while a file loads.
pub struct Playback {
    pub current_progress: Option<ParseProgress>,
    /// Ring of `(instant, bytes_done, events_done)` parse-progress snapshots kept for a
    /// ~0.5s window, so the parse rates are moving averages rather than jumpy deltas.
    pub parse_rate_samples: VecDeque<(Instant, u64, u64)>,
    /// Most recent parse speed in events per second, shown in the loading overlay.
    pub parse_eps: f64,
    /// Most recent parse speed in bytes per second, shown in the loading overlay.
    pub parse_bps: f64,
    pub is_dragging_seek: bool,
    pub last_state: PlayerState,
    pub is_loading: bool,
    /// Present only while the finished loading panel is animating out.
    pub parse_toast: Option<ParseToast>,
    pub is_tick_based: bool,
    /// Piano-roll scroll speed multiplier (higher = notes move faster, shorter
    /// visible window). Adjusted with the mouse wheel over the piano roll.
    pub note_speed: f32,
    /// Playback speed multiplier (0.05 to 4.0, slider-controlled; 1.0 = normal).
    pub playback_speed: f32,
    pub stats_clock: f32,
    pub nps_samples: VecDeque<(f32, u64)>,
    pub nps_value: f32,
    pub tempo_us_value: u32,
    pub tick_map: Option<Vec<(u64, u32, u64)>>,
    pub midi_data: Option<Arc<kazumidiparser::MIDIPlayerData>>,
    pub last_seek_sent_ns: Option<u64>,
}

impl Playback {
    pub fn new() -> Self {
        Self {
            current_progress: None,
            parse_rate_samples: VecDeque::new(),
            parse_eps: 0.0,
            parse_bps: 0.0,
            is_dragging_seek: false,
            last_state: PlayerState::Stopped,
            is_loading: false,
            parse_toast: None,
            is_tick_based: false,
            note_speed: 1.0,
            playback_speed: 1.0,
            stats_clock: 0.0,
            nps_samples: VecDeque::new(),
            nps_value: 0.0,
            tempo_us_value: 500_000,
            tick_map: None,
            midi_data: None,
            last_seek_sent_ns: None,
        }
    }
}
