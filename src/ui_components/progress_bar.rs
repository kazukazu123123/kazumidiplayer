use raylib::prelude::*;

/// Draw a progress bar that supports multiple colored ranges (segments).
///
/// `segments` is a list of `(start_fraction, end_fraction, color)` in `0..1` of
/// the bar width; each is filled in order on top of `bg_color`. A simple
/// `0..progress` bar is just one segment, e.g. `&[(0.0, progress, fg)]`.
pub fn draw_progress_bar(
    d: &mut impl RaylibDraw,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    segments: &[(f32, f32, Color)],
    text: Option<&str>,
    bg_color: Color,
) {
    // Background
    d.draw_rectangle(x as i32, y as i32, width as i32, height as i32, bg_color);

    // Colored ranges
    for &(start, end, color) in segments {
        let s = start.clamp(0.0, 1.0);
        let e = end.clamp(0.0, 1.0);
        if e > s {
            let sx = (x + s * width).round() as i32;
            let ex = (x + e * width).round() as i32;
            d.draw_rectangle(sx, y as i32, (ex - sx).max(0), height as i32, color);
        }
    }

    // Text
    if let Some(t) = text {
        d.draw_text(t, x as i32 + 5, y as i32 + 2, 18, Color::WHITE);
    }
}
