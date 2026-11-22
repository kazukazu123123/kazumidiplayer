#[cfg(debug_assertions)]
use crate::ui_components::numeric_slider::{NumericSliderInput, NumericSliderState};
#[cfg(debug_assertions)]
use crate::window_manager::InputEvent;
#[cfg(debug_assertions)]
use raylib::prelude::*;

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
