use crate::blur_requester::BlurRequester;
use crate::midi_parser_worker::ParseProgress;
use crate::overlay_manager::OverlayManager;
use crate::player::{PlayerCommand, PlayerFeedback, PlayerHandle, PlayerState};
use crate::shaking::ShakeState;
use crate::ui_components::piano_keyboard;
use crate::utils::get_note_color_by_track_channel;
use crate::window_manager::{self, InputEvent};
use raylib::prelude::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;

/// Hardware thread count: both the parse-threads default and the slider's maximum.
/// Oversubscribing past this only adds context switching, so it is the ceiling.
pub fn cpu_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

/// The loading panel's exit animation: once a parse finishes, the panel keeps its final
/// numbers, swings out to the bottom-right corner, lingers, then fades away.
pub struct ParseToast {
    pub elapsed: f32,
    /// The last progress snapshot of the finished parse, so the toast keeps showing the
    /// file's real totals after `current_progress` has been cleared.
    pub progress: Option<ParseProgress>,
    pub parse_bps: f64,
    pub parse_eps: f64,
}

impl ParseToast {
    /// Swing from center to corner. Long enough that the overshoot and settle are actually
    /// visible: below ~0.25s the bounce passes in a couple of frames and reads as a jump.
    pub const FLY: f32 = 0.38;
    /// Sit in the corner.
    pub const HOLD: f32 = 2.0;
    /// Fade out.
    pub const FADE: f32 = 0.4;

    pub fn total() -> f32 {
        Self::FLY + Self::HOLD + Self::FADE
    }

    /// 0.0 at the center, 1.0 once parked in the corner.
    pub fn fly_t(&self) -> f32 {
        (self.elapsed / Self::FLY).clamp(0.0, 1.0)
    }

