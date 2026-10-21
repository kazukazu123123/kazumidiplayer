use crate::player::{PlayerCommand, PlayerState};
use crate::ui_components::button::draw_button;
use crate::ui_components::combo_box::{
    ComboBoxState, draw_combo_box, draw_combo_box_popup,
};
use crate::ui_components::numeric_slider::{
    NumericSliderInput, NumericSliderState, draw_u32_numeric_slider,
};
use crate::ui_components::scrollbar::draw_scrollbar;
use crate::ui_components::toggle::draw_toggle;
use crate::utils::{cubic_bezier, format_bytes, format_count, format_count_short, format_time};
use crate::window_manager::InputEvent;
use raylib::prelude::*;
use std::sync::mpsc::Sender;

pub fn draw_progress_bar(
    d: &mut impl RaylibDraw,
    playback_position_ns: u64,
    weighted_position_ns: u64,
    total_weighted_ns: u64,
    bar_x: f32,
    bar_y: f32,
) {
    let display_position_ns = playback_position_ns;
    let time_text = format_time(display_position_ns);
    let measure_text = crate::utils::format_measures(weighted_position_ns, total_weighted_ns);
    let combined_text = format!("{}  [{}]", time_text, measure_text);

    d.draw_text(&combined_text, bar_x as i32, bar_y as i32, 20, Color::WHITE);
}

pub fn draw_seek_bar(
    d: &mut impl RaylibDraw,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    is_loading: bool,
    is_midi_loaded: bool,
    playback_position_ns: u64,
    duration_ns: u64,
    input_event: Option<InputEvent>,
    // Note-generation progress (furthest generated tick / whole MIDI), 0..1.
    // Drawn as a colored span under the playback fill so the scan is visible.
    gen_frac: Option<f32>,
) -> bool {
    let seek_bar_bg_color = if is_loading {
        Color::DARKGRAY
    } else {
        Color::GRAY
    };
    let rect = Rectangle::new(x, y, w, h);
    d.draw_rectangle(x as i32, y as i32, w as i32, h as i32, seek_bar_bg_color);

    if is_midi_loaded && !is_loading {
        // Note-generation span, under the playback fill.
        if let Some(frac) = gen_frac {
            crate::ui_components::progress_bar::draw_progress_bar(
                d,
                x,
                y,
                w,
                h,
                &[(0.0, frac, Color::new(70, 90, 150, 255))],
                None,
                Color::new(0, 0, 0, 0),
            );
        }

        // Playback progress
        let current_position_ns = playback_position_ns;
        // f64 throughout: an f32 only carries ~7 significant digits, so on a MIDI
        // hours long the nanosecond counts round to millisecond-sized steps and the
        // bar moves in visible jumps.
        let fill = ((current_position_ns as f64 / duration_ns as f64) * w as f64) as i32;
        d.draw_rectangle(x as i32, y as i32, fill, h as i32, Color::GREEN);

        // Check for seek initiation
        if let Some(InputEvent::MouseDown(p)) = input_event {
            if rect.check_collision_point_rec(Vector2::new(p.x as f32, p.y as f32)) {
                return true;
            }
        }
    }
    false
}

pub fn draw_media_controls(
    d: &mut impl RaylibDraw,
    tx: &Sender<PlayerCommand>,
    is_loading: bool,
    is_midi_loaded: bool,
    current_state: PlayerState,
    playback_position_ns: u64,
    duration_ns: u64,
    x_offset: f32,
    y_offset: f32,
    input_event: Option<InputEvent>,
    mouse_pos: Option<Vector2>,
    is_mouse_down: bool,
    playback_speed: &mut f32,
    speed_slider_dragging: &mut bool,
    content_right: f32,
) {
    let button_w = 80.0;
    let button_h = 30.0;
    let spacing = 10.0;

    let play_pause_rect = Rectangle::new(x_offset, y_offset, button_w, button_h);
    let stop_rect = Rectangle::new(x_offset + button_w + spacing, y_offset, button_w, button_h);

    let is_playing = current_state == PlayerState::Playing;
    let can_play_pause = !is_loading && is_midi_loaded;
    let can_stop = !is_loading
        && (current_state == PlayerState::Playing || current_state == PlayerState::Paused);

    let click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
        Some(Vector2::new(p.x as f32, p.y as f32))
    } else {
        None
    };

    // Play/Pause Button
    let pp_text = if is_playing { "Pause" } else { "Play" };
    if draw_button(
        d,
        play_pause_rect,
        pp_text,
        18,
        can_play_pause,
        1.0,
        click_pos,
        mouse_pos,
    ) {
        if is_playing {
            tx.send(PlayerCommand::Pause).ok();
        } else {
            if current_state == PlayerState::Stopped && playback_position_ns >= duration_ns {
                tx.send(PlayerCommand::Seek(0)).ok();
            }
            tx.send(PlayerCommand::Play).ok();
        }
    }

    // Stop Button
    if draw_button(
        d, stop_rect, "Stop", 18, can_stop, 1.0, click_pos, mouse_pos,
    ) {
        tx.send(PlayerCommand::Stop).ok();
    }

    // Playback speed slider: starts right after the transport buttons and stretches
    // to fill the remaining content width, so it grows/shrinks with the window.
    const SPEED_MIN: f32 = 0.05;
    const SPEED_MAX: f32 = 4.0;
    let buttons_right = stop_rect.x + button_w + spacing;
    let slider_x = buttons_right;
    let slider_w = (content_right - slider_x).max(40.0);
    let track_y = y_offset + 18.0;
    let text_color = Color::new(230, 230, 230, 255);

    d.draw_text(
        format!("{:.2}x", *playback_speed).as_str(),
        slider_x as i32,
        y_offset as i32,
        14,
        text_color,
    );
    d.draw_rectangle(
        slider_x as i32,
        track_y as i32,
        slider_w as i32,
        4,
        Color::new(80, 80, 80, 255),
    );
    let t = ((*playback_speed - SPEED_MIN) / (SPEED_MAX - SPEED_MIN)).clamp(0.0, 1.0);
    let thumb_x = slider_x + t * slider_w;
    let thumb_rect = Rectangle::new(thumb_x - 5.0, track_y - 6.0, 10.0, 16.0);
    d.draw_rectangle_rec(thumb_rect, Color::new(0, 120, 215, 255));

    let hit_rect = Rectangle::new(slider_x, track_y - 6.0, slider_w, 16.0);
    if let Some(cp) = click_pos {
        if thumb_rect.check_collision_point_rec(cp) || hit_rect.check_collision_point_rec(cp) {
            *speed_slider_dragging = true;
        }
    }
    if *speed_slider_dragging {
        if is_mouse_down {
            if let Some(mp) = mouse_pos {
                let clamped_x = mp.x.clamp(slider_x, slider_x + slider_w);
                let new_t = (clamped_x - slider_x) / slider_w;
                let mut v = SPEED_MIN + new_t * (SPEED_MAX - SPEED_MIN);
                v = (v / 0.05).round() * 0.05;
                if (v - 1.0).abs() < 0.08 {
                    v = 1.0;
                }
                *playback_speed = v.clamp(SPEED_MIN, SPEED_MAX);
            }
        } else {
            *speed_slider_dragging = false;
        }
    }
}

