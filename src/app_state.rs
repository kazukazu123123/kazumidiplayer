use crate::blur_requester::BlurRequester;
use crate::note_texture_generator::NoteTextureGenerator;
use crate::overlay_manager::OverlayManager;
use crate::player::{PlayerCommand, PlayerFeedback, PlayerState, PlayerHandle};
use crate::shaking::ShakeState;
use crate::texture_manager::NoteTextureManager;
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
    // --- Display & Texture ---
    pub chunk_display_height: f32,
    pub current_texture_count: usize,
    pub max_texture_count: usize,
    pub note_textures_enabled: bool,
    pub prev_note_textures_enabled: bool,
    pub note_textures_animation: f32,
    pub texture_height: usize,

    // --- Player State ---
    pub current_progress: Option<(u16, u16)>,
    pub is_dragging_seek: bool,
    pub last_update_position_ns: u64,
    pub texture_manager: NoteTextureManager,
    pub last_state: PlayerState,
    pub is_loading: bool,
    pub is_tick_based: bool,
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
    pub texture_worker_count: usize,
    pub prev_texture_worker_count: usize,
    pub unlimited_output_enabled: bool,
    pub unlimited_output_animation: f32,

    // --- Hover / Thumbnail ---
    pub hover_timer: f32,
    pub hover_fade_t: f32,
    pub hover_preview_ns: Option<u64>,
    pub show_thumbnail: bool,

    // --- UI (non-editor) ---
    pub last_input_event: Option<InputEvent>,
    pub settings_scroll_y: f32,
    pub settings_scrollbar_dragging: bool,
    pub settings_scrollbar_drag_start_y: f32,
    pub settings_scrollbar_drag_start_scroll: f32,
    pub settings_texture_height_slider_dragging: bool,
    pub settings_pb_multiplier_slider_dragging: bool,

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

