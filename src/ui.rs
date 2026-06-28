use crate::player::{PlayerCommand, PlayerState};
use crate::ui_components::button::draw_button;
use crate::ui_components::scrollbar::draw_scrollbar;
use crate::ui_components::toggle::draw_toggle;
use crate::utils::format_time;
use crate::window_manager::InputEvent;
use raylib::prelude::*;
use std::sync::mpsc::Sender;

pub fn draw_progress_bar(
    d: &mut impl RaylibDraw,
    playback_position_ns: u64,
    weighted_position_ns: u64,
    total_weighted_ns: u64,
    bar_x: f32,
    bar_y: f32,
) {
    let display_position_ns = playback_position_ns;
    let time_text = format_time(display_position_ns);
    let measure_text = crate::utils::format_measures(weighted_position_ns, total_weighted_ns);
    let combined_text = format!("{}  [{}]", time_text, measure_text);

    d.draw_text(&combined_text, bar_x as i32, bar_y as i32, 20, Color::WHITE);
}

pub fn draw_seek_bar(
    d: &mut impl RaylibDraw,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    is_loading: bool,
    is_midi_loaded: bool,
    playback_position_ns: u64,
    duration_ns: u64,
    input_event: Option<InputEvent>,
    // Note-generation progress (furthest generated tick / whole MIDI), 0..1.
    // Drawn as a colored span under the playback fill so the scan is visible.
    gen_frac: Option<f32>,
) -> bool {
    let seek_bar_bg_color = if is_loading {
        Color::DARKGRAY
    } else {
        Color::GRAY
    };
    let rect = Rectangle::new(x, y, w, h);
    d.draw_rectangle(x as i32, y as i32, w as i32, h as i32, seek_bar_bg_color);

    if is_midi_loaded && !is_loading {
        // Note-generation span, under the playback fill.
        if let Some(frac) = gen_frac {
            crate::ui_components::progress_bar::draw_progress_bar(
                d,
                x,
                y,
                w,
                h,
                &[(0.0, frac, Color::new(70, 90, 150, 255))],
                None,
                Color::new(0, 0, 0, 0),
            );
        }

        // Playback progress
        let current_position_ns = playback_position_ns;
        let fill = ((current_position_ns as f32 / duration_ns as f32) * w) as i32;
        d.draw_rectangle(x as i32, y as i32, fill, h as i32, Color::GREEN);

        // Check for seek initiation
        if let Some(InputEvent::MouseDown(p)) = input_event {
            if rect.check_collision_point_rec(Vector2::new(p.x as f32, p.y as f32)) {
                return true;
            }
        }
    }
    false
}

pub fn draw_media_controls(
    d: &mut impl RaylibDraw,
    tx: &Sender<PlayerCommand>,
    is_loading: bool,
    is_midi_loaded: bool,
    current_state: PlayerState,
    playback_position_ns: u64,
    duration_ns: u64,
    x_offset: f32,
    y_offset: f32,
    input_event: Option<InputEvent>,
    mouse_pos: Option<Vector2>,
) {
    let button_w = 80.0;
    let button_h = 30.0;
    let spacing = 10.0;

    let play_pause_rect = Rectangle::new(x_offset, y_offset, button_w, button_h);
    let stop_rect = Rectangle::new(x_offset + button_w + spacing, y_offset, button_w, button_h);

    let is_playing = current_state == PlayerState::Playing;
    let can_play_pause = !is_loading && is_midi_loaded;
    let can_stop = !is_loading
        && (current_state == PlayerState::Playing || current_state == PlayerState::Paused);

    let click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
        Some(Vector2::new(p.x as f32, p.y as f32))
    } else {
        None
    };

    // Play/Pause Button
    let pp_text = if is_playing { "Pause" } else { "Play" };
    if draw_button(
        d,
        play_pause_rect,
        pp_text,
        18,
        can_play_pause,
        1.0,
        click_pos,
        mouse_pos,
    ) {
        if is_playing {
            tx.send(PlayerCommand::Pause).ok();
        } else {
            if current_state == PlayerState::Stopped && playback_position_ns >= duration_ns {
                tx.send(PlayerCommand::Seek(0)).ok();
            }
            tx.send(PlayerCommand::Play).ok();
        }
    }

    // Stop Button
    if draw_button(
        d, stop_rect, "Stop", 18, can_stop, 1.0, click_pos, mouse_pos,
    ) {
        tx.send(PlayerCommand::Stop).ok();
    }
}

