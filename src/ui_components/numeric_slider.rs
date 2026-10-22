use raylib::prelude::*;

#[derive(Default)]
pub struct NumericSliderState {
    pub dragging: bool,
    pub editing: bool,
    pub edit_text: String,
}

pub struct NumericSliderInput {
    pub click_pos: Option<Vector2>,
    pub mouse_pos: Vector2,
    pub mouse_down: bool,
    pub wheel: f32,
    pub shift: bool,
    pub control: bool,
    pub typed: Vec<char>,
    pub backspace: bool,
    pub enter: bool,
    pub escape: bool,
}

/// Draws a coarse slider with exact stepping and direct numeric entry.
/// Returns true when the mouse wheel was consumed by this control.
pub fn draw_u32_numeric_slider(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    label: &str,
    value: &mut u32,
    min: u32,
    max: u32,
    state: &mut NumericSliderState,
    input: &NumericSliderInput,
    alpha: f32,
    enabled: bool,
    disabled_text: Option<&str>,
) -> bool {
    *value = (*value).clamp(min, max);
    if !enabled {
        state.dragging = false;
        state.editing = false;
    }
    let opacity = if enabled { 1.0 } else { 0.45 };
    let text_color = Color::new(240, 240, 240, (255.0 * alpha * opacity) as u8);
    let track_color = Color::new(80, 80, 80, (255.0 * alpha * opacity) as u8);
    let accent = if enabled {
        Color::new(0, 120, 215, (255.0 * alpha) as u8)
    } else {
        Color::new(95, 95, 95, (255.0 * alpha * opacity) as u8)
    };
    let slider = Rectangle::new(rect.x, rect.y + 28.0, 140.0, 20.0);
    let value_box_width = if enabled { 68.0 } else { 127.0 };
    let value_box = Rectangle::new(rect.x + 148.0, rect.y + 20.0, value_box_width, 28.0);
    let minus = Rectangle::new(rect.x + 222.0, rect.y + 20.0, 24.0, 28.0);
    let plus = Rectangle::new(rect.x + 251.0, rect.y + 20.0, 24.0, 28.0);

    d.draw_text(label, rect.x as i32, rect.y as i32, 16, text_color);
    d.draw_rectangle(
        slider.x as i32,
        (slider.y + 8.0) as i32,
        slider.width as i32,
        4,
        track_color,
    );
    let span = max.saturating_sub(min).max(1) as f32;
    let t = (*value - min) as f32 / span;
    let thumb_x = slider.x + t * slider.width;
    d.draw_rectangle_rec(
        Rectangle::new(thumb_x - 6.0, slider.y + 2.0, 12.0, 16.0),
        accent,
    );

    d.draw_rectangle_rec(value_box, Color::new(30, 30, 30, (255.0 * alpha) as u8));
    d.draw_rectangle_lines_ex(
        value_box,
        1.0,
        if state.editing { accent } else { track_color },
    );
    let shown = if state.editing {
        state.edit_text.as_str()
    } else {
        ""
    };
    let value_text = if !enabled {
        disabled_text.unwrap_or("Disabled").to_owned()
    } else if state.editing {
        shown.to_owned()
    } else {
        value.to_string()
    };
    d.draw_text(
        &value_text,
        value_box.x as i32 + 5,
        value_box.y as i32 + 6,
        if enabled { 16 } else { 14 },
        text_color,
    );

    if enabled {
        for (button, text) in [(minus, "-"), (plus, "+")] {
            let hover = button.check_collision_point_rec(input.mouse_pos);
            d.draw_rectangle_rec(
                button,
                if hover {
                    Color::new(70, 70, 74, (255.0 * alpha) as u8)
                } else {
                    Color::new(55, 55, 58, (255.0 * alpha) as u8)
                },
            );
            d.draw_rectangle_lines_ex(button, 1.0, track_color);
            d.draw_text(
                text,
                button.x as i32 + 7,
                button.y as i32 + 4,
                18,
                text_color,
            );
        }
    }

    if enabled && let Some(click) = input.click_pos {
        if slider.check_collision_point_rec(click) {
            state.dragging = true;
            state.editing = false;
        } else if value_box.check_collision_point_rec(click) {
            if !state.editing {
                state.edit_text.clear();
            }
            state.editing = true;
        } else {
            if minus.check_collision_point_rec(click) {
                *value = value.saturating_sub(1).max(min);
            } else if plus.check_collision_point_rec(click) {
                *value = value.saturating_add(1).min(max);
            }
            if !rect.check_collision_point_rec(click) {
                state.editing = false;
            }
        }
    }

    if enabled && state.dragging {
        if input.mouse_down {
            let x = input.mouse_pos.x.clamp(slider.x, slider.x + slider.width);
            let t = (x - slider.x) / slider.width;
            *value = (min as f32 + t * span).round() as u32;
        } else {
            state.dragging = false;
        }
    }

    let value_box_hovered = value_box.check_collision_point_rec(input.mouse_pos);
    if enabled && value_box_hovered && input.wheel != 0.0 {
        let step = if input.control {
            100
        } else if input.shift {
            10
        } else {
            1
        };
        let amount = (input.wheel.abs().round() as u32)
            .max(1)
            .saturating_mul(step);
        if input.wheel > 0.0 {
            *value = value.saturating_add(amount).min(max);
        } else {
            *value = value.saturating_sub(amount).max(min);
        }
    }

    if enabled && state.editing {
        for ch in &input.typed {
            if ch.is_ascii_digit() && state.edit_text.len() < 10 {
                state.edit_text.push(*ch);
            }
        }
        if input.backspace {
            state.edit_text.pop();
        }
        // Apply every edit immediately; Enter only leaves editing mode.
        if let Ok(parsed) = state.edit_text.parse::<u32>() {
            *value = parsed.clamp(min, max);
        }
        if input.enter {
            state.editing = false;
        } else if input.escape {
            state.editing = false;
        }
    }

    enabled && value_box_hovered && input.wheel != 0.0
}