/// The loading panel's size in its own (unscaled) coordinate space. The panel is drawn
/// under a caller-supplied camera so the same body can sit centered while parsing and
/// then fly to the corner as a toast.
pub const LOADING_PANEL_W: f32 = 400.0;
pub const LOADING_PANEL_H: f32 = 172.0;

pub fn draw_loading_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    t: f32,
    current_progress: &Option<crate::midi_parser_worker::ParseProgress>,
    parse_bps: f64,
    parse_eps: f64,
) {
    let eased_t = 1.0 - (1.0 - t).powi(2);
    let scale = 0.95 + 0.05 * eased_t;

    let camera = Camera2D {
        target: Vector2::new(LOADING_PANEL_W / 2.0, LOADING_PANEL_H / 2.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    draw_loading_panel(
        d,
        camera,
        eased_t,
        "Opening MIDI File...",
        current_progress,
        parse_bps,
        parse_eps,
    );
}

/// Draws the parse panel body. `alpha_factor` (0..1) scales every color's alpha, and
/// The terminal panel shown when the player thread has died. It offers no action
/// because there is none: the thread is gone, and every command the UI could send
/// would be dropped on the floor. It exists so the app says what happened instead of
/// going quiet and ignoring clicks.
pub fn draw_player_crashed_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    fade_t: f32,
) {
    let a = |v: f32| (v * fade_t.clamp(0.0, 1.0)) as u8;

    let w = 520.0f32.min(screen_width as f32 - 40.0);
    let h = 150.0;
    let x = (screen_width as f32 - w) / 2.0;
    let y = (screen_height as f32 - h) / 2.0;
    let panel = Rectangle::new(x, y, w, h);

    d.draw_rectangle_rec(panel, Color::new(40, 20, 22, a(245.0)));
    d.draw_rectangle_lines_ex(panel, 1.0, Color::new(220, 70, 70, a(255.0)));

    let title = Color::new(255, 110, 110, a(255.0));
    let body = Color::new(225, 215, 215, a(255.0));
    d.draw_text(
        "Player thread crashed",
        x as i32 + 20,
        y as i32 + 22,
        24,
        title,
    );
    d.draw_text(
        "Playback, seeking and loading no longer work.",
        x as i32 + 20,
        y as i32 + 62,
        18,
        body,
    );
    d.draw_text(
        "Restart the app. The panic is printed on the console.",
        x as i32 + 20,
        y as i32 + 88,
        18,
        body,
    );
}

/// Non-blocking banner drawn when OmniMIDI / KDMAPI was not found.
/// Fades out after 5 seconds. Unlike the PlayerCrashed overlay the UI stays
/// fully interactive.
pub fn draw_kdmapi_missing_banner(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    _screen_height: i32,
    timer: f32,
) {
    const DURATION: f32 = 5.0;
    const FADE: f32 = 0.8;
    let alpha = if timer < DURATION {
        210
    } else {
        let t = (timer - DURATION) / FADE;
        (210.0 * (1.0 - t).clamp(0.0, 1.0)) as u8
    };
    if alpha == 0 {
        return;
    }
    d.draw_rectangle(0, 0, screen_width, 28, Color::new(60, 30, 30, alpha));
    d.draw_text(
        "KDMAPI (OmniMIDI) not found, audio output disabled",
        16,
        6,
        16,
        Color::new(255, 180, 180, alpha),
    );
}

/// `camera` decides where and at what scale it lands.
pub fn draw_loading_panel(
    d: &mut RaylibDrawHandle,
    camera: Camera2D,
    alpha_factor: f32,
    title: &str,
    current_progress: &Option<crate::midi_parser_worker::ParseProgress>,
    parse_bps: f64,
    parse_eps: f64,
) {
    let eased_t = alpha_factor.clamp(0.0, 1.0);

    let panel_rect = Rectangle::new(0.0, 0.0, LOADING_PANEL_W, LOADING_PANEL_H);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    {
        let mut d2d = d.begin_mode2D(camera);

        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();
        let spacing = 1.0;

        d2d.draw_text_ex(
            &font,
            title,
            Vector2::new(20.0, 20.0),
            20.0,
            spacing,
            text_color,
        );

        if let Some(progress) = current_progress {
            // Bytes consumed is the smoothest completion fraction the parser exposes: it
            // advances within a track, so it keeps moving on single-huge-track MIDIs where
            // the track count would sit at 0/1 until the very end.
            let progress_value = if progress.total_bytes > 0 {
                progress.bytes_done as f32 / progress.total_bytes as f32
            } else {
                0.0
            };
            let progress_text = format!(
                "{} / {}  ({:.0}%)",
                format_bytes(progress.bytes_done),
                format_bytes(progress.total_bytes),
                progress_value * 100.0,
            );

            let bar_x = 20.0;
            let bar_y = 56.0;
            let bar_w = 360.0;
            let bar_h = 24.0;

            crate::ui_components::progress_bar::draw_progress_bar(
                &mut d2d,
                bar_x,
                bar_y,
                bar_w,
                bar_h,
                &[(
                    0.0,
                    progress_value,
                    Color::new(0, 120, 215, (255.0 * eased_t) as u8),
                )],
                Some(&progress_text),
                Color::new(80, 80, 80, (255.0 * eased_t) as u8),
            );

            let events = progress.events_done;
            let eps = parse_eps.max(0.0) as u64;
            let label_color = Color::new(140, 140, 140, (255.0 * eased_t) as u8);
            let value_color = Color::new(210, 210, 210, (255.0 * eased_t) as u8);

            // 2x2 grid: each stat sits in a fixed cell, so the values line up in two
            // columns instead of drifting with the width of whatever precedes them.
            let cells: [(&str, String); 4] = [
                (
                    "Tracks",
                    format!("{} / {}", progress.tracks_done, progress.total_tracks),
                ),
                (
                    "Events",
                    format!("{} ({})", format_count_short(events), format_count(events)),
                ),
                (
                    "Speed",
                    format!("{}/s", format_bytes(parse_bps.max(0.0) as u64)),
                ),
                (
                    "Events/s",
                    format!("{} ({})", format_count_short(eps), format_count(eps)),
                ),
            ];

            let grid_x = bar_x;
            let grid_y = bar_y + bar_h + 10.0;
            let col_w = bar_w / 2.0;
            let row_h = 32.0;

            for (i, (label, value)) in cells.iter().enumerate() {
                let x = grid_x + (i % 2) as f32 * col_w;
                let y = grid_y + (i / 2) as f32 * row_h;

                // A multi-million-event count is wider than its column, so clip each cell to
                // its own cell box: a long value is cut off rather than running into the
                // neighbouring column or past the panel edge.
                let clip_tl = d2d.get_world_to_screen2D(Vector2::new(x, y), camera);
                let clip_br =
                    d2d.get_world_to_screen2D(Vector2::new(x + col_w - 8.0, y + row_h), camera);
                let mut d_clip = d2d.begin_scissor_mode(
                    clip_tl.x as i32,
                    clip_tl.y as i32,
                    (clip_br.x - clip_tl.x) as i32,
                    (clip_br.y - clip_tl.y) as i32,
                );

                d_clip.draw_text_ex(&font, label, Vector2::new(x, y), 12.0, spacing, label_color);
                d_clip.draw_text_ex(
                    &font,
                    value,
                    Vector2::new(x, y + 14.0),
                    16.0,
                    spacing,
                    value_color,
                );
            }
        }
    }
}

/// Draws the finished-parse panel on its way out: it swings from the center of the
/// screen down to the bottom-right corner, sits there, then fades. `fly_t` is 0 at the
/// center and 1 at the corner; `alpha` fades the whole panel at the end.
pub fn draw_parse_toast(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    fly_t: f32,
    alpha: f32,
    progress: &Option<crate::midi_parser_worker::ParseProgress>,
    parse_bps: f64,
    parse_eps: f64,
) {
    const MARGIN: f32 = 16.0;

    // The swing, as a cubic-bezier: it accelerates out of the center, drifts past the
    // corner, then eases back into it. Tune the feel here: y1 > 1.0 is the overshoot,
    // and the second control point's late x is what gives the settle its weight.
    let eased = cubic_bezier(0.34, 1.28, 0.64, 1.0, fly_t);

    // The panel keeps its size the whole way: shrinking it made the text unreadable.
    let half_w = LOADING_PANEL_W / 2.0;
    let half_h = LOADING_PANEL_H / 2.0;

    // Both endpoints are where the panel's *center* sits on screen, so the overshoot
    // never pushes the panel off the visible area by more than the easing allows.
    let start = Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0);
    let end = Vector2::new(
        screen_width as f32 - MARGIN - half_w,
        screen_height as f32 - MARGIN - half_h,
    );
    let offset = Vector2::new(
        start.x + (end.x - start.x) * eased,
        start.y + (end.y - start.y) * eased,
    );

    let camera = Camera2D {
        target: Vector2::new(LOADING_PANEL_W / 2.0, LOADING_PANEL_H / 2.0),
        offset,
        rotation: 0.0,
        zoom: 1.0,
    };

    draw_loading_panel(
        d,
        camera,
        alpha,
        "Loaded!",
        progress,
        parse_bps,
        parse_eps,
    );
}

