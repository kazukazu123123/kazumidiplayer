//! Forwarding settings changes to the player. The settings overlay
//! edits `AppState.settings` in place, so while it is open the main
//! loop snapshots the values, lets the overlay draw, then diffs and
//! forwards whatever changed.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use crate::app_state::AppState;
use crate::audio_engine::{EngineSelection, EngineSettingValue, OutputStream};
use crate::player::PlayerHandle;

/// The settings as they were when the settings overlay opened this
/// frame, before its widgets edited them.
pub struct PrevSettings {
    pitch_bend_override_enabled: bool,
    vsync_enabled: bool,
    pitch_bend_multiplier: u8,
    unlimited_output_enabled: bool,
    s16_clipping_enabled: bool,
    midi_ev_buffer_size: u32,
    midi_ev_mode: usize,
    parse_threads: u32,
    audio_engine: usize,
    audio_buffer_secs: u32,
    engine_settings: HashMap<String, EngineSettingValue>,
    opus_bitrate_kbps: u32,
}

impl PrevSettings {
    pub fn snapshot(app_state: &AppState) -> Self {
        let settings = &app_state.settings;
        Self {
            pitch_bend_override_enabled: settings.pitch_bend_override_enabled,
            vsync_enabled: settings.vsync_enabled,
            pitch_bend_multiplier: settings.pitch_bend_multiplier,
            unlimited_output_enabled: settings.unlimited_output_enabled,
            s16_clipping_enabled: settings.s16_clipping_enabled,
            midi_ev_buffer_size: settings.midi_ev_buffer_size,
            midi_ev_mode: settings.midi_ev_mode,
            parse_threads: settings.parse_threads,
            audio_engine: settings.audio_engine,
            audio_buffer_secs: settings.audio_buffer_secs,
            engine_settings: settings.engine_settings.clone(),
            opus_bitrate_kbps: settings.opus_bitrate_kbps,
        }
    }
}

/// Diff the (possibly just-edited) settings against the snapshot and
/// forward every change to the player and the window. An engine
/// switch starts loading the new output into `pending_audio_engine`;
/// the current output keeps playing until it is ready.
pub fn apply_settings_changes(
    app_state: &mut AppState,
    prev: &PrevSettings,
    engine: &PlayerHandle,
    pending_audio_engine: &mut Option<(EngineSelection, Receiver<Result<OutputStream, String>>)>,
) {
    if app_state.settings.vsync_enabled != prev.vsync_enabled {
        // raylib switches GLFW's swap interval at runtime
        // through the window state flag.
        const FLAG_VSYNC_HINT: u32 = 0x0000_0040;
        unsafe {
            if app_state.settings.vsync_enabled {
                raylib::ffi::SetWindowState(FLAG_VSYNC_HINT);
            } else {
                raylib::ffi::ClearWindowState(FLAG_VSYNC_HINT);
            }
        }
    }
    if app_state.settings.pitch_bend_override_enabled != prev.pitch_bend_override_enabled {
        engine.set_pitch_bend_override(app_state.settings.pitch_bend_override_enabled);
    }
    if app_state.settings.pitch_bend_multiplier != prev.pitch_bend_multiplier {
        engine.set_pitch_bend_multiplier(app_state.settings.pitch_bend_multiplier);
    }
    if app_state.settings.unlimited_output_enabled != prev.unlimited_output_enabled {
        engine.set_unlimited_output(app_state.settings.unlimited_output_enabled);
    }
    if app_state.settings.s16_clipping_enabled != prev.s16_clipping_enabled {
        engine.set_s16_clipping(app_state.settings.s16_clipping_enabled);
    }
    if app_state.settings.midi_ev_buffer_size != prev.midi_ev_buffer_size {
        engine.set_midi_ev_buffer_size(app_state.settings.midi_ev_buffer_size);
    }
    if app_state.settings.midi_ev_mode != prev.midi_ev_mode {
        engine.set_legacy_midi_output(app_state.settings.midi_ev_mode == 0);
        engine.set_midi_ev_catch_up(app_state.settings.midi_ev_mode == 2);
    }
    if app_state.settings.audio_buffer_secs != prev.audio_buffer_secs {
        engine.set_audio_buffer_secs(app_state.settings.audio_buffer_secs);
    }
    for (key, value) in &app_state.settings.engine_settings {
        if prev.engine_settings.get(key) != Some(value) {
            engine.set_engine_setting(key, *value);
        }
    }
    if app_state.settings.opus_bitrate_kbps != prev.opus_bitrate_kbps {
        engine.set_opus_bitrate(app_state.settings.opus_bitrate_kbps);
    }
    if app_state.settings.parse_threads != prev.parse_threads {
        engine.set_parse_threads(app_state.settings.parse_threads);
    }
    if app_state.settings.audio_engine != prev.audio_engine {
        let selected =
            EngineSelection::from_index(app_state.settings.audio_engine);
        // Replacing a pending load drops it; the current output keeps
        // playing until the new one is ready.
        *pending_audio_engine = Some((selected, selected.start()));
        app_state.ui.audio_banner =
            Some((format!("Loading {}...", selected.label()), false));
        app_state.ui.audio_banner_timer = 0.0;
    }
}
