use raylib::prelude::*;

pub fn draw_scrollbar(
    d: &mut impl RaylibDraw,
    track_rect: Rectangle,
    content_height: f32,
    viewport_height: f32,
    scroll_y: &mut f32,
    dragging: &mut bool,
    drag_start_y: &mut f32,
    drag_start_scroll: &mut f32,
    mouse_pos: Vector2,
    click_pos: Option<Vector2>,
    is_mouse_down: bool,
    alpha: f32,
) -> bool {
    let max_scroll = f32::max(content_height - viewport_height, 0.0);
    *scroll_y = (*scroll_y).clamp(0.0, max_scroll);

    // No scrollbar needed when content fits entirely
    if max_scroll <= 0.0 {
        return false;
    }

    let mut interacted = false;

    let track_top = track_rect.y;
    let track_height = track_rect.height;
    let thumb_height = if content_height > 0.0 {
        (viewport_height / content_height * track_height)
            .max(20.0)
            .min(track_height)
    } else {
        track_height
    };
    let thumb_y = track_top + (*scroll_y / max_scroll) * (track_height - thumb_height);
    let thumb_rect = Rectangle::new(track_rect.x, thumb_y, track_rect.width, thumb_height);

    // Click handling: thumb drag start or page scroll on empty track area
    if let Some(cp) = click_pos {
        if thumb_rect.check_collision_point_rec(cp) {
            // Drag start on thumb
            *dragging = true;
            *drag_start_y = mouse_pos.y;
            *drag_start_scroll = *scroll_y;
            interacted = true;
        } else if track_rect.check_collision_point_rec(cp) {
            // Page scroll on empty track area
            if cp.y < thumb_rect.y {
                *scroll_y = (*scroll_y - viewport_height).clamp(0.0, max_scroll);
            } else {
                *scroll_y = (*scroll_y + viewport_height).clamp(0.0, max_scroll);
            }
            interacted = true;
        }
    }

    // Ongoing drag
    if *dragging {
        interacted = true;
        if is_mouse_down {
            let delta_y = mouse_pos.y - *drag_start_y;
            let scroll_ratio = if track_height > thumb_height {
                delta_y / (track_height - thumb_height)
            } else {
                0.0
            };
            *scroll_y = (*drag_start_scroll + scroll_ratio * max_scroll).clamp(0.0, max_scroll);
        } else {
            *dragging = false;
        }
    }

    // Track
    // Float rects: truncating y and height separately left the thumb up to
    // 1px short of the track's bottom when scrolled to the end.
    d.draw_rectangle_rec(track_rect, Color::new(60, 60, 60, (200.0 * alpha) as u8));

    // Thumb
    d.draw_rectangle_rec(thumb_rect, Color::new(120, 120, 120, (230.0 * alpha) as u8));

    interacted
}
