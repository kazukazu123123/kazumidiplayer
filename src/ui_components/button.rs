use raylib::prelude::*;

pub fn draw_button(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    text: &str,
    font_size: i32,
    enabled: bool,
    alpha: f32,
    click_pos: Option<Vector2>,
    mouse_pos: Option<Vector2>,
) -> bool {
    let is_hovered = enabled && mouse_pos.map_or(false, |pos| rect.check_collision_point_rec(pos));

    let color = if enabled {
        Color::WHITE
    } else {
        Color::DARKGRAY
    };

    let base_bg = if enabled {
        if is_hovered {
            Color::new(100, 100, 100, 255) // Lighter gray when hovered
        } else {
            Color::GRAY
        }
    } else {
        Color::BLACK
    };

    let bg_with_alpha = Color::new(
        base_bg.r,
        base_bg.g,
        base_bg.b,
        (base_bg.a as f32 * alpha) as u8,
    );
    let color_with_alpha = Color::new(color.r, color.g, color.b, (color.a as f32 * alpha) as u8);

    d.draw_rectangle(
        rect.x as i32,
        rect.y as i32,
        rect.width as i32,
        rect.height as i32,
        bg_with_alpha,
    );
    d.draw_rectangle_lines_ex(rect, 1.0, color_with_alpha);

    // Using a fixed offset for simplicity as RaylibDraw doesn't have measure_text generically
    d.draw_text(
        text,
        rect.x as i32 + 10,
        rect.y as i32 + 8,
        font_size,
        color_with_alpha,
    );

    if enabled {
        if let Some(pos) = click_pos {
            if rect.check_collision_point_rec(pos) {
                return true;
            }
        }
    }
    false
}
