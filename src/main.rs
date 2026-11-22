mod app_state;
mod audio_engine;
mod audio_limiter;
mod blur_requester;
mod flip_3d;
mod kazumidiparser_cursor;
mod kdmapi;
mod midi_parser_worker;
mod overlay_manager;
mod piano_roll;
mod player;
mod shaking;
mod settings_sync;
mod ui;
mod ui_components;
mod utils;
mod window_manager;

use app_state::AppState;
use flip_3d::Flip3D;
use player::{PlayerCommand, PlayerFeedback, PlayerHandle, PlayerState};
use raylib::prelude::*;
use std::sync::mpsc;

use crate::window_manager::WindowManager;

fn animate_toggle(condition: bool, value: &mut f32, dt: f32, speed: f32) {
    if condition {
        *value = (*value + dt * speed).min(1.0);
    } else {
        *value = (*value - dt * speed).max(0.0);
    }
}

/// Piano-roll speed multiplier for Beat Bounce. MIDI ticks make the phase follow
/// tempo changes automatically. `note_value` is the denominator in 1/N note.
fn beat_bounce_multiplier(
    data: &kazumidiparser::KazuMIDIParserData,
    position_ns: u64,
    playing: bool,
    amount_percent: u32,
    note_value: u32,
    offbeat: bool,
    animation: f32,
) -> f32 {
    if !playing || animation <= 0.0 || amount_percent == 0 {
        return 1.0;
    }
    let division = (data.header.division as u32).max(1);
    let tick = kazumidiparser_cursor::ns_to_tick(data, position_ns) as f64;
    let period_ticks = division as f64 * 4.0 / note_value.clamp(1, 64) as f64;
    let phase_offset = if offbeat { period_ticks * 0.5 } else { 0.0 };
    let phase = ((tick + phase_offset) % period_ticks) / period_ticks;
    // A quick attack at each selected note boundary and a smooth decay.
    let pulse = (1.0 - phase as f32).powi(4);
    1.0 + amount_percent.min(25) as f32 / 100.0 * pulse * animation
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

// Height the keyboard takes off the bottom of the Piano Roll window when it is
// integrated. The 75 white keys span the full width, so deriving the height from
// one key's width keeps a real keyboard's proportions as the window widens,
// instead of smearing the keys out against a fixed height. The roll keeps the
// larger half: on a wide, short window the ratio would otherwise eat all of it.
fn compute_integrated_keyboard_height(content_width: f32, content_height: f32) -> f32 {
    const WHITE_KEY_ASPECT: f32 = 6.0;
    let white_key_width = content_width / 75.0;
    (white_key_width * WHITE_KEY_ASPECT)
        .min(content_height * 0.5)
        .max(0.0)
}

fn main() {
    let mut app_state = AppState::new();

    let mut window_manager = WindowManager::new();
    let piano_roll_window_handle = window_manager.new_window("Piano Roll", 50, 50, 128, 150);
    window_manager.set_window_min_size(piano_roll_window_handle, Some(100), Some(80));

    let controls_window_handle = window_manager.new_window("Player Controls", 50, 250, 350, 110);
    window_manager.set_window_min_size(controls_window_handle, Some(280), Some(110));
    window_manager.set_window_max_size(controls_window_handle, None, Some(110));

    let piano_keyboard_window_handle =
        window_manager.new_window("Piano Keyboard", 50, 370, 800, 80);
    window_manager.set_window_min_size(piano_keyboard_window_handle, Some(300), Some(40));

    let piano_keyboard_channel_window_handle =
        window_manager.new_window("Piano Keyboard Channel", 50, 470, 800, 320);
    window_manager.set_window_min_size(piano_keyboard_channel_window_handle, Some(300), Some(160));

    // Debug builds: ViSQOL graph of the prerender Opus stream.
    #[cfg(debug_assertions)]
    let visqol_window_handle = window_manager.new_window("Audio ViSQOL (debug)", 50, 810, 800, 220);
    #[cfg(debug_assertions)]
    window_manager.set_window_min_size(visqol_window_handle, Some(300), Some(120));
    #[cfg(debug_assertions)]
    window_manager.set_window_visible(visqol_window_handle, false);

    let (mut rl, thread) = raylib::init()
        .size(800, 400)
        .resizable()
        .title("KazuMIDIPlayer")
        .vsync()
        .msaa_4x()
        .build();

    rl.set_exit_key(None);

    let (tx_commands_to_player, rx_commands_from_main) = mpsc::channel::<PlayerCommand>();
    let (tx_feedback_to_main, rx_feedback_from_player) = mpsc::channel::<PlayerFeedback>();

    // The output arrives asynchronously (an engine may take a while to open);
    // until then the player runs silent.
    let startup_audio_engine = audio_engine::EngineSelection::from_index(app_state.settings.audio_engine);
    let mut pending_audio_engine = Some((startup_audio_engine, startup_audio_engine.start()));
    let engine = PlayerHandle::spawn(rx_commands_from_main, tx_feedback_to_main, None);
    engine.set_audio_buffer_secs(app_state.settings.audio_buffer_secs);

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

    // Screen-sized layer for the FPS / statistics readouts. They are
    // composited into the scene texture above every window, so no
    // window can cover them, but the overlay backdrop blur still
    // applies to them like the rest of the scene.
    let mut readouts_texture = rl
        .load_render_texture(
            &thread,
            rl.get_screen_width() as u32,
            rl.get_screen_height() as u32,
        )
        .expect("Failed to create readouts texture");
    readouts_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);

    // Per-window content render textures (frame + content together)
    let mut content_rts: std::collections::HashMap<i32, RenderTexture2D> =
        std::collections::HashMap::new();
    // Extra vertical padding stored per-window (only Piano Keyboard uses it for bend).
    let mut content_rt_padding: std::collections::HashMap<i32, f32> =
        std::collections::HashMap::new();

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
    let mut last_integrate_keyboard = app_state.settings.integrate_keyboard;
    let mut last_piano_roll_fullscreen = app_state.settings.piano_roll_fullscreen;
    // Rectangle and z-order of the piano roll window, saved when the
    // fullscreen piano roll starts so it can be restored as it was.
    let mut saved_roll_layout: Option<(window_manager::Rectangle, i32)> = None;
    // Whether the player controls are currently docked to the top of
    // the application window, and the rectangle the window had before
    // docking, so it can be restored as it was.
    let mut last_controls_docked = false;
    let mut saved_controls_rect: Option<window_manager::Rectangle> = None;
    let mut dock_strip_h: i32 = 0;
    let mut last_screen_size = (0, 0);

    // Piano roll: scans ahead of the playhead on its own threads and hands the
    // render thread finished windows lock-free (see piano_roll.rs). Seeks, new
    // files and lookahead are handled inside `update`.
    let mut piano_roll_worker = piano_roll::PianoRoll::new();
    let mut pitch_bend_window_offset = 0;
    let mut pitch_bend_window_y_offset = 0;
    let mut pitch_bend_spring_position = Vector2::zero();
    let mut pitch_bend_spring_velocity = Vector2::zero();

    let mut flip_3d = Flip3D::new();

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

            readouts_texture = rl
                .load_render_texture(
                    &thread,
                    rl.get_screen_width() as u32,
                    rl.get_screen_height() as u32,
                )
                .expect("Failed to recreate readouts texture");
            readouts_texture.set_texture_filter(&thread, TextureFilter::TEXTURE_FILTER_BILINEAR);
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

        if let Some((audio_engine, rx)) = &pending_audio_engine {
            match rx.try_recv() {
                Ok(Ok(stream)) => {
                    engine.set_output(Some(stream));
                    if app_state.ui.audio_banner.is_some() {
                        app_state.ui.audio_banner =
                            Some((format!("{} ready", audio_engine.label()), false));
                        app_state.ui.audio_banner_timer = 0.0;
                    }
                    pending_audio_engine = None;
                }
                Ok(Err(e)) => {
                    eprintln!(
                        "[WARN] {}: {e}. Audio output unchanged.",
                        audio_engine.label()
                    );
                    app_state.ui.audio_banner = Some((format!("{}: {e}", audio_engine.label()), true));
                    app_state.ui.audio_banner_timer = 0.0;
                    pending_audio_engine = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Keep a loading banner up until the result arrives.
                    if matches!(app_state.ui.audio_banner, Some((_, false))) {
                        app_state.ui.audio_banner_timer = 0.0;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => pending_audio_engine = None,
            }
        }

        let st = engine.get_state();
        app_state.update_player_state(st);

        let playback_position_ns = engine.position_ns();
        let pitch_bend_normalized = engine.get_pitch_bend_normalized();
        let pitch_bend_range = engine.get_pitch_bend_range();
        if !rl.is_window_maximized() && !rl.is_window_minimized() && !rl.is_window_fullscreen() {
            let target_x = if app_state.settings.pitch_bend_move_window_enabled {
                pitch_bend_normalized.clamp(-1.0, 1.0) * (pitch_bend_range.max(0.0) / 12.0) * 40.0
            } else {
                0.0
            };
            let target = Vector2::new(target_x, (target_x / 10.0).sin() * 20.0);
            let [mass, stiffness, damping] = app_state.settings.pitch_bend_spring_parameters;
            // Small semi-implicit integration steps keep stiff springs stable.
            let dt = rl.get_frame_time().clamp(0.0, 0.1);
            let steps = (dt / 0.001).ceil().max(1.0) as u32;
            let step = dt / steps as f32;
            for _ in 0..steps {
                let acceleration = ((target - pitch_bend_spring_position) * stiffness as f32
                    - pitch_bend_spring_velocity * damping as f32)
                    * (1.0 / mass.max(1) as f32);
                pitch_bend_spring_velocity += acceleration * step;
                pitch_bend_spring_position += pitch_bend_spring_velocity * step;
            }
            let offset = pitch_bend_spring_position.x.round() as i32;
            let y_offset = pitch_bend_spring_position.y.round() as i32;
            if offset != pitch_bend_window_offset || y_offset != pitch_bend_window_y_offset {
                // Apply only the offset delta so manual window moves are preserved.
                let position = rl.get_window_position();
                rl.set_window_position(
                    position.x.round() as i32 + offset - pitch_bend_window_offset,
                    position.y.round() as i32 + y_offset - pitch_bend_window_y_offset,
                );
                pitch_bend_window_offset = offset;
                pitch_bend_window_y_offset = y_offset;
            }
        }
        let mut duration_ns = engine.duration_ns();

        let is_loading = st == PlayerState::Loading || app_state.playback.is_loading;
        let is_midi_loaded = duration_ns > 1 && !is_loading;

        if is_loading {
            duration_ns = 0;
        }

        // Update layout when integration setting changes
        if app_state.settings.integrate_keyboard != last_integrate_keyboard {
            current_layout_width = -1; // Force layout update
            last_integrate_keyboard = app_state.settings.integrate_keyboard;
        }

        // Apply a fullscreen piano roll toggle: on, the piano roll becomes
        // a background layer covering the whole application window
        // (the other windows stay visible and usable above it); off, its
        // saved rectangle and z-order are restored.
        if app_state.settings.piano_roll_fullscreen != last_piano_roll_fullscreen {
            last_piano_roll_fullscreen = app_state.settings.piano_roll_fullscreen;
            current_layout_width = -1; // Force layout update
            if app_state.settings.piano_roll_fullscreen {
                if let Some(window) = window_manager.get_window(piano_roll_window_handle) {
                    saved_roll_layout = Some((window.rectangle, window.z_order));
                }
                window_manager.send_window_to_back(piano_roll_window_handle);
                window_manager.set_window_background(piano_roll_window_handle, true);
                window_manager.set_window_rectangle(
                    piano_roll_window_handle,
                    window_manager::Rectangle::new(
                        0,
                        0,
                        rl.get_screen_width(),
                        rl.get_screen_height(),
                    ),
                );
            } else {
                window_manager.set_window_background(piano_roll_window_handle, false);
                if let Some((rectangle, z_order)) = saved_roll_layout.take() {
                    window_manager.set_window_rectangle(piano_roll_window_handle, rectangle);
                    window_manager.set_window_z_order(piano_roll_window_handle, z_order);
                }
            }
        }

        // Dock the player controls to the top of the application window
        // while the piano roll is fullscreen and the dock setting is on.
        // Docked, the window becomes a frameless strip holding just its
        // content (no titlebar or border), the piano roll starts below
        // it instead of running underneath, and the window is marked
        // background so input cannot drag or resize it. Undocking
        // restores the rectangle it had before docking.
        let controls_docked = app_state.settings.piano_roll_fullscreen && app_state.settings.dock_controls_top;
        if controls_docked != last_controls_docked {
            last_controls_docked = controls_docked;
            if controls_docked {
                if let Some(window) = window_manager.get_window(controls_window_handle) {
                    saved_controls_rect = Some(window.rectangle);
                    dock_strip_h = window.rectangle.height - window.get_window_titlebar_height();
                }
                window_manager.set_window_background(controls_window_handle, true);
            } else {
                window_manager.set_window_background(controls_window_handle, false);
                if let Some(rectangle) = saved_controls_rect.take() {
                    window_manager.set_window_rectangle(controls_window_handle, rectangle);
                }
                if app_state.settings.piano_roll_fullscreen {
                    window_manager.set_window_rectangle(
                        piano_roll_window_handle,
                        window_manager::Rectangle::new(
                            0,
                            0,
                            rl.get_screen_width(),
                            rl.get_screen_height(),
                        ),
                    );
                }
            }
        }
        if controls_docked {
            let screen_w = rl.get_screen_width();
            let screen_h = rl.get_screen_height();
            window_manager.set_window_rectangle(
                controls_window_handle,
                window_manager::Rectangle::new(0, 0, screen_w, dock_strip_h),
            );
            window_manager.set_window_rectangle(
                piano_roll_window_handle,
                window_manager::Rectangle::new(0, dock_strip_h, screen_w, screen_h - dock_strip_h),
            );
        }

        // Limit dt to prevent physics explosion on lag (max 100ms)
        let dt = rl.get_frame_time().min(0.1);

        // Update toggle animations
        let anim_speed = 10.0;
        animate_toggle(
            app_state.playback.is_tick_based,
            &mut app_state.ui.tick_based_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.beat_bounce_enabled,
            &mut app_state.ui.beat_bounce_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.beat_bounce_offbeat,
            &mut app_state.ui.beat_bounce_offbeat_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.shake_enabled,
            &mut app_state.ui.shake_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.integrate_keyboard,
            &mut app_state.ui.keyboard_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.piano_roll_fullscreen,
            &mut app_state.ui.piano_roll_fullscreen_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.dock_controls_top,
            &mut app_state.ui.dock_controls_top_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.autoplay_on_load,
            &mut app_state.ui.autoplay_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.pitch_bend_move_window_enabled,
            &mut app_state.ui.pitch_bend_move_window_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.pitch_bend_window_enabled,
            &mut app_state.ui.pitch_bend_window_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.pitch_bend_override_enabled,
            &mut app_state.ui.pitch_bend_override_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.unlimited_output_enabled,
            &mut app_state.ui.unlimited_output_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.s16_clipping_enabled,
            &mut app_state.ui.s16_clipping_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.show_stats,
            &mut app_state.ui.show_stats_animation,
            dt,
            anim_speed,
        );
        animate_toggle(
            app_state.settings.vsync_enabled,
            &mut app_state.ui.vsync_animation,
            dt,
            anim_speed,
        );
        #[cfg(debug_assertions)]
        animate_toggle(
            app_state.settings.visqol_window_enabled,
            &mut app_state.ui.visqol_window_animation,
            dt,
            anim_speed,
        );

        app_state.playback.stats_clock += dt;
        {
            let now_notes = engine.notes_played();
            let clock = app_state.playback.stats_clock;
            app_state.playback.nps_samples.push_back((clock, now_notes));
            while app_state.playback.nps_samples.len() > 1
                && app_state
                    .playback.nps_samples
                    .get(1)
                    .map_or(false, |&(t, _)| clock - t > 1.0)
            {
                app_state.playback.nps_samples.pop_front();
            }
            if let (Some(&(t0, n0)), Some(&(t1, n1))) =
                (app_state.playback.nps_samples.front(), app_state.playback.nps_samples.back())
            {
                let span = t1 - t0;
                app_state.playback.nps_value = if span > 1e-3 {
                    (n1.saturating_sub(n0)) as f32 / span
                } else {
                    0.0
                };
            }
        }
        app_state.playback.tempo_us_value = engine
            .get_midi_data()
            .map(|data| {
                let parser_data = unsafe { &*data.as_ptr() };
                kazumidiparser_cursor::tempo_us_at_ns(parser_data, playback_position_ns)
            })
            .unwrap_or(500_000);

        // Shaking Window Logic
        app_state.ui.shake_state.update(
            app_state.settings.shake_enabled,
            app_state.settings.integrate_keyboard,
            piano_roll_window_handle,
            piano_keyboard_window_handle,
            &mut window_manager,
            rl.get_screen_width(),
            rl.get_screen_height(),
            &engine,
            is_midi_loaded,
            is_loading,
            st,
            app_state.playback.is_dragging_seek,
            dt,
        );

        // While the piano roll is fullscreen, follow application window
        // resizes so the roll layer always covers the whole window. The
        // window itself is a background layer, so nothing else can
        // move or resize it.
        let screen_size = (rl.get_screen_width(), rl.get_screen_height());
        if app_state.settings.piano_roll_fullscreen && screen_size != last_screen_size {
            // While the controls are docked, the roll starts below the
            // dock strip instead of covering the whole window.
            let (roll_y, roll_h) = if last_controls_docked {
                (dock_strip_h, screen_size.1 - dock_strip_h)
            } else {
                (0, screen_size.1)
            };
            window_manager.set_window_rectangle(
                piano_roll_window_handle,
                window_manager::Rectangle::new(0, roll_y, screen_size.0, roll_h),
            );
        }
        last_screen_size = screen_size;

        // Check if keyboard layout needs update
        let layout_update_needed = {
            if app_state.settings.integrate_keyboard {
                if let Some(pr_win) = window_manager.get_window(piano_roll_window_handle) {
                    let content_w = pr_win.rectangle.width as f32;
                    let titlebar_h = if app_state.settings.piano_roll_fullscreen {
                        0
                    } else {
                        pr_win.get_window_titlebar_height()
                    };
                    let content_h = (pr_win.rectangle.height - titlebar_h) as f32;
                    let w = content_w as i32;
                    let h = compute_integrated_keyboard_height(content_w, content_h) as i32;
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
            // Regenerate as the window resizes, not once the drag ends: the old texture
            // would otherwise stretch to the new rectangle and snap back to the right key
            // widths only on release. One keyboard-sized image is cheap enough per frame.
            if w > 0 && h > 0 {
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

        // Update overlay manager. The crash overlay wins over everything and is never
        // taken back down: once the player thread is gone the session is over, and any
        // other overlay would only offer controls that no longer do anything.
        let player_crashed =
            app_state.ui.overlay_manager.current_kind() == Some(app_state::AppOverlay::PlayerCrashed);
        if !engine.is_alive() && !player_crashed {
            app_state.enter_player_crashed();
        }

        let is_processing = app_state.playback.current_progress.is_some();
        if !player_crashed && engine.is_alive() {
            if is_loading || is_processing {
                app_state
                    .ui.overlay_manager
                    .open(app_state::AppOverlay::Loading);
            } else if app_state.ui.overlay_manager.current_kind()
                == Some(app_state::AppOverlay::Loading)
            {
                app_state.ui.overlay_manager.close();
            }
        }
        app_state.ui.overlay_manager.update(dt);
        app_state.update_parse_toast(dt);

        // Aero Flip 3D. Ctrl+Tab cycles forward, Ctrl+Shift+Tab backward.
        // Releasing Ctrl (or Enter) switches to the focused window; Esc cancels.
        let ctrl_down = rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);
        let shift_down = rl.is_key_down(KeyboardKey::KEY_LEFT_SHIFT)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);
        let tab_pressed = rl.is_key_pressed(KeyboardKey::KEY_TAB);
        let overlay_blocks = app_state.ui.overlay_manager.blocks_input()
            || app_state.ui.overlay_manager.get_fade_t() > 0.0;

        if flip_3d.is_interactive() {
            if overlay_blocks || rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
                flip_3d.cancel();
            } else {
                if tab_pressed {
                    flip_3d.cycle(if shift_down { -1 } else { 1 });
                }
                // Wheel up brings the card behind forward, like rolling the stack.
                let flip_wheel = rl.get_mouse_wheel_move();
                if flip_wheel != 0.0 {
                    flip_3d.cycle(if flip_wheel > 0.0 { 1 } else { -1 });
                }
                if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_LEFT) {
                    flip_3d.click(rl.get_mouse_position(), &mut window_manager);
                }
                if rl.is_key_pressed(KeyboardKey::KEY_ENTER)
                    || rl.is_key_pressed(KeyboardKey::KEY_KP_ENTER)
                    || !ctrl_down
                {
                    flip_3d.commit(&mut window_manager);
                }
            }
        } else if !overlay_blocks && ctrl_down && tab_pressed {
            flip_3d.open(&window_manager, if shift_down { -1 } else { 1 });
        }
        flip_3d.update(dt);

        app_state.ui.last_input_event = None;
        let active_window_id;

        // Block interaction if an overlay or Flip 3D is active
        if app_state.ui.overlay_manager.blocks_input() || flip_3d.is_active() {
            active_window_id = None;
            if !flip_3d.is_active() {
                if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_LEFT) {
                    app_state.ui.last_input_event =
                        Some(window_manager::InputEvent::MouseDown(mouse_point));
                } else if rl.is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT) {
                    app_state.ui.last_input_event =
                        Some(window_manager::InputEvent::MouseUp(mouse_point));
                } else if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                    app_state.ui.last_input_event =
                        Some(window_manager::InputEvent::MouseMove(mouse_point));
                }
            }
        } else {
            // Normal interaction with window_manager
            if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_LEFT) {
                active_window_id = window_manager.mouse_down(mouse_point, screen_rect);
                if active_window_id.is_some() {
                    app_state.ui.last_input_event =
                        Some(window_manager::InputEvent::MouseDown(mouse_point));
                }
            } else if rl.is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT) {
                active_window_id = window_manager.mouse_up(mouse_point);
                app_state.ui.last_input_event = Some(window_manager::InputEvent::MouseUp(mouse_point));
            } else {
                active_window_id = window_manager.mouse_move(mouse_point);
                if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                    app_state.ui.last_input_event =
                        Some(window_manager::InputEvent::MouseMove(mouse_point));
                }
            }
        }

        let wheel = rl.get_mouse_wheel_move();

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
                // The note the roll shows at the playhead on each key (the
                // newest one sounding), so key and roll always agree.
                let colors = piano_roll_worker.sounding_colors(current_tick);
                for (note, chosen) in colors.iter().enumerate() {
                    if let Some(color) = *chosen {
                        let (r, g, b) = utils::get_note_color_by_index(color);
                        let o = note * 4;
                        hit_pixels[o] = r;
                        hit_pixels[o + 1] = g;
                        hit_pixels[o + 2] = b;
                        hit_pixels[o + 3] = 255;
                    }
                }
            }
            let _ = keyboard_hit_texture.update_texture(&hit_pixels);
        }

        // Mouse-wheel anywhere (when no overlay is open) adjusts note scroll speed.
        // Multiplicative steps give smooth, symmetric zoom in/out. The floor is only
        // there to keep the arithmetic alive (see MIN_NOTE_SPEED), not to bound how
        // far out you can zoom; the ceiling is the real zoom-in limit.
        if wheel != 0.0 && !app_state.ui.overlay_manager.blocks_input() && !flip_3d.is_active() {
            app_state.playback.note_speed = (app_state.playback.note_speed * 1.1_f32.powf(wheel))
                .clamp(piano_roll::MIN_NOTE_SPEED, piano_roll::MAX_NOTE_SPEED);
        }

        // Debug builds: track whether the ViSQOL window owns keyboard focus.
        #[cfg(debug_assertions)]
        let visqol_is_active = !app_state.ui.overlay_manager.blocks_input()
            && !flip_3d.is_active()
            && window_manager
                .get_window(visqol_window_handle)
                .is_some_and(|w| w.visible && w.is_active);
        #[cfg(debug_assertions)]
        if !visqol_is_active {
            app_state.ui.visqol_bitrate_field.editing = false;
            app_state.ui.visqol_bitrate_field.select_all = false;
            app_state.ui.visqol_slider_dragging = false;
        }
        #[cfg(debug_assertions)]
        let visqol_blocks_keys = visqol_is_active && app_state.ui.visqol_bitrate_field.editing;
        #[cfg(not(debug_assertions))]
        let visqol_blocks_keys = false;

        // Handle keyboard shortcuts only when overlay is completely closed
        if !app_state.ui.overlay_manager.blocks_input()
            && app_state.ui.overlay_manager.get_fade_t() == 0.0
            && !flip_3d.is_active()
            && !visqol_blocks_keys
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

        if (rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL))
            && rl.is_key_pressed(KeyboardKey::KEY_O)
            && !flip_3d.is_active()
        {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("MIDI Files", &["mid", "midi", "kar"])
                .pick_file()
            {
                let path_str = path.to_string_lossy().to_string();
                app_state.handle_file_drop(&path_str, &tx_commands_to_player);
            }
        }

        if rl.is_key_pressed(KeyboardKey::KEY_S) && !flip_3d.is_active() && !visqol_blocks_keys {
            if app_state.ui.overlay_manager.current_kind() == Some(app_state::AppOverlay::Settings) {
                app_state.ui.overlay_manager.close();
            } else if !app_state.ui.overlay_manager.blocks_input() {
                let text_editor_focused = app_state.ui.overlay_manager.current_kind()
                    == Some(app_state::AppOverlay::TextEditor)
                    && app_state.ui.text_editor.focused;
                if !text_editor_focused {
                    app_state.ui.settings_scroll_y = 0.0;
                    app_state.ui.settings_scrollbar_dragging = false;
                    app_state.ui.s16_warning_opened_at = None;
                    {
                        app_state.ui.settings_opus_bitrate_slider.editing = false;
                        app_state.ui.settings_opus_bitrate_slider.dragging = false;
                        app_state.ui.settings_opus_bitrate_slider.select_all = false;
                    }
                    app_state
                        .ui.overlay_manager
                        .open(app_state::AppOverlay::Settings);
                    window_manager.mouse_up(mouse_point);
                }
            }
        }

        if rl.is_key_pressed(KeyboardKey::KEY_P) && !flip_3d.is_active() && !visqol_blocks_keys {
            if app_state.ui.overlay_manager.current_kind() == Some(app_state::AppOverlay::TextEditor) {
                if !app_state.ui.text_editor.focused {
                    app_state.ui.overlay_manager.close();
                }
            } else if !app_state.ui.overlay_manager.blocks_input() {
                app_state
                    .ui.overlay_manager
                    .open(app_state::AppOverlay::TextEditor);
                app_state.ui.text_editor.reset_interaction();
                window_manager.mouse_up(mouse_point);
            }
        }

        if app_state.ui.overlay_manager.current_kind() == Some(app_state::AppOverlay::TextEditor) {
            // First Escape drops the editor's focus, the next one closes the overlay.
            let focus_dropped = app_state.ui.text_editor.update(&mut rl, dt);
            if rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) && !focus_dropped {
                app_state.ui.overlay_manager.close();
            }
        }

        // Drawing
        let screen_width = rl.get_screen_width();
        let screen_height = rl.get_screen_height();

        // Cursor handling: only use window_manager cursors when overlay is NOT shown or fading
        if app_state.ui.overlay_manager.blocks_input() || flip_3d.is_active() {
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

        // Capture ViSQOL bitrate text-entry keys before entering drawing modes.
        #[cfg(debug_assertions)]
        let (
            visqol_typed,
            visqol_backspace,
            visqol_delete,
            visqol_left,
            visqol_right,
            visqol_home,
            visqol_end,
            visqol_enter,
            visqol_escape,
            visqol_shift,
            visqol_control,
        ) = if visqol_is_active {
            let mut typed = Vec::new();
            while let Some(ch) = rl.get_char_pressed() {
                typed.push(ch);
            }
            let pressed = |rl: &RaylibHandle, key| {
                rl.is_key_pressed(key) || rl.is_key_pressed_repeat(key)
            };
            (
                typed,
                pressed(&rl, KeyboardKey::KEY_BACKSPACE),
                pressed(&rl, KeyboardKey::KEY_DELETE),
                pressed(&rl, KeyboardKey::KEY_LEFT),
                pressed(&rl, KeyboardKey::KEY_RIGHT),
                rl.is_key_pressed(KeyboardKey::KEY_HOME),
                rl.is_key_pressed(KeyboardKey::KEY_END),
                rl.is_key_pressed(KeyboardKey::KEY_ENTER)
                    || rl.is_key_pressed(KeyboardKey::KEY_KP_ENTER),
                rl.is_key_pressed(KeyboardKey::KEY_ESCAPE),
                rl.is_key_down(KeyboardKey::KEY_LEFT_SHIFT)
                    || rl.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT),
                rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
                    || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL),
            )
        } else {
            (
                Vec::new(),
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
            )
        };

        let is_mouse_down = rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT);
        let window_ids = window_manager.get_window_ids_sorted();

        // A fullscreen piano roll is frameless: no border and no titlebar,
        // so its content fills the whole application window while the
        // other windows float above it. Docked player controls are
        // frameless too, so the dock strip is content only.
        let roll_frameless = app_state.settings.piano_roll_fullscreen;
        let controls_frameless = controls_docked;
        let eff_bt = |wid: i32| -> i32 {
            if (roll_frameless && wid == piano_roll_window_handle)
                || (controls_frameless && wid == controls_window_handle)
            {
                0
            } else {
                window_manager::BORDER_THICKNESS
            }
        };

        // The composite render texture is exactly the screen; it is (re)created only on
        // window resize, above. It used to grow to enclose off-screen windows and bend
        // overflow, reallocating every frame as a window was dragged away, all to hold
        // pixels the physical screen clips anyway. So windows composite straight into
        // screen space and off-screen content clips as an OS window would.

        // Phase 0: Ensure per-window render textures exist and are sized correctly.
        for &wid in &window_ids {
            if let Some(window) = window_manager.get_window(wid) {
                if !window.visible {
                    content_rts.remove(&wid);
                    content_rt_padding.remove(&wid);
                    continue;
                }
                let bt = eff_bt(wid);
                let base_w = window.rectangle.width + 2 * bt;
                let base_h = window.rectangle.height + 2 * bt;

                // Piano Keyboard window (and Piano Roll when keyboard is integrated)
                // reserve extra vertical padding for bending.
                let needs_bend = wid == piano_keyboard_window_handle
                    || (wid == piano_roll_window_handle && app_state.settings.integrate_keyboard);
                let needs_bend = needs_bend && app_state.settings.pitch_bend_window_enabled;
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
                            rt.texture.width as u32 != rt_w_u || rt.texture.height as u32 != rt_h_u
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
        // Clear the backbuffer before the final composite blit covers it.
        d.clear_background(Color::BLACK);

        // Phase 1: Render each window (frame + titlebar + content) into its private RT.
        for &wid in &window_ids {
            let window = match window_manager.get_window(wid) {
                Some(w) if w.visible => w,
                _ => continue,
            };
            let bt = eff_bt(wid);
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

                let titlebar_h = if (roll_frameless && wid == piano_roll_window_handle)
                    || (controls_frameless && wid == controls_window_handle)
                {
                    0
                } else {
                    window.get_window_titlebar_height()
                };
                let content_w = window.rectangle.width as f32;
                let content_h = (window.rectangle.height - titlebar_h) as f32;

                let wm_border_color = window_manager.get_window_border_color();
                // A docked controls strip is brightened against the
                // window background so it does not blend into the piano
                // roll below it.
                let wm_bg_color = if controls_frameless && wid == controls_window_handle {
                    window_manager
                        .get_window_background_color()
                        .adjust_brightness(1.35)
                } else {
                    window_manager.get_window_background_color()
                };
                let border_color = Color::new(
                    wm_border_color.r,
                    wm_border_color.g,
                    wm_border_color.b,
                    wm_border_color.a,
                );
                let bg_color =
                    Color::new(wm_bg_color.r, wm_bg_color.g, wm_bg_color.b, wm_bg_color.a);

                let win_org = bt;
                let fw = content_w as i32 + 2 * bt;
                let fh = content_h as i32 + titlebar_h + 2 * bt;

                // Border strips (shifted down by padding to leave room for bending)
                if bt > 0 {
                d_rt.draw_rectangle(0, padding, fw, bt, border_color);
                d_rt.draw_rectangle(0, fh - bt + padding, fw, bt, border_color);
                d_rt.draw_rectangle(
                    0,
                    bt + padding,
                    bt,
                    titlebar_h + content_h as i32,
                    border_color,
                );
                d_rt.draw_rectangle(
                    fw - bt,
                    bt + padding,
                    bt,
                    titlebar_h + content_h as i32,
                    border_color,
                );
                }

                // Content background
                d_rt.draw_rectangle(
                    win_org,
                    win_org + titlebar_h + padding,
                    content_w as i32,
                    content_h as i32,
                    bg_color,
                );

                // Divider along the bottom of a docked controls strip.
                if controls_frameless && wid == controls_window_handle {
                    d_rt.draw_rectangle(
                        win_org,
                        win_org + titlebar_h + padding + content_h as i32 - 2,
                        content_w as i32,
                        2,
                        border_color,
                    );
                }

                // Title bar
                if titlebar_h > 0 {
                let wm_tb_active = window_manager.get_window_titlebar_active_color();
                let wm_tb_inactive = window_manager.get_window_titlebar_inactive_color();
                let wm_tb = if window.is_active {
                    wm_tb_active
                } else {
                    wm_tb_inactive
                };
                let tb_color = Color::new(wm_tb.r, wm_tb.g, wm_tb.b, 255);
                d_rt.draw_rectangle(
                    win_org,
                    win_org + padding,
                    content_w as i32,
                    titlebar_h,
                    tb_color,
                );
                d_rt.draw_text(
                    &window.title,
                    win_org + 3,
                    win_org + 5 + padding,
                    titlebar_h - 10,
                    Color::WHITE,
                );
                }

                // Content origin in RT-local coordinates
                let cx = win_org as f32;
                let cy = (win_org + titlebar_h + padding) as f32;
                let cw = content_w;
                let ch = content_h;

                // Apply a single content-area scissor for this window.
                // Individual content renderers no longer need to manage clipping themselves.
                {
                    let mut d_scissor =
                        d_rt.begin_scissor_mode(cx as i32, cy as i32, cw as i32, ch as i32);

                    match wid {
                        id if id == piano_roll_window_handle => {
                            // Reserve the bottom strip for the integrated keyboard.
                            let keyboard_h = if app_state.settings.integrate_keyboard {
                                compute_integrated_keyboard_height(cw, ch)
                            } else {
                                0.0
                            };
                            let roll_h = (ch - keyboard_h).max(0.0);

                            if roll_h > 0.0 {
                                if let Some(data) = engine.get_midi_data() {
                                    // Safety: as_ptr() yields a valid pointer to the C MIDI
                                    // struct, kept alive by the Arc for this call.
                                    let parser_data = unsafe { &*data.as_ptr() };

                                    let current_tick = kazumidiparser_cursor::ns_to_tick(
                                        parser_data,
                                        playback_position_ns,
                                    );
                                    // Topmost visible tick, and the span it covers: one
                                    // screenful. The scan is allowed a little beyond that,
                                    // which is what bounds the resident window.
                                    let top_tick = piano_roll::future_tick(
                                        parser_data,
                                        playback_position_ns,
                                        app_state.playback.is_tick_based,
                                        app_state.playback.note_speed,
                                        roll_h,
                                    )
                                    .min(parser_data.max_tick);
                                    let span = top_tick.saturating_sub(current_tick).max(1);
                                    // Generate a little past the top edge, not a lot: the slack
                                    // only has to cover what playback eats between frames plus
                                    // the scan's latency. Every extra screenful of lookahead is
                                    // another screenful of notes resident, which on a dense MIDI
                                    // is measured in hundreds of megabytes. The slack is a
                                    // quarter of the span with an absolute floor: purely
                                    // span-relative, it shrinks with the span at high zoom
                                    // until the scan can no longer stay ahead of the top edge
                                    // and the roll is cut off there.
                                    let margin_floor = kazumidiparser_cursor::ns_to_tick(
                                        parser_data,
                                        playback_position_ns
                                            .saturating_add(piano_roll::MARGIN_FLOOR_NS),
                                    )
                                    .saturating_sub(current_tick);
                                    let target = top_tick
                                        .saturating_add((span / 4).max(margin_floor))
                                        .min(parser_data.max_tick);

                                    // New file, seek, lookahead and window handoff are
                                    // all handled here; nothing in it blocks.
                                    piano_roll_worker.update(
                                        &data,
                                        parser_data,
                                        current_tick,
                                        top_tick,
                                        target,
                                    );

                                    let roll_area = Rectangle::new(cx, cy, cw, roll_h);
                                    let _draw_stats = piano_roll_worker.draw(
                                        &mut d_scissor,
                                        parser_data,
                                        playback_position_ns,
                                        roll_area,
                                        app_state.playback.is_tick_based,
                                        app_state.playback.note_speed
                                            * beat_bounce_multiplier(
                                                parser_data,
                                                playback_position_ns,
                                                st == PlayerState::Playing,
                                                app_state.settings.beat_bounce_amount,
                                                app_state.settings.beat_bounce_note_value,
                                                app_state.settings.beat_bounce_offbeat,
                                                app_state.ui.beat_bounce_animation,
                                            ),
                                    );
                                }
                            }

                            // Draw integrated keyboard if enabled
                            if app_state.settings.integrate_keyboard {
                                let keyboard_rect =
                                    Rectangle::new(cx, cy + ch - keyboard_h, cw, keyboard_h);
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

                            let is_input_blocked = app_state.ui.overlay_manager.blocks_input();
                            let current_event = if active_window_id == Some(controls_window_handle)
                                && is_inside_content
                                && !is_input_blocked
                            {
                                app_state.ui.last_input_event.map(|e| match e {
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
                                        window_manager::InputEvent::MouseUp(window_manager::Point {
                                            x: p.x - (window.rectangle.left() - bt),
                                            y: p.y - (window.rectangle.top() - bt),
                                        })
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

                            let weighted_pos = app_state.get_weighted_time(playback_position_ns);
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
                            // The roll no longer pre-generates the whole MIDI (it streams a
                            // window around the playhead), so there is no whole-file
                            // generation progress to show behind the seek bar.
                            let gen_frac: Option<f32> = None;
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
                                // Buffered audio is wall-clock time; scale to song time.
                                engine.audio_buffer_ns().map(|buffered| {
                                    playback_position_ns.saturating_add(
                                        (buffered as f64 * app_state.playback.playback_speed as f64) as u64,
                                    )
                                }),
                            ) {
                                let clamped_mouse_x = local_mouse.x.clamp(seek_x, seek_x + seek_w);
                                let t = (clamped_mouse_x - seek_x) / seek_w;
                                let seek_ns = (t as f64 * duration_ns as f64) as u64;
                                app_state.start_seeking(seek_ns, &engine);
                            }

                            let prev_speed = app_state.playback.playback_speed;
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
                                is_mouse_down,
                                &mut app_state.playback.playback_speed,
                                &mut app_state.ui.speed_slider_dragging,
                                cx + cw - 10.0,
                            );
                            if app_state.playback.playback_speed != prev_speed {
                                engine.set_speed(app_state.playback.playback_speed);
                            }

                            // Drag handling does not draw, so it can stay inside the scissor block.
                            if app_state.playback.is_dragging_seek && !app_state.ui.overlay_manager.is_active()
                            {
                                if d_scissor.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
                                    let seek_x = cx + 10.0;
                                    let seek_w = cw - 20.0;
                                    let clamped_mouse_x =
                                        local_mouse.x.clamp(seek_x, seek_x + seek_w);
                                    let t = (clamped_mouse_x - seek_x) / seek_w;
                                    let seek_ns = (t as f64 * duration_ns as f64) as u64;
                                    app_state.update_seek_preview(seek_ns, &engine);
                                }

                                if d_scissor
                                    .is_mouse_button_released(MouseButton::MOUSE_BUTTON_LEFT)
                                {
                                    app_state.release_seek();
                                }
                            }
                        }
                        id if id == piano_keyboard_window_handle
                            && !app_state.settings.integrate_keyboard =>
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
                        #[cfg(debug_assertions)]
                        id if id == visqol_window_handle => {
                            let local_mouse = Vector2::new(
                                mouse_pos.x - (window.rectangle.left() - bt) as f32,
                                mouse_pos.y - (window.rectangle.top() - bt) as f32,
                            );

                            let is_inside_content = local_mouse.x >= cx
                                && local_mouse.x < cx + cw
                                && local_mouse.y >= cy
                                && local_mouse.y < cy + ch;

                            let is_input_blocked =
                                app_state.ui.overlay_manager.blocks_input() || flip_3d.is_active();
                            let allow_input = active_window_id == Some(visqol_window_handle)
                                && is_inside_content
                                && !is_input_blocked;

                            if !is_mouse_down || is_input_blocked || !window.is_active {
                                app_state.ui.visqol_slider_dragging = false;
                            }

                            // Keep forwarding local mouse coordinates while dragging the vertical
                            // slider so dragging past the window edge clamps cleanly to 6..=510
                            // instead of freezing mid-drag.
                            let forward_mouse = allow_input || app_state.ui.visqol_slider_dragging;
                            let current_event = if forward_mouse {
                                app_state.ui.last_input_event.map(|e| match e {
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
                                        window_manager::InputEvent::MouseUp(window_manager::Point {
                                            x: p.x - (window.rectangle.left() - bt),
                                            y: p.y - (window.rectangle.top() - bt),
                                        })
                                    }
                                })
                            } else {
                                None
                            };

                            let ui_mouse_pos = if forward_mouse {
                                Some(local_mouse)
                            } else {
                                None
                            };

                            let click_pos = if allow_input {
                                match current_event {
                                    Some(window_manager::InputEvent::MouseDown(p)) => {
                                        Some(Vector2::new(p.x as f32, p.y as f32))
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };

                            let field_input = ui_components::numeric_slider::NumericSliderInput {
                                click_pos,
                                mouse_pos: local_mouse,
                                mouse_down: is_mouse_down && !is_input_blocked,
                                wheel: 0.0,
                                shift: visqol_shift,
                                control: visqol_control,
                                typed: visqol_typed.clone(),
                                backspace: visqol_backspace,
                                delete: visqol_delete,
                                left: visqol_left,
                                right: visqol_right,
                                home: visqol_home,
                                end: visqol_end,
                                enter: visqol_enter,
                                escape: visqol_escape,
                            };

                            let is_pcm = matches!(engine.buffer_memory(), Some((_, _, false)));

                            let prev_opus_bitrate_kbps = app_state.settings.opus_bitrate_kbps;
                            let series = engine.visqol_series();
                            ui::draw_visqol_window(
                                &mut d_scissor,
                                cx,
                                cy,
                                cw,
                                ch,
                                &series,
                                is_pcm,
                                &mut app_state.settings.opus_bitrate_kbps,
                                current_event,
                                ui_mouse_pos,
                                is_mouse_down && !is_input_blocked,
                                &mut app_state.ui.visqol_slider_dragging,
                                &mut app_state.ui.visqol_bitrate_field,
                                &field_input,
                            );
                            if app_state.settings.opus_bitrate_kbps != prev_opus_bitrate_kbps {
                                engine.set_opus_bitrate(app_state.settings.opus_bitrate_kbps);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Phase 2: Composite per-window RTs onto the global render_texture.
        {
            let mut d_rt = d.begin_texture_mode(&thread, &mut render_texture);

            if flip_3d.is_active() {
                // Same backdrop as the desktop, so the cards land on it seamlessly.
                // The cards themselves are drawn straight to the backbuffer below.
                d_rt.clear_background(Color::new(31, 31, 31, 255));
            } else {
                d_rt.clear_background(Color::new(31, 31, 31, 255));

                for &wid in &window_ids {
                    let window = match window_manager.get_window(wid) {
                        Some(w) if w.visible => w,
                        _ => continue,
                    };
                    let win_rt = match content_rts.get(&wid) {
                        Some(rt) => rt,
                        None => continue,
                    };

                    let bt = eff_bt(wid);
                    let rt_w = win_rt.texture.width as f32;
                    let rt_h = win_rt.texture.height as f32;
                    let screen_x = (window.rectangle.left() - bt) as f32;
                    let screen_y = (window.rectangle.top() - bt) as f32;

                    let needs_bend = wid == piano_keyboard_window_handle
                        || (wid == piano_roll_window_handle && app_state.settings.integrate_keyboard);
                    let needs_bend = needs_bend && app_state.settings.pitch_bend_window_enabled;
                    if needs_bend {
                        let win_w = (window.rectangle.width + 2 * bt) as f32;
                        let win_h = (window.rectangle.height + 2 * bt) as f32;
                        let padding = *content_rt_padding.get(&wid).unwrap_or(&0.0);
                        let window_top_uv = padding / rt_h;
                        let window_bottom_uv = 1.0 - padding / rt_h;

                        window_bend_shader
                            .set_shader_value(wb_bend_loc, pitch_bend_normalized.clamp(-1.0, 1.0));
                        window_bend_shader
                            .set_shader_value(wb_bend_range_loc, pitch_bend_range.max(0.0));
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
        }

        // FPS / statistics readouts: drawn on their own layer and
        // composited into the scene texture above every window, so no
        // window can cover them, but the overlay backdrop blur (Phase
        // 4 below) still applies to them like the rest of the scene.
        if !flip_3d.is_active() {
            let fps = 1.0 / d.get_frame_time();
            {
                let mut d_rt = d.begin_texture_mode(&thread, &mut readouts_texture);
                d_rt.clear_background(Color::new(0, 0, 0, 0));
                // While the controls are docked, the top strip is theirs:
                // the readouts move below it so they stay readable.
                let top_offset = if controls_docked { dock_strip_h } else { 0 };
                d_rt.draw_text(
                    format!("FPS: {:.2}", fps).as_str(),
                    10,
                    10 + top_offset,
                    20,
                    Color::WHITE,
                );

                // Statistics overlay, pinned to the top-right corner (toggled in Settings).
                ui::draw_stats_overlay(&mut d_rt, &app_state, &engine, screen_width, top_offset);
            }
            {
                let mut d_rt = d.begin_texture_mode(&thread, &mut render_texture);
                d_rt.draw_texture_rec(
                    &readouts_texture,
                    Rectangle::new(
                        0.0,
                        0.0,
                        readouts_texture.texture.width as f32,
                        -readouts_texture.texture.height as f32,
                    ),
                    Vector2::new(0.0, 0.0),
                    Color::WHITE,
                );
            }
        }
        // Phase 4: Blur request for overlays
        if app_state.ui.overlay_manager.blocks_input() {
            let eased_t = 1.0 - (1.0 - app_state.ui.overlay_manager.get_fade_t()).powi(2);
            let blur_strength = (eased_t * 8.0) as u16;
            if blur_strength > 0 {
                app_state.ui.blur_requester.apply_blur(
                    Rectangle::new(0.0, 0.0, screen_width as f32, screen_height as f32),
                    Some(blur_strength),
                );
            }
        }

        let blur_requests = app_state.ui.blur_requester.consume_all();
        if !blur_requests.is_empty() {
            let screen_w = render_texture.texture.width as f32;
            let screen_h = render_texture.texture.height as f32;

            for request in blur_requests {
                let uv_x = request.blur_region.x / screen_w;
                let uv_y =
                    (screen_h - request.blur_region.y - request.blur_region.height) / screen_h;
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
            Vector2::new(0.0, 0.0),
            Color::WHITE,
        );

        // Flip 3D draws to the backbuffer rather than render_texture because only
        // the backbuffer is multisampled, and the tilted card edges need MSAA.
        flip_3d.draw(
            &mut d,
            &window_manager,
            &content_rts,
            &content_rt_padding,
            screen_width,
            screen_height,
        );

        // Snapshot the settings before the overlay can edit them; any
        // changes are forwarded to the player after the draw below.
        let prev_settings = (app_state.ui.overlay_manager.current_kind()
            == Some(app_state::AppOverlay::Settings))
        .then(|| settings_sync::PrevSettings::snapshot(&app_state));
        let mut close_requested = false;
        app_state
            .ui.overlay_manager
            .draw(
                &mut d,
                screen_width,
                screen_height,
                |d, fade_t, kind| match kind {
                    app_state::AppOverlay::Settings => {
                        if ui::draw_settings_overlay(
                            d,
                            screen_width,
                            screen_height,
                            fade_t,
                            app_state.ui.last_input_event,
                            mouse_pos,
                            is_mouse_down,
                            &mut app_state.playback.is_tick_based,
                            app_state.ui.tick_based_animation,
                            &mut app_state.settings.beat_bounce_enabled,
                            app_state.ui.beat_bounce_animation,
                            &mut app_state.settings.beat_bounce_amount,
                            &mut app_state.ui.settings_beat_bounce_amount_slider,
                            &mut app_state.settings.beat_bounce_note_value,
                            &mut app_state.ui.settings_beat_bounce_note_value_slider,
                            &mut app_state.settings.beat_bounce_offbeat,
                            app_state.ui.beat_bounce_offbeat_animation,
                            &mut app_state.settings.shake_enabled,
                            app_state.ui.shake_animation,
                            &mut app_state.settings.integrate_keyboard,
                            app_state.ui.keyboard_animation,
                            &mut app_state.settings.piano_roll_fullscreen,
                            app_state.ui.piano_roll_fullscreen_animation,
                            &mut app_state.settings.dock_controls_top,
                            app_state.ui.dock_controls_top_animation,
                            &mut app_state.ui.settings_scroll_y,
                            wheel,
                            &mut app_state.settings.autoplay_on_load,
                            app_state.ui.autoplay_animation,
                            &mut app_state.settings.pitch_bend_window_enabled,
                            app_state.ui.pitch_bend_window_animation,
                            &mut app_state.settings.pitch_bend_move_window_enabled,
                            app_state.ui.pitch_bend_move_window_animation,
                            &mut app_state.settings.pitch_bend_spring_parameters,
                            &mut app_state.ui.pitch_bend_spring_sliders,
                            &mut app_state.settings.pitch_bend_override_enabled,
                            app_state.ui.pitch_bend_override_animation,
                            &mut app_state.settings.pitch_bend_multiplier,
                            &mut app_state.settings.unlimited_output_enabled,
                            app_state.ui.unlimited_output_animation,
                            &mut app_state.settings.s16_clipping_enabled,
                            app_state.ui.s16_clipping_animation,
                            &mut app_state.ui.s16_warning_opened_at,
                            &mut app_state.settings.midi_ev_buffer_size,
                            &mut app_state.ui.settings_scrollbar_dragging,
                            &mut app_state.ui.settings_scrollbar_drag_start_y,
                            &mut app_state.ui.settings_scrollbar_drag_start_scroll,
                            &mut app_state.ui.settings_pb_multiplier_slider,
                            &mut app_state.ui.settings_midi_ev_buffer_size_slider,
                            &mut app_state.settings.audio_engine,
                            &mut app_state.ui.settings_audio_engine_combo,
                            &mut app_state.settings.audio_buffer_secs,
                            &mut app_state.ui.settings_audio_buffer_slider,
                            engine.buffer_memory(),
                            &mut app_state.settings.opus_bitrate_kbps,
                            &mut app_state.ui.settings_opus_bitrate_slider,
                            #[cfg(debug_assertions)]
                            &mut app_state.settings.visqol_window_enabled,
                            #[cfg(debug_assertions)]
                            app_state.ui.visqol_window_animation,
                            &mut app_state.settings.midi_ev_mode,
                            &mut app_state.ui.settings_midi_ev_mode_combo,
                            &mut app_state.settings.parse_threads,
                            &mut app_state.ui.settings_parse_threads_slider,
                            &mut app_state.settings.show_stats,
                            app_state.ui.show_stats_animation,
                            &mut app_state.settings.vsync_enabled,
                            app_state.ui.vsync_animation,
                            &mut app_state.settings.engine_settings,
                            &mut app_state.ui.engine_setting_sliders,
                        ) {
                            close_requested = true;
                        }

                        window_manager.set_window_visible(
                            piano_keyboard_window_handle,
                            !app_state.settings.integrate_keyboard,
                        );
                        #[cfg(debug_assertions)]
                        window_manager.set_window_visible(
                            visqol_window_handle,
                            app_state.settings.visqol_window_enabled,
                        );
                    }
                    app_state::AppOverlay::PlayerCrashed => {
                        ui::draw_player_crashed_overlay(d, screen_width, screen_height, fade_t);
                    }
                    app_state::AppOverlay::Loading => {
                        // Once the parse is done the toast owns the panel and carries it to
                        // the corner; only the dimming backdrop is left to fade out here.
                        if app_state.playback.parse_toast.is_none() {
                            ui::draw_loading_overlay(
                                d,
                                screen_width,
                                screen_height,
                                fade_t,
                                &app_state.playback.current_progress,
                                app_state.playback.parse_bps,
                                app_state.playback.parse_eps,
                            );
                        }
                    }
                    app_state::AppOverlay::TextEditor => {
                        let (close, apply) = ui::draw_text_editor_overlay(
                            d,
                            screen_width,
                            screen_height,
                            fade_t,
                            app_state.ui.last_input_event,
                            mouse_pos,
                            is_mouse_down,
                            wheel,
                            &mut app_state.ui.text_editor,
                        );
                        if close {
                            close_requested = true;
                        }
                        if apply {
                            println!(
                                "[TextEditor] Apply clicked. Content:\n{}",
                                app_state.ui.text_editor.text()
                            );
                        }
                    }
                },
            );
        if let Some(prev_settings) = &prev_settings {
            settings_sync::apply_settings_changes(
                &mut app_state,
                prev_settings,
                &engine,
                &mut pending_audio_engine,
            );
        }

        // Drawn outside the overlay manager: it outlives the overlay's fade-out and must
        // not be dimmed by the backdrop.
        if let Some(toast) = &app_state.playback.parse_toast {
            ui::draw_parse_toast(
                &mut d,
                screen_width,
                screen_height,
                toast.fly_t(),
                toast.alpha(),
                &toast.progress,
                toast.parse_bps,
                toast.parse_eps,
                toast.parse_duration,
            );
        }

        if let Some((message, is_error)) = &app_state.ui.audio_banner {
            app_state.ui.audio_banner_timer += dt;
            ui::draw_audio_banner(
                &mut d,
                screen_width,
                screen_height,
                app_state.ui.audio_banner_timer,
                message,
                *is_error,
            );
        }

        // Topmost layer.
        ui::draw_pending_tooltip(&mut d, screen_width, screen_height);

        if close_requested {
            app_state.ui.overlay_manager.close();
        }
    }

    // Stop and join the piano-roll worker before teardown, so it is never
    // force-killed mid-allocation (which can deadlock process exit on Windows).
    piano_roll_worker.shutdown();
}