pub fn draw_loading_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    t: f32,
    current_progress: &Option<(u16, u16)>,
) {
    let eased_t = 1.0 - (1.0 - t).powi(2);
    let scale = 0.95 + 0.05 * eased_t;

    let camera = Camera2D {
        target: Vector2::new(200.0, 60.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    let panel_rect = Rectangle::new(0.0, 0.0, 400.0, 120.0);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    {
        let mut d2d = d.begin_mode2D(camera);

        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();
        let spacing = 1.0;

        d2d.draw_text_ex(
            &font,
            "Opening MIDI File...",
            Vector2::new(20.0, 20.0),
            20.0,
            spacing,
            text_color,
        );

        if let Some((current, total)) = current_progress {
            let progress_text = format!("Processing tracks: {}/{}", current, total);
            let bar_x = 20.0;
            let bar_y = 60.0;
            let bar_w = 360.0;
            let bar_h = 24.0;
            let progress_value = if *total > 0 {
                *current as f32 / *total as f32
            } else {
                0.0
            };

            crate::ui_components::progress_bar::draw_progress_bar(
                &mut d2d,
                bar_x,
                bar_y,
                bar_w,
                bar_h,
                &[(0.0, progress_value, Color::new(0, 120, 215, (255.0 * eased_t) as u8))],
                Some(&progress_text),
                Color::new(80, 80, 80, (255.0 * eased_t) as u8),
            );
        }
    }
}

pub fn draw_settings_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    alpha_factor: f32, // Linear timer 0.0 to 1.0
    input_event: Option<InputEvent>,
    mouse_pos: Vector2,
    is_mouse_down: bool,
    is_tick_based: &mut bool,
    tick_based_animation: f32,
    shake_enabled: &mut bool,
    shake_animation: f32,
    integrate_keyboard: &mut bool,
    keyboard_animation: f32,
    scroll_y: &mut f32,
    mouse_wheel: f32,
    autoplay_on_load: &mut bool,
    autoplay_animation: f32,
    pitch_bend_override: &mut bool,
    pitch_bend_override_animation: f32,
    pitch_bend_multiplier: &mut u8,
    unlimited_output: &mut bool,
    unlimited_output_animation: f32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    pb_multiplier_slider_dragging: &mut bool,
    scan_delay_ms: &mut u32,
    scan_delay_slider_dragging: &mut bool,
) -> bool {
    // Ease-out cubic: 1 - (1 - t)^3
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);

    // Panel drawing with Camera2D for perfect scaling and no drift
    let scale = 0.95 + 0.05 * eased_t;
    let camera = Camera2D {
        target: Vector2::new(200.0, 175.0), // Center of 400x350 panel
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0), // Center of screen
        rotation: 0.0,
        zoom: scale,
    };

    let mut close_clicked = false;
    let panel_rect = Rectangle::new(0.0, 0.0, 400.0, 350.0);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    // Scroll area configuration
    let content_top = 50.0;
    let content_bottom = 300.0;
    let viewport_height = content_bottom - content_top;

    // Simple vertical layout engine: tracks next Y and max content height automatically
    struct VerticalLayout {
        y: f32,
        margin: f32,
        max_bottom: f32,
    }
    impl VerticalLayout {
        fn new(start_y: f32, margin: f32) -> Self {
            Self { y: start_y, margin, max_bottom: start_y }
        }
        fn next(&mut self, height: f32) -> f32 {
            let y = self.y;
            self.y += height + self.margin;
            self.max_bottom = self.y - self.margin;
            y
        }
        fn content_height(&self, bottom_padding: f32) -> f32 {
            self.max_bottom + bottom_padding
        }
    }
    // Pre-calculate content height for scrollbar
    let mut measure_layout = VerticalLayout::new(content_top, 5.0);
    measure_layout.next(18.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    measure_layout.next(30.0);
    if *pitch_bend_override {
        measure_layout.next(38.0);
    }
    measure_layout.next(38.0); // Scan Delay slider (debug)
    let content_height = measure_layout.content_height(20.0);
    let max_scroll = f32::max(content_height - viewport_height, 0.0);

    let mut layout = VerticalLayout::new(content_top, 5.0);

    {
        let mut d2d = d.begin_mode2D(camera);

        // Panel Background and Border
        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();
        let spacing = 1.0;

        // Title (outside scroll area)
        d2d.draw_text_ex(
            &font,
            "Settings",
            Vector2::new(20.0, 20.0),
            24.0,
            spacing,
            text_color,
        );

        let mouse_world_x = (mouse_pos.x - camera.offset.x) / scale + camera.target.x;
        let mouse_world_y = (mouse_pos.y - camera.offset.y) / scale + camera.target.y;
        let mouse_world_pos = Vector2::new(mouse_world_x, mouse_world_y);

        let click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
            let mouse_world_x = (p.x as f32 - camera.offset.x) / scale + camera.target.x;
            let mouse_world_y = (p.y as f32 - camera.offset.y) / scale + camera.target.y;
            Some(Vector2::new(mouse_world_x, mouse_world_y))
        } else {
            None
        };

        // Scroll with mouse wheel when hovering the panel content area
        let scroll_area_rect = Rectangle::new(0.0, content_top, panel_rect.width, viewport_height);
        if scroll_area_rect.check_collision_point_rec(mouse_world_pos) && mouse_wheel != 0.0 {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        let track_rect = Rectangle::new(panel_rect.width - 14.0, content_top, 8.0, viewport_height);
        draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height - content_top,
            viewport_height,
            scroll_y,
            scrollbar_dragging,
            scrollbar_drag_start_y,
            scrollbar_drag_start_scroll,
            mouse_world_pos,
            click_pos,
            is_mouse_down,
            eased_t,
        );

        let sy = |y: f32| y - *scroll_y;

        let is_visible = |y: f32, h: f32| {
            let bottom = y + h;
            let top = y;
            bottom >= content_top && top <= content_bottom
        };

        // Scissor: clip scrollable content to the content area
        {
            let clip_tl = d2d.get_world_to_screen2D(Vector2::new(0.0, content_top), camera);
            let clip_br = d2d.get_world_to_screen2D(Vector2::new(panel_rect.width, content_bottom), camera);
            let sx = clip_tl.x as i32;
            let sy_scissor = clip_tl.y as i32;
            let sw = (clip_br.x - clip_tl.x) as i32;
            let sh = (clip_br.y - clip_tl.y) as i32;
            let mut d_scissor = d2d.begin_scissor_mode(sx, sy_scissor, sw, sh);

            // Label for the section
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Rendering:",
                    Vector2::new(20.0, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Render Mode Toggle
            let y = layout.next(30.0);
            let toggle_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(toggle_rect.y, toggle_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    toggle_rect,
                    "Tick Based Mode",
                    tick_based_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *is_tick_based = !*is_tick_based;
                }
            }

            // Shaking Window Toggle
            let y = layout.next(30.0);
            let shake_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(shake_rect.y, shake_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    shake_rect,
                    "Shaking Window",
                    shake_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *shake_enabled = !*shake_enabled;
                }
            }

            // Integrate Keyboard Toggle
            let y = layout.next(30.0);
            let integrate_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(integrate_rect.y, integrate_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    integrate_rect,
                    "Integrate Keyboard",
                    keyboard_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *integrate_keyboard = !*integrate_keyboard;
                }
            }

            // Autoplay on Load Toggle
            let y = layout.next(30.0);
            let autoplay_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(autoplay_rect.y, autoplay_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    autoplay_rect,
                    "Autoplay on Load",
                    autoplay_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *autoplay_on_load = !*autoplay_on_load;
                }
            }

            // Unlimited Output Toggle
            let y = layout.next(30.0);
            let unlimited_output_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(unlimited_output_rect.y, unlimited_output_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    unlimited_output_rect,
                    "Unlimited Output",
                    unlimited_output_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *unlimited_output = !*unlimited_output;
                }
            }

            // Pitch Bend Override Toggle
            let y = layout.next(30.0);
            let pb_override_rect = Rectangle::new(20.0, sy(y), 200.0, 30.0);
            if is_visible(pb_override_rect.y, pb_override_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pb_override_rect,
                    "PB Range Override",
                    pitch_bend_override_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    let was_enabled = *pitch_bend_override;
                    *pitch_bend_override = !was_enabled;
                    if was_enabled {
                        *pitch_bend_multiplier = 12;
                    } else {
                        *pitch_bend_multiplier = (*pitch_bend_multiplier).max(1);
                    }
                }
            }

            // Pitch Bend Multiplier Slider (only visible when override is on)
            if *pitch_bend_override {
                let y = layout.next(38.0);
                if is_visible(sy(y), 38.0) {
                    let slider_rect = Rectangle::new(20.0, sy(y) + 18.0, 200.0, 20.0);
                    let label = format!("PB Multiplier: {}", *pitch_bend_multiplier);
                    d_scissor.draw_text_ex(
                        &font,
                        &label,
                        Vector2::new(slider_rect.x, sy(y)),
                        16.0,
                        1.0,
                        text_color,
                    );
                    d_scissor.draw_rectangle(
                        slider_rect.x as i32,
                        (slider_rect.y + 8.0) as i32,
                        slider_rect.width as i32,
                        4,
                        Color::new(80, 80, 80, (255.0 * eased_t) as u8),
                    );
                    let t = ((*pitch_bend_multiplier as f32 - 1.0) / 126.0).clamp(0.0, 1.0);
                    let thumb_x = slider_rect.x + t * slider_rect.width;
                    let thumb_rect = Rectangle::new(thumb_x - 6.0, slider_rect.y + 2.0, 12.0, 16.0);
                    d_scissor.draw_rectangle_rec(thumb_rect, Color::new(0, 120, 215, (255.0 * eased_t) as u8));

                    // Start drag on click
                    if let Some(cp) = click_pos {
                        if thumb_rect.check_collision_point_rec(cp) || slider_rect.check_collision_point_rec(cp) {
                            *pb_multiplier_slider_dragging = true;
                        }
                    }

                    // Ongoing drag: value follows the mouse even outside the slider rect
                    if *pb_multiplier_slider_dragging {
                        if is_mouse_down {
                            let clamped_x = mouse_world_pos.x.clamp(slider_rect.x, slider_rect.x + slider_rect.width);
                            let new_t = (clamped_x - slider_rect.x) / slider_rect.width;
                            *pitch_bend_multiplier = (new_t * 126.0 + 1.0).round() as u8;
                        } else {
                            *pb_multiplier_slider_dragging = false;
                        }
                    }
                }
            }

            // Scan Delay slider (debug): artificially slow the piano-roll scan.
            const SCAN_DELAY_MAX: f32 = 1000.0; // milliseconds
            {
                let y = layout.next(38.0);
                if is_visible(sy(y), 38.0) {
                    let slider_rect = Rectangle::new(20.0, sy(y) + 18.0, 200.0, 20.0);
                    let label = format!("Scan Delay: {} ms", *scan_delay_ms);
                    d_scissor.draw_text_ex(
                        &font,
                        &label,
                        Vector2::new(slider_rect.x, sy(y)),
                        16.0,
                        1.0,
                        text_color,
                    );
                    d_scissor.draw_rectangle(
                        slider_rect.x as i32,
                        (slider_rect.y + 8.0) as i32,
                        slider_rect.width as i32,
                        4,
                        Color::new(80, 80, 80, (255.0 * eased_t) as u8),
                    );
                    let t = (*scan_delay_ms as f32 / SCAN_DELAY_MAX).clamp(0.0, 1.0);
                    let thumb_x = slider_rect.x + t * slider_rect.width;
                    let thumb_rect = Rectangle::new(thumb_x - 6.0, slider_rect.y + 2.0, 12.0, 16.0);
                    d_scissor.draw_rectangle_rec(thumb_rect, Color::new(0, 120, 215, (255.0 * eased_t) as u8));

                    if let Some(cp) = click_pos {
                        if thumb_rect.check_collision_point_rec(cp) || slider_rect.check_collision_point_rec(cp) {
                            *scan_delay_slider_dragging = true;
                        }
                    }

                    if *scan_delay_slider_dragging {
                        if is_mouse_down {
                            let clamped_x = mouse_world_pos.x.clamp(slider_rect.x, slider_rect.x + slider_rect.width);
                            let new_t = (clamped_x - slider_rect.x) / slider_rect.width;
                            *scan_delay_ms = (new_t * SCAN_DELAY_MAX).round() as u32;
                        } else {
                            *scan_delay_slider_dragging = false;
                        }
                    }
                }
            }

        }

        // Close button (fixed coordinates, outside scroll area)
        let btn_rect = Rectangle::new(300.0, 310.0, 80.0, 30.0);
        if draw_button(
            &mut d2d,
            btn_rect,
            "Close",
            18,
            true,
            eased_t,
            click_pos,
            Some(mouse_world_pos),
        ) {
            close_clicked = true;
        } else if let Some(pos) = click_pos {
            if !panel_rect.check_collision_point_rec(pos) {
                close_clicked = true;
            }
        }

    }

    close_clicked
}