pub fn draw_settings_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    alpha_factor: f32, // Linear timer 0.0 to 1.0
    input_event: Option<InputEvent>,
    mouse_pos: Vector2,
    is_mouse_down: bool,
    is_tick_based: &mut bool,
    tick_based_animation: f32,
    beat_bounce_enabled: &mut bool,
    beat_bounce_animation: f32,
    beat_bounce_amount: &mut u32,
    beat_bounce_amount_slider: &mut NumericSliderState,
    beat_bounce_note_value: &mut u32,
    beat_bounce_note_value_slider: &mut NumericSliderState,
    beat_bounce_offbeat: &mut bool,
    beat_bounce_offbeat_animation: f32,
    shake_enabled: &mut bool,
    shake_animation: f32,
    integrate_keyboard: &mut bool,
    keyboard_animation: f32,
    scroll_y: &mut f32,
    mouse_wheel: f32,
    autoplay_on_load: &mut bool,
    autoplay_animation: f32,
    pitch_bend_window_enabled: &mut bool,
    pitch_bend_window_animation: f32,
    pitch_bend_move_window_enabled: &mut bool,
    pitch_bend_move_window_animation: f32,
    pitch_bend_spring_parameters: &mut [u32; 3],
    pitch_bend_spring_sliders: &mut [NumericSliderState; 3],
    pitch_bend_override: &mut bool,
    pitch_bend_override_animation: f32,
    pitch_bend_multiplier: &mut u8,
    unlimited_output: &mut bool,
    unlimited_output_animation: f32,
    midi_ev_buffer_size: &mut u32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    pb_multiplier_slider: &mut NumericSliderState,
    midi_ev_buffer_size_slider: &mut NumericSliderState,
    audio_engine: &mut usize,
    audio_engine_combo: &mut ComboBoxState,
    midi_ev_mode: &mut usize,
    midi_ev_mode_combo: &mut ComboBoxState,
    parse_threads: &mut u32,
    parse_threads_slider_dragging: &mut bool,
    scan_delay_ms: &mut u32,
    scan_delay_slider_dragging: &mut bool,
    show_stats: &mut bool,
    show_stats_animation: f32,
) -> bool {
    // Capture text-entry input before entering drawing modes.
    let mut numeric_typed = Vec::new();
    while let Some(ch) = d.get_char_pressed() {
        numeric_typed.push(ch);
    }
    let numeric_backspace = d.is_key_pressed(KeyboardKey::KEY_BACKSPACE);
    let numeric_enter = d.is_key_pressed(KeyboardKey::KEY_ENTER);
    let numeric_escape = d.is_key_pressed(KeyboardKey::KEY_ESCAPE);
    let numeric_shift =
        d.is_key_down(KeyboardKey::KEY_LEFT_SHIFT) || d.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
    let numeric_control = d.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
        || d.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);

    // Ease-out cubic: 1 - (1 - t)^3
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);

    // Panel drawing with Camera2D for perfect scaling and no drift
    let scale = 0.95 + 0.05 * eased_t;
    let camera = Camera2D {
        target: Vector2::new(200.0, 175.0), // Center of 400x350 panel
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0), // Center of screen
        rotation: 0.0,
        zoom: scale,
    };

    let mut close_clicked = false;
    let panel_rect = Rectangle::new(0.0, 0.0, 400.0, 350.0);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    // Scroll area configuration
    let content_top = 50.0;
    let content_bottom = 300.0;
    let viewport_height = content_bottom - content_top;

    // Simple vertical layout engine: tracks next Y and max content height automatically
    struct VerticalLayout {
        y: f32,
        margin: f32,
        max_bottom: f32,
    }
    impl VerticalLayout {
        fn new(start_y: f32, margin: f32) -> Self {
            Self {
                y: start_y,
                margin,
                max_bottom: start_y,
            }
        }
        fn next(&mut self, height: f32) -> f32 {
            let y = self.y;
            self.y += height + self.margin;
            self.max_bottom = self.y - self.margin;
            y
        }
        fn content_height(&self, bottom_padding: f32) -> f32 {
            self.max_bottom + bottom_padding
        }
    }
    // Pre-calculate content height for scrollbar
    let mut measure_layout = VerticalLayout::new(content_top, 5.0);
    measure_layout.next(18.0);
    measure_layout.next(30.0); // Tick Based Mode
    measure_layout.next(30.0); // Beat Bounce
    if *beat_bounce_enabled {
        measure_layout.next(52.0); // Beat Bounce Amount
        measure_layout.next(52.0); // Beat Bounce Note Value
        measure_layout.next(30.0); // Beat Bounce Offbeat
    }
    measure_layout.next(30.0); // Shaking Window
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(52.0); // MIDI EV Buffer Size slider
    measure_layout.next(52.0); // EV buffer mode combo box
    measure_layout.next(30.0); // Pitch Bend OS Window Movement
    if *pitch_bend_move_window_enabled {
        for _ in 0..3 {
            measure_layout.next(52.0);
        }
    }
    if *pitch_bend_override {
        measure_layout.next(52.0);
    }
    measure_layout.next(18.0); // "Parsing:" label
    measure_layout.next(38.0); // Parse Threads slider
    measure_layout.next(18.0); // "Statistics:" label
    measure_layout.next(30.0); // Show Statistics toggle
    measure_layout.next(38.0); // Scan Delay slider (debug)
    let content_height = measure_layout.content_height(20.0);
    let max_scroll = f32::max(content_height - viewport_height, 0.0);

    let mut layout = VerticalLayout::new(content_top, 5.0);

    {
        let mut d2d = d.begin_mode2D(camera);

        // Panel Background and Border
        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();
        let spacing = 1.0;

        // Title (outside scroll area)
        d2d.draw_text_ex(
            &font,
            "Settings",
            Vector2::new(20.0, 20.0),
            24.0,
            spacing,
            text_color,
        );

        let mouse_world_x = (mouse_pos.x - camera.offset.x) / scale + camera.target.x;
        let mouse_world_y = (mouse_pos.y - camera.offset.y) / scale + camera.target.y;
        let mouse_world_pos = Vector2::new(mouse_world_x, mouse_world_y);

        let mut click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
            let mouse_world_x = (p.x as f32 - camera.offset.x) / scale + camera.target.x;
            let mouse_world_y = (p.y as f32 - camera.offset.y) / scale + camera.target.y;
            Some(Vector2::new(mouse_world_x, mouse_world_y))
        } else {
            None
        };

        // Scroll with mouse wheel when hovering the panel content area. The numeric
        // EV control owns the wheel while hovered for exact stepping.
        let scroll_area_rect = Rectangle::new(0.0, content_top, panel_rect.width, viewport_height);
        let conditional_beat_height = if *beat_bounce_enabled { 149.0 } else { 0.0 };
        let beat_amount_rect = Rectangle::new(20.0, 143.0 - *scroll_y, 275.0, 144.0);
        let midi_ev_rect = Rectangle::new(
            20.0,
            283.0 + conditional_beat_height - *scroll_y,
            275.0,
            52.0,
        );
        if scroll_area_rect.check_collision_point_rec(mouse_world_pos)
            && !midi_ev_rect.check_collision_point_rec(mouse_world_pos)
            && !(*beat_bounce_enabled
                && beat_amount_rect.check_collision_point_rec(mouse_world_pos))
            && mouse_wheel != 0.0
        {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        let track_rect = Rectangle::new(panel_rect.width - 14.0, content_top, 8.0, viewport_height);
        draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height - content_top,
            viewport_height,
            scroll_y,
            scrollbar_dragging,
            scrollbar_drag_start_y,
            scrollbar_drag_start_scroll,
            mouse_world_pos,
            click_pos,
            is_mouse_down,
            eased_t,
        );

        let sy = |y: f32| y - *scroll_y;

        let is_visible = |y: f32, h: f32| {
            let bottom = y + h;
            let top = y;
            bottom >= content_top && top <= content_bottom
        };

        // Scissor: clip scrollable content to the content area
        {
            let mut midi_ev_mode_combo_rect = None;
            let mut audio_engine_combo_rect = None;
            let clip_tl = d2d.get_world_to_screen2D(Vector2::new(0.0, content_top), camera);
            let clip_br =
                d2d.get_world_to_screen2D(Vector2::new(panel_rect.width, content_bottom), camera);
            let sx = clip_tl.x as i32;
            let sy_scissor = clip_tl.y as i32;
            let sw = (clip_br.x - clip_tl.x) as i32;
            let sh = (clip_br.y - clip_tl.y) as i32;
            let mut d_scissor = d2d.begin_scissor_mode(sx, sy_scissor, sw, sh);

            // Label for the section
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Rendering:",
                    Vector2::new(20.0, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Render Mode Toggle
            let y = layout.next(30.0);
            let toggle_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(toggle_rect.y, toggle_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    toggle_rect,
                    "Tick Based Mode",
                    tick_based_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *is_tick_based = !*is_tick_based;
                }
            }

            // Beat Bounce pulses only the piano-roll zoom at the selected note interval.
            let y = layout.next(30.0);
            let beat_bounce_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(beat_bounce_rect.y, beat_bounce_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    beat_bounce_rect,
                    "Beat Bounce",
                    beat_bounce_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *beat_bounce_enabled = !*beat_bounce_enabled;
                }
            }

            if *beat_bounce_enabled {
                let y = layout.next(52.0);
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(20.0, sy(y), 275.0, 52.0),
                        "Beat Bounce Amount (%)",
                        beat_bounce_amount,
                        0,
                        25,
                        beat_bounce_amount_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }

                let y = layout.next(52.0);
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(20.0, sy(y), 275.0, 52.0),
                        "Beat Interval (1/N Note)",
                        beat_bounce_note_value,
                        1,
                        64,
                        beat_bounce_note_value_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }

                let y = layout.next(30.0);
                let offbeat_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
                if is_visible(offbeat_rect.y, offbeat_rect.height) {
                    if draw_toggle(
                        &mut d_scissor,
                        offbeat_rect,
                        "Offbeat",
                        beat_bounce_offbeat_animation,
                        true,
                        eased_t,
                        click_pos,
                        Some(mouse_world_pos),
                    ) {
                        *beat_bounce_offbeat = !*beat_bounce_offbeat;
                    }
                }
            }

            // Shaking Window Toggle
            let y = layout.next(30.0);
            let shake_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(shake_rect.y, shake_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    shake_rect,
                    "Shaking Window",
                    shake_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *shake_enabled = !*shake_enabled;
                }
            }

            // Integrate Keyboard Toggle
            let y = layout.next(30.0);
            let integrate_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(integrate_rect.y, integrate_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    integrate_rect,
                    "Integrate Keyboard",
                    keyboard_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *integrate_keyboard = !*integrate_keyboard;
                }
            }

            // Autoplay on Load Toggle
            let y = layout.next(30.0);
            let autoplay_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(autoplay_rect.y, autoplay_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    autoplay_rect,
                    "Autoplay on Load",
                    autoplay_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *autoplay_on_load = !*autoplay_on_load;
                }
            }

            // Audio engine selector
            let y = layout.next(52.0);
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(20.0, sy(y), 275.0, 52.0);
                audio_engine_combo_rect = Some(combo_rect);
                let was_open = audio_engine_combo.open;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "Audio Engine",
                    &crate::audio_engine::AudioEngine::LABELS,
                    audio_engine,
                    audio_engine_combo,
                    click_pos,
                    eased_t,
                    true,
                    None,
                );

                // Same click shielding as the EV buffer mode popup below.
                if let Some(click) = click_pos {
                    let header = Rectangle::new(
                        combo_rect.x,
                        combo_rect.y + 20.0,
                        combo_rect.width,
                        28.0,
                    );
                    let popup = Rectangle::new(
                        header.x,
                        header.y + header.height,
                        header.width,
                        28.0 * crate::audio_engine::AudioEngine::LABELS.len() as f32,
                    );
                    if header.check_collision_point_rec(click)
                        || (was_open && popup.check_collision_point_rec(click))
                    {
                        click_pos = None;
                    }
                }
            }

            // Unlimited Output Toggle
            let y = layout.next(30.0);
            let unlimited_output_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(unlimited_output_rect.y, unlimited_output_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    unlimited_output_rect,
                    "Unlimited Output",
                    unlimited_output_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *unlimited_output = !*unlimited_output;
                }
            }

            // MIDI events consumed per 60 Hz output frame.
            {
                let y = layout.next(52.0);
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let slider_enabled = !*unlimited_output && *midi_ev_mode != 0;
                    let mut fixed_value = if *midi_ev_mode == 0 { 8192 } else { 1 };
                    let displayed_value = if slider_enabled {
                        &mut *midi_ev_buffer_size
                    } else {
                        &mut fixed_value
                    };
                    let label = if *midi_ev_mode == 0 {
                        "Events Per Pass"
                    } else {
                        "MIDI EV Buffer Size"
                    };
                    let disabled_text = if *unlimited_output {
                        "Bypassed (Direct)"
                    } else {
                        "8192 (Fixed)"
                    };
                    draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(20.0, sy(y), 275.0, 52.0),
                        label,
                        displayed_value,
                        crate::player::MIN_MIDI_EV_BUFFER_SIZE,
                        crate::player::MAX_MIDI_EV_BUFFER_SIZE,
                        midi_ev_buffer_size_slider,
                        &input,
                        eased_t,
                        slider_enabled,
                        Some(disabled_text),
                    );
                }
            }

            let y = layout.next(52.0);
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(20.0, sy(y), 275.0, 52.0);
                midi_ev_mode_combo_rect = Some(combo_rect);
                let was_open = midi_ev_mode_combo.open && !*unlimited_output;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "EV Buffer Mode",
                    &["Legacy (No Buffer)", "Buffered: Slowdown", "Buffered: Catch Up"],
                    midi_ev_mode,
                    midi_ev_mode_combo,
                    click_pos,
                    eased_t,
                    !*unlimited_output,
                    Some("Bypassed (Direct)"),
                );

                // The popup overlays later settings, so clicks handled by its header
                // or option list must not continue into controls underneath it.
                if let Some(click) = click_pos {
                    let header = Rectangle::new(
                        combo_rect.x,
                        combo_rect.y + 20.0,
                        combo_rect.width,
                        28.0,
                    );
                    let popup = Rectangle::new(
                        header.x,
                        header.y + header.height,
                        header.width,
                        28.0 * 3.0,
                    );
                    if header.check_collision_point_rec(click)
                        || (was_open && popup.check_collision_point_rec(click))
                    {
                        click_pos = None;
                    }
                }
            }

            // Pitch Bend Window Toggle
            let y = layout.next(30.0);
            let pitch_bend_window_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(pitch_bend_window_rect.y, pitch_bend_window_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pitch_bend_window_rect,
                    "Pitch Bend Window",
                    pitch_bend_window_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *pitch_bend_window_enabled = !*pitch_bend_window_enabled;
                }
            }

            // Pitch Bend OS Window Movement Toggle
            let y = layout.next(30.0);
            let pb_move_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(pb_move_rect.y, pb_move_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pb_move_rect,
                    "PB Move OS Window",
                    pitch_bend_move_window_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *pitch_bend_move_window_enabled = !*pitch_bend_move_window_enabled;
                }
            }

            if *pitch_bend_move_window_enabled {
                for (index, (label, min, max)) in [
                    ("Spring Mass", 1, 20),
                    ("Spring Stiffness", 1, 1000),
                    ("Spring Damping", 0, 200),
                ].into_iter().enumerate() {
                    let y = layout.next(52.0);
                    if is_visible(sy(y), 52.0) {
                        let input = NumericSliderInput {
                            click_pos,
                            mouse_pos: mouse_world_pos,
                            mouse_down: is_mouse_down,
                            wheel: mouse_wheel,
                            shift: numeric_shift,
                            control: numeric_control,
                            typed: numeric_typed.clone(),
                            backspace: numeric_backspace,
                            enter: numeric_enter,
                            escape: numeric_escape,
                        };
                        draw_u32_numeric_slider(
                            &mut d_scissor,
                            Rectangle::new(20.0, sy(y), 275.0, 52.0),
                            label,
                            &mut pitch_bend_spring_parameters[index],
                            min,
                            max,
                            &mut pitch_bend_spring_sliders[index],
                            &input,
                            eased_t,
                            true,
                            None,
                        );
                    }
                }
            }

            // Pitch Bend Override Toggle
            let y = layout.next(30.0);
            let pb_override_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(pb_override_rect.y, pb_override_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pb_override_rect,
                    "PB Range Override",
                    pitch_bend_override_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    let was_enabled = *pitch_bend_override;
                    *pitch_bend_override = !was_enabled;
                    if was_enabled {
                        *pitch_bend_multiplier = 12;
                    } else {
                        *pitch_bend_multiplier = (*pitch_bend_multiplier).max(1);
                    }
                }
            }

            // Pitch Bend Multiplier Slider (only visible when override is on)
            if *pitch_bend_override {
                let y = layout.next(52.0);
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let mut multiplier = *pitch_bend_multiplier as u32;
                    draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(20.0, sy(y), 275.0, 52.0),
                        "PB Multiplier",
                        &mut multiplier,
                        1,
                        127,
                        pb_multiplier_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                    *pitch_bend_multiplier = multiplier.clamp(1, 127) as u8;
                }
            }

            // Parsing section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Parsing:",
                    Vector2::new(20.0, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Parse Threads slider: 1..=hardware threads. Applies to the next load.
            {
                let max_threads = crate::app_state::cpu_threads();
                let y = layout.next(38.0);
                if is_visible(sy(y), 38.0) {
                    *parse_threads = (*parse_threads).clamp(1, max_threads);
                    let slider_rect = Rectangle::new(20.0, sy(y) + 18.0, 200.0, 20.0);
                    let label = format!("Parse Threads: {} / {}", *parse_threads, max_threads);
                    d_scissor.draw_text_ex(
                        &font,
                        &label,
                        Vector2::new(slider_rect.x, sy(y)),
                        16.0,
                        1.0,
                        text_color,
                    );
                    d_scissor.draw_rectangle(
                        slider_rect.x as i32,
                        (slider_rect.y + 8.0) as i32,
                        slider_rect.width as i32,
                        4,
                        Color::new(80, 80, 80, (255.0 * eased_t) as u8),
                    );
                    // A single-core machine has no range to drag: pin the thumb at the end.
                    let span = (max_threads - 1) as f32;
                    let t = if span > 0.0 {
                        ((*parse_threads - 1) as f32 / span).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let thumb_x = slider_rect.x + t * slider_rect.width;
                    let thumb_rect = Rectangle::new(thumb_x - 6.0, slider_rect.y + 2.0, 12.0, 16.0);
                    d_scissor.draw_rectangle_rec(
                        thumb_rect,
                        Color::new(0, 120, 215, (255.0 * eased_t) as u8),
                    );

                    if let Some(cp) = click_pos {
                        if thumb_rect.check_collision_point_rec(cp)
                            || slider_rect.check_collision_point_rec(cp)
                        {
                            *parse_threads_slider_dragging = true;
                        }
                    }

                    if *parse_threads_slider_dragging {
                        if is_mouse_down {
                            if span > 0.0 {
                                let clamped_x = mouse_world_pos
                                    .x
                                    .clamp(slider_rect.x, slider_rect.x + slider_rect.width);
                                let new_t = (clamped_x - slider_rect.x) / slider_rect.width;
                                *parse_threads = (new_t * span).round() as u32 + 1;
                            }
                        } else {
                            *parse_threads_slider_dragging = false;
                        }
                    }
                }
            }

            // Statistics section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Statistics:",
                    Vector2::new(20.0, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Show Statistics toggle
            let y = layout.next(30.0);
            let show_stats_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(show_stats_rect.y, show_stats_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    show_stats_rect,
                    "Show Statistics",
                    show_stats_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *show_stats = !*show_stats;
                }
            }

            // Scan Delay slider (debug): artificially slow the piano-roll scan.
            const SCAN_DELAY_MAX: f32 = 1000.0; // milliseconds
            {
                let y = layout.next(38.0);
                if is_visible(sy(y), 38.0) {
                    let slider_rect = Rectangle::new(20.0, sy(y) + 18.0, 200.0, 20.0);
                    let label = format!("Scan Delay: {} ms", *scan_delay_ms);
                    d_scissor.draw_text_ex(
                        &font,
                        &label,
                        Vector2::new(slider_rect.x, sy(y)),
                        16.0,
                        1.0,
                        text_color,
                    );
                    d_scissor.draw_rectangle(
                        slider_rect.x as i32,
                        (slider_rect.y + 8.0) as i32,
                        slider_rect.width as i32,
                        4,
                        Color::new(80, 80, 80, (255.0 * eased_t) as u8),
                    );
                    let t = (*scan_delay_ms as f32 / SCAN_DELAY_MAX).clamp(0.0, 1.0);
                    let thumb_x = slider_rect.x + t * slider_rect.width;
                    let thumb_rect = Rectangle::new(thumb_x - 6.0, slider_rect.y + 2.0, 12.0, 16.0);
                    d_scissor.draw_rectangle_rec(
                        thumb_rect,
                        Color::new(0, 120, 215, (255.0 * eased_t) as u8),
                    );

                    if let Some(cp) = click_pos {
                        if thumb_rect.check_collision_point_rec(cp)
                            || slider_rect.check_collision_point_rec(cp)
                        {
                            *scan_delay_slider_dragging = true;
                        }
                    }

                    if *scan_delay_slider_dragging {
                        if is_mouse_down {
                            let clamped_x = mouse_world_pos
                                .x
                                .clamp(slider_rect.x, slider_rect.x + slider_rect.width);
                            let new_t = (clamped_x - slider_rect.x) / slider_rect.width;
                            *scan_delay_ms = (new_t * SCAN_DELAY_MAX).round() as u32;
                        } else {
                            *scan_delay_slider_dragging = false;
                        }
                    }
                }
            }

            // Popups are deliberately drawn last so they float above the controls below.
            if let Some(combo_rect) = audio_engine_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &crate::audio_engine::AudioEngine::LABELS,
                    audio_engine_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
            if let Some(combo_rect) = midi_ev_mode_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &["Legacy (No Buffer)", "Buffered: Slowdown", "Buffered: Catch Up"],
                    midi_ev_mode_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
        }

        // Close button (fixed coordinates, outside scroll area)
        let btn_rect = Rectangle::new(300.0, 310.0, 80.0, 30.0);
        if draw_button(
            &mut d2d,
            btn_rect,
            "Close",
            18,
            true,
            eased_t,
            click_pos,
            Some(mouse_world_pos),
        ) {
            close_clicked = true;
        } else if let Some(pos) = click_pos {
            if !panel_rect.check_collision_point_rec(pos) {
                close_clicked = true;
            }
        }
    }

    close_clicked
}

