use crate::ui_components::button::draw_button;
use crate::ui_components::combo_box::{ComboBoxState, draw_combo_box, draw_combo_box_popup};
use crate::ui_components::numeric_slider::{NumericSliderInput, NumericSliderState, draw_u32_numeric_slider};
use crate::ui_components::scrollbar::draw_scrollbar;
use crate::ui_components::toggle::draw_toggle;
use crate::utils::{format_bytes};
use crate::window_manager::InputEvent;
use raylib::prelude::*;
use super::tooltip::{queue_tooltip, settings_tooltip_ready, wrap_text};

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
    beat_bounce_enabled: &mut bool,
    beat_bounce_animation: f32,
    beat_bounce_amount: &mut u32,
    beat_bounce_amount_slider: &mut NumericSliderState,
    beat_bounce_note_value: &mut u32,
    beat_bounce_note_value_slider: &mut NumericSliderState,
    beat_bounce_offbeat: &mut bool,
    beat_bounce_offbeat_animation: f32,
    shake_enabled: &mut bool,
    shake_animation: f32,
    integrate_keyboard: &mut bool,
    keyboard_animation: f32,
    piano_roll_fullscreen: &mut bool,
    piano_roll_fullscreen_animation: f32,
    dock_controls_top: &mut bool,
    dock_controls_top_animation: f32,
    scroll_y: &mut f32,
    mouse_wheel: f32,
    autoplay_on_load: &mut bool,
    autoplay_animation: f32,
    pitch_bend_window_enabled: &mut bool,
    pitch_bend_window_animation: f32,
    pitch_bend_move_window_enabled: &mut bool,
    pitch_bend_move_window_animation: f32,
    pitch_bend_spring_parameters: &mut [u32; 3],
    pitch_bend_spring_sliders: &mut [NumericSliderState; 3],
    pitch_bend_override: &mut bool,
    pitch_bend_override_animation: f32,
    pitch_bend_multiplier: &mut u8,
    unlimited_output: &mut bool,
    unlimited_output_animation: f32,
    s16_clipping: &mut bool,
    s16_clipping_animation: f32,
    // When the S16 warning popup was opened (program time); `None` closed.
    s16_warning_opened_at: &mut Option<f64>,
    midi_ev_buffer_size: &mut u32,
    scrollbar_dragging: &mut bool,
    scrollbar_drag_start_y: &mut f32,
    scrollbar_drag_start_scroll: &mut f32,
    pb_multiplier_slider: &mut NumericSliderState,
    midi_ev_buffer_size_slider: &mut NumericSliderState,
    audio_engine: &mut usize,
    audio_engine_combo: &mut ComboBoxState,
    audio_buffer_secs: &mut u32,
    audio_buffer_slider: &mut NumericSliderState,
    // Prerender buffer (allocated bytes, output rate, Opus-compressed);
    // `None` while no prerender engine is open.
    buffer_memory: Option<(u64, u32, bool)>,
    // Opus bitrate of the prerender buffer.
    opus_bitrate_kbps: &mut u32,
    opus_bitrate_slider: &mut NumericSliderState,
    // Debug builds: the ViSQOL graph window toggle.
    #[cfg(debug_assertions)]
    visqol_window_enabled: &mut bool,
    #[cfg(debug_assertions)]
    visqol_window_animation: f32,
    midi_ev_mode: &mut usize,
    midi_ev_mode_combo: &mut ComboBoxState,
    parse_threads: &mut u32,
    parse_threads_slider: &mut NumericSliderState,
    show_stats: &mut bool,
    show_stats_animation: f32,
    vsync: &mut bool,
    vsync_animation: f32,
    engine_settings: &mut std::collections::HashMap<String, crate::audio_engine::EngineSettingValue>,
    engine_setting_sliders: &mut std::collections::HashMap<String, NumericSliderState>,
) -> bool {
    // Capture text-entry input before entering drawing modes.
    let mut numeric_typed = Vec::new();
    while let Some(ch) = d.get_char_pressed() {
        numeric_typed.push(ch);
    }
    // Editing keys repeat while held.
    let pressed = |d: &RaylibDrawHandle, key| d.is_key_pressed(key) || d.is_key_pressed_repeat(key);
    let numeric_backspace = pressed(d, KeyboardKey::KEY_BACKSPACE);
    let numeric_delete = pressed(d, KeyboardKey::KEY_DELETE);
    let numeric_left = pressed(d, KeyboardKey::KEY_LEFT);
    let numeric_right = pressed(d, KeyboardKey::KEY_RIGHT);
    let numeric_home = d.is_key_pressed(KeyboardKey::KEY_HOME);
    let numeric_end = d.is_key_pressed(KeyboardKey::KEY_END);
    let numeric_enter = d.is_key_pressed(KeyboardKey::KEY_ENTER);
    let numeric_kp_enter = d.is_key_pressed(KeyboardKey::KEY_KP_ENTER);
    let numeric_escape = d.is_key_pressed(KeyboardKey::KEY_ESCAPE);
    let numeric_shift =
        d.is_key_down(KeyboardKey::KEY_LEFT_SHIFT) || d.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
    let numeric_control = d.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
        || d.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);

    // Ease-out cubic: 1 - (1 - t)^3
    let eased_t = 1.0 - (1.0 - alpha_factor).powi(3);

    // Expand into unused window space while keeping a compact minimum size.
    let scale = 0.95 + 0.05 * eased_t;
    let panel_width = (screen_width as f32 - 32.0).clamp(400.0, 720.0);
    let panel_height = (screen_height as f32 - 32.0).clamp(350.0, 760.0);
    let camera = Camera2D {
        target: Vector2::new(panel_width / 2.0, panel_height / 2.0),
        offset: Vector2::new(screen_width as f32 / 2.0, screen_height as f32 / 2.0),
        rotation: 0.0,
        zoom: scale,
    };

    let mut close_clicked = false;
    let panel_rect = Rectangle::new(0.0, 0.0, panel_width, panel_height);
    let border_color = Color::new(0, 135, 200, (255.0 * eased_t) as u8);
    let text_color = Color::new(240, 240, 240, (255.0 * eased_t) as u8);

    // Scroll area configuration
    let content_top = 50.0;
    let content_bottom = panel_height - 50.0;
    let viewport_height = content_bottom - content_top;

    // Simple vertical layout engine: tracks next Y and max content height automatically
    struct VerticalLayout {
        y: f32,
        margin: f32,
        max_bottom: f32,
    }
    impl VerticalLayout {
        fn new(start_y: f32, margin: f32) -> Self {
            Self {
                y: start_y,
                margin,
                max_bottom: start_y,
            }
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
    let mut left_layout = VerticalLayout::new(content_top, 5.0);
    let mut right_layout = VerticalLayout::new(content_top, 5.0);
    let two_columns = panel_width >= 660.0;
    let column_gap = 20.0;
    let column_width = if two_columns {
        // Numeric controls use a fixed internal layout that needs this much room.
        ((panel_width - 60.0) / 2.0).max(300.0)
    } else {
        panel_width - 40.0
    };
    let mut layout = &mut left_layout;
    let mut column_x = 20.0;
    // Per row: (area in panel space, description shown on hover).
    let mut tips: Vec<(Rectangle, &'static str)> = Vec::new();

    {
        let mut d2d = d.begin_mode2D(camera);
        // Program time, shared by the S16 warning popup and its countdown.
        let frame_time = d2d.get_time();

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

        let mut click_pos = if let Some(InputEvent::MouseDown(p)) = input_event {
            let mouse_world_x = (p.x as f32 - camera.offset.x) / scale + camera.target.x;
            let mouse_world_y = (p.y as f32 - camera.offset.y) / scale + camera.target.y;
            Some(Vector2::new(mouse_world_x, mouse_world_y))
        } else {
            None
        };

        // The S16 warning sits above every control: while it is open nothing
        // underneath may react. Its clicks come from the original position.
        let warning_open = s16_warning_opened_at.is_some();
        let warning_click = if warning_open { click_pos } else { None };
        if warning_open {
            click_pos = None;
            audio_engine_combo.open = false;
            midi_ev_mode_combo.open = false;
            // Escape closes the warning without touching the toggle.
            if numeric_escape {
                *s16_warning_opened_at = None;
            }
        }

        // Wheel scrolling is applied after the controls are drawn, unless one of
        // them (a numeric slider's value box) used the wheel itself.
        let scroll_area_rect = Rectangle::new(0.0, content_top, panel_rect.width, viewport_height);
        let mut wheel_consumed = false;

        let sy = |y: f32| y - *scroll_y;

        let is_visible = |y: f32, h: f32| {
            let bottom = y + h;
            let top = y;
            bottom >= content_top && top <= content_bottom
        };

        // Scissor: clip scrollable content to the content area
        {
            let mut midi_ev_mode_combo_rect = None;
            let mut audio_engine_combo_rect = None;
            let clip_tl = d2d.get_world_to_screen2D(Vector2::new(0.0, content_top), camera);
            let clip_br =
                d2d.get_world_to_screen2D(Vector2::new(panel_rect.width, content_bottom), camera);
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
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Render Mode Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Scroll the piano roll by beats instead of by time. Notes keep the same spacing when the tempo changes, so fast sections don't squash together."));
            let toggle_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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

            // Beat Bounce pulses only the piano-roll zoom at the selected note interval.
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Pulse the piano roll's zoom in time with the song's tempo.",
            ));
            let beat_bounce_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(beat_bounce_rect.y, beat_bounce_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    beat_bounce_rect,
                    "Beat Bounce",
                    beat_bounce_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *beat_bounce_enabled = !*beat_bounce_enabled;
                }
            }

            if *beat_bounce_enabled {
                let y = layout.next(52.0);
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 52.0),
                    "How far the piano roll zooms in on each pulse. 0 turns the pulse off.",
                ));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Beat Bounce Amount (%)",
                        beat_bounce_amount,
                        0,
                        25,
                        beat_bounce_amount_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }

                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How often it pulses: 4 = every quarter note, 8 = every eighth note, and so on."));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Beat Interval (1/N Note)",
                        beat_bounce_note_value,
                        1,
                        64,
                        beat_bounce_note_value_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }

                let y = layout.next(30.0);
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 30.0),
                    "Pulse halfway between beats instead of on them.",
                ));
                let offbeat_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
                if is_visible(offbeat_rect.y, offbeat_rect.height) {
                    if draw_toggle(
                        &mut d_scissor,
                        offbeat_rect,
                        "Offbeat",
                        beat_bounce_offbeat_animation,
                        true,
                        eased_t,
                        click_pos,
                        Some(mouse_world_pos),
                    ) {
                        *beat_bounce_offbeat = !*beat_bounce_offbeat;
                    }
                }
            }

            // Shaking Window Toggle
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Shake the keyboard window along with the notes being played.",
            ));
            let shake_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Show the piano keyboard inside the piano roll window instead of as a separate window."));
            let integrate_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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

            // Fullscreen Piano Roll Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Spread the piano roll across the whole application window as a background layer, with the other windows floating above it."));
            let fullscreen_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(fullscreen_rect.y, fullscreen_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    fullscreen_rect,
                    "Fullscreen Piano Roll",
                    piano_roll_fullscreen_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *piano_roll_fullscreen = !*piano_roll_fullscreen;
                }
            }

            // Dock Player Controls to Top Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Pin the Player Controls window to the top of the application window, spanning its full width, while the piano roll is fullscreen."));
            let dock_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(dock_rect.y, dock_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    dock_rect,
                    "Dock Player Controls to Top",
                    dock_controls_top_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *dock_controls_top = !*dock_controls_top;
                }
            }

            // Autoplay on Load Toggle
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Start playing as soon as a MIDI file finishes loading.",
            ));
            let autoplay_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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

            // Audio engine selector
            let y = layout.next(52.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "What makes the sound. KDMAPI sends notes to OmniMIDI as they play. CSynth plays a piano sample set. Prerender engines render ahead into a buffer: smoother on heavy files, but changes take a moment to be heard."));
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(column_x, sy(y), column_width, 52.0);
                audio_engine_combo_rect = Some(combo_rect);
                let was_open = audio_engine_combo.open;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "Audio Engine",
                    &crate::audio_engine::EngineSelection::LABELS,
                    audio_engine,
                    audio_engine_combo,
                    click_pos,
                    eased_t,
                    true,
                    None,
                );

                // Same click shielding as the EV buffer mode popup below.
                if let Some(click) = click_pos {
                    let header =
                        Rectangle::new(combo_rect.x, combo_rect.y + 20.0, combo_rect.width, 28.0);
                    let popup = Rectangle::new(
                        header.x,
                        header.y + header.height,
                        header.width,
                        28.0 * crate::audio_engine::EngineSelection::LABELS.len() as f32,
                    );
                    if header.check_collision_point_rec(click)
                        || (was_open && popup.check_collision_point_rec(click))
                    {
                        click_pos = None;
                    }
                }
            }

            // Prerender buffer length; applies immediately.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How far ahead a prerender engine renders, from 1 second to 10 minutes. More rides out heavy parts but uses more memory. When less than 2 seconds is left, quiet notes are skipped to keep up."));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let enabled = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                        .render_mode()
                        == crate::audio_engine::RenderMode::Prerender;
                    let note = buffer_memory.map(|(used, _, compressed)| {
                        if compressed {
                            format!("Memory: {}", format_bytes(used))
                        } else {
                            format!("Memory: {} (PCM)", format_bytes(used))
                        }
                    });
                    wheel_consumed |= crate::ui_components::numeric_slider::draw_duration_spinner(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Max Audio Buffer",
                        if enabled { note.as_deref() } else { None },
                        audio_buffer_secs,
                        crate::audio_engine::prerender::MIN_MAX_BUFFER_SECS,
                        crate::audio_engine::prerender::MAX_MAX_BUFFER_SECS,
                        audio_buffer_slider,
                        &input,
                        eased_t,
                        enabled,
                        Some("Realtime engine"),
                    );
                }
            }

            // Opus bitrate of the prerender buffer.
            {
                let y = layout.next(52.0);
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter || numeric_kp_enter,
                        escape: numeric_escape,
                    };
                    let enabled = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                        .render_mode()
                        == crate::audio_engine::RenderMode::Prerender;
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Opus Bitrate",
                        opus_bitrate_kbps,
                        6,
                        510,
                        opus_bitrate_slider,
                        &input,
                        eased_t,
                        enabled,
                        Some("Prerender engine only"),
                    );
                }
            }

            // Debug builds: ViSQOL graph window toggle.
            #[cfg(debug_assertions)]
            {
                let y = layout.next(30.0);
                let visqol_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
                if is_visible(visqol_rect.y, visqol_rect.height) {
                    if draw_toggle(
                        &mut d_scissor,
                        visqol_rect,
                        "Audio ViSQOL Graph (debug)",
                        visqol_window_animation,
                        true,
                        eased_t,
                        click_pos,
                        Some(mouse_world_pos),
                    ) {
                        *visqol_window_enabled = !*visqol_window_enabled;
                    }
                }
            }

            // Settings the selected engine publishes for itself (see
            // `EngineSettingDef`): drawn generically here, so an engine's own
            // controls appear without the panel hardcoding any of them.
            for def in crate::audio_engine::EngineSelection::from_index(*audio_engine).settings() {
                match def.kind {
                    crate::audio_engine::EngineSettingKind::U32Range { min, max } => {
                        let y = layout.next(52.0);
                        tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), def.help));
                        if is_visible(sy(y), 52.0) {
                            let input = NumericSliderInput {
                                click_pos,
                                mouse_pos: mouse_world_pos,
                                mouse_down: is_mouse_down,
                                wheel: mouse_wheel,
                                shift: numeric_shift,
                                control: numeric_control,
                                typed: numeric_typed.clone(),
                                backspace: numeric_backspace,
                                delete: numeric_delete,
                                left: numeric_left,
                                right: numeric_right,
                                home: numeric_home,
                                end: numeric_end,
                                enter: numeric_enter,
                                escape: numeric_escape,
                            };
                            let crate::audio_engine::EngineSettingValue::U32(value) = engine_settings
                                .entry(def.key.to_string())
                                .or_insert(def.default)
                            else {
                                continue;
                            };
                            let slider = engine_setting_sliders
                                .entry(def.key.to_string())
                                .or_default();
                            wheel_consumed |= draw_u32_numeric_slider(
                                &mut d_scissor,
                                Rectangle::new(column_x, sy(y), column_width, 52.0),
                                def.label,
                                value,
                                min,
                                max,
                                slider,
                                &input,
                                eased_t,
                                true,
                                None,
                            );
                        }
                    }
                    crate::audio_engine::EngineSettingKind::Bool => {
                        let y = layout.next(30.0);
                        tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), def.help));
                        let rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
                        if is_visible(rect.y, rect.height) {
                            let crate::audio_engine::EngineSettingValue::Bool(value) = engine_settings
                                .entry(def.key.to_string())
                                .or_insert(def.default)
                            else {
                                continue;
                            };
                            if draw_toggle(
                                &mut d_scissor,
                                rect,
                                def.label,
                                if *value { 1.0 } else { 0.0 },
                                true,
                                eased_t,
                                click_pos,
                                Some(mouse_world_pos),
                            ) {
                                *value = !*value;
                            }
                        }
                    }
                }
            }
            // Prerender engines render the file themselves, so the MIDI output
            // pacing settings below have nothing to act on.
            let prerender_engine = crate::audio_engine::EngineSelection::from_index(*audio_engine)
                .render_mode()
                == crate::audio_engine::RenderMode::Prerender;
            let ev_bypassed = *unlimited_output || prerender_engine;

            // S16 Clipping Toggle (prerender engines only)
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Reproduce the old 16-bit output: no limiter, so samples past full scale wrap around and heavy parts fuzz and hiss. Only prerender engines have a limiter to skip.",
            ));
            let s16_clipping_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(s16_clipping_rect.y, s16_clipping_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    s16_clipping_rect,
                    "S16 Clipping",
                    s16_clipping_animation,
                    prerender_engine,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    if *s16_clipping {
                        *s16_clipping = false;
                    } else {
                        // Enabling goes through the read-through warning.
                        *s16_warning_opened_at = Some(frame_time);
                    }
                }
            }

            let bypass_text = if prerender_engine {
                "Bypassed (Prerender)"
            } else {
                "Bypassed (Direct)"
            };
            if ev_bypassed {
                midi_ev_mode_combo.open = false;
            }

            // Unlimited Output Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Send every MIDI event the moment it is due, with no per-frame limit. Most accurate timing, but very heavy files can overload the synth."));
            let unlimited_output_rect =
                Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(unlimited_output_rect.y, unlimited_output_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    unlimited_output_rect,
                    "Unlimited Output",
                    unlimited_output_animation,
                    !prerender_engine,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *unlimited_output = !*unlimited_output;
                }
            }

            // MIDI and pitch-bend settings use the right column on wide panels.
            if two_columns {
                layout = &mut right_layout;
                column_x = 20.0 + column_width + column_gap;
            }

            // MIDI events consumed per 60 Hz output frame.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "How many MIDI events can be sent per frame. Larger keeps up with denser files; smaller protects the synth from floods."));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let slider_enabled = !ev_bypassed && *midi_ev_mode != 0;
                    let mut fixed_value = if *midi_ev_mode == 0 { 8192 } else { 1 };
                    let displayed_value = if slider_enabled {
                        &mut *midi_ev_buffer_size
                    } else {
                        &mut fixed_value
                    };
                    let label = if *midi_ev_mode == 0 {
                        "Events Per Pass"
                    } else {
                        "MIDI EV Buffer Size"
                    };
                    let disabled_text = if ev_bypassed {
                        bypass_text
                    } else {
                        "8192 (Fixed)"
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        label,
                        displayed_value,
                        crate::player::MIN_MIDI_EV_BUFFER_SIZE,
                        crate::player::MAX_MIDI_EV_BUFFER_SIZE,
                        midi_ev_buffer_size_slider,
                        &input,
                        eased_t,
                        slider_enabled,
                        Some(disabled_text),
                    );
                }
            }

            let y = layout.next(52.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "What to do when a file has more events than one frame can send. Legacy: old behaviour, no buffer. Slowdown: playback slows down until it catches up. Catch Up: keeps real time and skips ahead after the backlog."));
            if is_visible(sy(y), 52.0) {
                let combo_rect = Rectangle::new(column_x, sy(y), column_width, 52.0);
                midi_ev_mode_combo_rect = Some(combo_rect);
                let was_open = midi_ev_mode_combo.open && !ev_bypassed;
                draw_combo_box(
                    &mut d_scissor,
                    combo_rect,
                    "EV Buffer Mode",
                    &[
                        "Legacy (No Buffer)",
                        "Buffered: Slowdown",
                        "Buffered: Catch Up",
                    ],
                    midi_ev_mode,
                    midi_ev_mode_combo,
                    click_pos,
                    eased_t,
                    !ev_bypassed,
                    Some(bypass_text),
                );

                // The popup overlays later settings, so clicks handled by its header
                // or option list must not continue into controls underneath it.
                if let Some(click) = click_pos {
                    let header =
                        Rectangle::new(combo_rect.x, combo_rect.y + 20.0, combo_rect.width, 28.0);
                    let popup = Rectangle::new(
                        header.x,
                        header.y + header.height,
                        header.width,
                        28.0 * 3.0,
                    );
                    if header.check_collision_point_rec(click)
                        || (was_open && popup.check_collision_point_rec(click))
                    {
                        click_pos = None;
                    }
                }
            }

            // Pitch Bend Window Toggle
            let y = layout.next(30.0);
            tips.push((
                Rectangle::new(column_x, sy(y), column_width, 30.0),
                "Bend the keyboard window's shape with the song's pitch bend.",
            ));
            let pitch_bend_window_rect =
                Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(pitch_bend_window_rect.y, pitch_bend_window_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pitch_bend_window_rect,
                    "Pitch Bend Window",
                    pitch_bend_window_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *pitch_bend_window_enabled = !*pitch_bend_window_enabled;
                }
            }

            // Pitch Bend OS Window Movement Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Move the whole app window sideways with pitch bend. The three spring settings below control how it moves."));
            let pb_move_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(pb_move_rect.y, pb_move_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    pb_move_rect,
                    "PB Move OS Window",
                    pitch_bend_move_window_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *pitch_bend_move_window_enabled = !*pitch_bend_move_window_enabled;
                }
            }

            if *pitch_bend_move_window_enabled {
                for (index, (label, min, max, tip)) in [
                    ("Spring Mass", 1, 20, "How heavy the window feels when pitch bend moves it. Heavier starts and stops more slowly."),
                    ("Spring Stiffness", 1, 1000, "How strongly the window is pulled to where pitch bend puts it. Higher follows faster."),
                    ("Spring Damping", 0, 200, "How quickly the window stops wobbling. Low bounces around; high settles at once."),
                ]
                .into_iter()
                .enumerate()
                {
                    let y = layout.next(52.0);
                    tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), tip));
                    if is_visible(sy(y), 52.0) {
                        let input = NumericSliderInput {
                            click_pos,
                            mouse_pos: mouse_world_pos,
                            mouse_down: is_mouse_down,
                            wheel: mouse_wheel,
                            shift: numeric_shift,
                            control: numeric_control,
                            typed: numeric_typed.clone(),
                            backspace: numeric_backspace,
                            delete: numeric_delete,
                            left: numeric_left,
                            right: numeric_right,
                            home: numeric_home,
                            end: numeric_end,
                            enter: numeric_enter,
                            escape: numeric_escape,
                        };
                        wheel_consumed |= draw_u32_numeric_slider(
                            &mut d_scissor,
                            Rectangle::new(column_x, sy(y), column_width, 52.0),
                            label,
                            &mut pitch_bend_spring_parameters[index],
                            min,
                            max,
                            &mut pitch_bend_spring_sliders[index],
                            &input,
                            eased_t,
                            true,
                            None,
                        );
                    }
                }
            }

            // Pitch Bend Override Toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Ignore the pitch bend range the file sets and use the PB Multiplier below for every channel."));
            let pb_override_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
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
                let y = layout.next(52.0);
                tips.push((
                    Rectangle::new(column_x, sy(y), column_width, 52.0),
                    "The pitch bend range, in semitones, used when PB Range Override is on",
                ));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    let mut multiplier = *pitch_bend_multiplier as u32;
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "PB Multiplier",
                        &mut multiplier,
                        1,
                        127,
                        pb_multiplier_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                    *pitch_bend_multiplier = multiplier.clamp(1, 127) as u8;
                }
            }

            // Parsing returns to the left column on wide panels.
            if two_columns {
                layout = &mut left_layout;
                column_x = 20.0;
            }

            // Parsing section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Parsing:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Parse Threads slider: 1..=hardware threads. Applies to the next load.
            {
                let y = layout.next(52.0);
                tips.push((Rectangle::new(column_x, sy(y), column_width, 52.0), "CPU threads used to load a MIDI file. More loads faster. Takes effect on the next load."));
                if is_visible(sy(y), 52.0) {
                    let input = NumericSliderInput {
                        click_pos,
                        mouse_pos: mouse_world_pos,
                        mouse_down: is_mouse_down,
                        wheel: mouse_wheel,
                        shift: numeric_shift,
                        control: numeric_control,
                        typed: numeric_typed.clone(),
                        backspace: numeric_backspace,
                        delete: numeric_delete,
                        left: numeric_left,
                        right: numeric_right,
                        home: numeric_home,
                        end: numeric_end,
                        enter: numeric_enter,
                        escape: numeric_escape,
                    };
                    wheel_consumed |= draw_u32_numeric_slider(
                        &mut d_scissor,
                        Rectangle::new(column_x, sy(y), column_width, 52.0),
                        "Parse Threads",
                        parse_threads,
                        1,
                        crate::app_state::cpu_threads(),
                        parse_threads_slider,
                        &input,
                        eased_t,
                        true,
                        None,
                    );
                }
            }

            // Statistics shares the right column with MIDI and pitch-bend settings.
            if two_columns {
                layout = &mut right_layout;
                column_x = 20.0 + column_width + column_gap;
            }

            // Statistics section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Statistics:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // Show Statistics toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Show the stats panel in the top-right corner: notes per second, note count, BPM, and for prerender engines polyphony and buffer."));
            let show_stats_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(show_stats_rect.y, show_stats_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    show_stats_rect,
                    "Show Statistics",
                    show_stats_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *show_stats = !*show_stats;
                }
            }

            // Display section label
            let y = layout.next(18.0);
            if is_visible(sy(y), 18.0) {
                d_scissor.draw_text_ex(
                    &font,
                    "Display:",
                    Vector2::new(column_x, sy(y)),
                    18.0,
                    spacing,
                    text_color,
                );
            }

            // VSync toggle
            let y = layout.next(30.0);
            tips.push((Rectangle::new(column_x, sy(y), column_width, 30.0), "Wait for the monitor's refresh before showing each frame. On: smooth and capped at the refresh rate. Off: frames as fast as the PC can draw them, using more CPU and GPU, with possible tearing."));
            let vsync_rect = Rectangle::new(column_x, sy(y), column_width.min(240.0), 30.0);
            if is_visible(vsync_rect.y, vsync_rect.height) {
                if draw_toggle(
                    &mut d_scissor,
                    vsync_rect,
                    "VSync",
                    vsync_animation,
                    true,
                    eased_t,
                    click_pos,
                    Some(mouse_world_pos),
                ) {
                    *vsync = !*vsync;
                }
            }

            // Popups are deliberately drawn last so they float above the controls below.
            if let Some(combo_rect) = audio_engine_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &crate::audio_engine::EngineSelection::LABELS,
                    audio_engine_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
            if let Some(combo_rect) = midi_ev_mode_combo_rect {
                draw_combo_box_popup(
                    &mut d_scissor,
                    combo_rect,
                    &[
                        "Legacy (No Buffer)",
                        "Buffered: Slowdown",
                        "Buffered: Catch Up",
                    ],
                    midi_ev_mode_combo,
                    mouse_world_pos,
                    eased_t,
                );
            }
        }

        // Measured from what was just laid out, so the scroll range always
        // matches the controls actually drawn.
        // `layout` starts at `content_top`, so its bottom includes that offset.
        let content_height = left_layout
            .content_height(20.0)
            .max(right_layout.content_height(20.0))
            - content_top;
        let max_scroll = f32::max(content_height - viewport_height, 0.0);
        *scroll_y = (*scroll_y).clamp(0.0, max_scroll);

        let track_rect = Rectangle::new(panel_rect.width - 14.0, content_top, 8.0, viewport_height);
        draw_scrollbar(
            &mut d2d,
            track_rect,
            content_height,
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

        if !wheel_consumed
            && mouse_wheel != 0.0
            && !warning_open
            && scroll_area_rect.check_collision_point_rec(mouse_world_pos)
        {
            let scroll_speed = 30.0;
            *scroll_y = (*scroll_y - mouse_wheel * scroll_speed).clamp(0.0, max_scroll);
        }

        // Tooltip for the row under the mouse, once it has rested there.
        let popup_open = audio_engine_combo.open || midi_ev_mode_combo.open || warning_open;
        let hovered_tip = (!popup_open
            && !is_mouse_down
            && !*scrollbar_dragging
            && scroll_area_rect.check_collision_point_rec(mouse_world_pos))
        .then(|| {
            tips.iter()
                .find(|(rect, _)| rect.check_collision_point_rec(mouse_world_pos))
                .map(|&(_, tip)| tip)
        })
        .flatten();
        let now = d2d.get_time();
        if let Some(tip) = settings_tooltip_ready(hovered_tip, now) {
            // Drawn at the end of the frame (`draw_pending_tooltip`), above
            // every other layer.
            queue_tooltip(tip, mouse_pos, eased_t);
        }

        // Close button (fixed coordinates, outside scroll area)
        let btn_rect = Rectangle::new(panel_width - 100.0, panel_height - 40.0, 80.0, 30.0);
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

        // S16 Clipping warning: drawn above every control. It must be read
        // through (5 s) before Enable lights up; Shift+Click skips the wait.
        if let Some(opened_at) = *s16_warning_opened_at {
            const WAIT_SECS: f64 = 5.0;
            let elapsed = (frame_time - opened_at).max(0.0);
            let remaining = (WAIT_SECS - elapsed).max(0.0);
            let ready = remaining == 0.0 || numeric_shift;

            // Dim the whole panel, then put the window above the dim layer.
            d2d.draw_rectangle_rec(panel_rect, Color::new(0, 0, 0, (190.0 * eased_t) as u8));

            let window_width = (panel_width - 60.0).clamp(340.0, 560.0);
            let window_height = 235.0;
            let window = Rectangle::new(
                (panel_width - window_width) / 2.0,
                (panel_height - window_height) / 2.0,
                window_width,
                window_height,
            );
            d2d.draw_rectangle_rec(window, Color::new(60, 6, 6, (255.0 * eased_t) as u8));
            // Flashy: the border pulses red the whole time.
            let pulse = (0.55 + 0.45 * (frame_time * 5.0).sin().abs()) as f32;
            let border_alpha = (pulse * eased_t * 255.0) as u8;
            d2d.draw_rectangle_lines_ex(window, 3.0, Color::new(255, 45, 45, border_alpha));

            // Centered text via the default font, as the tooltip helper does.
            let measured = |text: &str, size: i32| -> i32 {
                let c = std::ffi::CString::new(text).unwrap_or_default();
                // Safety: a valid NUL-terminated string; the default font is loaded.
                unsafe { raylib::ffi::MeasureText(c.as_ptr(), size) }
            };
            let centered_x = |text: &str, size: i32| -> i32 {
                (window.x + (window.width - measured(text, size) as f32) / 2.0) as i32
            };

            let title = "WARNING";
            d2d.draw_text(
                title,
                centered_x(title, 28),
                (window.y + 17.0) as i32,
                28,
                Color::new(255, 60, 50, (255.0 * eased_t) as u8),
            );

            const BODY: &str = "Enabling S16 Clipping skips the output limiter. Samples past full scale wrap around to the opposite end, like 16-bit overflow, so dense parts turn into harsh fuzz. It can get seriously loud: turn the volume down first.";
            let mut line_y = window.y + 49.0;
            for line in wrap_text(BODY, 14, (window.width - 40.0) as i32) {
                d2d.draw_text(
                    line.as_str(),
                    (window.x + 20.0) as i32,
                    line_y as i32,
                    14,
                    Color::new(235, 220, 220, (255.0 * eased_t) as u8),
                );
                line_y += 19.0;
            }

            let status: String = if ready {
                "You can enable it now.".to_string()
            } else {
                format!("You can enable it in {}s", remaining.ceil() as u64)
            };
            d2d.draw_text(
                status.as_str(),
                centered_x(status.as_str(), 16),
                (line_y + 6.0) as i32,
                16,
                Color::new(255, 230, 180, (255.0 * eased_t) as u8),
            );
            let hint = "Shift+Click skips the wait.";
            d2d.draw_text(
                hint,
                centered_x(hint, 13),
                (line_y + 28.0) as i32,
                13,
                Color::new(170, 140, 140, (255.0 * eased_t) as u8),
            );

            let buttons_y = window.y + window.height - 47.0;
            let enable_rect = Rectangle::new(window.x + 20.0, buttons_y, 148.0, 32.0);
            let cancel_rect =
                Rectangle::new(window.x + window.width - 168.0, buttons_y, 148.0, 32.0);
            if ready {
                // Unlocked: hot red, pulsing like the border.
                let is_hovered = enable_rect.check_collision_point_rec(mouse_world_pos);
                let bg = if is_hovered { 235 } else { 170 };
                d2d.draw_rectangle_rec(
                    enable_rect,
                    Color::new(bg, 22, 22, (255.0 * eased_t) as u8),
                );
                d2d.draw_rectangle_lines_ex(
                    enable_rect,
                    2.0,
                    Color::new(255, 80, 60, border_alpha),
                );
                d2d.draw_text(
                    "Enable",
                    enable_rect.x as i32 + 10,
                    enable_rect.y as i32 + 8,
                    18,
                    Color::new(255, 240, 240, (255.0 * eased_t) as u8),
                );
            } else {
                let label = format!("Enable in {}s", remaining.ceil() as u64);
                draw_button(
                    &mut d2d,
                    enable_rect,
                    label.as_str(),
                    18,
                    false,
                    eased_t,
                    warning_click,
                    Some(mouse_world_pos),
                );
            }
            draw_button(
                &mut d2d,
                cancel_rect,
                "Cancel",
                18,
                true,
                eased_t,
                warning_click,
                Some(mouse_world_pos),
            );

            if let Some(click) = warning_click {
                if cancel_rect.check_collision_point_rec(click) {
                    *s16_warning_opened_at = None;
                } else if ready && enable_rect.check_collision_point_rec(click) {
                    *s16_warning_opened_at = None;
                    *s16_clipping = true;
                }
            }
        }
    }

    close_clicked
}