pub fn draw_text_editor_overlay(
    d: &mut RaylibDrawHandle,
    screen_width: i32,
    screen_height: i32,
    alpha_factor: f32,
    input_event: Option<InputEvent>,
    mouse_pos: Vector2,
    is_mouse_down: bool,
    text: &str,
    scroll_y: &mut f32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    mouse_wheel: f32,
    focused: &mut bool,
    cursor_pos: usize,
    cursor_visible: bool,
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

    let text_area_x = 20.0;
    let text_area_y = 50.0;
    let text_area_w = panel_w - 40.0;
    let text_area_h = 250.0;
    let line_height = 20.0;

    let lines: Vec<&str> = text.split('\n').collect();
    let content_height = (lines.len() as f32 * line_height) + 16.0;
    let max_scroll: f32 = f32::max(content_height - text_area_h, 0.0);
    *scroll_y = (*scroll_y).clamp(0.0, max_scroll);

    let mut close_clicked = false;
    let mut apply_clicked = false;

    {
        let mut d2d = d.begin_mode2D(camera);

        let bg_color = Color::new(45, 45, 48, (255.0 * eased_t) as u8);
        d2d.draw_rectangle_rec(panel_rect, bg_color);
        d2d.draw_rectangle_lines_ex(panel_rect, 1.0, border_color);

        let font = d2d.get_font_default();

        // Title
        d2d.draw_text_ex(
            &font,
            "Text Editor",
            Vector2::new(20.0, 20.0),
            24.0,
            1.0,
            text_color,
        );

        let mouse_world_x = (mouse_pos.x - camera.offset.x) / scale + camera.target.x;
        let mouse_world_y = (mouse_pos.y - camera.offset.y) / scale + camera.target.y;
        let mouse_world_pos = Vector2::new(mouse_world_x, mouse_world_y);

        let click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
            let wx = (p.x as f32 - camera.offset.x) / scale + camera.target.x;
            let wy = (p.y as f32 - camera.offset.y) / scale + camera.target.y;
            Some(Vector2::new(wx, wy))
        } else {
            None
        };

        // Text area background
        let text_area_rect = Rectangle::new(text_area_x, text_area_y, text_area_w, text_area_h);
        d2d.draw_rectangle_rec(text_area_rect, Color::new(30, 30, 30, (255.0 * eased_t) as u8));
        let text_area_border = if *focused {
            Color::new(0, 170, 255, (255.0 * eased_t) as u8)
        } else {
            Color::new(80, 80, 80, (255.0 * eased_t) as u8)
        };
        d2d.draw_rectangle_lines_ex(text_area_rect, 1.0, text_area_border);

        // Mouse wheel scrolling
        if text_area_rect.check_collision_point_rec(mouse_world_pos) && mouse_wheel != 0.0 {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        // Scissor for text area
        {
            let clip_tl = d2d.get_world_to_screen2D(Vector2::new(text_area_x, text_area_y), camera);
            let clip_br = d2d.get_world_to_screen2D(Vector2::new(text_area_x + text_area_w, text_area_y + text_area_h), camera);
            let sx = clip_tl.x as i32;
            let sy_scissor = clip_tl.y as i32;
            let sw = (clip_br.x - clip_tl.x) as i32;
            let sh = (clip_br.y - clip_tl.y) as i32;
            let mut d_scissor = d2d.begin_scissor_mode(sx, sy_scissor, sw, sh);

            let mut cursor_row = 0;
            let mut cursor_col = 0;
            let mut char_count = 0;
            for (i, line) in lines.iter().enumerate() {
                let line_len = line.chars().count();
                if cursor_pos >= char_count && cursor_pos <= char_count + line_len {
                    cursor_row = i;
                    cursor_col = cursor_pos - char_count;
                    break;
                }
                char_count += line_len + 1; // +1 for '\n'
            }

            // Auto-scroll to keep cursor visible
            if *focused {
                let cursor_y_in_content = text_area_y + 8.0 + cursor_row as f32 * line_height;
                let cursor_y_on_screen = cursor_y_in_content - *scroll_y;
                if cursor_y_on_screen < text_area_y {
                    *scroll_y = cursor_y_in_content - text_area_y;
                } else if cursor_y_on_screen + line_height > text_area_y + text_area_h {
                    *scroll_y = cursor_y_in_content + line_height - (text_area_y + text_area_h);
                }
                *scroll_y = (*scroll_y).clamp(0.0, max_scroll);
            }

            for (i, line) in lines.iter().enumerate() {
                let y = text_area_y + 8.0 + i as f32 * line_height - *scroll_y;
                if y + line_height >= text_area_y && y <= text_area_y + text_area_h {
                    d_scissor.draw_text_ex(
                        &font,
                        line,
                        Vector2::new(text_area_x + 8.0, y),
                        16.0,
                        1.0,
                        text_color,
                    );

                    if *focused && i == cursor_row && cursor_visible {
                        let prefix: String = line.chars().take(cursor_col).collect();
                        let prefix_width = font.measure_text(&prefix, 16.0, 1.0).x;
                        let cx = text_area_x + 8.0 + prefix_width;
                        let cy1 = y;
                        let cy2 = y + line_height;
                        d_scissor.draw_line_v(
                            Vector2::new(cx, cy1),
                            Vector2::new(cx, cy2),
                            text_color,
                        );
                    }
                }
            }
        }

        // Scrollbar (drawn after scissored text so it's always on top)
        let track_rect = Rectangle::new(text_area_x + text_area_w - 9.0, text_area_y + 1.0, 8.0, text_area_h - 2.0);
        let scrollbar_interacted = draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height,
            text_area_h,
            scroll_y,
            scrollbar_dragging,
            scrollbar_drag_start_y,
            scrollbar_drag_start_scroll,
            mouse_world_pos,
            click_pos,
            is_mouse_down,
            eased_t,
        );

        if let Some(pos) = click_pos {
            if scrollbar_interacted {
                // スクロールバー操作時はフォーカス変更しない
            } else if text_area_rect.check_collision_point_rec(pos) {
                *focused = true;
            } else if panel_rect.check_collision_point_rec(pos) {
                *focused = false;
            }
        }

        // Buttons
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
    }

    (close_clicked, apply_clicked)
}