fn draw_piano_notes(
    d: &mut impl RaylibDraw,
    texture: &Texture2D,
    regions: &[(f32, f32)],
    window_x: f32,
    src_y: f32,
    src_h: f32,
    dest_y: f32,
    dest_h: f32,
    black_keys: bool,
) {
    for note in 0..128u8 {
        if piano_keyboard::is_black_key(note) == black_keys {
            let (nx, nw) = regions[note as usize];
            d.draw_texture_pro(
                texture,
                Rectangle::new(note as f32, src_y, 1.0, src_h),
                Rectangle::new(window_x + nx, dest_y, nw, dest_h),
                Vector2::new(0.0, 0.0),
                0.0,
                Color::WHITE,
            );
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            // --- Display & Texture ---
            chunk_display_height: 100.0,
            current_texture_count: 0,
            max_texture_count: 0,
            note_textures_enabled: true,
            prev_note_textures_enabled: true,
            note_textures_animation: 1.0,
            texture_height: 16384,

            // --- Player State ---
            current_progress: None,
            is_dragging_seek: false,
            last_update_position_ns: 0,
            texture_manager: NoteTextureManager::new(),
            last_state: PlayerState::Stopped,
            is_loading: false,
            is_tick_based: false,
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
            texture_worker_count: 1,
            prev_texture_worker_count: 1,
            unlimited_output_enabled: false,
            unlimited_output_animation: 0.0,

            // --- Hover / Thumbnail ---
            hover_timer: 0.0,
            hover_fade_t: 0.0,
            hover_preview_ns: None,
            show_thumbnail: false,

            // --- UI (non-editor) ---
            last_input_event: None,
            settings_scroll_y: 0.0,
            settings_scrollbar_dragging: false,
            settings_scrollbar_drag_start_y: 0.0,
            settings_scrollbar_drag_start_scroll: 0.0,
            settings_texture_height_slider_dragging: false,
            settings_pb_multiplier_slider_dragging: false,

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
        texture_generator: &NoteTextureGenerator,
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
                self.last_update_position_ns = 0;
                self.current_texture_count = 0;
                self.max_texture_count = 0;
                self.texture_manager.clear();
                texture_generator.cancel_all();
                self.hover_timer = 0.0;
                self.hover_fade_t = 0.0;
                self.show_thumbnail = false;
                self.hover_preview_ns = None;

                if let Some(midi_data) = engine.get_midi_data() {
                    self.tick_map = Some(calculate_tick_map(&midi_data));
                    self.midi_data = Some(midi_data.clone());
                    if self.note_textures_enabled {
                        texture_generator.send(
                            crate::note_texture_generator::TextureGenCommand::UpdateData(midi_data),
                        );
                    }
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
        texture_generator: &NoteTextureGenerator,
    ) {
        self.is_loading = true;
        self.texture_manager.clear();
        self.is_dragging_seek = false;
        self.last_seek_sent_ns = None;
        self.last_update_position_ns = 0;
        self.current_progress = None;
        self.current_texture_count = 0;
        self.max_texture_count = 0;
        self.midi_data = None;
        self.hover_timer = 0.0;
        self.hover_fade_t = 0.0;
        self.show_thumbnail = false;
        self.hover_preview_ns = None;
        texture_generator.cancel_all();
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
        texture_generator: &NoteTextureGenerator,
    ) {
        engine.send(PlayerCommand::Seek(new_position_ns));
        texture_generator.update_position(new_position_ns);
    }

    pub fn start_seeking(&mut self, preview_ns: u64, engine: &PlayerHandle, texture_generator: &NoteTextureGenerator) {
        self.is_dragging_seek = true;
        self.last_seek_sent_ns = Some(preview_ns);
        engine.send(PlayerCommand::Seek(preview_ns));
        texture_generator.update_position(preview_ns);
    }

    pub fn update_seek_preview(
        &mut self,
        seek_ns: u64,
        engine: &PlayerHandle,
        texture_generator: &NoteTextureGenerator,
    ) {
        texture_generator.update_position(seek_ns);
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

    pub fn adjust_zoom_by_wheel(&mut self, wheel: f32) {
        let factor = 1.2f32;
        if wheel > 0.0 {
            self.chunk_display_height *= factor.powf(wheel);
        } else if wheel < 0.0 {
            self.chunk_display_height /= factor.powf(-wheel);
        }
        self.chunk_display_height = self.chunk_display_height.clamp(1.0, 20000.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn iterate_tempo_map_segments(
        &self,
        map: &[(u64, u32, u64)],
        start_time_ns: u64,
        end_time_ns: u64,
        display_pos_val: f64,
        pixels_per_ns: f64,
        playhead_y: f32,
        chunk_duration_ns: f64,
        tex_h: f32,
        mut draw_slice: impl FnMut(f32, f32, f32, f32),
    ) {
        let mut map_idx = map.partition_point(|(t, _, _)| *t <= start_time_ns);
        if map_idx > 0 {
            map_idx -= 1;
        }

        let mut current_t = start_time_ns;
        while current_t < end_time_ns {
            let (seg_start, seg_tempo, seg_accum_w) = map[map_idx];
            let next_change_t =
                map.get(map_idx + 1).map(|m| m.0).unwrap_or(u64::MAX);
            let slice_end_t = end_time_ns.min(next_change_t);

            if slice_end_t > current_t {
                let duration = slice_end_t - current_t;
                let scale = BASE_TEMPO as f64 / seg_tempo as f64;
                let weighted_duration = (duration as f64 * scale) as u64;

                let w_start_abs = seg_accum_w
                    + ((current_t - seg_start) as f64 * scale) as u64;
                let val_until_slice = w_start_abs as f64 - display_pos_val;
                let dist_to_playhead = (val_until_slice * pixels_per_ns) as f32;

                let slice_height =
                    (weighted_duration as f64 * pixels_per_ns) as f32;
                let slice_y = playhead_y - dist_to_playhead - slice_height;

                let norm_start =
                    (current_t - start_time_ns) as f64 / chunk_duration_ns;
                let norm_end =
                    (slice_end_t - start_time_ns) as f64 / chunk_duration_ns;

                let src_y = tex_h * (1.0 - norm_end as f32);
                let src_h = (tex_h * (1.0 - norm_start as f32)) - src_y;

                draw_slice(src_y, src_h, slice_y, slice_height);
            }

            current_t = slice_end_t;
            if current_t >= next_change_t && map_idx + 1 < map.len() {
                map_idx += 1;
            } else {
                break;
            }
        }
    }

    pub fn draw_piano_roll(
        &self,
        d: &mut impl RaylibDraw,
        window: &window_manager::Window,
        playback_position_ns: u64,
        texture_generator: &NoteTextureGenerator,
        origin_x: f32,
        origin_y: f32,
    ) {
        if self.texture_manager.len() == 0 {
            return;
        }

        let window_x = origin_x;
        let window_y = origin_y;
        let mut window_height =
            (window.rectangle.height - window.get_window_titlebar_height()) as f32;

        if self.integrate_keyboard {
            window_height -= 80.0;
        }

        let window_display_width = window.rectangle.width as f32;
        let regions = piano_keyboard::get_keyboard_note_regions(window_display_width);
        let window_playhead_y = window_y + window_height;
        let display_position_ns = playback_position_ns;

        let chunk_duration_ns = texture_generator.get_chunk_duration_ns() as f64;
        let pixels_per_ns = self.chunk_display_height as f64 / chunk_duration_ns;

        let display_pos_val = if self.is_tick_based {
            self.get_weighted_time(display_position_ns) as f64
        } else {
            display_position_ns as f64
        };

        let textures = self.texture_manager.as_slice();

        let start_idx = textures.partition_point(|(_, start, _)| {
            let end_time = *start + chunk_duration_ns as u64;
            if self.is_tick_based {
                (self.get_weighted_time(end_time) as f64) < display_pos_val
            } else {
                (*start as f64 + chunk_duration_ns) < display_pos_val
            }
        });

        for i in start_idx..textures.len() {
            let (texture, start_time_ns, _) = &textures[i];
            let end_time_ns = *start_time_ns + chunk_duration_ns as u64;

            if self.is_tick_based {
                if let Some(map) = &self.tick_map {
                    let w_start_chunk = self.get_weighted_time(*start_time_ns) as f64;
                    let w_end_chunk = self.get_weighted_time(end_time_ns) as f64;

                    let dist_start = (w_start_chunk - display_pos_val) * pixels_per_ns;
                    let dist_end = (w_end_chunk - display_pos_val) * pixels_per_ns;

                    let chunk_y_bottom = window_playhead_y - dist_start as f32;
                    let chunk_y_top = window_playhead_y - dist_end as f32;

                    if chunk_y_bottom < window_y {
                        break;
                    }
                    if chunk_y_top > window_playhead_y {
                        continue;
                    }

                    self.iterate_tempo_map_segments(
                        map,
                        *start_time_ns,
                        end_time_ns,
                        display_pos_val,
                        pixels_per_ns,
                        window_playhead_y,
                        chunk_duration_ns,
                        texture.height as f32,
                        |src_y, src_h, slice_y, slice_height| {
                            draw_piano_notes(
                                d, texture, &regions, window_x, src_y, src_h, slice_y,
                                slice_height, false,
                            );
                            draw_piano_notes(
                                d, texture, &regions, window_x, src_y, src_h, slice_y,
                                slice_height, true,
                            );
                        },
                    );
                }
            } else {
                let start_val = *start_time_ns as f64;
                let end_val = end_time_ns as f64;
                let duration_val = end_val - start_val;
                let current_chunk_height = (duration_val * pixels_per_ns) as f32;
                let val_until_chunk_start = start_val - display_pos_val;
                let distance_to_playhead = (val_until_chunk_start * pixels_per_ns) as f32;
                let texture_y =
                    window_playhead_y - distance_to_playhead - current_chunk_height;

                if texture_y + current_chunk_height < window_y {
                    break;
                }
                if texture_y > window_playhead_y {
                    continue;
                }

                let tex_h = texture.height as f32;
                draw_piano_notes(
                    d,
                    texture,
                    &regions,
                    window_x,
                    0.0,
                    tex_h,
                    texture_y,
                    current_chunk_height,
                    false,
                );
                draw_piano_notes(
                    d,
                    texture,
                    &regions,
                    window_x,
                    0.0,
                    tex_h,
                    texture_y,
                    current_chunk_height,
                    true,
                );
            }
        }
    }

    pub fn draw_keyboard(
        &mut self,
        d: &mut impl RaylibDraw,
        window: &window_manager::Window,
        playback_position_ns: u64,
        keyboard_shader: &mut Shader,
        layout_texture: &Texture2D,
        note_texture_generator: &NoteTextureGenerator,
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

        let display_position_ns = playback_position_ns;
        let chunk_duration_ns = note_texture_generator.get_chunk_duration_ns() as f64;
        let textures = self.texture_manager.as_slice();

        let target_texture_idx =
            textures.partition_point(|(_, start, _)| *start + chunk_duration_ns as u64 <= display_position_ns);

        let loc_hit = keyboard_shader.get_shader_location("hit_data_texture");
        let loc_playback_y = keyboard_shader.get_shader_location("playback_y");
        let loc_valid = keyboard_shader.get_shader_location("is_texture_valid");
        let loc_kb_mode = keyboard_shader.get_shader_location("u_kb_mode");

        let mut is_shader_ready = false;
        let mut hit_texture: Option<&Texture2D> = None;
        let mut playback_y_value: f32 = 0.0;

        if target_texture_idx < textures.len() {
            let (tex, start_ns, _) = &textures[target_texture_idx];
            if display_position_ns >= *start_ns && display_position_ns < *start_ns + chunk_duration_ns as u64 {
                let relative_ns = display_position_ns - start_ns;
                let normalized_time = relative_ns as f64 / chunk_duration_ns;
                playback_y_value = 1.0 - normalized_time as f32;
                hit_texture = Some(tex);
                is_shader_ready = true;
            }
        }

        if is_shader_ready {
            keyboard_shader.set_shader_value(loc_valid, 1.0f32);
            keyboard_shader.set_shader_value(loc_playback_y, playback_y_value);
            if let Some(tex) = hit_texture {
                // Safety: rlActiveTextureSlot/rlEnableTexture are raylib FFI calls that
                // manipulate OpenGL state. tex.id is a valid GL texture from raylib.
                unsafe {
                    raylib::ffi::rlActiveTextureSlot(1);
                    raylib::ffi::rlEnableTexture(tex.id);
                }
                keyboard_shader.set_shader_value(loc_hit, 1i32);
            }
        } else {
            keyboard_shader.set_shader_value(loc_valid, 0.0f32);
            keyboard_shader.set_shader_value(loc_playback_y, 0.0f32);
        }

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

        // Safety: raylib FFI calls to unbind the texture from texture slot 1
        // and restore active slot to 0.
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

    pub fn draw_thumbnail(
        &self,
        d: &mut impl RaylibDraw,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        position_ns: u64,
        texture_generator: &NoteTextureGenerator,
    ) {
        let alpha = (self.hover_fade_t * 255.0) as u8;
        if alpha == 0 {
            return;
        }

        let border_thickness = 2.0;
        d.draw_rectangle(
            x as i32,
            y as i32,
            width as i32,
            height as i32,
            Color::new(20, 20, 20, (alpha as f32 * 0.94) as u8),
        );

        if self.texture_manager.len() > 0 {
            let thumb_zoom = 50.0;
            let chunk_duration_ns = texture_generator.get_chunk_duration_ns() as f64;
            let pixels_per_ns = thumb_zoom as f64 / chunk_duration_ns;

            let display_pos_val = if self.is_tick_based {
                self.get_weighted_time(position_ns) as f64
            } else {
                position_ns as f64
            };

            let playhead_y = y + height;

            {
                let mut d_scissor = d.begin_scissor_mode(
                    (x + border_thickness) as i32,
                    (y + border_thickness) as i32,
                    (width - border_thickness * 2.0) as i32,
                    (height - border_thickness * 2.0) as i32,
                );

                let textures = self.texture_manager.as_slice();

                let tint = Color::new(255, 255, 255, alpha);

                for i in 0..textures.len() {
                    let (texture, start_time_ns, _end_time_ns) = &textures[i];
                    let end_time_ns = *start_time_ns + chunk_duration_ns as u64;

                    if self.is_tick_based {
                        if let Some(map) = &self.tick_map {
                            let w_start_chunk = self.get_weighted_time(*start_time_ns) as f64;
                            let w_end_chunk = self.get_weighted_time(end_time_ns) as f64;

                            let dist_start = (w_start_chunk - display_pos_val) * pixels_per_ns;
                            let dist_end = (w_end_chunk - display_pos_val) * pixels_per_ns;

                            let chunk_y_bottom = playhead_y - dist_start as f32;
                            let chunk_y_top = playhead_y - dist_end as f32;

                            if chunk_y_bottom < y {
                                break;
                            }
                            if chunk_y_top > playhead_y {
                                continue;
                            }

                            self.iterate_tempo_map_segments(
                                map,
                                *start_time_ns,
                                end_time_ns,
                                display_pos_val,
                                pixels_per_ns,
                                playhead_y,
                                chunk_duration_ns,
                                texture.height as f32,
                                |src_y, src_h, slice_y, slice_height| {
                                    d_scissor.draw_texture_pro(
                                        texture,
                                        Rectangle::new(0.0, src_y, texture.width as f32, src_h),
                                        Rectangle::new(x, slice_y, width, slice_height),
                                        Vector2::new(0.0, 0.0),
                                        0.0,
                                        tint,
                                    );
                                },
                            );
                        }
                    } else {
                        let start_val = *start_time_ns as f64;
                        let end_val = end_time_ns as f64;
                        let duration_val = end_val - start_val;
                        let current_chunk_height = (duration_val * pixels_per_ns) as f32;
                        let val_until_chunk_start = start_val - display_pos_val;
                        let distance_to_playhead = (val_until_chunk_start * pixels_per_ns) as f32;
                        let texture_y = playhead_y - distance_to_playhead - current_chunk_height;

                        if texture_y + current_chunk_height < y {
                            break;
                        }
                        if texture_y > playhead_y {
                            continue;
                        }

                        d_scissor.draw_texture_pro(
                            texture,
                            Rectangle::new(0.0, 0.0, texture.width as f32, texture.height as f32),
                            Rectangle::new(x, texture_y, width, current_chunk_height),
                            Vector2::new(0.0, 0.0),
                            0.0,
                            tint,
                        );
                    }
                }
            }
        }

        d.draw_rectangle_lines_ex(
            Rectangle::new(x, y, width, height),
            border_thickness,
            Color::new(0, 120, 215, alpha),
        );

        let time_text = crate::utils::format_time(position_ns);
        d.draw_text(
            &time_text,
            x as i32 + 5,
            y as i32 + 5,
            16,
            Color::new(255, 255, 255, alpha),
        );
    }
}
