use raylib::prelude::*;

#[derive(Default)]
pub struct NumericSliderState {
    pub dragging: bool,
    pub editing: bool,
    pub edit_text: String,
    /// The whole `edit_text` is selected: the next key replaces it.
    pub select_all: bool,
    /// Caret position in `edit_text` (bytes; the text is ASCII).
    pub cursor: usize,
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
    pub delete: bool,
    pub left: bool,
    pub right: bool,
    pub home: bool,
    pub end: bool,
    pub enter: bool,
    pub escape: bool,
}

const FIELD_PAD: f32 = 6.0;

fn measure(text: &str, size: i32) -> i32 {
    let c_text = std::ffi::CString::new(text).unwrap_or_default();
    // Safety: a valid NUL-terminated string; the default font is loaded.
    unsafe { raylib::ffi::MeasureText(c_text.as_ptr(), size) }
}

/// Caret position closest to `x` pixels into `text`.
fn cursor_at(text: &str, x: f32, size: i32) -> usize {
    let mut best = (0, x.abs());
    for i in 1..=text.len() {
        let d = (measure(&text[..i], size) as f32 - x).abs();
        if d < best.1 {
            best = (i, d);
        }
    }
    best.0
}

/// A one-line text field for numeric entry, shared by the numeric widgets.
/// The first click selects the current value (typing replaces it); later
/// clicks place the caret. Arrows, Home/End, Backspace and Delete edit at
/// the caret; Enter or Escape stops editing.
struct Field;

impl Field {
    /// Handle a click; `true` if it landed in `rect`.
    fn click(
        state: &mut NumericSliderState,
        rect: Rectangle,
        click: Vector2,
        size: i32,
        current: impl FnOnce() -> String,
    ) -> bool {
        if !rect.check_collision_point_rec(click) {
            state.editing = false;
            state.select_all = false;
            return false;
        }
        if state.editing {
            state.select_all = false;
            state.cursor = cursor_at(&state.edit_text, click.x - rect.x - FIELD_PAD, size);
        } else {
            state.edit_text = current();
            state.cursor = state.edit_text.len();
            state.select_all = true;
            state.editing = true;
        }
        true
    }

    /// Apply this frame's keys. `allow(text, cursor, ch)` decides whether
    /// `ch` may be inserted at `cursor`. Returns `true` if the text changed.
    fn keys(
        state: &mut NumericSliderState,
        input: &NumericSliderInput,
        max_len: usize,
        allow: impl Fn(&str, usize, char) -> bool,
    ) -> bool {
        let mut changed = false;
        let mut erase = input.backspace || input.delete;
        if state.select_all {
            if !input.typed.is_empty() || erase {
                state.edit_text.clear();
                state.cursor = 0;
                changed = true;
                // The selection was the deletion.
                erase = false;
            } else if input.left || input.home {
                state.cursor = 0;
            } else if input.right || input.end {
                state.cursor = state.edit_text.len();
            }
            if !input.typed.is_empty()
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
                changed = true;
            }
            if input.delete && state.cursor < state.edit_text.len() {
                state.edit_text.remove(state.cursor);
                changed = true;
            }
        }
        for &ch in &input.typed {
            if state.edit_text.len() < max_len && allow(&state.edit_text, state.cursor, ch) {
                state.edit_text.insert(state.cursor, ch);
                state.cursor += 1;
                changed = true;
            }
        }
        if input.enter || input.escape {
            state.editing = false;
            state.select_all = false;
        }
        changed
    }

    /// Draw the box with `idle` shown while not editing.
    fn draw(
        d: &mut impl RaylibDraw,
        rect: Rectangle,
        state: &NumericSliderState,
        idle: &str,
        size: i32,
        alpha: f32,
        text_color: Color,
        border: Color,
        accent: Color,
    ) {
        d.draw_rectangle_rec(rect, Color::new(30, 30, 30, (255.0 * alpha) as u8));
        d.draw_rectangle_lines_ex(rect, 1.0, if state.editing { accent } else { border });
        let text = if state.editing {
            state.edit_text.as_str()
        } else {
            idle
        };
        let x = (rect.x + FIELD_PAD) as i32;
        let y = (rect.y + (rect.height - size as f32) / 2.0) as i32;
        if state.editing && state.select_all && !text.is_empty() {
            d.draw_rectangle(
                x - 2,
                y - 1,
                measure(text, size) + 4,
                size + 2,
                Color::new(0, 90, 170, (255.0 * alpha) as u8),
            );
        }
        d.draw_text(text, x, y, size, text_color);
        if state.editing && !state.select_all {
            let cx = x + measure(&text[..state.cursor.min(text.len())], size) + 1;
            d.draw_rectangle(cx, y - 1, 1, size + 2, text_color);
        }
    }
}

