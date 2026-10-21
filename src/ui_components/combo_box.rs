use raylib::prelude::*;

#[derive(Default)]
pub struct ComboBoxState {
    pub open: bool,
}

/// Draws a compact labelled combo box. Returns true when the selection changes.
pub fn draw_combo_box(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    label: &str,
    options: &[&str],
    selected: &mut usize,
    state: &mut ComboBoxState,
    click_pos: Option<Vector2>,
    alpha: f32,
    enabled: bool,
    disabled_text: Option<&str>,
) -> bool {
    if options.is_empty() {
        return false;
    }
    *selected = (*selected).min(options.len() - 1);
    if !enabled {
        state.open = false;
    }
    let opacity = if enabled { 1.0 } else { 0.45 };
    let a = (255.0 * alpha * opacity) as u8;
    let text = Color::new(240, 240, 240, a);
    let border = Color::new(100, 100, 100, a);
    let box_rect = Rectangle::new(rect.x, rect.y + 20.0, rect.width, 28.0);

    d.draw_text(label, rect.x as i32, rect.y as i32, 16, text);
    d.draw_rectangle_rec(box_rect, Color::new(30, 30, 30, a));
    d.draw_rectangle_lines_ex(box_rect, 1.0, border);
    let shown = if enabled {
        options[*selected]
    } else {
        disabled_text.unwrap_or("Disabled")
    };
    d.draw_text(
        shown,
        box_rect.x as i32 + 7,
        box_rect.y as i32 + 6,
        16,
        text,
    );
    if enabled {
        d.draw_text(
            "v",
            (box_rect.x + box_rect.width - 18.0) as i32,
            box_rect.y as i32 + 5,
            16,
            text,
        );
    }

    let mut changed = false;
    if enabled && let Some(click) = click_pos {
        if box_rect.check_collision_point_rec(click) {
            state.open = !state.open;
        } else if state.open {
            let mut hit = false;
            for (index, _) in options.iter().enumerate() {
                let option_rect = Rectangle::new(
                    box_rect.x,
                    box_rect.y + box_rect.height + index as f32 * 28.0,
                    box_rect.width,
                    28.0,
                );
                if option_rect.check_collision_point_rec(click) {
                    *selected = index;
                    state.open = false;
                    changed = true;
                    hit = true;
                    break;
                }
            }
            if !hit {
                state.open = false;
            }
        }
    }

    changed
}

/// Draws only the expanded list. Call after neighbouring controls so the popup
/// overlays them instead of changing the surrounding layout.
pub fn draw_combo_box_popup(
    d: &mut impl RaylibDraw,
    rect: Rectangle,
    options: &[&str],
    state: &ComboBoxState,
    mouse_pos: Vector2,
    alpha: f32,
) {
    if !state.open {
        return;
    }
    let a = (255.0 * alpha) as u8;
    let text = Color::new(240, 240, 240, a);
    let border = Color::new(100, 100, 100, a);
    let box_rect = Rectangle::new(rect.x, rect.y + 20.0, rect.width, 28.0);
    for (index, option) in options.iter().enumerate() {
        let option_rect = Rectangle::new(
            box_rect.x,
            box_rect.y + box_rect.height + index as f32 * 28.0,
            box_rect.width,
            28.0,
        );
        let hovered = option_rect.check_collision_point_rec(mouse_pos);
        d.draw_rectangle_rec(
            option_rect,
            if hovered {
                Color::new(70, 70, 74, a)
            } else {
                Color::new(45, 45, 48, a)
            },
        );
        d.draw_rectangle_lines_ex(option_rect, 1.0, border);
        d.draw_text(
            option,
            option_rect.x as i32 + 7,
            option_rect.y as i32 + 6,
            16,
            text,
        );
    }
}