/// `m:ss` for a duration in seconds.
pub fn format_duration_secs(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Parses `m:ss`, `m:` or plain seconds (`90`). `None` while incomplete.
pub fn parse_duration_secs(text: &str) -> Option<u32> {
    match text.split_once(':') {
        Some((m, s)) => {
            let m: u32 = if m.is_empty() { 0 } else { m.parse().ok()? };
            let s: u32 = if s.is_empty() { 0 } else { s.parse().ok()? };
            m.checked_mul(60)?.checked_add(s)
        }
        None => text.parse().ok(),
    }
}

/// A duration spinner: an `m:ss` box with - / + buttons, no slider. The box
/// takes typed `m:ss` or seconds; the buttons and the wheel over the box
/// step 1 s (Shift: 10 s, Ctrl: 1 min). `note` is drawn after the label.
pub fn draw_duration_spinner(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    label: &str,
    note: Option<&str>,
    value: &mut u32,
    min: u32,
    max: u32,
    state: &mut NumericSliderState,
    input: &NumericSliderInput,
    alpha: f32,
    enabled: bool,
    disabled_text: Option<&str>,
) -> bool {
    *value = (*value).clamp(min, max);
    state.dragging = false;
    if !enabled {
        state.editing = false;
    }
    let opacity = if enabled { 1.0 } else { 0.45 };
    let text_color = Color::new(240, 240, 240, (255.0 * alpha * opacity) as u8);
    let note_color = Color::new(160, 175, 195, (255.0 * alpha * opacity) as u8);
    let border = Color::new(80, 80, 80, (255.0 * alpha * opacity) as u8);
    let accent = Color::new(0, 120, 215, (255.0 * alpha) as u8);
    let minus = Rectangle::new(rect.x, rect.y + 20.0, 28.0, 28.0);
    let value_box = Rectangle::new(
        rect.x + 33.0,
        rect.y + 20.0,
        if enabled { 90.0 } else { 155.0 },
        28.0,
    );
    let plus = Rectangle::new(rect.x + 128.0, rect.y + 20.0, 28.0, 28.0);

    d.draw_text(label, rect.x as i32, rect.y as i32, 16, text_color);
    if let Some(note) = note {
        let c_label = std::ffi::CString::new(label).unwrap_or_default();
        // Safety: a valid NUL-terminated string; the default font is loaded.
        let w = unsafe { raylib::ffi::MeasureText(c_label.as_ptr(), 16) };
        d.draw_text(
            note,
            rect.x as i32 + w + 10,
            rect.y as i32 + 2,
            14,
            note_color,
        );
    }

    let value_box = if enabled {
        value_box
    } else {
        Rectangle::new(rect.x, value_box.y, value_box.width, value_box.height)
    };
    d.draw_rectangle_rec(value_box, Color::new(30, 30, 30, (255.0 * alpha) as u8));
    d.draw_rectangle_lines_ex(value_box, 1.0, if state.editing { accent } else { border });
    let value_text = if !enabled {
        disabled_text.unwrap_or("Disabled").to_owned()
    } else if state.editing {
        format!("{}_", state.edit_text)
    } else {
        format_duration_secs(*value)
    };
    d.draw_text(
        &value_text,
        value_box.x as i32 + 6,
        value_box.y as i32 + 6,
        if enabled { 16 } else { 14 },
        text_color,
    );
    if !enabled {
        return false;
    }

    for (button, text) in [(minus, "-"), (plus, "+")] {
        let hover = button.check_collision_point_rec(input.mouse_pos);
        d.draw_rectangle_rec(
            button,
            if hover {
                Color::new(70, 70, 74, (255.0 * alpha) as u8)
            } else {
                Color::new(55, 55, 58, (255.0 * alpha) as u8)
            },
        );
        d.draw_rectangle_lines_ex(button, 1.0, border);
        d.draw_text(
            text,
            button.x as i32 + 9,
            button.y as i32 + 4,
            18,
            text_color,
        );
    }

    let step = if input.control {
        60
    } else if input.shift {
        10
    } else {
        1
    };
    if let Some(click) = input.click_pos {
        if value_box.check_collision_point_rec(click) {
            if !state.editing {
                state.edit_text.clear();
            }
            state.editing = true;
        } else {
            if minus.check_collision_point_rec(click) {
                *value = value.saturating_sub(step).max(min);
            } else if plus.check_collision_point_rec(click) {
                *value = value.saturating_add(step).min(max);
            }
            state.editing = false;
        }
    }

    let hovered = value_box.check_collision_point_rec(input.mouse_pos);
    if hovered && input.wheel != 0.0 {
        let amount = (input.wheel.abs().round() as u32)
            .max(1)
            .saturating_mul(step);
        if input.wheel > 0.0 {
            *value = value.saturating_add(amount).min(max);
        } else {
            *value = value.saturating_sub(amount).max(min);
        }
    }

    if state.editing {
        for &ch in &input.typed {
            let colon_ok = ch == ':' && !state.edit_text.contains(':');
            if (ch.is_ascii_digit() || colon_ok) && state.edit_text.len() < 8 {
                state.edit_text.push(ch);
            }
        }
        if input.backspace {
            state.edit_text.pop();
        }
        // Apply every edit immediately; Enter only leaves editing mode.
        if let Some(parsed) = parse_duration_secs(&state.edit_text) {
            *value = parsed.clamp(min, max);
        }
        if input.enter || input.escape {
            state.editing = false;
        }
    }

    hovered && input.wheel != 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_format() {
        assert_eq!(parse_duration_secs("90"), Some(90));
        assert_eq!(parse_duration_secs("1:30"), Some(90));
        assert_eq!(parse_duration_secs("5:"), Some(300));
        assert_eq!(parse_duration_secs(":45"), Some(45));
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(format_duration_secs(300), "5:00");
        assert_eq!(format_duration_secs(9), "0:09");
    }
}