fn step_value(value: &mut u32, up: bool, amount: u32, min: u32, max: u32) {
    *value = if up {
        value.saturating_add(amount).min(max)
    } else {
        value.saturating_sub(amount).max(min)
    };
}

fn draw_step_buttons(
    d: &mut impl RaylibDraw,
    buttons: [(Rectangle, &str); 2],
    input: &NumericSliderInput,
    alpha: f32,
    text_color: Color,
    border: Color,
) {
    for (button, text) in buttons {
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
        let tx = button.x + (button.width - measure(text, 18) as f32) / 2.0;
        d.draw_text(text, tx as i32, button.y as i32 + 4, 18, text_color);
    }
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

    if !enabled {
        Field::draw(
            d,
            value_box,
            state,
            disabled_text.unwrap_or("Disabled"),
            14,
            alpha,
            text_color,
            track_color,
            accent,
        );
        return false;
    }
    draw_step_buttons(
        d,
        [(minus, "-"), (plus, "+")],
        input,
        alpha,
        text_color,
        track_color,
    );

    if let Some(click) = input.click_pos {
        if slider.check_collision_point_rec(click) {
            state.dragging = true;
            state.editing = false;
        } else if !Field::click(state, value_box, click, 16, || value.to_string()) {
            if minus.check_collision_point_rec(click) {
                step_value(value, false, 1, min, max);
            } else if plus.check_collision_point_rec(click) {
                step_value(value, true, 1, min, max);
            }
        }
    }

    if state.dragging {
        if input.mouse_down {
            let x = input.mouse_pos.x.clamp(slider.x, slider.x + slider.width);
            let t = (x - slider.x) / slider.width;
            *value = (min as f32 + t * span).round() as u32;
        } else {
            state.dragging = false;
        }
    }

    let value_box_hovered = value_box.check_collision_point_rec(input.mouse_pos);
    if value_box_hovered && input.wheel != 0.0 {
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
        step_value(value, input.wheel > 0.0, amount, min, max);
        state.editing = false;
    }

    if state.editing
        && Field::keys(state, input, 10, |_, _, ch| ch.is_ascii_digit())
        && let Ok(parsed) = state.edit_text.parse::<u32>()
    {
        // Apply every edit immediately; Enter only leaves editing mode.
        *value = parsed.clamp(min, max);
    }

    let idle = value.to_string();
    Field::draw(
        d,
        value_box,
        state,
        &idle,
        16,
        alpha,
        text_color,
        track_color,
        accent,
    );

    value_box_hovered && input.wheel != 0.0
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

/// A duration spinner: an `m:ss` field with - / + buttons, no slider. The
/// field takes typed `m:ss` or seconds; the buttons and the wheel over the
/// field step 1 s (Shift: 10 s, Ctrl: 1 min). `note` is drawn after the label.
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
    let value_box = Rectangle::new(rect.x + 33.0, rect.y + 20.0, 90.0, 28.0);
    let plus = Rectangle::new(rect.x + 128.0, rect.y + 20.0, 28.0, 28.0);

    d.draw_text(label, rect.x as i32, rect.y as i32, 16, text_color);
    if let Some(note) = note {
        let w = measure(label, 16);
        d.draw_text(
            note,
            rect.x as i32 + w + 10,
            rect.y as i32 + 2,
            14,
            note_color,
        );
    }

    if !enabled {
        let disabled_box = Rectangle::new(rect.x, value_box.y, 155.0, value_box.height);
        Field::draw(
            d,
            disabled_box,
            state,
            disabled_text.unwrap_or("Disabled"),
            14,
            alpha,
            text_color,
            border,
            accent,
        );
        return false;
    }
    draw_step_buttons(
        d,
        [(minus, "-"), (plus, "+")],
        input,
        alpha,
        text_color,
        border,
    );

    let step = if input.control {
        60
    } else if input.shift {
        10
    } else {
        1
    };
    if let Some(click) = input.click_pos
        && !Field::click(state, value_box, click, 16, || format_duration_secs(*value))
    {
        if minus.check_collision_point_rec(click) {
            step_value(value, false, step, min, max);
        } else if plus.check_collision_point_rec(click) {
            step_value(value, true, step, min, max);
        }
    }

    let hovered = value_box.check_collision_point_rec(input.mouse_pos);
    if hovered && input.wheel != 0.0 {
        let amount = (input.wheel.abs().round() as u32)
            .max(1)
            .saturating_mul(step);
        step_value(value, input.wheel > 0.0, amount, min, max);
        state.editing = false;
    }

    if state.editing
        && Field::keys(state, input, 8, |text, _, ch| {
            ch.is_ascii_digit() || ch == ':' && !text.contains(':')
        })
        && let Some(parsed) = parse_duration_secs(&state.edit_text)
    {
        // Apply every edit immediately; Enter only leaves editing mode.
        *value = parsed.clamp(min, max);
    }

    let idle = format_duration_secs(*value);
    Field::draw(
        d, value_box, state, &idle, 16, alpha, text_color, border, accent,
    );

    hovered && input.wheel != 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(typed: &str) -> NumericSliderInput {
        NumericSliderInput {
            click_pos: None,
            mouse_pos: Vector2::zero(),
            mouse_down: false,
            wheel: 0.0,
            shift: false,
            control: false,
            typed: typed.chars().collect(),
            backspace: false,
            delete: false,
            left: false,
            right: false,
            home: false,
            end: false,
            enter: false,
            escape: false,
        }
    }

    fn editing(text: &str, cursor: usize, select_all: bool) -> NumericSliderState {
        NumericSliderState {
            editing: true,
            edit_text: text.into(),
            cursor,
            select_all,
            ..Default::default()
        }
    }

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

    #[test]
    fn field_edits_at_the_caret() {
        let digits = |_: &str, _: usize, ch: char| ch.is_ascii_digit();
        // Selected text is replaced by typing.
        let mut s = editing("130", 3, true);
        assert!(Field::keys(&mut s, &keys("9"), 10, digits));
        assert_eq!(
            (s.edit_text.as_str(), s.cursor, s.select_all),
            ("9", 1, false)
        );
        // Insert in the middle, then move and delete.
        let mut s = editing("13", 1, false);
        Field::keys(&mut s, &keys("2"), 10, digits);
        assert_eq!((s.edit_text.as_str(), s.cursor), ("123", 2));
        let mut input = keys("");
        input.left = true;
        Field::keys(&mut s, &input, 10, digits);
        assert_eq!(s.cursor, 1);
        let mut input = keys("");
        input.delete = true;
        Field::keys(&mut s, &input, 10, digits);
        assert_eq!((s.edit_text.as_str(), s.cursor), ("13", 1));
        let mut input = keys("");
        input.backspace = true;
        Field::keys(&mut s, &input, 10, digits);
        assert_eq!((s.edit_text.as_str(), s.cursor), ("3", 0));
        // Arrow keys drop the selection without changing the text.
        let mut s = editing("42", 2, true);
        let mut input = keys("");
        input.home = true;
        assert!(!Field::keys(&mut s, &input, 10, digits));
        assert_eq!((s.cursor, s.select_all), (0, false));
        // Filtered characters are not inserted.
        let mut s = editing("1", 1, false);
        assert!(!Field::keys(&mut s, &keys("a"), 10, digits));
    }
}