pub fn draw_text_editor_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    alpha_factor: f32,
    input_event: Option<InputEvent>,
    mouse_pos: Vector2,
    is_mouse_down: bool,
    text: &str,
    scroll_y: &mut f32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    mouse_wheel: f32,
    focused: &mut bool,
    cursor_pos: usize,
    cursor_visible: bool,
) -> (bool, bool) {
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);
    let scale = 0.95 + 0.05 * eased_t;

    let panel_w = 600.0;
    let panel_h = 350.0;
    let camera = Camera2D {
        target: Vector2::new(panel_w / 2.0, panel_h / 2.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    let panel_rect = Rectangle::new(0.0, 0.0, panel_w, panel_h);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    let text_area_x = 20.0;
    let text_area_y = 50.0;
    let text_area_w = panel_w - 40.0;
    let text_area_h = 250.0;
    let line_height = 20.0;

    let lines: Vec<&str> = text.split('\n').collect();
    let content_height = (lines.len() as f32 * line_height) + 16.0;
    let max_scroll: f32 = f32::max(content_height - text_area_h, 0.0);
    *scroll_y = (*scroll_y).clamp(0.0, max_scroll);

    let mut close_clicked = false;
    let mut apply_clicked = false;

    {
        let mut d2d = d.begin_mode2D(camera);

        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();

        // Title
        d2d.draw_text_ex(
            &font,
            "Text Editor",
            Vector2::new(20.0, 20.0),
            24.0,
            1.0,
            text_color,
        );

        let mouse_world_x = (mouse_pos.x - camera.offset.x) / scale + camera.target.x;
        let mouse_world_y = (mouse_pos.y - camera.offset.y) / scale + camera.target.y;
        let mouse_world_pos = Vector2::new(mouse_world_x, mouse_world_y);

        let click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
            let wx = (p.x as f32 - camera.offset.x) / scale + camera.target.x;
            let wy = (p.y as f32 - camera.offset.y) / scale + camera.target.y;
            Some(Vector2::new(wx, wy))
        } else {
            None
        };

        // Text area background
        let text_area_rect = Rectangle::new(text_area_x, text_area_y, text_area_w, text_area_h);
        d2d.draw_rectangle_rec(
            text_area_rect,
            Color::new(30, 30, 30, (255.0 * eased_t) as u8),
        );
        let text_area_border = if *focused {
            Color::new(0, 170, 255, (255.0 * eased_t) as u8)
        } else {
            Color::new(80, 80, 80, (255.0 * eased_t) as u8)
        };
        d2d.draw_rectangle_lines_ex(text_area_rect, 1.0, text_area_border);

        // Mouse wheel scrolling
        if text_area_rect.check_collision_point_rec(mouse_world_pos) && mouse_wheel != 0.0 {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        // Scissor for text area
        {
            let clip_tl = d2d.get_world_to_screen2D(Vector2::new(text_area_x, text_area_y), camera);
            let clip_br = d2d.get_world_to_screen2D(
                Vector2::new(text_area_x + text_area_w, text_area_y + text_area_h),
                camera,
            );
            let sx = clip_tl.x as i32;
            let sy_scissor = clip_tl.y as i32;
            let sw = (clip_br.x - clip_tl.x) as i32;
            let sh = (clip_br.y - clip_tl.y) as i32;
            let mut d_scissor = d2d.begin_scissor_mode(sx, sy_scissor, sw, sh);

            let mut cursor_row = 0;
            let mut cursor_col = 0;
            let mut char_count = 0;
            for (i, line) in lines.iter().enumerate() {
                let line_len = line.chars().count();
                if cursor_pos >= char_count && cursor_pos <= char_count + line_len {
                    cursor_row = i;
                    cursor_col = cursor_pos - char_count;
                    break;
                }
                char_count += line_len + 1; // +1 for '\n'
            }

            // Auto-scroll to keep cursor visible
            if *focused {
                let cursor_y_in_content = text_area_y + 8.0 + cursor_row as f32 * line_height;
                let cursor_y_on_screen = cursor_y_in_content - *scroll_y;
                if cursor_y_on_screen < text_area_y {
                    *scroll_y = cursor_y_in_content - text_area_y;
                } else if cursor_y_on_screen + line_height > text_area_y + text_area_h {
                    *scroll_y = cursor_y_in_content + line_height - (text_area_y + text_area_h);
                }
                *scroll_y = (*scroll_y).clamp(0.0, max_scroll);
            }

            for (i, line) in lines.iter().enumerate() {
                let y = text_area_y + 8.0 + i as f32 * line_height - *scroll_y;
                if y + line_height >= text_area_y && y <= text_area_y + text_area_h {
                    d_scissor.draw_text_ex(
                        &font,
                        line,
                        Vector2::new(text_area_x + 8.0, y),
                        16.0,
                        1.0,
                        text_color,
                    );

                    if *focused && i == cursor_row && cursor_visible {
                        let prefix: String = line.chars().take(cursor_col).collect();
                        let prefix_width = font.measure_text(&prefix, 16.0, 1.0).x;
                        let cx = text_area_x + 8.0 + prefix_width;
                        let cy1 = y;
                        let cy2 = y + line_height;
                        d_scissor.draw_line_v(
                            Vector2::new(cx, cy1),
                            Vector2::new(cx, cy2),
                            text_color,
                        );
                    }
                }
            }
        }

        // Scrollbar (drawn after scissored text so it's always on top)
        let track_rect = Rectangle::new(
            text_area_x + text_area_w - 9.0,
            text_area_y + 1.0,
            8.0,
            text_area_h - 2.0,
        );
        let scrollbar_interacted = draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height,
            text_area_h,
            scroll_y,
            scrollbar_dragging,
            scrollbar_drag_start_y,
            scrollbar_drag_start_scroll,
            mouse_world_pos,
            click_pos,
            is_mouse_down,
            eased_t,
        );

        if let Some(pos) = click_pos {
            if scrollbar_interacted {
                // スクロールバー操作時はフォーカス変更しない
            } else if text_area_rect.check_collision_point_rec(pos) {
                *focused = true;
            } else if panel_rect.check_collision_point_rec(pos) {
                *focused = false;
            }
        }

        // Buttons
        let close_rect = Rectangle::new(370.0, 305.0, 100.0, 30.0);
        let apply_rect = Rectangle::new(480.0, 305.0, 100.0, 30.0);

        if draw_button(
            &mut d2d,
            close_rect,
            "Close",
            18,
            true,
            eased_t,
            click_pos,
            Some(mouse_world_pos),
        ) {
            close_clicked = true;
        }

        if draw_button(
            &mut d2d,
            apply_rect,
            "Apply",
            18,
            true,
            eased_t,
            click_pos,
            Some(mouse_world_pos),
        ) {
            apply_clicked = true;
        }

        // Click outside panel does NOT close the editor (intentional)
    }

    (close_clicked, apply_clicked)
}
