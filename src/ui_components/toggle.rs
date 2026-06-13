use raylib::prelude::*;

pub fn draw_toggle(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    label: &str,
    t: f32, // Animation factor 0.0 to 1.0
    enabled: bool,
    alpha: f32,
    click_pos: Option<Vector2>,
    mouse_pos: Option<Vector2>,
) -> bool {
    let is_hovered = enabled && mouse_pos.map_or(false, |pos| rect.check_collision_point_rec(pos));

    // Toggle dimensions
    let toggle_w = 40.0;
    let toggle_h = 20.0;
    let toggle_x = rect.x.round();
    // Center the toggle vertically within the rect
    let toggle_y = (rect.y + (rect.height - toggle_h) / 2.0).ceil();

    let color_alpha = (255.0 * alpha) as u8;

    // Smooth lerp for colors
    let on_color = Color::new(0, 120, 215, color_alpha);
    let off_color = if is_hovered {
        Color::new(100, 100, 100, color_alpha)
    } else {
        Color::new(60, 60, 60, color_alpha)
    };

    let track_color = lerp_color(off_color, on_color, t);

    let thumb_color = if enabled {
        Color::new(255, 255, 255, color_alpha)
    } else {
        Color::new(160, 160, 160, color_alpha)
    };

    // Draw track
    // To solve the "too bright" issue, we don't stack two solid fills.
    // To solve the "1px gap", we draw the fill slightly LARGER than the lines,
    // so any subpixel gaps in line rendering show the track color instead of the background.

    // Draw track
    // Use inset method: draw border as outer fill, then track as inner fill
    // This ensures perfect roundness matching.
    let track_w = 38.0;
    let track_h = 18.0;
    let track_x = toggle_x + 1.0;
    let track_y = toggle_y + 1.0;

    let border_rect = Rectangle::new(track_x, track_y, track_w, track_h);
    // Inner rect: 2px smaller on each dimension (1px border on each side)
    let inner_rect = Rectangle::new(track_x + 1.0, track_y + 1.0, track_w - 2.0, track_h - 2.0);

    let border_color = Color::new(200, 200, 200, color_alpha);

    // 1. Draw border as outer rounded rect
    d.draw_rectangle_rounded(border_rect, 1.0, 30, border_color);
    // 2. Draw track fill as inner rounded rect
    d.draw_rectangle_rounded(inner_rect, 1.0, 30, track_color);

    // Draw thumb with animation
    // Centered relative to the 18px height track
    let thumb_radius = 6.0;
    let thumb_start_x = track_x + 9.0;
    let thumb_end_x = track_x + track_w - 9.0;
    let thumb_x = thumb_start_x + (thumb_end_x - thumb_start_x) * t;

    let thumb_center = Vector2::new(thumb_x, track_y + track_h / 2.0);

    d.draw_circle_v(thumb_center, thumb_radius, thumb_color);

    // Draw label
    let text_color = Color::new(240, 240, 240, color_alpha);
    let label_y = (rect.y + (rect.height - 18.0) / 2.0).ceil() as i32;
    d.draw_text(
        label,
        (toggle_x + toggle_w + 10.0).round() as i32,
        label_y,
        18,
        text_color,
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

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color::new(
        (a.r as f32 + (b.r as f32 - a.r as f32) * t) as u8,
        (a.g as f32 + (b.g as f32 - a.g as f32) * t) as u8,
        (a.b as f32 + (b.b as f32 - a.b as f32) * t) as u8,
        (a.a as f32 + (b.a as f32 - a.a as f32) * t) as u8,
    )
}
