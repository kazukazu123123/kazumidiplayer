use crate::player::{PlayerCommand, PlayerState};
use crate::ui_components::button::draw_button;
use crate::ui_components::combo_box::{ComboBoxState, draw_combo_box, draw_combo_box_popup};
use crate::ui_components::numeric_slider::{
    NumericSliderInput, NumericSliderState, draw_u32_numeric_slider,
};
use crate::ui_components::scrollbar::draw_scrollbar;
use crate::ui_components::text_editor::{
    LineCommentHighlighter, TextEditorMouse, TextEditorState, draw_text_editor,
};
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

#[cfg(debug_assertions)]
fn visqol_measure_text(text: &str, size: i32) -> i32 {
    let c_text = std::ffi::CString::new(text).unwrap_or_default();
    // Safety: a valid NUL-terminated string; the default font is loaded.
    unsafe { raylib::ffi::MeasureText(c_text.as_ptr(), size) }
}

#[cfg(debug_assertions)]
fn visqol_cursor_at(text: &str, x: f32, size: i32) -> usize {
    let mut best = (0, x.abs());
    for i in 1..=text.len() {
        let d = (visqol_measure_text(&text[..i], size) as f32 - x).abs();
        if d < best.1 {
            best = (i, d);
        }
    }
    best.0
}

/// Input handling for the ViSQOL window bitrate text field (6..=510 kbps).
/// Reuses `NumericSliderState` and `NumericSliderInput` with Enter-commits
/// and Escape-cancels semantics so edits are not clobbered mid-entry.
/// Returns `true` if this frame's click landed inside `rect`.
#[cfg(debug_assertions)]
fn update_visqol_bitrate_field(
    rect: Rectangle,
    value: &mut u32,
    state: &mut NumericSliderState,
    input: &NumericSliderInput,
) -> bool {
    const FIELD_PAD: f32 = 5.0;
    const FONT_SIZE: i32 = 14;
    const MAX_LEN: usize = 5;

    let mut clicked_inside = false;
    if let Some(click) = input.click_pos {
        if rect.check_collision_point_rec(click) {
            clicked_inside = true;
            if state.editing {
                state.select_all = false;
                state.cursor =
                    visqol_cursor_at(&state.edit_text, click.x - rect.x - FIELD_PAD, FONT_SIZE);
            } else {
                state.edit_text = value.to_string();
                state.cursor = state.edit_text.len();
                state.select_all = true;
                state.editing = true;
            }
        } else if state.editing {
            state.editing = false;
            state.select_all = false;
        }
    }

    if state.editing {
        if input.escape {
            state.editing = false;
            state.select_all = false;
        } else if input.enter {
            if let Ok(parsed) = state.edit_text.parse::<u32>() {
                *value = parsed.clamp(6, 510);
            }
            state.editing = false;
            state.select_all = false;
        } else {
            let has_digits = input.typed.iter().any(|ch| ch.is_ascii_digit());
            let mut erase = input.backspace || input.delete;
            if state.select_all {
                if has_digits || erase {
                    state.edit_text.clear();
                    state.cursor = 0;
                    erase = false;
                } else if input.left || input.home {
                    state.cursor = 0;
                } else if input.right || input.end {
                    state.cursor = state.edit_text.len();
                }
                if has_digits
                    || input.backspace
                    || input.delete
                    || input.left
                    || input.right
                    || input.home
                    || input.end
                {
                    state.select_all = false;
                }
            } else {
                let len = state.edit_text.len();
                if input.left {
                    state.cursor = state.cursor.saturating_sub(1);
                }
                if input.right {
                    state.cursor = (state.cursor + 1).min(len);
                }
                if input.home {
                    state.cursor = 0;
                }
                if input.end {
                    state.cursor = len;
                }
            }
            state.cursor = state.cursor.min(state.edit_text.len());
            if erase {
                if input.backspace && state.cursor > 0 {
                    state.cursor -= 1;
                    state.edit_text.remove(state.cursor);
                }
                if input.delete && state.cursor < state.edit_text.len() {
                    state.edit_text.remove(state.cursor);
                }
            }
            for &ch in &input.typed {
                if state.edit_text.len() < MAX_LEN && ch.is_ascii_digit() {
                    state.edit_text.insert(state.cursor, ch);
                    state.cursor += 1;
                }
            }
        }
    }

    clicked_inside
}

/// Draws the ViSQOL window bitrate text field box matching `numeric_slider.rs`.
#[cfg(debug_assertions)]
fn draw_visqol_bitrate_field_box(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    value: u32,
    state: &NumericSliderState,
    accent: Color,
) {
    const FIELD_PAD: f32 = 5.0;
    const FONT_SIZE: i32 = 14;

    let border = Color::new(80, 80, 80, 255);
    let text_color = Color::new(240, 240, 240, 255);
    d.draw_rectangle_rec(rect, Color::new(30, 30, 30, 255));
    d.draw_rectangle_lines_ex(rect, 1.0, if state.editing { accent } else { border });
    let idle = value.to_string();
    let text = if state.editing {
        state.edit_text.as_str()
    } else {
        idle.as_str()
    };
    let tx = (rect.x + FIELD_PAD) as i32;
    let ty = (rect.y + (rect.height - FONT_SIZE as f32) / 2.0) as i32;
    if state.editing && state.select_all && !text.is_empty() {
        d.draw_rectangle(
            tx - 2,
            ty - 1,
            visqol_measure_text(text, FONT_SIZE) + 4,
            FONT_SIZE + 2,
            Color::new(0, 90, 170, 255),
        );
    }
    d.draw_text(text, tx, ty, FONT_SIZE, text_color);
    if state.editing && !state.select_all {
        let cx = tx + visqol_measure_text(&text[..state.cursor.min(text.len())], FONT_SIZE) + 1;
        d.draw_rectangle(cx, ty - 1, 1, FONT_SIZE + 2, text_color);
    }
}

