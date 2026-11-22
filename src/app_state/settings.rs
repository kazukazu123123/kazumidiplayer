use super::cpu_threads;
/// User-facing configuration: every value the settings overlay can
/// change, plus the engine's own published settings. Plain data only;
/// nothing here is persisted between runs.
pub struct Settings {
    /// Worker threads kazumidiparser uses to parse a file (1..=cpu_threads()).
    pub parse_threads: u32,
    pub show_stats: bool,
    pub vsync_enabled: bool,
    /// Pulses the piano-roll zoom in time with the MIDI tempo.
    pub beat_bounce_enabled: bool,
    /// Peak piano-roll zoom added at the start of a pulse, in percent (0..=25).
    pub beat_bounce_amount: u32,
    /// Rhythmic note value used for each pulse: 4 = quarter, 8 = eighth, etc.
    pub beat_bounce_note_value: u32,
    /// Shift each pulse by half of the selected note interval.
    pub beat_bounce_offbeat: bool,
    pub shake_enabled: bool,
    pub integrate_keyboard: bool,
    /// Show the piano roll as a frameless background layer filling the
    /// whole application window, with the other windows above it.
    pub piano_roll_fullscreen: bool,
    /// While the piano roll is fullscreen, pin the player controls
    /// window to the top of the application window.
    pub dock_controls_top: bool,
    pub autoplay_on_load: bool,
    pub pitch_bend_spring_parameters: [u32; 3],
    pub pitch_bend_move_window_enabled: bool,
    pub pitch_bend_window_enabled: bool,
    pub pitch_bend_override_enabled: bool,
    pub pitch_bend_multiplier: u8, // 1-127 (semitones)
    pub unlimited_output_enabled: bool,
    /// S16 Clipping: no limiter, wrap-around overflow instead
    /// (prerender engines only).
    pub s16_clipping_enabled: bool,
    pub midi_ev_buffer_size: u32,
    /// 0 = legacy/no buffer, 1 = buffered slowdown, 2 = buffered catch-up.
    pub midi_ev_mode: usize,
    /// Index into `AudioEngine::ALL`.
    pub audio_engine: usize,
    /// Longest prerendered audio buffer, in seconds.
    pub audio_buffer_secs: u32,
    /// Opus bitrate of the prerender buffer, in kbps.
    pub opus_bitrate_kbps: u32,
    /// Debug builds: whether the ViSQOL graph window is shown.
    #[cfg(debug_assertions)]
    pub visqol_window_enabled: bool,
    /// Built-in piano synth voice limit; 0 = unlimited.
    pub engine_settings: std::collections::HashMap<String, crate::audio_engine::EngineSettingValue>,
}

impl Settings {
    pub fn new() -> Self {
        Self {
            parse_threads: cpu_threads(),
            show_stats: true,
            vsync_enabled: true,
            beat_bounce_enabled: false,
            beat_bounce_amount: 8,
            beat_bounce_note_value: 4,
            beat_bounce_offbeat: false,
            shake_enabled: false,
            integrate_keyboard: false,
            piano_roll_fullscreen: false,
            dock_controls_top: false,
            autoplay_on_load: false,
            pitch_bend_spring_parameters: [1, 170, 26],
            pitch_bend_move_window_enabled: true,
            pitch_bend_window_enabled: true,
            pitch_bend_override_enabled: false,
            pitch_bend_multiplier: 12, // default 12 semitones
            unlimited_output_enabled: false,
            s16_clipping_enabled: false,
            midi_ev_buffer_size: crate::player::DEFAULT_MIDI_EV_BUFFER_SIZE,
            midi_ev_mode: 0,
            audio_engine: 0,
            audio_buffer_secs: crate::audio_engine::prerender::DEFAULT_MAX_BUFFER_SECS,
            opus_bitrate_kbps: 256,
            #[cfg(debug_assertions)]
            visqol_window_enabled: false,
            engine_settings: Default::default(),
        }
    }
}
