use raylib::prelude::*;

pub fn draw_progress_bar(
    d: &mut impl RaylibDraw,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    progress: f32,
    text: Option<&str>,
    fg_color: Color,
    bg_color: Color,
) {
    // Background
    d.draw_rectangle(x as i32, y as i32, width as i32, height as i32, bg_color);

    // Foreground (Progress)
    if progress > 0.0 {
        let fill_width = (progress.clamp(0.0, 1.0) * width) as i32;
        d.draw_rectangle(x as i32, y as i32, fill_width, height as i32, fg_color);
    }

    // Text
    if let Some(t) = text {
        d.draw_text(t, x as i32 + 5, y as i32 + 2, 18, Color::WHITE);
    }
}
