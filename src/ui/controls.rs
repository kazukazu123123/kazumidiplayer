use crate::player::{PlayerCommand, PlayerState};
use crate::ui_components::button::draw_button;
use crate::utils::{format_time};
use crate::window_manager::InputEvent;
use std::sync::mpsc::Sender;
use raylib::prelude::*;

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
    // Prerender only: song position up to which audio has been rendered.
    rendered_end_ns: Option<u64>,
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

        // Rendered-ahead span, under the playback fill so only the part past
        // the playhead shows.
        if let Some(end_ns) = rendered_end_ns {
            let end = ((end_ns.min(duration_ns) as f64 / duration_ns as f64) * w as f64) as i32;
            d.draw_rectangle(
                x as i32,
                y as i32,
                end,
                h as i32,
                Color::new(90, 160, 230, 255),
            );
        }

        // Playback progress
        let current_position_ns = playback_position_ns;
        // f64 throughout: an f32 only carries ~7 significant digits, so on a MIDI
        // hours long the nanosecond counts round to millisecond-sized steps and the
        // bar moves in visible jumps.
        let fill =
            ((current_position_ns.min(duration_ns) as f64 / duration_ns as f64) * w as f64) as i32;
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
