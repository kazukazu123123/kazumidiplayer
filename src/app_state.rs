use crate::blur_requester::BlurRequester;
use crate::overlay_manager::OverlayManager;
use crate::player::{PlayerCommand, PlayerFeedback, PlayerState, PlayerHandle};
use crate::shaking::ShakeState;
use crate::ui_components::piano_keyboard;
use crate::utils::get_note_color_by_track_channel;
use crate::window_manager::{self, InputEvent};
use raylib::prelude::*;
use std::sync::mpsc;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppOverlay {
    Settings,
    Loading,
    TextEditor,
}

pub struct AppState {
    // --- Player State ---
    pub current_progress: Option<(u16, u16)>,
    pub is_dragging_seek: bool,
    pub last_state: PlayerState,
    pub is_loading: bool,
    pub is_tick_based: bool,
    /// Piano-roll scroll speed multiplier (higher = notes move faster, shorter
    /// visible window). Adjusted with the mouse wheel over the piano roll.
    pub note_speed: f32,
    /// Debug: artificial piano-roll scan delay in milliseconds (slider-controlled).
    pub scan_delay_ms: u32,
    pub tick_map: Option<Vec<(u64, u32, u64)>>,
    pub midi_data: Option<Arc<kazumidiparser::MIDIPlayerData>>,
    pub last_seek_sent_ns: Option<u64>,

    // --- Animation ---
    pub tick_based_animation: f32,
    pub shake_enabled: bool,
    pub shake_animation: f32,
    pub shake_state: ShakeState,
    pub integrate_keyboard: bool,
    pub keyboard_animation: f32,
    pub autoplay_on_load: bool,
    pub autoplay_animation: f32,
    pub pitch_bend_override_enabled: bool,
    pub pitch_bend_override_animation: f32,
    pub pitch_bend_multiplier: u8, // 1-127 (semitones)
    pub unlimited_output_enabled: bool,
    pub unlimited_output_animation: f32,

    // --- UI (non-editor) ---
    pub last_input_event: Option<InputEvent>,
    pub settings_scroll_y: f32,
    pub settings_scrollbar_dragging: bool,
    pub settings_scrollbar_drag_start_y: f32,
    pub settings_scrollbar_drag_start_scroll: f32,
    pub settings_pb_multiplier_slider_dragging: bool,
    pub settings_scan_delay_slider_dragging: bool,

    // --- Editor ---
    pub editor_text: String,
    pub editor_scroll_y: f32,
    pub editor_scrollbar_dragging: bool,
    pub editor_scrollbar_drag_start_y: f32,
    pub editor_scrollbar_drag_start_scroll: f32,
    pub editor_focused: bool,
    pub editor_cursor_pos: usize,
    pub editor_cursor_blink_timer: f32,
    pub editor_cursor_visible: bool,

    // --- Systems ---
    pub blur_requester: BlurRequester,
    pub overlay_manager: OverlayManager<AppOverlay>,
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
            // --- Player State ---
            current_progress: None,
            is_dragging_seek: false,
            last_state: PlayerState::Stopped,
            is_loading: false,
            is_tick_based: false,
            note_speed: 1.0,
            scan_delay_ms: 0,
            tick_map: None,
            midi_data: None,
            last_seek_sent_ns: None,

            // --- Animation ---
            tick_based_animation: 0.0,
            shake_enabled: false,
            shake_animation: 0.0,
            shake_state: ShakeState::new(),
            integrate_keyboard: false,
            keyboard_animation: 0.0,
            autoplay_on_load: false,
            autoplay_animation: 0.0,
            pitch_bend_override_enabled: false,
            pitch_bend_override_animation: 0.0,
            pitch_bend_multiplier: 12, // default 12 semitones
            unlimited_output_enabled: false,
            unlimited_output_animation: 0.0,

            // --- UI (non-editor) ---
            last_input_event: None,
            settings_scroll_y: 0.0,
            settings_scrollbar_dragging: false,
            settings_scrollbar_drag_start_y: 0.0,
            settings_scrollbar_drag_start_scroll: 0.0,
            settings_pb_multiplier_slider_dragging: false,
            settings_scan_delay_slider_dragging: false,

            // --- Editor ---
            editor_text: String::new(),
            editor_scroll_y: 0.0,
            editor_scrollbar_dragging: false,
            editor_scrollbar_drag_start_y: 0.0,
            editor_scrollbar_drag_start_scroll: 0.0,
            editor_focused: false,
            editor_cursor_pos: 0,
            editor_cursor_blink_timer: 0.0,
            editor_cursor_visible: true,

            // --- Systems ---
            blur_requester: BlurRequester::new(),
            overlay_manager: OverlayManager::new(OVERLAY_FADE_DURATION),
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
                self.current_progress = None;
                println!("[AppState] Parse failed: {}", path);
            }
            PlayerFeedback::ParseCancelled => {
                self.is_loading = false;
                self.current_progress = None;
            }
            PlayerFeedback::Loaded(path) => {
                self.is_loading = false;
                self.current_progress = None;
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
            PlayerFeedback::Progress(current, total) => {
                self.current_progress = Some((current, total));
            }
        }
    }

    pub fn handle_file_drop(
        &mut self,
        path: &str,
        tx_commands: &mpsc::Sender<PlayerCommand>,
    ) {
        self.is_loading = true;
        self.is_dragging_seek = false;
        self.last_seek_sent_ns = None;
        self.current_progress = None;
        self.midi_data = None;
        tx_commands.send(PlayerCommand::LoadFile(path.to_string())).ok();
    }

    pub fn update_player_state(&mut self, st: PlayerState) {
        if st != self.last_state && st != PlayerState::Loading {
            self.last_state = st;
        }
    }

    pub fn handle_keyboard_seek(
        &mut self,
        new_position_ns: u64,
        engine: &PlayerHandle,
    ) {
        engine.send(PlayerCommand::Seek(new_position_ns));
    }

    pub fn start_seeking(&mut self, preview_ns: u64, engine: &PlayerHandle) {
        self.is_dragging_seek = true;
        self.last_seek_sent_ns = Some(preview_ns);
        engine.send(PlayerCommand::Seek(preview_ns));
    }

    pub fn update_seek_preview(
        &mut self,
        seek_ns: u64,
        engine: &PlayerHandle,
    ) {
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

        let loc_playback_y = keyboard_shader.get_shader_location("playback_y");
        let loc_valid = keyboard_shader.get_shader_location("is_texture_valid");
        let loc_kb_mode = keyboard_shader.get_shader_location("u_kb_mode");

        // No note data is available; render the static keyboard layout only.
        keyboard_shader.set_shader_value(loc_valid, 0.0f32);
        keyboard_shader.set_shader_value(loc_playback_y, 0.0f32);
        keyboard_shader.set_shader_value(loc_kb_mode, 1.0f32);

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
