mod app_state;
mod blur_requester;
mod kazumidiparser_cursor;
mod kdmapi;
mod midi_parser_worker;
mod overlay_manager;
mod piano_roll;
mod player;
mod shaking;
mod ui;
mod ui_components;
mod utils;
mod window_manager;

use app_state::AppState;
use player::{PlayerCommand, PlayerFeedback, PlayerState, PlayerHandle};
use raylib::prelude::*;
use std::sync::mpsc;

use crate::kdmapi::get_kdmapi;
use crate::window_manager::WindowManager;

fn insert_char_at(text: &mut String, char_pos: usize, c: char) {
    let byte_pos = text.char_indices().nth(char_pos).map(|(i, _)| i).unwrap_or(text.len());
    text.insert(byte_pos, c);
}

fn remove_char_at(text: &mut String, char_pos: usize) {
    if let Some((byte_pos, c)) = text.char_indices().nth(char_pos) {
        let char_len = c.len_utf8();
        text.drain(byte_pos..byte_pos + char_len);
    }
}

fn move_cursor_up(text: &str, cursor_pos: usize) -> usize {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut char_count = 0;
    let mut current_row = 0;
    let mut current_col = 0;
    for (row_idx, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if cursor_pos >= char_count && cursor_pos <= char_count + line_len {
            current_row = row_idx;
            current_col = cursor_pos - char_count;
            break;
        }
        char_count += line_len + 1;
    }
    if current_row == 0 {
        return 0;
    }
    let mut new_pos = 0;
    for i in 0..current_row - 1 {
        new_pos += lines[i].chars().count() + 1;
    }
    let prev_line_len = lines[current_row - 1].chars().count();
    new_pos += current_col.min(prev_line_len);
    new_pos
}

fn move_cursor_down(text: &str, cursor_pos: usize) -> usize {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut char_count = 0;
    let mut current_row = 0;
    let mut current_col = 0;
    for (row_idx, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if cursor_pos >= char_count && cursor_pos <= char_count + line_len {
            current_row = row_idx;
            current_col = cursor_pos - char_count;
            break;
        }
        char_count += line_len + 1;
    }
    if current_row + 1 >= lines.len() {
        return text.chars().count();
    }
    let mut new_pos = 0;
    for i in 0..current_row + 1 {
        new_pos += lines[i].chars().count() + 1;
    }
    let next_line_len = lines[current_row + 1].chars().count();
    new_pos += current_col.min(next_line_len);
    new_pos
}

fn animate_toggle(condition: bool, value: &mut f32, dt: f32, speed: f32) {
    if condition {
        *value = (*value + dt * speed).min(1.0);
    } else {
        *value = (*value - dt * speed).max(0.0);
    }
}

// Vertical padding required so a bent Piano Keyboard window can extend
// beyond its original rectangle without clipping.
fn compute_kb_bend_padding(win_width: f32, pitch_bend_range: f32) -> f32 {
    // Scale displacement linearly with pitch bend range so extreme ranges
    // keep growing instead of wrapping around tan().
    let base_max_dy = (15.0f32.to_radians()).tan() * (win_width * 0.5);
    let max_dy = base_max_dy * (pitch_bend_range / 12.0);
    (max_dy + 2.0).max(0.0)
}