    pub fn alpha(&self) -> f32 {
        let fade_start = Self::FLY + Self::HOLD;
        if self.elapsed <= fade_start {
            1.0
        } else {
            (1.0 - (self.elapsed - fade_start) / Self::FADE).clamp(0.0, 1.0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppOverlay {
    Settings,
    Loading,
    TextEditor,
    /// The player thread died. Nothing the app can do will work from here, so this
    /// one is terminal: it is never closed, and it swallows all input.
    PlayerCrashed,
}

pub struct AppState {
    // Player State
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
    /// Debug: artificial piano-roll scan delay in milliseconds (slider-controlled).
    pub scan_delay_ms: u32,
    /// Playback speed multiplier (0.05 to 4.0, slider-controlled; 1.0 = normal).
    pub playback_speed: f32,
    /// Worker threads kazumidiparser uses to parse a file (1..=cpu_threads()).
    pub parse_threads: u32,

    // Statistics overlay
    pub show_stats: bool,
    pub show_stats_animation: f32,
    pub stats_clock: f32,
    pub nps_samples: VecDeque<(f32, u64)>,
    pub nps_value: f32,
    pub tempo_us_value: u32,
    pub tick_map: Option<Vec<(u64, u32, u64)>>,
    pub midi_data: Option<Arc<kazumidiparser::MIDIPlayerData>>,
    pub last_seek_sent_ns: Option<u64>,

    // Animation
    pub tick_based_animation: f32,
    /// Pulses the piano-roll zoom in time with the MIDI tempo.
    pub beat_bounce_enabled: bool,
    pub beat_bounce_animation: f32,
    /// Peak piano-roll zoom added at the start of a pulse, in percent (0..=25).
    pub beat_bounce_amount: u32,
    /// Rhythmic note value used for each pulse: 4 = quarter, 8 = eighth, etc.
    pub beat_bounce_note_value: u32,
    /// Shift each pulse by half of the selected note interval.
    pub beat_bounce_offbeat: bool,
    pub beat_bounce_offbeat_animation: f32,
    pub shake_enabled: bool,
    pub shake_animation: f32,
    pub shake_state: ShakeState,
    pub integrate_keyboard: bool,
    pub keyboard_animation: f32,
    pub autoplay_on_load: bool,
    pub autoplay_animation: f32,
    pub pitch_bend_spring_parameters: [u32; 3],
    pub pitch_bend_spring_sliders: [crate::ui_components::numeric_slider::NumericSliderState; 3],
    pub pitch_bend_move_window_enabled: bool,
    pub pitch_bend_move_window_animation: f32,
    pub pitch_bend_window_enabled: bool,
    pub pitch_bend_window_animation: f32,
    pub pitch_bend_override_enabled: bool,
    pub pitch_bend_override_animation: f32,
    pub pitch_bend_multiplier: u8, // 1-127 (semitones)
    pub unlimited_output_enabled: bool,
    pub unlimited_output_animation: f32,
    pub midi_ev_buffer_size: u32,
    /// 0 = legacy/no buffer, 1 = buffered slowdown, 2 = buffered catch-up.
    pub midi_ev_mode: usize,

    // UI (non-editor)
    pub last_input_event: Option<InputEvent>,
    pub settings_scroll_y: f32,
    pub settings_scrollbar_dragging: bool,
    pub settings_scrollbar_drag_start_y: f32,
    pub settings_scrollbar_drag_start_scroll: f32,
    pub settings_pb_multiplier_slider: crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_beat_bounce_amount_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_beat_bounce_note_value_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_midi_ev_buffer_size_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_midi_ev_mode_combo: crate::ui_components::combo_box::ComboBoxState,
    pub settings_scan_delay_slider_dragging: bool,
    pub settings_parse_threads_slider_dragging: bool,
    pub speed_slider_dragging: bool,

    // Editor
    pub editor_text: String,
    pub editor_scroll_y: f32,
    pub editor_scrollbar_dragging: bool,
    pub editor_scrollbar_drag_start_y: f32,
    pub editor_scrollbar_drag_start_scroll: f32,
    pub editor_focused: bool,
    pub editor_cursor_pos: usize,
    pub editor_cursor_blink_timer: f32,
    pub editor_cursor_visible: bool,

    // Systems
    pub blur_requester: BlurRequester,
    pub overlay_manager: OverlayManager<AppOverlay>,
    pub kdmapi_missing: bool,
    pub kdmapi_missing_timer: f32,
}

pub const OVERLAY_FADE_DURATION: f32 = 0.2;
const BASE_TEMPO: u64 = 500_000;

fn calculate_tick_map(data: &kazumidiparser::MIDIPlayerData) -> Vec<(u64, u32, u64)> {
    let mut tick_map = Vec::new();
    let mut current_weighted_time = 0u64;
    let mut last_time = 0u64;
    let mut last_tempo = 500_000u32;

    // Safety: data.as_ptr() points to valid KazuMIDIParserData allocated by C FFI.
    // The tempo arrays (tempo_tick, tempo_ns, tempo_val) are valid for tempo_count elements.
    unsafe {
        let raw = &*data.as_ptr();
        if raw.tempo_count == 0 || *raw.tempo_tick.add(0) > 0 {
            tick_map.push((0, 500_000, 0));
        }

        for i in 0..raw.tempo_count {
            let time = *raw.tempo_ns.add(i);
            let tempo = *raw.tempo_val.add(i);

            let duration = time.saturating_sub(last_time);
            let scale = BASE_TEMPO as f64 / last_tempo as f64;
            let weighted_duration = (duration as f64 * scale) as u64;

            current_weighted_time += weighted_duration;
            tick_map.push((time, tempo, current_weighted_time));

            last_time = time;
            last_tempo = tempo;
        }
    }
    tick_map
}

impl AppState {
    pub fn new() -> Self {
        Self {
            // Player State
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
            scan_delay_ms: 0,
            playback_speed: 1.0,
            parse_threads: cpu_threads(),
            show_stats: true,
            show_stats_animation: 0.0,
            stats_clock: 0.0,
            nps_samples: VecDeque::new(),
            nps_value: 0.0,
            tempo_us_value: 500_000,
            tick_map: None,
            midi_data: None,
            last_seek_sent_ns: None,

            // Animation
            tick_based_animation: 0.0,
            beat_bounce_enabled: false,
            beat_bounce_animation: 0.0,
            beat_bounce_amount: 8,
            beat_bounce_note_value: 4,
            beat_bounce_offbeat: false,
            beat_bounce_offbeat_animation: 0.0,
            shake_enabled: false,
            shake_animation: 0.0,
            shake_state: ShakeState::new(),
            integrate_keyboard: false,
            keyboard_animation: 0.0,
            autoplay_on_load: false,
            autoplay_animation: 0.0,
            pitch_bend_spring_parameters: [1, 170, 26],
            pitch_bend_spring_sliders: std::array::from_fn(|_| Default::default()),
            pitch_bend_move_window_enabled: true,
            pitch_bend_move_window_animation: 1.0,
            pitch_bend_window_enabled: true,
            pitch_bend_window_animation: 1.0,
            pitch_bend_override_enabled: false,
            pitch_bend_override_animation: 0.0,
            pitch_bend_multiplier: 12, // default 12 semitones
            unlimited_output_enabled: false,
            unlimited_output_animation: 0.0,
            midi_ev_buffer_size: crate::player::DEFAULT_MIDI_EV_BUFFER_SIZE,
            midi_ev_mode: 0,

            // UI (non-editor)
            last_input_event: None,
            settings_scroll_y: 0.0,
            settings_scrollbar_dragging: false,
            settings_scrollbar_drag_start_y: 0.0,
            settings_scrollbar_drag_start_scroll: 0.0,
            settings_pb_multiplier_slider: Default::default(),
            settings_beat_bounce_amount_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_beat_bounce_note_value_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_midi_ev_buffer_size_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_midi_ev_mode_combo:
                crate::ui_components::combo_box::ComboBoxState::default(),
            settings_scan_delay_slider_dragging: false,
            settings_parse_threads_slider_dragging: false,
            speed_slider_dragging: false,

            // Editor
            editor_text: String::new(),
            editor_scroll_y: 0.0,
            editor_scrollbar_dragging: false,
            editor_scrollbar_drag_start_y: 0.0,
            editor_scrollbar_drag_start_scroll: 0.0,
            editor_focused: false,
            editor_cursor_pos: 0,
            editor_cursor_blink_timer: 0.0,
            editor_cursor_visible: true,

            // Systems
            blur_requester: BlurRequester::new(),
            overlay_manager: OverlayManager::new(OVERLAY_FADE_DURATION),
            kdmapi_missing: false,
            kdmapi_missing_timer: 0.0,
        }
    }

    pub fn get_weighted_time(&self, time_ns: u64) -> u64 {
        if let Some(map) = &self.tick_map {
            let idx = map.partition_point(|(t, _, _)| *t <= time_ns);
            if idx == 0 {
                return time_ns;
            }
            let (segment_start_time, segment_tempo, segment_accumulated) = map[idx - 1];
            let duration = time_ns - segment_start_time;
            let scale = BASE_TEMPO as f64 / segment_tempo as f64;
            let weighted_duration = (duration as f64 * scale) as u64;
            segment_accumulated + weighted_duration
        } else {
            time_ns
        }
    }

    #[allow(unused_variables)]
    pub fn handle_player_feedback(
        &mut self,
        feedback: PlayerFeedback,
        engine: &PlayerHandle,
        tx_commands: &std::sync::mpsc::Sender<PlayerCommand>,
    ) {
        match feedback {
            PlayerFeedback::ParseFailed(path) => {
                self.is_loading = false;
                self.reset_parse_progress();
                println!("[AppState] Parse failed: {}", path);
            }
            PlayerFeedback::ParseCancelled => {
                self.is_loading = false;
                self.reset_parse_progress();
            }
            PlayerFeedback::Loaded(path) => {
                self.is_loading = false;
                // Snapshot the final numbers before they are cleared: the toast carries the
                // panel out of the screen still showing them.
                self.parse_toast = Some(ParseToast {
                    elapsed: 0.0,
                    progress: self.current_progress,
                    parse_bps: self.parse_bps,
                    parse_eps: self.parse_eps,
                });
                self.reset_parse_progress();
                self.is_dragging_seek = false;
                self.last_seek_sent_ns = None;

                if let Some(midi_data) = engine.get_midi_data() {
                    self.tick_map = Some(calculate_tick_map(&midi_data));
                    self.midi_data = Some(midi_data.clone());
                } else {
                    self.midi_data = None;
                    self.tick_map = None;
                }

                if self.autoplay_on_load {
                    tx_commands.send(PlayerCommand::Play).ok();
                }
            }
            PlayerFeedback::Progress(progress) => {
                self.current_progress = Some(progress);
                self.update_parse_rate(progress, Instant::now());
            }
        }
    }

    /// Advances the finished-parse panel's exit animation and drops it when it is done.
    pub fn update_parse_toast(&mut self, dt: f32) {
        if let Some(toast) = &mut self.parse_toast {
            toast.elapsed += dt;
            if toast.elapsed >= ParseToast::total() {
                self.parse_toast = None;
            }
        }
    }

    /// The player thread has died: commands are going nowhere, the position and state
    /// it publishes are frozen, and nothing here can revive it. Tear the session down
    /// to something inert and raise the terminal overlay, rather than leaving a UI that
    /// looks alive and answers nothing.
    pub fn enter_player_crashed(&mut self) {
        self.is_loading = false;
        self.parse_toast = None;
        self.current_progress = None;
        self.reset_parse_progress();
        self.is_dragging_seek = false;
        self.last_seek_sent_ns = None;
        self.midi_data = None;
        self.tick_map = None;
        self.last_state = PlayerState::Stopped;
        self.overlay_manager.open(AppOverlay::PlayerCrashed);
    }

    fn reset_parse_progress(&mut self) {
        self.current_progress = None;
        self.parse_rate_samples.clear();
        self.parse_eps = 0.0;
        self.parse_bps = 0.0;
    }

    /// Fold one progress snapshot into the moving-average parse rates. The parser polls
    /// roughly every millisecond, so a ~0.5s window keeps the numbers readable without
    /// lagging far behind the real speed.
    fn update_parse_rate(&mut self, progress: ParseProgress, now: Instant) {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

        self.parse_rate_samples
            .push_back((now, progress.bytes_done, progress.events_done));
        while let Some(&(t, _, _)) = self.parse_rate_samples.front() {
            if now.duration_since(t) > WINDOW && self.parse_rate_samples.len() > 2 {
                self.parse_rate_samples.pop_front();
            } else {
                break;
            }
        }

        if let (Some(&(t0, b0, e0)), Some(&(t1, b1, e1))) = (
            self.parse_rate_samples.front(),
            self.parse_rate_samples.back(),
        ) {
            let dt = t1.duration_since(t0).as_secs_f64();
            if dt > 0.0 {
                self.parse_bps = b1.saturating_sub(b0) as f64 / dt;
                self.parse_eps = e1.saturating_sub(e0) as f64 / dt;
            }
        }
    }

    pub fn handle_file_drop(&mut self, path: &str, tx_commands: &mpsc::Sender<PlayerCommand>) {
        // With the player gone the load command has no reader; accepting the file would
        // only spin the loading overlay forever.
        if self.overlay_manager.current_kind() == Some(AppOverlay::PlayerCrashed) {
            return;
        }
        self.is_loading = true;
        self.is_dragging_seek = false;
        self.last_seek_sent_ns = None;
        self.parse_toast = None;
        self.current_progress = None;
        self.midi_data = None;
        self.tick_map = None;
        tx_commands
            .send(PlayerCommand::LoadFile(path.to_string()))
            .ok();
    }

    pub fn update_player_state(&mut self, st: PlayerState) {
        if st != self.last_state && st != PlayerState::Loading {
            self.last_state = st;
        }
    }

    pub fn handle_keyboard_seek(&mut self, new_position_ns: u64, engine: &PlayerHandle) {
        engine.send(PlayerCommand::Seek(new_position_ns));
    }

    pub fn start_seeking(&mut self, preview_ns: u64, engine: &PlayerHandle) {
        self.is_dragging_seek = true;
        self.last_seek_sent_ns = Some(preview_ns);
        engine.send(PlayerCommand::Seek(preview_ns));
    }

    pub fn update_seek_preview(&mut self, seek_ns: u64, engine: &PlayerHandle) {
        // Only send Seek to the engine when the position changes meaningfully
        // to avoid flooding the player with seek commands every frame.
        let should_seek = match self.last_seek_sent_ns {
            Some(prev) => seek_ns.abs_diff(prev) > 1_000_000,
            None => true,
        };
        if should_seek {
            self.last_seek_sent_ns = Some(seek_ns);
            engine.send(PlayerCommand::Seek(seek_ns));
        }
    }

    pub fn release_seek(&mut self) {
        self.is_dragging_seek = false;
        self.last_seek_sent_ns = None;
    }

    pub fn draw_keyboard(
        &mut self,
        d: &mut impl RaylibDraw,
        window: &window_manager::Window,
        keyboard_shader: &mut Shader,
        layout_texture: &Texture2D,
        hit_texture: &Texture2D,
        override_rect: Option<Rectangle>,
    ) {
        let (wx, wy, ww, wh) = if let Some(r) = override_rect {
            (r.x, r.y, r.width, r.height)
        } else {
            let x = window.rectangle.left() as f32;
            let y = (window.rectangle.top() + window.get_window_titlebar_height()) as f32;
            let w = window.rectangle.width as f32;
            let h = (window.rectangle.height - window.get_window_titlebar_height()) as f32;
            (x, y, w, h)
        };

        let loc_hit = keyboard_shader.get_shader_location("hit_data_texture");
        let loc_playback_y = keyboard_shader.get_shader_location("playback_y");
        let loc_valid = keyboard_shader.get_shader_location("is_texture_valid");
        let loc_kb_mode = keyboard_shader.get_shader_location("u_kb_mode");

        // Feed the per-key hit texture so the shader lights up sounding keys. The
        // texture is 128x1, so the sampled row (`playback_y`) is always 0. The hit
        // sampler must live on texture unit 1; binding it via the manual rlgl slot
        // calls below avoids clobbering texture0 (the layout) bound by the draw.
        keyboard_shader.set_shader_value(loc_valid, 1.0f32);
        keyboard_shader.set_shader_value(loc_playback_y, 0.0f32);
        keyboard_shader.set_shader_value(loc_kb_mode, 1.0f32);
        // Safety: rlgl FFI that manipulate the active GL texture slot. `hit_texture.id`
        // is a valid GL texture owned by raylib for the duration of this call.
        unsafe {
            raylib::ffi::rlActiveTextureSlot(1);
            raylib::ffi::rlEnableTexture(hit_texture.id);
        }
        keyboard_shader.set_shader_value(loc_hit, 1i32);

        {
            let mut d_shader = d.begin_shader_mode(keyboard_shader);
            d_shader.draw_texture_pro(
                layout_texture,
                Rectangle::new(
                    0.0,
                    0.0,
                    layout_texture.width as f32,
                    layout_texture.height as f32,
                ),
                Rectangle::new(wx, wy, ww, wh),
                Vector2::new(0.0, 0.0),
                0.0,
                Color::WHITE,
            );
        }

        keyboard_shader.set_shader_value(loc_kb_mode, 0.0f32);
        keyboard_shader.set_shader_value(loc_valid, 0.0f32);

        // Safety: unbind the hit texture from slot 1 and restore the active slot to
        // 0 so subsequent draws are unaffected.
        unsafe {
            raylib::ffi::rlActiveTextureSlot(1);
            raylib::ffi::rlDisableTexture();
            raylib::ffi::rlActiveTextureSlot(0);
        }
    }

    pub fn draw_keyboard_channels(
        &self,
        d: &mut impl RaylibDraw,
        window: &window_manager::Window,
        active_notes: &[Option<(u16, u8)>; 128],
        origin_x: f32,
        origin_y: f32,
    ) {
        let x = origin_x;
        let y = origin_y;
        let w = window.rectangle.width as f32;
        let h = (window.rectangle.height - window.get_window_titlebar_height()) as f32;

        if w <= 0.0 || h <= 0.0 {
            return;
        }

        let label_w = 34.0;
        let row_gap = 1.0;
        let row_h = ((h - row_gap * 15.0) / 16.0).max(4.0);
        let key_x = x + label_w;
        let key_w = (w - label_w).max(1.0);
        let regions = piano_keyboard::get_keyboard_note_regions(key_w);
        let mut channel_notes = [[false; 128]; 16];

        for note in 0..128usize {
            if let Some((_track, channel)) = active_notes[note] {
                if channel < 16 {
                    channel_notes[channel as usize][note] = true;
                }
            }
        }

        for channel in 0..16usize {
            let row_y = y + channel as f32 * (row_h + row_gap);
            let (r, g, b) = get_note_color_by_track_channel(u16::MAX, channel as u8);
            let channel_color = Color::new(r, g, b, 255);
            let bg = Color::new(18, 18, 18, 255);
            d.draw_rectangle(x as i32, row_y as i32, w as i32, row_h.ceil() as i32, bg);
            d.draw_text(
                &format!("{:02}", channel + 1),
                (x + 6.0) as i32,
                (row_y + (row_h - 12.0) * 0.5) as i32,
                12,
                channel_color,
            );

            // Fill the key area with the inactive white-key color first so that
            // any 1px gaps caused by integer rounding/sub-pixel positioning are
            // invisible instead of showing the dark background.
            d.draw_rectangle(
                key_x as i32,
                row_y as i32,
                key_w as i32,
                row_h.ceil() as i32,
                Color::new(225, 225, 225, 255),
            );

            for note in 0..128u8 {
                if !piano_keyboard::is_black_key(note) {
                    let (nx, nw) = regions[note as usize];
                    let key_rect = Rectangle::new(key_x + nx, row_y, nw.max(1.0), row_h);
                    let color = if channel_notes[channel][note as usize] {
                        channel_color
                    } else {
                        Color::new(225, 225, 225, 255)
                    };
                    d.draw_rectangle_rec(key_rect, color);
                    d.draw_rectangle_lines_ex(key_rect, 1.0, Color::new(55, 55, 55, 255));
                }
            }

            for note in 0..128u8 {
                if piano_keyboard::is_black_key(note) {
                    let (nx, nw) = regions[note as usize];
                    let key_h = row_h * 0.65;
                    let key_rect = Rectangle::new(key_x + nx, row_y, nw.max(1.0), key_h.max(2.0));
                    let color = if channel_notes[channel][note as usize] {
                        channel_color
                    } else {
                        Color::new(20, 20, 20, 255)
                    };
                    d.draw_rectangle_rec(key_rect, color);
                    d.draw_rectangle_lines_ex(key_rect, 1.0, Color::new(0, 0, 0, 255));
                }
            }
        }
    }
}
