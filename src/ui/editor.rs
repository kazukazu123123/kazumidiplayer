use crate::ui_components::button::draw_button;
use crate::ui_components::text_editor::{LineCommentHighlighter, TextEditorMouse, TextEditorState, draw_text_editor};
use crate::window_manager::InputEvent;
use raylib::prelude::*;

pub fn draw_text_editor_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    alpha_factor: f32,
    input_event: Option<InputEvent>,
    mouse_pos: Vector2,
    is_mouse_down: bool,
    mouse_wheel: f32,
    editor: &mut TextEditorState,
) -> (bool, bool) {
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);
    let scale = 0.95 + 0.05 * eased_t;

    let panel_w = 600.0;
    let panel_h = 350.0;
    let camera = Camera2D {
        target: Vector2::new(panel_w / 2.0, panel_h / 2.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    let panel_rect = Rectangle::new(0.0, 0.0, panel_w, panel_h);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    let mut close_clicked = false;
    let mut apply_clicked = false;

    let shift =
        d.is_key_down(KeyboardKey::KEY_LEFT_SHIFT) || d.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
    let mut d2d = d.begin_mode2D(camera);

    let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
    d2d.draw_rectangle_rec(panel_rect, bg_color);
    d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

    let font = d2d.get_font_default();
    d2d.draw_text_ex(
        &font,
        "Text Editor",
        Vector2::new(20.0, 20.0),
        24.0,
        1.0,
        text_color,
    );

    let to_world = |x: f32, y: f32| {
        Vector2::new(
            (x - camera.offset.x) / scale + camera.target.x,
            (y - camera.offset.y) / scale + camera.target.y,
        )
    };
    let mouse_world_pos = to_world(mouse_pos.x, mouse_pos.y);
    let click_pos = match input_event {
        Some(InputEvent::MouseDown(p)) => Some(to_world(p.x as f32, p.y as f32)),
        _ => None,
    };

    // Clicks outside the panel leave focus alone, as before.
    let editor_click = click_pos.filter(|p| panel_rect.check_collision_point_rec(*p));
    draw_text_editor(
        &mut d2d,
        &camera,
        &font,
        Rectangle::new(20.0, 50.0, panel_w - 40.0, 250.0),
        editor,
        &TextEditorMouse {
            pos: mouse_world_pos,
            click: editor_click,
            down: is_mouse_down,
            wheel: mouse_wheel,
            shift,
        },
        eased_t,
        Some(&DEMO_HIGHLIGHTER),
    );

    let close_rect = Rectangle::new(370.0, 305.0, 100.0, 30.0);
    let apply_rect = Rectangle::new(480.0, 305.0, 100.0, 30.0);
    if draw_button(
        &mut d2d,
        close_rect,
        "Close",
        18,
        true,
        eased_t,
        click_pos,
        Some(mouse_world_pos),
    ) {
        close_clicked = true;
    }
    if draw_button(
        &mut d2d,
        apply_rect,
        "Apply",
        18,
        true,
        eased_t,
        click_pos,
        Some(mouse_world_pos),
    ) {
        apply_clicked = true;
    }
    // Click outside panel does NOT close the editor (intentional)

    (close_clicked, apply_clicked)
}

/// Shows off highlighting in the debug editor: `#` and `//` lines are dimmed.
const DEMO_HIGHLIGHTER: LineCommentHighlighter = LineCommentHighlighter {
    markers: &["#", "//"],
    color: Color::new(110, 160, 110, 255),
};