fn main() {
    let mut app_state = AppState::new();

    let mut window_manager = WindowManager::new();
    let piano_roll_window_handle = window_manager.new_window("Piano Roll", 50, 50, 128, 150);
    window_manager.set_window_min_size(piano_roll_window_handle, Some(100), Some(80));

    let controls_window_handle = window_manager.new_window("Player Controls", 50, 250, 280, 110);
    window_manager.set_window_min_size(controls_window_handle, Some(280), Some(110));
    window_manager.set_window_max_size(controls_window_handle, None, Some(110));

    let piano_keyboard_window_handle =
        window_manager.new_window("Piano Keyboard", 50, 370, 800, 80);
    window_manager.set_window_min_size(piano_keyboard_window_handle, Some(300), Some(40));

    let piano_keyboard_channel_window_handle =
        window_manager.new_window("Piano Keyboard Channel", 50, 470, 800, 320);
    window_manager.set_window_min_size(piano_keyboard_channel_window_handle, Some(300), Some(160));

    let (mut rl, thread) = raylib::init()
        .size(800, 400)
        .resizable()
        .title("KazuMIDIPlayer")
        .vsync()
        .build();

    rl.set_exit_key(None);

    let kdmapi = get_kdmapi();
    if !kdmapi.is_kdmapi_available() {
        panic!(
            "KDMAPI (OmniMIDI) is not available. Please ensure OmniMIDI is installed and configured."
        );
    }
    let stream = kdmapi.open_stream();

    let (tx_commands_to_player, rx_commands_from_main) = mpsc::channel::<PlayerCommand>();
    let (tx_feedback_to_main, rx_feedback_from_player) = mpsc::channel::<PlayerFeedback>();
    let engine = PlayerHandle::spawn(rx_commands_from_main, tx_feedback_to_main, stream);

    let mut render_texture = rl
        .load_render_texture(
            &thread,
            rl.get_screen_width() as u32,
            rl.get_screen_height() as u32,
        )
        .expect("Failed to create render texture");
    render_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);

    let mut blur_intermediate_texture = rl
        .load_render_texture(
            &thread,
            rl.get_screen_width() as u32,
            rl.get_screen_height() as u32,
        )
        .expect("Failed to create render texture");
    blur_intermediate_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);

    // Per-window content render textures (frame + content together)
    let mut content_rts: std::collections::HashMap<i32, RenderTexture2D> =
        std::collections::HashMap::new();
    // Extra vertical padding stored per-window (only Piano Keyboard uses it for bend).
    let mut content_rt_padding: std::collections::HashMap<i32, f32> =
        std::collections::HashMap::new();

    // Canvas bounds for compositing: may be larger than the screen so windows
    // that extend past the screen edges (or bend beyond their rect) are not clipped.
    let mut canvas_offset_x = 0i32;
    let mut canvas_offset_y = 0i32;
    let mut canvas_width = rl.get_screen_width();
    let mut canvas_height = rl.get_screen_height();

    let blur_shader_src = include_str!("../gaussian_blur.fs");
    let mut blur_shader = rl.load_shader_from_memory(&thread, None, Some(blur_shader_src));
    if blur_shader.id == 0 {
        eprintln!("[WARN] Failed to load gaussian_blur.fs shader");
    }
    let blur_rect_uv_loc = blur_shader.get_shader_location("blur_rect_uv");
    let blur_radius_loc = blur_shader.get_shader_location("blur_radius");
    let blur_direction_loc = blur_shader.get_shader_location("direction");

    let piano_keyboard_shader_src = include_str!("../piano_keyboard.fs");
    let mut piano_keyboard_shader =
        rl.load_shader_from_memory(&thread, None, Some(piano_keyboard_shader_src));
    if piano_keyboard_shader.id == 0 {
        eprintln!("[WARN] Failed to load piano_keyboard.fs shader");
    }

    let window_bend_shader_src = include_str!("../window_bend.fs");
    let mut window_bend_shader =
        rl.load_shader_from_memory(&thread, None, Some(window_bend_shader_src));
    if window_bend_shader.id == 0 {
        eprintln!("[WARN] Failed to load window_bend.fs shader");
    }
    let wb_bend_loc = window_bend_shader.get_shader_location("u_bend");
    let wb_bend_range_loc = window_bend_shader.get_shader_location("u_bend_range");
    let wb_max_bend_loc = window_bend_shader.get_shader_location("u_max_bend_degrees");
    let wb_win_width_loc = window_bend_shader.get_shader_location("u_win_width");
    let wb_win_height_loc = window_bend_shader.get_shader_location("u_win_height");
    let wb_curve_power_loc = window_bend_shader.get_shader_location("u_curve_power");
    let wb_window_top_uv_loc = window_bend_shader.get_shader_location("u_window_top_uv");
    let wb_window_bottom_uv_loc = window_bend_shader.get_shader_location("u_window_bottom_uv");

    // Keyboard Layout Texture (Initial)
    let mut keyboard_layout_image = Image::gen_image_color(800, 80, Color::BLANK);
    ui_components::piano_keyboard::draw_piano_keyboard_layout_to_image(
        &mut keyboard_layout_image,
        800.0,
        80.0,
    );
    let mut keyboard_layout_texture = rl
        .load_texture_from_image(&thread, &keyboard_layout_image)
        .expect("Failed to create keyboard layout texture");
    keyboard_layout_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_POINT);

    // Per-key "hit" data for the keyboard shader: a 128x1 texture where column
    // `note` holds the active voice's color (alpha 255) when that key is sounding,
    // or transparent when idle. Updated each frame from the player's active notes.
    let keyboard_hit_image = Image::gen_image_color(128, 1, Color::BLANK);
    let mut keyboard_hit_texture = rl
        .load_texture_from_image(&thread, &keyboard_hit_image)
        .expect("Failed to create keyboard hit texture");
    keyboard_hit_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_POINT);

    let mut current_layout_width = 800;
    let mut current_layout_height = 80;
    let mut last_integrate_keyboard = app_state.integrate_keyboard;

    // Background generator for piano-roll draw data. `roll_req_*` track the
    // coverage we last asked for, so we only re-request when the cached chunk no
    // longer spans the visible window (advanced past it, seeked back, new file).
    let mut piano_roll_worker = piano_roll::PianoRollWorker::new();
    // The chunk covers the whole MIDI (tick 0 -> end), so it only needs to be
    // regenerated when the file itself changes; seeks and stops reuse it.
    let mut piano_roll_req_data: Option<std::sync::Arc<kazumidiparser::MIDIPlayerData>> = None;
    // Per-key forward-scan cursors for the draw, persisted across frames.
    let mut piano_roll_draw_state = piano_roll::PianoRollDrawState::new();
    // Keyboard-highlight forward cursors: which note (if any) is sounding on each
    // key at the current playback tick. Position-based, so it works the same during
    // play, pause and stop. Reset on rewind or when the chunk is rebuilt.
    let mut kb_hl_cursor = [0u32; 128];
    let mut kb_hl_last_tick = 0u32;
    let mut kb_hl_last_gen = u64::MAX;

    while !rl.window_should_close() {
        if rl.is_window_resized() {
            render_texture = rl
                .load_render_texture(
                    &thread,
                    rl.get_screen_width() as u32,
                    rl.get_screen_height() as u32,
                )
                .expect("Failed to recreate render texture");
            render_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);

            blur_intermediate_texture = rl
                .load_render_texture(
                    &thread,
                    rl.get_screen_width() as u32,
                    rl.get_screen_height() as u32,
                )
                .expect("Failed to recreate render texture");
            blur_intermediate_texture
                .set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);
        }

        while let Ok(feedback) = rx_feedback_from_player.try_recv() {
            app_state.handle_player_feedback(feedback, &engine, &tx_commands_to_player);
        }

        if rl.is_file_dropped() {
            let dropped_files = rl.load_dropped_files();
            if let Some(path) = dropped_files.paths().first() {
                app_state.handle_file_drop(path, &tx_commands_to_player);
            }
        }

        let st = engine.get_state();
        app_state.update_player_state(st);

        let playback_position_ns = engine.position_ns();
        let pitch_bend_normalized = engine.get_pitch_bend_normalized();
        let pitch_bend_range = engine.get_pitch_bend_range();
        let mut duration_ns = engine.duration_ns();

        let is_loading = st == PlayerState::Loading || app_state.is_loading;
        let is_midi_loaded = duration_ns > 1 && !is_loading;

        if is_loading {
            duration_ns = 0;
        }

        // Update layout when integration setting changes
        if app_state.integrate_keyboard != last_integrate_keyboard {
            current_layout_width = -1; // Force layout update
            last_integrate_keyboard = app_state.integrate_keyboard;
        }

        // Limit dt to prevent physics explosion on lag (max 100ms)
        let dt = rl.get_frame_time().min(0.1);

        // Update toggle animations
        let anim_speed = 10.0;
        animate_toggle(app_state.is_tick_based, &mut app_state.tick_based_animation, dt, anim_speed);
        animate_toggle(app_state.shake_enabled, &mut app_state.shake_animation, dt, anim_speed);
        animate_toggle(app_state.integrate_keyboard, &mut app_state.keyboard_animation, dt, anim_speed);
        animate_toggle(app_state.autoplay_on_load, &mut app_state.autoplay_animation, dt, anim_speed);
        animate_toggle(app_state.pitch_bend_override_enabled, &mut app_state.pitch_bend_override_animation, dt, anim_speed);
        animate_toggle(app_state.unlimited_output_enabled, &mut app_state.unlimited_output_animation, dt, anim_speed);

        // Shaking Window Logic
        app_state.shake_state.update(
            app_state.shake_enabled,
            app_state.integrate_keyboard,
            piano_roll_window_handle,
            piano_keyboard_window_handle,
            &mut window_manager,
            rl.get_screen_width(),
            rl.get_screen_height(),
            &engine,
            is_midi_loaded,
            is_loading,
            st,
            app_state.is_dragging_seek,
            dt,
        );

        // Check if keyboard layout needs update
        let layout_update_needed = {
            if app_state.integrate_keyboard {
                if let Some(pr_win) = window_manager.get_window(piano_roll_window_handle) {
                    let w = pr_win.rectangle.width as i32;
                    let h = 80; // Fixed height when integrated
                    if w != current_layout_width || h != current_layout_height {
                        Some((w, h))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                if let Some(kb_win) = window_manager.get_window(piano_keyboard_window_handle) {
                    let w = kb_win.rectangle.width as i32;
                    let h = (kb_win.rectangle.height - kb_win.get_window_titlebar_height()) as i32;
                    if w != current_layout_width || h != current_layout_height {
                        Some((w, h))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        if let Some((w, h)) = layout_update_needed {
            // Defer layout updates while the mouse is held down (window resize in progress)
            // to avoid regenerating the texture every frame during drag.
            if w > 0 && h > 0 && !rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                current_layout_width = w;
                current_layout_height = h;

                let mut layout_image = Image::gen_image_color(w, h, Color::BLANK);
                ui_components::piano_keyboard::draw_piano_keyboard_layout_to_image(
                    &mut layout_image,
                    w as f32,
                    h as f32,
                );
                keyboard_layout_texture = rl
                    .load_texture_from_image(&thread, &layout_image)
                    .expect("Failed to recreate keyboard layout texture");
                keyboard_layout_texture
                    .set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_POINT);
            }
        }

        // Input
        let raw_mouse_pos = rl.get_mouse_position();
        // ... (rest of input handling remains the same)
        let screen_rect =
            window_manager::Rectangle::new(0, 0, rl.get_screen_width(), rl.get_screen_height());
        let mouse_point = window_manager::Point {
            x: raw_mouse_pos.x as i32,
            y: raw_mouse_pos.y as i32,
        };
        let mouse_pos = Vector2::new(mouse_point.x as f32, mouse_point.y as f32);

        // Update overlay manager
        let is_processing = app_state.current_progress.is_some();
        if is_loading || is_processing {
            app_state
                .overlay_manager
                .open(app_state::AppOverlay::Loading);
        } else if app_state.overlay_manager.current_kind() == Some(app_state::AppOverlay::Loading) {
            app_state.overlay_manager.close();
        }
        app_state.overlay_manager.update(dt);

        app_state.last_input_event = None;
        let active_window_id;

        // Block interaction if overlay is active
        if app_state.overlay_manager.blocks_input() {
            active_window_id = None;
            if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_LEFT) {
                app_state.last_input_event =
                    Some(window_manager::InputEvent::MouseDown(mouse_point));
            } else if rl.is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT) {
                app_state.last_input_event = Some(window_manager::InputEvent::MouseUp(mouse_point));
            } else if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                app_state.last_input_event =
                    Some(window_manager::InputEvent::MouseMove(mouse_point));
            }
        } else {
            // Normal interaction with window_manager
            if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_LEFT) {
                active_window_id = window_manager.mouse_down(mouse_point, screen_rect);
                if active_window_id.is_some() {
                    app_state.last_input_event =
                        Some(window_manager::InputEvent::MouseDown(mouse_point));
                }
            } else if rl.is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT) {
                active_window_id = window_manager.mouse_up(mouse_point);
                app_state.last_input_event = Some(window_manager::InputEvent::MouseUp(mouse_point));
            } else {
                active_window_id = window_manager.mouse_move(mouse_point);
                if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                    app_state.last_input_event =
                        Some(window_manager::InputEvent::MouseMove(mouse_point));
                }
            }
        }

        let wheel = rl.get_mouse_wheel_move();

        // Push the debug scan-delay knob to the worker.
        piano_roll_worker.set_scan_delay_ms(app_state.scan_delay_ms);

        // Refresh the keyboard hit texture from the notes sounding at the current
        // playback tick (position-based, derived from the piano-roll chunk). This
        // behaves identically during play, pause and stop, and handles overlapping
        // same-key notes. Per-key forward cursors keep it cheap on dense MIDI.
        {
            let mut hit_pixels = [0u8; 128 * 4];
            if let Some(data) = engine.get_midi_data() {
                // Safety: as_ptr() yields a valid pointer for this call.
                let parser_data = unsafe { &*data.as_ptr() };
                let current_tick =
                    kazumidiparser_cursor::ns_to_tick(parser_data, playback_position_ns);
                piano_roll_worker.with_chunk(|chunk| {
                    // Reset the cursors on rewind or when the chunk was rebuilt.
                    if current_tick < kb_hl_last_tick || chunk.generation != kb_hl_last_gen {
                        kb_hl_cursor = [0u32; 128];
                    }
                    kb_hl_last_tick = current_tick;
                    kb_hl_last_gen = chunk.generation;

                    for note in 0..128usize {
                        let bucket = &chunk.notes_by_key[note];
                        // Advance past notes that have already ended.
                        let mut lo = kb_hl_cursor[note] as usize;
                        while lo < bucket.len() && bucket[lo].end_tick <= current_tick {
                            lo += 1;
                        }
                        kb_hl_cursor[note] = lo as u32;

                        // Among notes covering `now` (start <= now < end), pick the
                        // one with the latest start: that is the note drawn on top in
                        // the piano roll (later notes overwrite earlier ones), so the
                        // key shows the same color as the topmost overlapping note.
                        let mut chosen: Option<&piano_roll::RollNote> = None;
                        let mut i = lo;
                        while let Some(n) = bucket.get(i) {
                            if n.start_tick > current_tick {
                                break;
                            }
                            if n.end_tick > current_tick {
                                chosen = Some(n);
                            }
                            i += 1;
                        }
                        if let Some(n) = chosen {
                            let (r, g, b) =
                                utils::get_note_color_by_track_channel(n.track, n.channel);
                            let o = note * 4;
                            hit_pixels[o] = r;
                            hit_pixels[o + 1] = g;
                            hit_pixels[o + 2] = b;
                            hit_pixels[o + 3] = 255;
                        }
                    }
                });
            }
            let _ = keyboard_hit_texture.update_texture(&hit_pixels);
        }

        // Mouse-wheel anywhere (when no overlay is open) adjusts note scroll speed.
        // Multiplicative steps give smooth, symmetric zoom in/out; clamped.
        if wheel != 0.0 && !app_state.overlay_manager.blocks_input() {
            app_state.note_speed =
                (app_state.note_speed * 1.1_f32.powf(wheel)).clamp(0.01, 12.0);
        }

        // Handle keyboard shortcuts only when overlay is completely closed
        if !app_state.overlay_manager.blocks_input()
            && app_state.overlay_manager.get_fade_t() == 0.0
        {
            if is_midi_loaded && !is_loading {
                if rl.is_key_pressed(KeyboardKey::KEY_SPACE) {
                    if st == PlayerState::Playing {
                        tx_commands_to_player.send(PlayerCommand::Pause).ok();
                    } else {
                        if st == PlayerState::Stopped && playback_position_ns >= duration_ns {
                            tx_commands_to_player.send(PlayerCommand::Seek(0)).ok();
                        }
                        tx_commands_to_player.send(PlayerCommand::Play).ok();
                    }
                }

                let jump_ns = 5_000_000_000; // 5 seconds
                let base_pos = playback_position_ns;
                if rl.is_key_pressed(KeyboardKey::KEY_RIGHT) {
                    let new_pos = (base_pos + jump_ns).min(duration_ns);
                    app_state.handle_keyboard_seek(new_pos, &engine);
                }
                if rl.is_key_pressed(KeyboardKey::KEY_LEFT) {
                    let new_pos = base_pos.saturating_sub(jump_ns);
                    app_state.handle_keyboard_seek(new_pos, &engine);
                }
            }
        }

        if (rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL) || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL))
            && rl.is_key_pressed(KeyboardKey::KEY_O)
        {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("MIDI Files", &["mid", "midi", "kar"])
                .pick_file()
            {
                let path_str = path.to_string_lossy().to_string();
                app_state.handle_file_drop(&path_str, &tx_commands_to_player);
            }
        }

        if rl.is_key_pressed(KeyboardKey::KEY_S) {
            if app_state.overlay_manager.current_kind() == Some(app_state::AppOverlay::Settings) {
                app_state.overlay_manager.close();
            } else if !app_state.overlay_manager.blocks_input() {
                let text_editor_focused = app_state.overlay_manager.current_kind()
                    == Some(app_state::AppOverlay::TextEditor)
                    && app_state.editor_focused;
                if !text_editor_focused {
                    app_state.settings_scroll_y = 0.0;
                    app_state.settings_scrollbar_dragging = false;
                    app_state
                        .overlay_manager
                        .open(app_state::AppOverlay::Settings);
                    window_manager.mouse_up(mouse_point);
                }
            }
        }

        if rl.is_key_pressed(KeyboardKey::KEY_P) {
            if app_state.overlay_manager.current_kind() == Some(app_state::AppOverlay::TextEditor) {
                if !app_state.editor_focused {
                    app_state.overlay_manager.close();
                }
            } else if !app_state.overlay_manager.blocks_input() {
                app_state
                    .overlay_manager
                    .open(app_state::AppOverlay::TextEditor);
                app_state.editor_focused = false;
                app_state.editor_cursor_pos = app_state.editor_text.chars().count();
                app_state.editor_cursor_visible = true;
                app_state.editor_cursor_blink_timer = 0.0;
                window_manager.mouse_up(mouse_point);
            }
        }

        if rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
            if app_state.overlay_manager.current_kind() == Some(app_state::AppOverlay::TextEditor) {
                app_state.overlay_manager.close();
            }
        }

        // Text input for text editor overlay
        if app_state.overlay_manager.current_kind() == Some(app_state::AppOverlay::TextEditor)
            && app_state.editor_focused
        {
            while let Some(key) = rl.get_char_pressed() {
                insert_char_at(&mut app_state.editor_text, app_state.editor_cursor_pos, key);
                app_state.editor_cursor_pos += 1;
            }
            if rl.is_key_pressed(KeyboardKey::KEY_BACKSPACE) {
                if app_state.editor_cursor_pos > 0 {
                    remove_char_at(&mut app_state.editor_text, app_state.editor_cursor_pos - 1);
                    app_state.editor_cursor_pos -= 1;
                }
            }
            if rl.is_key_pressed(KeyboardKey::KEY_ENTER) {
                insert_char_at(&mut app_state.editor_text, app_state.editor_cursor_pos, '\n');
                app_state.editor_cursor_pos += 1;
            }
            if rl.is_key_pressed(KeyboardKey::KEY_LEFT) {
                app_state.editor_cursor_pos = app_state.editor_cursor_pos.saturating_sub(1);
            }
            if rl.is_key_pressed(KeyboardKey::KEY_RIGHT) {
                let max_pos = app_state.editor_text.chars().count();
                app_state.editor_cursor_pos = (app_state.editor_cursor_pos + 1).min(max_pos);
            }
            if rl.is_key_pressed(KeyboardKey::KEY_UP) {
                app_state.editor_cursor_pos = move_cursor_up(&app_state.editor_text, app_state.editor_cursor_pos);
            }
            if rl.is_key_pressed(KeyboardKey::KEY_DOWN) {
                app_state.editor_cursor_pos = move_cursor_down(&app_state.editor_text, app_state.editor_cursor_pos);
            }

            app_state.editor_cursor_blink_timer += dt;
            if app_state.editor_cursor_blink_timer >= 0.5 {
                app_state.editor_cursor_blink_timer -= 0.5;
                app_state.editor_cursor_visible = !app_state.editor_cursor_visible;
            }
        } else {
            app_state.editor_cursor_visible = true;
            app_state.editor_cursor_blink_timer = 0.0;
        }

        // Drawing
        let screen_width = rl.get_screen_width();
        let screen_height = rl.get_screen_height();

        // Cursor handling: only use window_manager cursors when overlay is NOT shown or fading
        if app_state.overlay_manager.blocks_input() {
            rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_DEFAULT);
        } else {
            let cursor = window_manager.get_cursor_type(mouse_point);
            match cursor {
                window_manager::CursorType::ResizeEW => {
                    rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_RESIZE_EW)
                }
                window_manager::CursorType::ResizeNS => {
                    rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_RESIZE_NS)
                }
                window_manager::CursorType::ResizeNWSE => {
                    rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_RESIZE_NWSE)
                }
                window_manager::CursorType::ResizeNESW => {
                    rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_RESIZE_NESW)
                }
                window_manager::CursorType::NotAllowed => {
                    rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_NOT_ALLOWED)
                }
                _ => rl.set_mouse_cursor(MouseCursor::MOUSE_CURSOR_DEFAULT),
            }
        }

        let is_mouse_down = rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT);
        let window_ids = window_manager.get_window_ids_sorted();

        // Compute canvas bounds so off-screen windows (or windows that bend beyond
        // their rectangle) are not clipped against the global render texture.
        let bt = window_manager::BORDER_THICKNESS;
        let mut min_x = 0i32;
        let mut min_y = 0i32;
        let mut max_x = rl.get_screen_width();
        let mut max_y = rl.get_screen_height();
        for &wid in &window_ids {
            if let Some(window) = window_manager.get_window(wid) {
                if !window.visible {
                    continue;
                }
                let left = window.rectangle.left() - bt;
                let top = window.rectangle.top() - bt;
                let right = left + window.rectangle.width + 2 * bt;
                let bottom = top + window.rectangle.height + 2 * bt;

                // Bending windows can extend beyond their rectangle; include padding.
                let mut extra_top = 0i32;
                let mut extra_bottom = 0i32;
                let needs_bend = wid == piano_keyboard_window_handle
                    || (wid == piano_roll_window_handle && app_state.integrate_keyboard);
                if needs_bend {
                    let padding = compute_kb_bend_padding(
                        (window.rectangle.width + 2 * bt) as f32,
                        pitch_bend_range,
                    ) as i32;
                    extra_top = padding;
                    extra_bottom = padding;
                }

                min_x = min_x.min(left);
                min_y = min_y.min(top - extra_top);
                max_x = max_x.max(right);
                max_y = max_y.max(bottom + extra_bottom);
            }
        }
        let new_canvas_offset_x = -min_x;
        let new_canvas_offset_y = -min_y;
        let new_canvas_width = max_x - min_x;
        let new_canvas_height = max_y - min_y;

        if new_canvas_width != canvas_width
            || new_canvas_height != canvas_height
            || new_canvas_offset_x != canvas_offset_x
            || new_canvas_offset_y != canvas_offset_y
        {
            canvas_offset_x = new_canvas_offset_x;
            canvas_offset_y = new_canvas_offset_y;
            canvas_width = new_canvas_width;
            canvas_height = new_canvas_height;

            if canvas_width > 0 && canvas_height > 0 {
                render_texture = rl
                    .load_render_texture(&thread, canvas_width as u32, canvas_height as u32)
                    .expect("Failed to recreate render texture");
                render_texture
                    .set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);

                blur_intermediate_texture = rl
                    .load_render_texture(&thread, canvas_width as u32, canvas_height as u32)
                    .expect("Failed to recreate render texture");
                blur_intermediate_texture
                    .set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);
            }
        }

        // Phase 0: Ensure per-window render textures exist and are sized correctly.
        for &wid in &window_ids {
            if let Some(window) = window_manager.get_window(wid) {
                if !window.visible {
                    content_rts.remove(&wid);
                    content_rt_padding.remove(&wid);
                    continue;
                }
                let bt = window_manager::BORDER_THICKNESS;
                let base_w = window.rectangle.width + 2 * bt;
                let base_h = window.rectangle.height + 2 * bt;

                // Piano Keyboard window (and Piano Roll when keyboard is integrated)
                // reserve extra vertical padding for bending.
                let needs_bend = wid == piano_keyboard_window_handle
                    || (wid == piano_roll_window_handle && app_state.integrate_keyboard);
                let padding = if needs_bend {
                    compute_kb_bend_padding(base_w as f32, pitch_bend_range) as i32
                } else {
                    0
                };
                content_rt_padding.insert(wid, padding as f32);

                let rt_w_u = base_w as u32;
                let rt_h_u = (base_h + 2 * padding) as u32;
                if rt_w_u > 0 && rt_h_u > 0 {
                    let needs_recreate = match content_rts.get(&wid) {
                        Some(rt) => {
                            rt.texture.width as u32 != rt_w_u
                                || rt.texture.height as u32 != rt_h_u
                        }
                        None => true,
                    };
                    if needs_recreate {
                        content_rts.remove(&wid);
                        if let Ok(new_rt) = rl.load_render_texture(&thread, rt_w_u, rt_h_u) {
                            content_rts.insert(wid, new_rt);
                        }
                    }
                }
            }
        }

        let mut d = rl.begin_drawing(&thread);

        // Phase 1: Render each window (frame + titlebar + content) into its private RT.
        for &wid in &window_ids {
            let window = match window_manager.get_window(wid) {
                Some(w) if w.visible => w,
                _ => continue,
            };
            let bt = window_manager::BORDER_THICKNESS;
            let rt_w = window.rectangle.width + 2 * bt;
            let rt_h = window.rectangle.height + 2 * bt;
            let padding = *content_rt_padding.get(&wid).unwrap_or(&0.0) as i32;
            if rt_w <= 0 || rt_h + 2 * padding <= 0 {
                continue;
            }
            let win_rt = match content_rts.get_mut(&wid) {
                Some(rt) => rt,
                None => continue,
            };
            {
                let mut d_rt = d.begin_texture_mode(&thread, win_rt);
                d_rt.clear_background(Color::new(0, 0, 0, 0));

                let titlebar_h = window.get_window_titlebar_height();
                let content_w = window.rectangle.width as f32;
                let content_h = (window.rectangle.height - titlebar_h) as f32;

                let wm_border_color = window_manager.get_window_border_color();
                let wm_bg_color = window_manager.get_window_background_color();
                let border_color = Color::new(
                    wm_border_color.r,
                    wm_border_color.g,
                    wm_border_color.b,
                    wm_border_color.a,
                );
                let bg_color = Color::new(
                    wm_bg_color.r,
                    wm_bg_color.g,
                    wm_bg_color.b,
                    wm_bg_color.a,
                );

                let win_org = bt;
                let fw = content_w as i32 + 2 * bt;
                let fh = content_h as i32 + titlebar_h + 2 * bt;

                // Border strips (shifted down by padding to leave room for bending)
                d_rt.draw_rectangle(0, padding, fw, bt, border_color);
                d_rt.draw_rectangle(0, fh - bt + padding, fw, bt, border_color);
                d_rt.draw_rectangle(0, bt + padding, bt, titlebar_h + content_h as i32, border_color);
                d_rt.draw_rectangle(
                    fw - bt,
                    bt + padding,
                    bt,
                    titlebar_h + content_h as i32,
                    border_color,
                );

                // Content background
                d_rt.draw_rectangle(
                    win_org,
                    win_org + titlebar_h + padding,
                    content_w as i32,
                    content_h as i32,
                    bg_color,
                );

                // Title bar
                let wm_tb_active = window_manager.get_window_titlebar_active_color();
                let wm_tb_inactive = window_manager.get_window_titlebar_inactive_color();
                let wm_tb = if window.is_active {
                    wm_tb_active
                } else {
                    wm_tb_inactive
                };
                let tb_color = Color::new(wm_tb.r, wm_tb.g, wm_tb.b, 255);
                d_rt.draw_rectangle(win_org, win_org + padding, content_w as i32, titlebar_h, tb_color);
                d_rt.draw_text(
                    &window.title,
                    win_org + 3,
                    win_org + 5 + padding,
                    titlebar_h - 10,
                    Color::WHITE,
                );

                // Content origin in RT-local coordinates
                let cx = win_org as f32;
                let cy = (win_org + titlebar_h + padding) as f32;
                let cw = content_w;
                let ch = content_h;

                // Apply a single content-area scissor for this window.
                // Individual content renderers no longer need to manage clipping themselves.
                {
                    let mut d_scissor = d_rt.begin_scissor_mode(
                        cx as i32,
                        cy as i32,
                        cw as i32,
                        ch as i32,
                    );

                    match wid {
                        id if id == piano_roll_window_handle => {
                            // Reserve the bottom strip for the integrated keyboard.
                            let keyboard_h = if app_state.integrate_keyboard { 80.0 } else { 0.0 };
                            let roll_h = (ch - keyboard_h).max(0.0);

                            if roll_h > 0.0 {
                                if let Some(data) = engine.get_midi_data() {
                                    // Safety: as_ptr() yields a valid pointer to the C MIDI
                                    // struct, kept alive by the Arc for this call.
                                    let parser_data = unsafe { &*data.as_ptr() };

                                    // Generate once from the very start of the MIDI to its
                                    // end, and only regenerate when the file itself changes.
                                    // Because the whole tick range is covered, seeks and stops
                                    // never need a regeneration. The worker's scan frontier
                                    // (read in the draw) drives the bottom-to-top reveal.
                                    let data_changed = match &piano_roll_req_data {
                                        Some(d) => !std::sync::Arc::ptr_eq(d, &data),
                                        None => true,
                                    };
                                    if data_changed {
                                        // Cancel any in-flight scan and regenerate from tick 0
                                        // to the end. `top_tick` is the priority target so the
                                        // visible window shows at once.
                                        let top_tick = piano_roll::future_tick(
                                            parser_data,
                                            playback_position_ns,
                                            app_state.is_tick_based,
                                            app_state.note_speed,
                                            roll_h,
                                        )
                                        .min(parser_data.max_tick);
                                        piano_roll_worker.regenerate(data.clone(), 0, top_tick);
                                        piano_roll_req_data = Some(data.clone());
                                    }

                                    let roll_area = Rectangle::new(cx, cy, cw, roll_h);
                                    // Render every frame from the (possibly still
                                    // generating) chunk via CPU rasterization.
                                    piano_roll_worker.with_chunk(|chunk| {
                                        piano_roll::draw_piano_roll_chunk(
                                            &mut d_scissor,
                                            parser_data,
                                            playback_position_ns,
                                            roll_area,
                                            app_state.is_tick_based,
                                            app_state.note_speed,
                                            chunk,
                                            &mut piano_roll_draw_state,
                                        );
                                    });
                                }
                            }

                            // Draw integrated keyboard if enabled
                            if app_state.integrate_keyboard {
                                let keyboard_rect =
                                    Rectangle::new(cx, cy + ch - 80.0, cw, 80.0);
                                app_state.draw_keyboard(
                                    &mut d_scissor,
                                    window,
                                    &mut piano_keyboard_shader,
                                    &keyboard_layout_texture,
                                    &keyboard_hit_texture,
                                    Some(keyboard_rect),
                                );
                            }
                        }
                        id if id == controls_window_handle => {
                            let local_mouse = Vector2::new(
                                mouse_pos.x - (window.rectangle.left() - bt) as f32,
                                mouse_pos.y - (window.rectangle.top() - bt) as f32,
                            );

                            let is_inside_content = local_mouse.x >= cx
                                && local_mouse.x < cx + cw
                                && local_mouse.y >= cy
                                && local_mouse.y < cy + ch;

                            let is_input_blocked = app_state.overlay_manager.blocks_input();
                            let current_event = if active_window_id
                                == Some(controls_window_handle)
                                && is_inside_content
                                && !is_input_blocked
                            {
                                app_state.last_input_event.map(|e| match e {
                                    window_manager::InputEvent::MouseDown(p) => {
                                        window_manager::InputEvent::MouseDown(
                                            window_manager::Point {
                                                x: p.x - (window.rectangle.left() - bt),
                                                y: p.y - (window.rectangle.top() - bt),
                                            },
                                        )
                                    }
                                    window_manager::InputEvent::MouseMove(p) => {
                                        window_manager::InputEvent::MouseMove(
                                            window_manager::Point {
                                                x: p.x - (window.rectangle.left() - bt),
                                                y: p.y - (window.rectangle.top() - bt),
                                            },
                                        )
                                    }
                                    window_manager::InputEvent::MouseUp(p) => {
                                        window_manager::InputEvent::MouseUp(
                                            window_manager::Point {
                                                x: p.x - (window.rectangle.left() - bt),
                                                y: p.y - (window.rectangle.top() - bt),
                                            },
                                        )
                                    }
                                })
                            } else {
                                None
                            };

                            let ui_mouse_pos = if is_input_blocked
                                || active_window_id != Some(controls_window_handle)
                            {
                                None
                            } else {
                                Some(local_mouse)
                            };

                            let weighted_pos =
                                app_state.get_weighted_time(playback_position_ns);
                            let total_weighted = app_state.get_weighted_time(duration_ns);

                            ui::draw_progress_bar(
                                &mut d_scissor,
                                playback_position_ns,
                                weighted_pos,
                                total_weighted,
                                cx + 10.0,
                                cy + 10.0,
                            );

                            let seek_x = cx + 10.0;
                            let seek_y = cy + 40.0;
                            let seek_w = cw - 20.0;
                            let seek_h = 20.0;
                            // Note-generation progress over the whole MIDI.
                            let gen_frac = if is_midi_loaded && !is_loading {
                                engine.get_midi_data().map(|data| {
                                    // Safety: as_ptr() valid for this call.
                                    let max_tick = unsafe { (*data.as_ptr()).max_tick };
                                    piano_roll_worker.gen_progress(max_tick).0
                                })
                            } else {
                                None
                            };
                            if ui::draw_seek_bar(
                                &mut d_scissor,
                                seek_x,
                                seek_y,
                                seek_w,
                                seek_h,
                                is_loading,
                                is_midi_loaded,
                                playback_position_ns,
                                duration_ns,
                                current_event,
                                gen_frac,
                            ) {
                                let clamped_mouse_x =
                                    local_mouse.x.clamp(seek_x, seek_x + seek_w);
                                let t = (clamped_mouse_x - seek_x) / seek_w;
                                let seek_ns = (t * duration_ns as f32) as u64;
                                app_state.start_seeking(seek_ns, &engine);
                            }

                            ui::draw_media_controls(
                                &mut d_scissor,
                                &tx_commands_to_player,
                                is_loading,
                                is_midi_loaded,
                                engine.get_state(),
                                playback_position_ns,
                                duration_ns,
                                cx + 10.0,
                                cy + 70.0,
                                current_event,
                                ui_mouse_pos,
                            );

                            // Drag handling does not draw, so it can stay inside the scissor block.
                            if app_state.is_dragging_seek
                                && !app_state.overlay_manager.is_active()
                            {
                                if d_scissor.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT)
                                {
                                    let seek_x = cx + 10.0;
                                    let seek_w = cw - 20.0;
                                    let clamped_mouse_x =
                                        local_mouse.x.clamp(seek_x, seek_x + seek_w);
                                    let t = (clamped_mouse_x - seek_x) / seek_w;
                                    let seek_ns = (t * duration_ns as f32) as u64;
                                    app_state.update_seek_preview(
                                        seek_ns,
                                        &engine,
                                    );
                                }

                                if d_scissor
                                    .is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT)
                                {
                                    app_state.release_seek();
                                }
                            }
                        }
                        id if id == piano_keyboard_window_handle
                            && !app_state.integrate_keyboard =>
                        {
                            app_state.draw_keyboard(
                                &mut d_scissor,
                                window,
                                &mut piano_keyboard_shader,
                                &keyboard_layout_texture,
                                &keyboard_hit_texture,
                                Some(Rectangle::new(cx, cy, cw, ch)),
                            );
                        }
                        id if id == piano_keyboard_channel_window_handle => {
                            let active_notes = engine.get_active_notes();
                            app_state.draw_keyboard_channels(
                                &mut d_scissor,
                                window,
                                &active_notes,
                                cx,
                                cy,
                            );
                        }
                        _ => {}
                    }
                }
            }
        }

        // Phase 2: Composite per-window RTs onto the global render_texture.
        {
            let mut d_rt = d.begin_texture_mode(&thread, &mut render_texture);
            d_rt.clear_background(Color::new(31, 31, 31, 255));

            let fps = 1.0 / d_rt.get_frame_time();
            // Offset by canvas_offset so the FPS text stays at a fixed screen position
            // after the render_texture is shifted by -canvas_offset during final blit.
            d_rt.draw_text(
                format!("FPS: {:.2}", fps).as_str(),
                10 + canvas_offset_x,
                10 + canvas_offset_y,
                20,
                Color::WHITE,
            );

            for &wid in &window_ids {
                let window = match window_manager.get_window(wid) {
                    Some(w) if w.visible => w,
                    _ => continue,
                };
                let win_rt = match content_rts.get(&wid) {
                    Some(rt) => rt,
                    None => continue,
                };

                let rt_w = win_rt.texture.width as f32;
                let rt_h = win_rt.texture.height as f32;
                let screen_x = (window.rectangle.left() - bt) as f32 + canvas_offset_x as f32;
                let screen_y = (window.rectangle.top() - bt) as f32 + canvas_offset_y as f32;

                let needs_bend = wid == piano_keyboard_window_handle
                    || (wid == piano_roll_window_handle && app_state.integrate_keyboard);
                if needs_bend {
                    let win_w = (window.rectangle.width + 2 * bt) as f32;
                    let win_h = (window.rectangle.height + 2 * bt) as f32;
                    let padding = *content_rt_padding.get(&wid).unwrap_or(&0.0);
                    let window_top_uv = padding / rt_h;
                    let window_bottom_uv = 1.0 - padding / rt_h;

                    window_bend_shader.set_shader_value(
                        wb_bend_loc,
                        pitch_bend_normalized.clamp(-1.0, 1.0),
                    );
                    window_bend_shader.set_shader_value(
                        wb_bend_range_loc,
                        pitch_bend_range.max(0.0),
                    );
                    window_bend_shader.set_shader_value(wb_max_bend_loc, 15.0f32);
                    window_bend_shader.set_shader_value(wb_win_width_loc, win_w);
                    window_bend_shader.set_shader_value(wb_win_height_loc, win_h);
                    window_bend_shader.set_shader_value(wb_curve_power_loc, 2.5f32);
                    window_bend_shader.set_shader_value(wb_window_top_uv_loc, window_top_uv);
                    window_bend_shader
                        .set_shader_value(wb_window_bottom_uv_loc, window_bottom_uv);

                    {
                        let mut d_sh = d_rt.begin_shader_mode(&mut window_bend_shader);
                        d_sh.draw_texture_rec(
                            win_rt,
                            Rectangle::new(0.0, 0.0, rt_w, -rt_h),
                            Vector2::new(screen_x, screen_y - padding),
                            Color::WHITE,
                        );
                    }

                    // Show the original window bounds while bending.
                    if pitch_bend_normalized.abs() > 0.001 {
                        d_rt.draw_rectangle_lines_ex(
                            Rectangle::new(screen_x, screen_y, win_w, win_h),
                            2.0,
                            Color::RED,
                        );
                    }
                } else {
                    d_rt.draw_texture_rec(
                        win_rt,
                        Rectangle::new(0.0, 0.0, rt_w, -rt_h),
                        Vector2::new(screen_x, screen_y),
                        Color::WHITE,
                    );
                }
            }
        }

        // Phase 4: Blur request for overlays
        if app_state.overlay_manager.blocks_input() {
            let eased_t = 1.0 - (1.0 - app_state.overlay_manager.get_fade_t()).powi(2);
            let blur_strength = (eased_t * 8.0) as u16;
            if blur_strength > 0 {
                app_state.blur_requester.apply_blur(
                    Rectangle::new(0.0, 0.0, screen_width as f32, screen_height as f32),
                    Some(blur_strength),
                );
            }
        }

        let blur_requests = app_state.blur_requester.consume_all();
        if !blur_requests.is_empty() {
            let screen_w = render_texture.texture.width as f32;
            let screen_h = render_texture.texture.height as f32;

            for request in blur_requests {
                let uv_x = (request.blur_region.x + canvas_offset_x as f32) / screen_w;
                let uv_y = (screen_h
                    - (request.blur_region.y + canvas_offset_y as f32)
                    - request.blur_region.height)
                    / screen_h;
                let uv_w = request.blur_region.width / screen_w;
                let uv_h = request.blur_region.height / screen_h;

                blur_shader.set_shader_value(blur_rect_uv_loc, [uv_x, uv_y, uv_w, uv_h]);
                blur_shader.set_shader_value(blur_radius_loc, request.blur_strength as f32);

                {
                    // Pass 1: Horizontal blur from render_texture to blur_intermediate_texture
                    blur_shader.set_shader_value(blur_direction_loc, [1.0f32, 0.0f32]);
                    let mut d_rt2 = d.begin_texture_mode(&thread, &mut blur_intermediate_texture);
                    let mut d_sh = d_rt2.begin_shader_mode(&mut blur_shader);
                    d_sh.draw_texture_rec(
                        &render_texture,
                        Rectangle::new(
                            0.0,
                            0.0,
                            render_texture.texture.width as f32,
                            -render_texture.texture.height as f32,
                        ),
                        Vector2::new(0.0, 0.0),
                        Color::WHITE,
                    );
                }

                {
                    // Pass 2: Vertical blur from blur_intermediate_texture back to render_texture
                    blur_shader.set_shader_value(blur_direction_loc, [0.0f32, 1.0f32]);
                    let mut d_rt = d.begin_texture_mode(&thread, &mut render_texture);
                    let mut d_sh = d_rt.begin_shader_mode(&mut blur_shader);
                    d_sh.draw_texture_rec(
                        &blur_intermediate_texture,
                        Rectangle::new(
                            0.0,
                            0.0,
                            blur_intermediate_texture.texture.width as f32,
                            -blur_intermediate_texture.texture.height as f32,
                        ),
                        Vector2::new(0.0, 0.0),
                        Color::WHITE,
                    );
                }
            }
        }

        d.draw_texture_rec(
            &render_texture,
            Rectangle::new(
                0.0,
                0.0,
                render_texture.texture.width as f32,
                -render_texture.texture.height as f32,
            ),
            Vector2::new(-canvas_offset_x as f32, -canvas_offset_y as f32),
            Color::WHITE,
        );

        let mut close_requested = false;
        app_state
            .overlay_manager
            .draw(
                &mut d,
                screen_width,
                screen_height,
                |d, fade_t, kind| match kind {
                    app_state::AppOverlay::Settings => {
                        let prev_pb_override = app_state.pitch_bend_override_enabled;
                        let prev_pb_multiplier = app_state.pitch_bend_multiplier;
                        let prev_unlimited_output = app_state.unlimited_output_enabled;
                        if ui::draw_settings_overlay(
                            d,
                            screen_width,
                            screen_height,
                            fade_t,
                            app_state.last_input_event,
                            mouse_pos,
                            is_mouse_down,
                            &mut app_state.is_tick_based,
                            app_state.tick_based_animation,
                            &mut app_state.shake_enabled,
                            app_state.shake_animation,
                            &mut app_state.integrate_keyboard,
                            app_state.keyboard_animation,
                            &mut app_state.settings_scroll_y,
                            wheel,
                            &mut app_state.autoplay_on_load,
                            app_state.autoplay_animation,
                            &mut app_state.pitch_bend_override_enabled,
                            app_state.pitch_bend_override_animation,
                            &mut app_state.pitch_bend_multiplier,
                            &mut app_state.unlimited_output_enabled,
                            app_state.unlimited_output_animation,
                            &mut app_state.settings_scrollbar_dragging,
                            &mut app_state.settings_scrollbar_drag_start_y,
                            &mut app_state.settings_scrollbar_drag_start_scroll,
                            &mut app_state.settings_pb_multiplier_slider_dragging,
                            &mut app_state.scan_delay_ms,
                            &mut app_state.settings_scan_delay_slider_dragging,
                        ) {
                            close_requested = true;
                        }
                        if app_state.pitch_bend_override_enabled != prev_pb_override {
                            engine.set_pitch_bend_override(app_state.pitch_bend_override_enabled);
                        }
                        if app_state.pitch_bend_multiplier != prev_pb_multiplier {
                            engine.set_pitch_bend_multiplier(app_state.pitch_bend_multiplier);
                        }
                        if app_state.unlimited_output_enabled != prev_unlimited_output {
                            engine.set_unlimited_output(app_state.unlimited_output_enabled);
                        }

                        window_manager.set_window_visible(
                            piano_keyboard_window_handle,
                            !app_state.integrate_keyboard,
                        );
                    }
                    app_state::AppOverlay::Loading => {
                        ui::draw_loading_overlay(
                            d,
                            screen_width,
                            screen_height,
                            fade_t,
                            &app_state.current_progress,
                        );
                    }
                    app_state::AppOverlay::TextEditor => {
                        let (close, apply) = ui::draw_text_editor_overlay(
                            d,
                            screen_width,
                            screen_height,
                            fade_t,
                            app_state.last_input_event,
                            mouse_pos,
                            is_mouse_down,
                            &app_state.editor_text,
                            &mut app_state.editor_scroll_y,
                            &mut app_state.editor_scrollbar_dragging,
                            &mut app_state.editor_scrollbar_drag_start_y,
                            &mut app_state.editor_scrollbar_drag_start_scroll,
                            wheel,
                            &mut app_state.editor_focused,
                            app_state.editor_cursor_pos,
                            app_state.editor_cursor_visible,
                        );
                        if close {
                            close_requested = true;
                        }
                        if apply {
                            println!("[TextEditor] Apply clicked. Content:\n{}", app_state.editor_text);
                        }
                    }
                },
            );

        if close_requested {
            app_state.overlay_manager.close();
        }
    }

    // Stop and join the piano-roll worker before teardown, so it is never
    // force-killed mid-allocation (which can deadlock process exit on Windows).
    piano_roll_worker.shutdown();
}
