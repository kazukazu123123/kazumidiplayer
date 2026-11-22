use raylib::prelude::*;

/// Seconds the mouse rests on a settings row before its tooltip shows.
const TOOLTIP_DELAY: f64 = 0.5;

/// `tip` once the mouse has rested on it for [`TOOLTIP_DELAY`].
pub(super) fn settings_tooltip_ready(tip: Option<&'static str>, now: f64) -> Option<&'static str> {
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

pub(super) fn queue_tooltip(text: &'static str, mouse: Vector2, alpha: f32) {
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
pub(super) fn wrap_text(text: &str, size: i32, max_w: i32) -> Vec<String> {
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
