use crate::midi_parser_worker::ParseProgress;
use crate::player::{PlayerCommand, PlayerFeedback, PlayerHandle, PlayerState};
use crate::ui_components::piano_keyboard;
use crate::utils::get_note_color_by_track_channel;
use crate::window_manager;
use raylib::prelude::*;
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
    /// How long the finished parse took, measured around the parser call itself.
    pub parse_duration: std::time::Duration,
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

mod playback;
mod settings;
mod ui_state;

pub use playback::Playback;
pub use settings::Settings;
pub use ui_state::UiState;

/// Everything the app remembers between frames, in three groups by
/// role: the user's settings, the playback session, and transient
/// UI state (animations, drags and widget states).
pub struct AppState {
    pub settings: Settings,
    pub playback: Playback,
    pub ui: UiState,
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
            settings: Settings::new(),
            playback: Playback::new(),
            ui: UiState::new(),
        }
    }


    pub fn get_weighted_time(&self, time_ns: u64) -> u64 {
        if let Some(map) = &self.playback.tick_map {
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
                self.playback.is_loading = false;
                self.reset_parse_progress();
                println!("[AppState] Parse failed: {}", path);
            }
            PlayerFeedback::ParseCancelled => {
                self.playback.is_loading = false;
                self.reset_parse_progress();
            }
            PlayerFeedback::Loaded(path, parse_duration) => {
                self.playback.is_loading = false;
                // The toast summarizes the finished load, so its rates are the average
                // over the whole parse, measured by the worker. The live rates are a
                // recent-window figure and may never have opened on a fast parse,
                // which would leave the toast showing 0/s.
                let secs = parse_duration.as_secs_f64();
                let (final_bps, final_eps) = match self.playback.current_progress {
                    Some(progress) if secs > 0.0 => (
                        progress.total_bytes as f64 / secs,
                        progress.events_done as f64 / secs,
                    ),
                    _ => (self.playback.parse_bps, self.playback.parse_eps),
                };
                // Snapshot the final numbers before they are cleared: the toast carries the
                // panel out of the screen still showing them.
                self.playback.parse_toast = Some(ParseToast {
                    elapsed: 0.0,
                    parse_duration,
                    progress: self.playback.current_progress,
                    parse_bps: final_bps,
                    parse_eps: final_eps,
                });
                self.reset_parse_progress();
                self.playback.is_dragging_seek = false;
                self.playback.last_seek_sent_ns = None;

                if let Some(midi_data) = engine.get_midi_data() {
                    self.playback.tick_map = Some(calculate_tick_map(&midi_data));
                    self.playback.midi_data = Some(midi_data.clone());
                } else {
                    self.playback.midi_data = None;
                    self.playback.tick_map = None;
                }

                if self.settings.autoplay_on_load {
                    tx_commands.send(PlayerCommand::Play).ok();
                }
            }
            PlayerFeedback::Progress(progress) => {
                self.playback.current_progress = Some(progress);
                self.update_parse_rate(progress, Instant::now());
            }
        }
    }

    /// Advances the finished-parse panel's exit animation and drops it when it is done.
    pub fn update_parse_toast(&mut self, dt: f32) {
        if let Some(toast) = &mut self.playback.parse_toast {
            toast.elapsed += dt;
            if toast.elapsed >= ParseToast::total() {
                self.playback.parse_toast = None;
            }
        }
    }

    /// The player thread has died: commands are going nowhere, the position and state
    /// it publishes are frozen, and nothing here can revive it. Tear the session down
    /// to something inert and raise the terminal overlay, rather than leaving a UI that
    /// looks alive and answers nothing.
    pub fn enter_player_crashed(&mut self) {
        self.playback.is_loading = false;
        self.playback.parse_toast = None;
        self.playback.current_progress = None;
        self.reset_parse_progress();
        self.playback.is_dragging_seek = false;
        self.playback.last_seek_sent_ns = None;
        self.playback.midi_data = None;
        self.playback.tick_map = None;
        self.playback.last_state = PlayerState::Stopped;
        self.ui.overlay_manager.open(AppOverlay::PlayerCrashed);
    }

    fn reset_parse_progress(&mut self) {
        self.playback.current_progress = None;
        self.playback.parse_rate_samples.clear();
        self.playback.parse_eps = 0.0;
        self.playback.parse_bps = 0.0;
    }

    /// Fold one progress snapshot into the moving-average parse rates. The parser polls
    /// roughly every millisecond, so a ~0.5s window keeps the numbers readable without
    /// lagging far behind the real speed.
    ///
    /// Snapshots are stamped when the UI processes them, not when the parser took them,
    /// and the UI drains every queued snapshot once per frame, so a fast parse arrives
    /// as a burst of snapshots processed within microseconds of each other. Kept as
    /// separate samples they would collapse the measured span and send the rates to
    /// absurd values, so a snapshot landing within MIN_SAMPLE_GAP of the newest sample
    /// folds into it instead (keeping its timestamp, taking the newest counts), and
    /// the rates are only recomputed once the samples span at least MIN_RATE_SPAN.
    fn update_parse_rate(&mut self, progress: ParseProgress, now: Instant) {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
        const MIN_SAMPLE_GAP: std::time::Duration = std::time::Duration::from_millis(10);
        const MIN_RATE_SPAN: std::time::Duration = std::time::Duration::from_millis(10);

        let samples = &mut self.playback.parse_rate_samples;
        let fold = match samples.back() {
            Some(&(t, _, _)) => now.duration_since(t) < MIN_SAMPLE_GAP,
            None => false,
        };
        if fold {
            if let Some(back) = samples.back_mut() {
                back.1 = progress.bytes_done;
                back.2 = progress.events_done;
            }
        } else {
            samples.push_back((now, progress.bytes_done, progress.events_done));
        }
        while let Some(&(t, _, _)) = samples.front() {
            if now.duration_since(t) > WINDOW && samples.len() > 2 {
                samples.pop_front();
            } else {
                break;
            }
        }
        let endpoints = match (samples.front(), samples.back()) {
            (Some(&front), Some(&back)) => Some((front, back)),
            _ => None,
        };

        if let Some(((t0, b0, e0), (t1, b1, e1))) = endpoints {
            let span = t1.duration_since(t0);
            if span >= MIN_RATE_SPAN {
                let dt = span.as_secs_f64();
                self.playback.parse_bps = b1.saturating_sub(b0) as f64 / dt;
                self.playback.parse_eps = e1.saturating_sub(e0) as f64 / dt;
            }
        }
    }

    pub fn handle_file_drop(&mut self, path: &str, tx_commands: &mpsc::Sender<PlayerCommand>) {
        // With the player gone the load command has no reader; accepting the file would
        // only spin the loading overlay forever.
        if self.ui.overlay_manager.current_kind() == Some(AppOverlay::PlayerCrashed) {
            return;
        }
        self.playback.is_loading = true;
        self.playback.is_dragging_seek = false;
        self.playback.last_seek_sent_ns = None;
        self.playback.parse_toast = None;
        self.playback.current_progress = None;
        self.playback.midi_data = None;
        self.playback.tick_map = None;
        tx_commands
            .send(PlayerCommand::LoadFile(path.to_string()))
            .ok();
    }

    pub fn update_player_state(&mut self, st: PlayerState) {
        if st != self.playback.last_state && st != PlayerState::Loading {
            self.playback.last_state = st;
        }
    }

    pub fn handle_keyboard_seek(&mut self, new_position_ns: u64, engine: &PlayerHandle) {
        engine.send(PlayerCommand::Seek(new_position_ns));
    }

    pub fn start_seeking(&mut self, preview_ns: u64, engine: &PlayerHandle) {
        self.playback.is_dragging_seek = true;
        self.playback.last_seek_sent_ns = Some(preview_ns);
        engine.send(PlayerCommand::Seek(preview_ns));
    }

    pub fn update_seek_preview(&mut self, seek_ns: u64, engine: &PlayerHandle) {
        // Only send Seek to the engine when the position changes meaningfully
        // to avoid flooding the player with seek commands every frame.
        let should_seek = match self.playback.last_seek_sent_ns {
            Some(prev) => seek_ns.abs_diff(prev) > 1_000_000,
            None => true,
        };
        if should_seek {
            self.playback.last_seek_sent_ns = Some(seek_ns);
            engine.send(PlayerCommand::Seek(seek_ns));
        }
    }

    pub fn release_seek(&mut self) {
        self.playback.is_dragging_seek = false;
        self.playback.last_seek_sent_ns = None;
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
