use crate::utils::{cubic_bezier, format_bytes, format_count, format_count_short};
use raylib::prelude::*;

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

/// Non-blocking audio engine status banner (loading, ready, or failure such as
/// OmniMIDI / KDMAPI not found). Fades out after 5 seconds. Unlike the
/// PlayerCrashed overlay the UI stays fully interactive.
pub fn draw_audio_banner(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    _screen_height: i32,
    timer: f32,
    message: &str,
    is_error: bool,
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
    let (bg, fg) = if is_error {
        (
            Color::new(60, 30, 30, alpha),
            Color::new(255, 180, 180, alpha),
        )
    } else {
        (
            Color::new(30, 45, 60, alpha),
            Color::new(180, 215, 255, alpha),
        )
    };
    d.draw_rectangle(0, 0, screen_width, 28, bg);
    d.draw_text(message, 16, 6, 16, fg);
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
    parse_duration: std::time::Duration,
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

    let title = format!(
        "Loaded! ({})",
        crate::utils::format_parse_duration(parse_duration)
    );
    draw_loading_panel(d, camera, alpha, &title, progress, parse_bps, parse_eps);
}