/// Debug builds: content of the Audio ViSQOL window. Plots the
/// ViSQOL MOS-LQO score of the Opus round trip alongside the active
/// bitrate (one value per 1-second window scored every half second)
/// over the whole rendered stream so far; each pixel column shows
/// the worst score and latest bitrate in its span, so brief quality
/// dips stay visible. Scores run from 1 (bad) to 5 (excellent),
/// bitrates from 6 to 510 kbps (controlled by the right-rail slider
/// and numeric field).
#[cfg(debug_assertions)]
pub fn draw_visqol_window(
    d: &mut impl RaylibDraw,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    series: &[(f32, u32)],
    is_pcm: bool,
    bitrate_kbps: &mut u32,
    input_event: Option<InputEvent>,
    mouse_pos: Option<Vector2>,
    is_mouse_down: bool,
    slider_dragging: &mut bool,
    bitrate_field: &mut NumericSliderState,
    field_input: &NumericSliderInput,
) {
    const RAIL_W: f32 = 64.0;
    const BITRATE_MIN: f32 = 6.0;
    const BITRATE_MAX: f32 = 510.0;

    *bitrate_kbps = (*bitrate_kbps).clamp(6, 510);

    let score_color = Color::new(90, 200, 120, 255);
    let bitrate_color = Color::new(230, 170, 60, 255);

    d.draw_rectangle(
        x as i32,
        y as i32,
        w as i32,
        h as i32,
        Color::new(16, 16, 20, 255),
    );

    // Plot area under the header, leaving a right rail for the bitrate field and slider.
    let rail_x = x + w - RAIL_W;
    let plot_x = x + 34.0;
    let plot_y = y + 32.0;
    let plot_w = (w - 44.0 - RAIL_W).max(1.0);
    let plot_h = (h - 42.0).max(1.0);

    let bitrate_y = |kbps: u32| {
        let t = ((kbps as f32 - BITRATE_MIN) / (BITRATE_MAX - BITRATE_MIN)).clamp(0.0, 1.0);
        plot_y + plot_h - t * plot_h
    };
    let bitrate_from_y = |my: f32| -> u32 {
        let t = ((plot_y + plot_h - my) / plot_h).clamp(0.0, 1.0);
        ((BITRATE_MIN + t * (BITRATE_MAX - BITRATE_MIN)).round() as u32).clamp(6, 510)
    };

    // Numeric bitrate field at the top of the right rail.
    let field_rect = Rectangle::new(rail_x + 4.0, y + 4.0, RAIL_W - 8.0, 22.0);
    let field_clicked =
        update_visqol_bitrate_field(field_rect, bitrate_kbps, bitrate_field, field_input);
    if field_clicked || bitrate_field.editing {
        *slider_dragging = false;
    }

    let track_cx = rail_x + RAIL_W * 0.5;
    let initial_handle_y = bitrate_y(*bitrate_kbps);
    let initial_handle_rect = Rectangle::new(track_cx - 9.0, initial_handle_y - 5.0, 18.0, 10.0);
    let hit_rect = Rectangle::new(rail_x + 4.0, plot_y - 5.0, RAIL_W - 8.0, plot_h + 10.0);

    if !bitrate_field.editing {
        if let Some(InputEvent::MouseDown(_)) = input_event {
            if !field_clicked {
                if let Some(mp) = mouse_pos {
                    if initial_handle_rect.check_collision_point_rec(mp)
                        || hit_rect.check_collision_point_rec(mp)
                    {
                        *slider_dragging = true;
                        *bitrate_kbps = bitrate_from_y(mp.y);
                    }
                }
            }
        }
        if matches!(input_event, Some(InputEvent::MouseUp(_))) || !is_mouse_down {
            *slider_dragging = false;
        }
        if *slider_dragging && is_mouse_down {
            if let Some(mp) = mouse_pos {
                *bitrate_kbps = bitrate_from_y(mp.y);
            }
        }
    }

    if series.is_empty() {
        let msg = if is_pcm {
            "PCM buffer (uncompressed) - Opus bitrate unused"
        } else {
            "No ViSQOL data (needs an Opus prerender stream)"
        };
        d.draw_text(
            msg,
            x as i32 + 10,
            y as i32 + 10,
            16,
            Color::new(160, 160, 170, 255),
        );
    } else {
        let last = series[series.len() - 1].0;
        let min = series
            .iter()
            .fold(f32::INFINITY, |a, &(score, _)| a.min(score));
        let avg = series.iter().map(|&(score, _)| score).sum::<f32>() / series.len() as f32;
        let header = if is_pcm {
            format!("PCM (bitrate unused)   ViSQOL last {last:.2}   avg {avg:.2}   min {min:.2}")
        } else {
            format!(
                "Opus {} kbps   ViSQOL last {last:.2}   avg {avg:.2}   min {min:.2}",
                *bitrate_kbps
            )
        };
        d.draw_text(&header, x as i32 + 10, y as i32 + 6, 16, Color::WHITE);

        let y_of = |score: f32| plot_y + plot_h - ((score.clamp(1.0, 5.0) - 1.0) / 4.0) * plot_h;
        for score in [2.0f32, 3.0, 4.0] {
            let gy = y_of(score) as i32;
            d.draw_line(
                plot_x as i32,
                gy,
                (plot_x + plot_w) as i32,
                gy,
                Color::new(60, 60, 70, 255),
            );
            d.draw_text(
                &format!("{score:.0}"),
                x as i32 + 6,
                gy - 7,
                12,
                Color::new(140, 140, 150, 255),
            );
        }

        let n = series.len();
        let cols = plot_w as usize;
        if cols > 0 {
            let mut prev_score: Option<(i32, i32)> = None;
            let mut prev_bitrate: Option<(i32, i32)> = None;
            for col in 0..cols {
                let start = col * n / cols;
                let end = ((col + 1) * n / cols).max(start + 1).min(n);
                let worst = series[start..end]
                    .iter()
                    .fold(f32::INFINITY, |a, &(score, _)| a.min(score));
                let col_bitrate = series[end - 1].1;
                let px = (plot_x + col as f32) as i32;
                let py_score = y_of(worst) as i32;
                let py_bitrate = bitrate_y(col_bitrate) as i32;
                if let Some((ox, oy)) = prev_bitrate {
                    d.draw_line(ox, oy, px, py_bitrate, bitrate_color);
                }
                if let Some((ox, oy)) = prev_score {
                    d.draw_line(ox, oy, px, py_score, score_color);
                }
                prev_bitrate = Some((px, py_bitrate));
                prev_score = Some((px, py_score));
            }
        }

        // Small legend identifying the two polylines.
        let legend_w = 124.0;
        let legend_h = 18.0;
        let legend_x = (plot_x + plot_w - legend_w - 4.0).max(plot_x + 2.0);
        let legend_y = plot_y + 4.0;
        d.draw_rectangle(
            legend_x as i32,
            legend_y as i32,
            legend_w as i32,
            legend_h as i32,
            Color::new(16, 16, 20, 190),
        );
        d.draw_line(
            legend_x as i32 + 6,
            legend_y as i32 + 9,
            legend_x as i32 + 16,
            legend_y as i32 + 9,
            score_color,
        );
        d.draw_text(
            "ViSQOL",
            legend_x as i32 + 20,
            legend_y as i32 + 3,
            12,
            score_color,
        );
        d.draw_line(
            legend_x as i32 + 68,
            legend_y as i32 + 9,
            legend_x as i32 + 78,
            legend_y as i32 + 9,
            bitrate_color,
        );
        d.draw_text(
            "Bitrate",
            legend_x as i32 + 82,
            legend_y as i32 + 3,
            12,
            bitrate_color,
        );
    }

    // Right rail: numeric bitrate field and vertical slider (510 kbps at top, 6 kbps at bottom).
    draw_visqol_bitrate_field_box(
        d,
        field_rect,
        *bitrate_kbps,
        bitrate_field,
        Color::new(0, 120, 215, 255),
    );
    d.draw_rectangle(
        (track_cx - 2.0) as i32,
        plot_y as i32,
        4,
        plot_h as i32,
        Color::new(70, 70, 80, 255),
    );
    let handle_y = bitrate_y(*bitrate_kbps);
    let handle_rect = Rectangle::new(track_cx - 9.0, handle_y - 5.0, 18.0, 10.0);
    d.draw_rectangle_rec(handle_rect, bitrate_color);
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

/// Seconds the mouse rests on a settings row before its tooltip shows.
const TOOLTIP_DELAY: f64 = 0.5;

/// `tip` once the mouse has rested on it for [`TOOLTIP_DELAY`].
fn settings_tooltip_ready(tip: Option<&'static str>, now: f64) -> Option<&'static str> {
    thread_local! {
        static HOVER: std::cell::Cell<Option<(&'static str, f64)>> =
            const { std::cell::Cell::new(None) };
    }
    HOVER.with(|hover| match (tip, hover.get()) {
        (None, _) => {
            hover.set(None);
            None
        }
        (Some(tip), Some((last, since))) if std::ptr::eq(last, tip) => {
            (now - since >= TOOLTIP_DELAY).then_some(tip)
        }
        (Some(tip), _) => {
            hover.set(Some((tip, now)));
            None
        }
    })
}

thread_local! {
    /// Tooltip to draw on top of the frame: (text, mouse in screen space, alpha).
    static PENDING_TOOLTIP: std::cell::Cell<Option<(&'static str, Vector2, f32)>> =
        const { std::cell::Cell::new(None) };
}

fn queue_tooltip(text: &'static str, mouse: Vector2, alpha: f32) {
    PENDING_TOOLTIP.with(|p| p.set(Some((text, mouse, alpha))));
}

/// Draw the tooltip queued this frame, if any. Call last, so it lies above
/// everything else.
pub fn draw_pending_tooltip(d: &mut impl RaylibDraw, screen_width: i32, screen_height: i32) {
    if let Some((text, mouse, alpha)) = PENDING_TOOLTIP.with(|p| p.take()) {
        let bounds = Rectangle::new(0.0, 0.0, screen_width as f32, screen_height as f32);
        draw_tooltip(d, text, mouse, bounds, alpha);
    }
}

/// Split `text` into lines no wider than `max_w` pixels.
fn wrap_text(text: &str, size: i32, max_w: i32) -> Vec<String> {
    let measure = |t: &str| {
        let c = std::ffi::CString::new(t).unwrap_or_default();
        // Safety: a valid NUL-terminated string; the default font is loaded.
        unsafe { raylib::ffi::MeasureText(c.as_ptr(), size) }
    };
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if measure(&candidate) > max_w && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line = word.to_owned();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// A wrapped description box next to the mouse, kept inside `bounds`.
fn draw_tooltip(
    d: &mut impl RaylibDraw,
    text: &str,
    mouse: Vector2,
    bounds: Rectangle,
    alpha: f32,
) {
    const SIZE: i32 = 14;
    const LINE_H: i32 = 17;
    const PAD: i32 = 8;
    const MAX_W: i32 = 260;
    let lines = wrap_text(text, SIZE, MAX_W);
    let text_w = lines
        .iter()
        .map(|l| {
            let c = std::ffi::CString::new(l.as_str()).unwrap_or_default();
            // Safety: a valid NUL-terminated string; the default font is loaded.
            unsafe { raylib::ffi::MeasureText(c.as_ptr(), SIZE) }
        })
        .max()
        .unwrap_or(0);
    let w = (text_w + PAD * 2) as f32;
    let h = (lines.len() as i32 * LINE_H + PAD * 2 - (LINE_H - SIZE)) as f32;
    let mut x = mouse.x + 12.0;
    let mut y = mouse.y + 18.0;
    if x + w > bounds.x + bounds.width - 4.0 {
        x = (bounds.x + bounds.width - 4.0 - w).max(bounds.x + 4.0);
    }
    if y + h > bounds.y + bounds.height - 4.0 {
        // Above the cursor instead.
        y = (mouse.y - 8.0 - h).max(bounds.y + 4.0);
    }
    let rect = Rectangle::new(x, y, w, h);
    d.draw_rectangle_rec(rect, Color::new(25, 25, 30, (240.0 * alpha) as u8));
    d.draw_rectangle_lines_ex(rect, 1.0, Color::new(0, 135, 200, (255.0 * alpha) as u8));
    for (i, line) in lines.iter().enumerate() {
        d.draw_text(
            line,
            x as i32 + PAD,
            y as i32 + PAD + i as i32 * LINE_H,
            SIZE,
            Color::new(225, 230, 240, (255.0 * alpha) as u8),
        );
    }
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
    s16_clipping: &mut bool,
    s16_clipping_animation: f32,
    // When the S16 warning popup was opened (program time); `None` closed.
    s16_warning_opened_at: &mut Option<f64>,
    midi_ev_buffer_size: &mut u32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    pb_multiplier_slider: &mut NumericSliderState,
    midi_ev_buffer_size_slider: &mut NumericSliderState,
    audio_engine: &mut usize,
    audio_engine_combo: &mut ComboBoxState,
    audio_buffer_secs: &mut u32,
    audio_buffer_slider: &mut NumericSliderState,
    // Prerender buffer (allocated bytes, output rate, Opus-compressed);
    // `None` while no prerender engine is open.
    buffer_memory: Option<(u64, u32, bool)>,
    // Debug builds: Opus bitrate of the prerender buffer, and the
    // ViSQOL graph window toggle.
    #[cfg(debug_assertions)]
    opus_bitrate_kbps: &mut u32,
    #[cfg(debug_assertions)]
    opus_bitrate_slider: &mut NumericSliderState,
    #[cfg(debug_assertions)]
    visqol_window_enabled: &mut bool,
    #[cfg(debug_assertions)]
    visqol_window_animation: f32,
    midi_ev_mode: &mut usize,
    midi_ev_mode_combo: &mut ComboBoxState,
    parse_threads: &mut u32,
    parse_threads_slider: &mut NumericSliderState,
    show_stats: &mut bool,
    show_stats_animation: f32,
    vsync: &mut bool,
    vsync_animation: f32,
    voice_limit: &mut u32,
    voice_limit_slider: &mut NumericSliderState,
) -> bool {
    // Capture text-entry input before entering drawing modes.
    let mut numeric_typed = Vec::new();
    while let Some(ch) = d.get_char_pressed() {
        numeric_typed.push(ch);
    }
    // Editing keys repeat while held.
    let pressed = |d: &RaylibDrawHandle, key| d.is_key_pressed(key) || d.is_key_pressed_repeat(key);
    let numeric_backspace = pressed(d, KeyboardKey::KEY_BACKSPACE);
    let numeric_delete = pressed(d, KeyboardKey::KEY_DELETE);
    let numeric_left = pressed(d, KeyboardKey::KEY_LEFT);
    let numeric_right = pressed(d, KeyboardKey::KEY_RIGHT);
    let numeric_home = d.is_key_pressed(KeyboardKey::KEY_HOME);
    let numeric_end = d.is_key_pressed(KeyboardKey::KEY_END);
    let numeric_enter = d.is_key_pressed(KeyboardKey::KEY_ENTER);
    #[cfg(debug_assertions)]
    let numeric_kp_enter = d.is_key_pressed(KeyboardKey::KEY_KP_ENTER);
    let numeric_escape = d.is_key_pressed(KeyboardKey::KEY_ESCAPE);
    let numeric_shift =
        d.is_key_down(KeyboardKey::KEY_LEFT_SHIFT) || d.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
    let numeric_control = d.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
        || d.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);

    // Ease-out cubic: 1 - (1 - t)^3
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);

    // Expand into unused window space while keeping a compact minimum size.
    let scale = 0.95 + 0.05 * eased_t;
    let panel_width = (screen_width as f32 - 32.0).clamp(400.0, 720.0);
    let panel_height = (screen_height as f32 - 32.0).clamp(350.0, 760.0);
    let camera = Camera2D {
        target: Vector2::new(panel_width / 2.0, panel_height / 2.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    let mut close_clicked = false;
    let panel_rect = Rectangle::new(0.0, 0.0, panel_width, panel_height);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    // Scroll area configuration
    let content_top = 50.0;
    let content_bottom = panel_height - 50.0;
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
    let mut left_layout = VerticalLayout::new(content_top, 5.0);
    let mut right_layout = VerticalLayout::new(content_top, 5.0);
    let two_columns = panel_width >= 660.0;
    let column_gap = 20.0;
    let column_width = if two_columns {
        // Numeric controls use a fixed internal layout that needs this much room.
        ((panel_width - 60.0) / 2.0).max(300.0)
    } else {
        panel_width - 40.0
    };
    let mut layout = &mut left_layout;
    let mut column_x = 20.0;
    // Per row: (area in panel space, description shown on hover).
    let mut tips: Vec<(Rectangle, &'static str)> = Vec::new();

    {
        let mut d2d = d.begin_mode2D(camera);
        // Program time, shared by the S16 warning popup and its countdown.
        let frame_time = d2d.get_time();

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

        // The S16 warning sits above every control: while it is open nothing
        // underneath may react. Its clicks come from the original position.
        let warning_open = s16_warning_opened_at.is_some();
        let warning_click = if warning_open { click_pos } else { None };
        if warning_open {
            click_pos = None;
            audio_engine_combo.open = false;
            midi_ev_mode_combo.open = false;
            // Escape closes the warning without touching the toggle.
            if numeric_escape {
                *s16_warning_opened_at = None;
            }
        }

        // Wheel scrolling is applied after the controls are drawn, unless one of
        // them (a numeric slider's value box) used the wheel itself.
        let scroll_area_rect = Rectangle::new(0.0, content_top, panel_rect.width, viewport_height);
        let mut wheel_consumed = false;

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
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Render Mode Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Scroll the piano roll by beats instead of by time. Notes keep the same spacing when the tempo changes, so fast sections don't squash together."));
            let toggle_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Pulse the piano roll's zoom in time with the song's tempo.",
            ));
            let beat_bounce_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 52.0),
                    "How far the piano roll zooms in on each pulse. 0 turns the pulse off.",
                ));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
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
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How often it pulses: 4 = every quarter note, 8 = every eighth note, and so on."));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
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
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 30.0),
                    "Pulse halfway between beats instead of on them.",
                ));
                let offbeat_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Shake the keyboard window along with the notes being played.",
            ));
            let shake_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Show the piano keyboard inside the piano roll window instead of as a separate window."));
            let integrate_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Start playing as soon as a MIDI file finishes loading.",
            ));
            let autoplay_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "What makes the sound. KDMAPI sends notes to OmniMIDI as they play. CSynth plays a piano sample set. Prerender engines render ahead into a buffer: smoother on heavy files, but changes take a moment to be heard."));
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(column_x, sy(y), column_width, 52.0);
                audio_engine_combo_rect = Some(combo_rect);
                let was_open = audio_engine_combo.open;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "Audio Engine",
                    &crate::audio_engine::EngineSelection::LABELS,
                    audio_engine,
                    audio_engine_combo,
                    click_pos,
                    eased_t,
                    true,
                    None,
                );

                // Same click shielding as the EV buffer mode popup below.
                if let Some(click) = click_pos {
                    let header =
                        Rectangle::new(combo_rect.x, combo_rect.y + 20.0, combo_rect.width, 28.0);
                    let popup = Rectangle::new(
                        header.x,
                        header.y + header.height,
                        header.width,
                        28.0 * crate::audio_engine::EngineSelection::LABELS.len() as f32,
                    );
                    if header.check_collision_point_rec(click)
                        || (was_open && popup.check_collision_point_rec(click))
                    {
                        click_pos = None;
                    }
                }
            }

            // Prerender buffer length; applies immediately.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How far ahead a prerender engine renders, from 1 second to 10 minutes. More rides out heavy parts but uses more memory. When less than 2 seconds is left, quiet notes are skipped to keep up."));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let enabled = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                        .render_mode()
                        == crate::audio_engine::RenderMode::Prerender;
                    let note = buffer_memory.map(|(used, _, compressed)| {
                        if compressed {
                            format!("Memory: {}", format_bytes(used))
                        } else {
                            format!("Memory: {} (PCM)", format_bytes(used))
                        }
                    });
                    wheel_consumed |= crate::ui_components::numeric_slider::draw_duration_spinner(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Max Audio Buffer",
                        if enabled { note.as_deref() } else { None },
                        audio_buffer_secs,
                        crate::audio_engine::prerender::MIN_MAX_BUFFER_SECS,
                        crate::audio_engine::prerender::MAX_MAX_BUFFER_SECS,
                        audio_buffer_slider,
                        &input,
                        eased_t,
                        enabled,
                        Some("Realtime engine"),
                    );
                }
            }

            // Debug builds: Opus bitrate of the prerender buffer.
            #[cfg(debug_assertions)]
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter || numeric_kp_enter,
                        escape: numeric_escape,
                    };
                    let enabled = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                        .render_mode()
                        == crate::audio_engine::RenderMode::Prerender;
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Opus Bitrate (debug)",
                        opus_bitrate_kbps,
                        6,
                        510,
                        opus_bitrate_slider,
                        &input,
                        eased_t,
                        enabled,
                        Some("Prerender engine only"),
                    );
                }
            }

            // Debug builds: ViSQOL graph window toggle.
            #[cfg(debug_assertions)]
            {
                let y = layout.next(30.0);
                let visqol_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
                if is_visible(visqol_rect.y, visqol_rect.height) {
                    if draw_toggle(
                        &mut d_scissor,
                        visqol_rect,
                        "Audio ViSQOL Graph (debug)",
                        visqol_window_animation,
                        true,
                        eased_t,
                        click_pos,
                        Some(mouse_world_pos),
                    ) {
                        *visqol_window_enabled = !*visqol_window_enabled;
                    }
                }
            }

            // Voice limit of the built-in piano synth; applies immediately.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "Most notes the Piano engine plays at once (0 = unlimited). Over the limit, the quietest notes are faded out first, not the oldest, so loud and long-ringing notes stay. Lower uses less CPU on dense MIDIs."));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let enabled = crate::audio_engine::EngineSelection::LABELS
                        .get(*audio_engine)
                        .is_some_and(|l| *l == "Piano (Prerender)");
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Voice Limit (0 = unlimited)",
                        voice_limit,
                        0,
                        100_000,
                        voice_limit_slider,
                        &input,
                        eased_t,
                        enabled,
                        Some("Piano engine only"),
                    );
                }
            }

            // Prerender engines render the file themselves, so the MIDI output
            // pacing settings below have nothing to act on.
            let prerender_engine = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                .render_mode()
                == crate::audio_engine::RenderMode::Prerender;
            let ev_bypassed = *unlimited_output || prerender_engine;

            // S16 Clipping Toggle (prerender engines only)
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Reproduce the old 16-bit output: no limiter, so samples past full scale wrap around and heavy parts fuzz and hiss. Only prerender engines have a limiter to skip.",
            ));
            let s16_clipping_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(s16_clipping_rect.y, s16_clipping_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    s16_clipping_rect,
                    "S16 Clipping",
                    s16_clipping_animation,
                    prerender_engine,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    if *s16_clipping {
                        *s16_clipping = false;
                    } else {
                        // Enabling goes through the read-through warning.
                        *s16_warning_opened_at = Some(frame_time);
                    }
                }
            }

            let bypass_text = if prerender_engine {
                "Bypassed (Prerender)"
            } else {
                "Bypassed (Direct)"
            };
            if ev_bypassed {
                midi_ev_mode_combo.open = false;
            }

            // Unlimited Output Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Send every MIDI event the moment it is due, with no per-frame limit. Most accurate timing, but very heavy files can overload the synth."));
            let unlimited_output_rect =
                Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(unlimited_output_rect.y, unlimited_output_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    unlimited_output_rect,
                    "Unlimited Output",
                    unlimited_output_animation,
                    !prerender_engine,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *unlimited_output = !*unlimited_output;
                }
            }

            // MIDI and pitch-bend settings use the right column on wide panels.
            if two_columns {
                layout = &mut right_layout;
                column_x = 20.0 + column_width + column_gap;
            }

            // MIDI events consumed per 60 Hz output frame.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How many MIDI events can be sent per frame. Larger keeps up with denser files; smaller protects the synth from floods."));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let slider_enabled = !ev_bypassed && *midi_ev_mode != 0;
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
                    let disabled_text = if ev_bypassed {
                        bypass_text
                    } else {
                        "8192 (Fixed)"
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "What to do when a file has more events than one frame can send. Legacy: old behaviour, no buffer. Slowdown: playback slows down until it catches up. Catch Up: keeps real time and skips ahead after the backlog."));
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(column_x, sy(y), column_width, 52.0);
                midi_ev_mode_combo_rect = Some(combo_rect);
                let was_open = midi_ev_mode_combo.open && !ev_bypassed;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "EV Buffer Mode",
                    &[
                        "Legacy (No Buffer)",
                        "Buffered: Slowdown",
                        "Buffered: Catch Up",
                    ],
                    midi_ev_mode,
                    midi_ev_mode_combo,
                    click_pos,
                    eased_t,
                    !ev_bypassed,
                    Some(bypass_text),
                );

                // The popup overlays later settings, so clicks handled by its header
                // or option list must not continue into controls underneath it.
                if let Some(click) = click_pos {
                    let header =
                        Rectangle::new(combo_rect.x, combo_rect.y + 20.0, combo_rect.width, 28.0);
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
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Bend the keyboard window's shape with the song's pitch bend.",
            ));
            let pitch_bend_window_rect =
                Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Move the whole app window sideways with pitch bend. The three spring settings below control how it moves."));
            let pb_move_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
                for (index, (label, min, max, tip)) in [
                    ("Spring Mass", 1, 20, "How heavy the window feels when pitch bend moves it. Heavier starts and stops more slowly."),
                    ("Spring Stiffness", 1, 1000, "How strongly the window is pulled to where pitch bend puts it. Higher follows faster."),
                    ("Spring Damping", 0, 200, "How quickly the window stops wobbling. Low bounces around; high settles at once."),
                ]
                .into_iter()
                .enumerate()
                {
                    let y = layout.next(52.0);
                    tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), tip));
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
                            delete: numeric_delete,
                            left: numeric_left,
                            right: numeric_right,
                            home: numeric_home,
                            end: numeric_end,
                            enter: numeric_enter,
                            escape: numeric_escape,
                        };
                        wheel_consumed |= draw_u32_numeric_slider(
                            &mut d_scissor,
                            Rectangle::new(column_x, sy(y), column_width, 52.0),
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Ignore the pitch bend range the file sets and use the PB Multiplier below for every channel."));
            let pb_override_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 52.0),
                    "The pitch bend range, in semitones, used when PB Range Override is on",
                ));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let mut multiplier = *pitch_bend_multiplier as u32;
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
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

            // Parsing returns to the left column on wide panels.
            if two_columns {
                layout = &mut left_layout;
                column_x = 20.0;
            }

            // Parsing section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Parsing:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Parse Threads slider: 1..=hardware threads. Applies to the next load.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "CPU threads used to load a MIDI file. More loads faster. Takes effect on the next load."));
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
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Parse Threads",
                        parse_threads,
                        1,
                        crate::app_state::cpu_threads(),
                        parse_threads_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }
            }

            // Statistics shares the right column with MIDI and pitch-bend settings.
            if two_columns {
                layout = &mut right_layout;
                column_x = 20.0 + column_width + column_gap;
            }

            // Statistics section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Statistics:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Show Statistics toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Show the stats panel in the top-right corner: notes per second, note count, BPM, and for prerender engines polyphony and buffer."));
            let show_stats_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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

            // Display section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Display:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // VSync toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Wait for the monitor's refresh before showing each frame. On: smooth and capped at the refresh rate. Off: frames as fast as the PC can draw them, using more CPU and GPU, with possible tearing."));
            let vsync_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(vsync_rect.y, vsync_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    vsync_rect,
                    "VSync",
                    vsync_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *vsync = !*vsync;
                }
            }

            // Popups are deliberately drawn last so they float above the controls below.
            if let Some(combo_rect) = audio_engine_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &crate::audio_engine::EngineSelection::LABELS,
                    audio_engine_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
            if let Some(combo_rect) = midi_ev_mode_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &[
                        "Legacy (No Buffer)",
                        "Buffered: Slowdown",
                        "Buffered: Catch Up",
                    ],
                    midi_ev_mode_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
        }

        // Measured from what was just laid out, so the scroll range always
        // matches the controls actually drawn.
        // `layout` starts at `content_top`, so its bottom includes that offset.
        let content_height = left_layout
            .content_height(20.0)
            .max(right_layout.content_height(20.0))
            - content_top;
        let max_scroll = f32::max(content_height - viewport_height, 0.0);
        *scroll_y = (*scroll_y).clamp(0.0, max_scroll);

        let track_rect = Rectangle::new(panel_rect.width - 14.0, content_top, 8.0, viewport_height);
        draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height,
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

        if !wheel_consumed
            && mouse_wheel != 0.0
            && !warning_open
            && scroll_area_rect.check_collision_point_rec(mouse_world_pos)
        {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        // Tooltip for the row under the mouse, once it has rested there.
        let popup_open = audio_engine_combo.open || midi_ev_mode_combo.open || warning_open;
        let hovered_tip = (!popup_open
            && !is_mouse_down
            && !*scrollbar_dragging
            && scroll_area_rect.check_collision_point_rec(mouse_world_pos))
        .then(|| {
            tips.iter()
                .find(|(rect, _)| rect.check_collision_point_rec(mouse_world_pos))
                .map(|&(_, tip)| tip)
        })
        .flatten();
        let now = d2d.get_time();
        if let Some(tip) = settings_tooltip_ready(hovered_tip, now) {
            // Drawn at the end of the frame (`draw_pending_tooltip`), above
            // every other layer.
            queue_tooltip(tip, mouse_pos, eased_t);
        }

        // Close button (fixed coordinates, outside scroll area)
        let btn_rect = Rectangle::new(panel_width - 100.0, panel_height - 40.0, 80.0, 30.0);
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

        // S16 Clipping warning: drawn above every control. It must be read
        // through (5 s) before Enable lights up; Shift+Click skips the wait.
        if let Some(opened_at) = *s16_warning_opened_at {
            const WAIT_SECS: f64 = 5.0;
            let elapsed = (frame_time - opened_at).max(0.0);
            let remaining = (WAIT_SECS - elapsed).max(0.0);
            let ready = remaining == 0.0 || numeric_shift;

            // Dim the whole panel, then put the window above the dim layer.
            d2d.draw_rectangle_rec(panel_rect, Color::new(0, 0, 0, (190.0 * eased_t) as u8));

            let window_width = (panel_width - 60.0).clamp(340.0, 560.0);
            let window_height = 235.0;
            let window = Rectangle::new(
                (panel_width - window_width) / 2.0,
                (panel_height - window_height) / 2.0,
                window_width,
                window_height,
            );
            d2d.draw_rectangle_rec(window, Color::new(60, 6, 6, (255.0 * eased_t) as u8));
            // Flashy: the border pulses red the whole time.
            let pulse = (0.55 + 0.45 * (frame_time * 5.0).sin().abs()) as f32;
            let border_alpha = (pulse * eased_t * 255.0) as u8;
            d2d.draw_rectangle_lines_ex(window, 3.0, Color::new(255, 45, 45, border_alpha));

            // Centered text via the default font, as the tooltip helper does.
            let measured = |text: &str, size: i32| -> i32 {
                let c = std::ffi::CString::new(text).unwrap_or_default();
                // Safety: a valid NUL-terminated string; the default font is loaded.
                unsafe { raylib::ffi::MeasureText(c.as_ptr(), size) }
            };
            let centered_x = |text: &str, size: i32| -> i32 {
                (window.x + (window.width - measured(text, size) as f32) / 2.0) as i32
            };

            let title = "WARNING";
            d2d.draw_text(
                title,
                centered_x(title, 28),
                (window.y + 17.0) as i32,
                28,
                Color::new(255, 60, 50, (255.0 * eased_t) as u8),
            );

            const BODY: &str = "Enabling S16 Clipping skips the output limiter. Samples past full scale wrap around to the opposite end, like 16-bit overflow, so dense parts turn into harsh fuzz. It can get seriously loud: turn the volume down first.";
            let mut line_y = window.y + 49.0;
            for line in wrap_text(BODY, 14, (window.width - 40.0) as i32) {
                d2d.draw_text(
                    line.as_str(),
                    (window.x + 20.0) as i32,
                    line_y as i32,
                    14,
                    Color::new(235, 220, 220, (255.0 * eased_t) as u8),
                );
                line_y += 19.0;
            }

            let status: String = if ready {
                "You can enable it now.".to_string()
            } else {
                format!("You can enable it in {}s", remaining.ceil() as u64)
            };
            d2d.draw_text(
                status.as_str(),
                centered_x(status.as_str(), 16),
                (line_y + 6.0) as i32,
                16,
                Color::new(255, 230, 180, (255.0 * eased_t) as u8),
            );
            let hint = "Shift+Click skips the wait.";
            d2d.draw_text(
                hint,
                centered_x(hint, 13),
                (line_y + 28.0) as i32,
                13,
                Color::new(170, 140, 140, (255.0 * eased_t) as u8),
            );

            let buttons_y = window.y + window.height - 47.0;
            let enable_rect = Rectangle::new(window.x + 20.0, buttons_y, 148.0, 32.0);
            let cancel_rect =
                Rectangle::new(window.x + window.width - 168.0, buttons_y, 148.0, 32.0);
            if ready {
                // Unlocked: hot red, pulsing like the border.
                let is_hovered = enable_rect.check_collision_point_rec(mouse_world_pos);
                let bg = if is_hovered { 235 } else { 170 };
                d2d.draw_rectangle_rec(
                    enable_rect,
                    Color::new(bg, 22, 22, (255.0 * eased_t) as u8),
                );
                d2d.draw_rectangle_lines_ex(
                    enable_rect,
                    2.0,
                    Color::new(255, 80, 60, border_alpha),
                );
                d2d.draw_text(
                    "Enable",
                    enable_rect.x as i32 + 10,
                    enable_rect.y as i32 + 8,
                    18,
                    Color::new(255, 240, 240, (255.0 * eased_t) as u8),
                );
            } else {
                let label = format!("Enable in {}s", remaining.ceil() as u64);
                draw_button(
                    &mut d2d,
                    enable_rect,
                    label.as_str(),
                    18,
                    false,
                    eased_t,
                    warning_click,
                    Some(mouse_world_pos),
                );
            }
            draw_button(
                &mut d2d,
                cancel_rect,
                "Cancel",
                18,
                true,
                eased_t,
                warning_click,
                Some(mouse_world_pos),
            );

            if let Some(click) = warning_click {
                if cancel_rect.check_collision_point_rec(click) {
                    *s16_warning_opened_at = None;
                } else if ready && enable_rect.check_collision_point_rec(click) {
                    *s16_warning_opened_at = None;
                    *s16_clipping = true;
                }
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
    mouse_wheel: f32,
    editor: &mut TextEditorState,
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

    let mut close_clicked = false;
    let mut apply_clicked = false;

    let shift =
        d.is_key_down(KeyboardKey::KEY_LEFT_SHIFT) || d.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
    let mut d2d = d.begin_mode2D(camera);

    let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
    d2d.draw_rectangle_rec(panel_rect, bg_color);
    d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

    let font = d2d.get_font_default();
    d2d.draw_text_ex(
        &font,
        "Text Editor",
        Vector2::new(20.0, 20.0),
        24.0,
        1.0,
        text_color,
    );

    let to_world = |x: f32, y: f32| {
        Vector2::new(
            (x - camera.offset.x) / scale + camera.target.x,
            (y - camera.offset.y) / scale + camera.target.y,
        )
    };
    let mouse_world_pos = to_world(mouse_pos.x, mouse_pos.y);
    let click_pos = match input_event {
        Some(InputEvent::MouseDown(p)) => Some(to_world(p.x as f32, p.y as f32)),
        _ => None,
    };

    // Clicks outside the panel leave focus alone, as before.
    let editor_click = click_pos.filter(|p| panel_rect.check_collision_point_rec(*p));
    draw_text_editor(
        &mut d2d,
        &camera,
        &font,
        Rectangle::new(20.0, 50.0, panel_w - 40.0, 250.0),
        editor,
        &TextEditorMouse {
            pos: mouse_world_pos,
            click: editor_click,
            down: is_mouse_down,
            wheel: mouse_wheel,
            shift,
        },
        eased_t,
        Some(&DEMO_HIGHLIGHTER),
    );

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

    (close_clicked, apply_clicked)
}

/// Shows off highlighting in the debug editor: `#` and `//` lines are dimmed.
const DEMO_HIGHLIGHTER: LineCommentHighlighter = LineCommentHighlighter {
    markers: &["#", "//"],
    color: Color::new(110, 160, 110, 255),
};
