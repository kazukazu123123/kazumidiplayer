use crate::app_state::AppState;
use crate::player::PlayerHandle;
use crate::utils::{format_bytes, format_count};
use raylib::prelude::*;

/// The statistics panel pinned to the top-right corner of the scene
/// (toggled in Settings): NPS, notes played against the song total,
/// BPM, and whatever the engine reports (polyphony, render load,
/// skipping velocity, audio buffer). Drawn as a translucent panel;
/// fades with its animation. `top_offset` moves it below the docked
/// player-controls strip so the two never overlap.
pub fn draw_stats_overlay(
    d: &mut impl RaylibDraw,
    app_state: &AppState,
    engine: &PlayerHandle,
    screen_width: i32,
    top_offset: i32,
) {
    let stats_alpha = app_state.ui.show_stats_animation.clamp(0.0, 1.0);
    if stats_alpha <= 0.01 {
        return;
    }
    let tempo_us = app_state.playback.tempo_us_value.max(1);
    let bpm = 60_000_000.0 / tempo_us as f64;
    let total = app_state
        .playback.midi_data
        .as_ref()
        .map(|data| data.total_note_on_events())
        .unwrap_or(0);
    let mut lines = vec![
        format!(
            "NPS   {}",
            format_count(app_state.playback.nps_value.round() as u64)
        ),
        format!(
            "NOTES {} / {}",
            format_count(engine.notes_played()),
            format_count(total)
        ),
        format!("BPM   {:.2} ({})", bpm, format_count(tempo_us as u64)),
    ];
    // Polyphony and render load; engines that do not
    // report them show N/A.
    lines.push(format!(
        "POLY  {}",
        engine
            .polyphony()
            .map(|voices| format_count(voices as u64))
            .unwrap_or_else(|| "N/A".to_string())
    ));
    lines.push(format!(
        "LOAD  {}",
        engine
            .render_load()
            .map(|load| format!("{:.1}%", load * 100.0))
            .unwrap_or_else(|| "N/A".to_string())
    ));
    if let Some(vel) = engine.skipping_velocity() {
        lines.push(format!("Skipping Vel: {vel}"));
    }
    if let Some(buffered_ns) = engine.audio_buffer_ns() {
        // m:ss.xx, rounded to centiseconds.
        let mss = |ns: u64| {
            let cs = (ns + 5_000_000) / 10_000_000;
            format!("{}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
        };
        let max_ns = app_state.settings.audio_buffer_secs as u64 * 1_000_000_000;
        lines.push(format!("AUDIO BUF {} / {}", mss(buffered_ns), mss(max_ns)));
    }
    if let Some((used, _, compressed)) = engine.buffer_memory() {
        if compressed {
            lines.push(format!("AUDIO BUF {}", format_bytes(used)));
        } else {
            lines.push(format!("AUDIO BUF {} (PCM)", format_bytes(used)));
        }
    }
    let font_size = 18;
    let pad = 8;
    let line_h = 22;
    let panel_w = 330;
    let panel_h = pad * 2 + line_h * lines.len() as i32;
    let px = screen_width - panel_w - 10;
    let py = 10 + top_offset;
    let bg_a = (180.0 * stats_alpha) as u8;
    let border_a = (200.0 * stats_alpha) as u8;
    let text_a = (255.0 * stats_alpha) as u8;
    d.draw_rectangle(px, py, panel_w, panel_h, Color::new(20, 20, 24, bg_a));
    // draw_rectangle_lines is GL-line based and rasterizes off-by-one from the
    // filled rect, leaving a 1px sliver of fill outside the border. draw_rectangle_lines_ex
    // draws the border as filled rects inside the rectangle bounds, so it lands
    // exactly on the fill's edge columns/rows.
    d.draw_rectangle_lines_ex(
        Rectangle::new(px as f32, py as f32, panel_w as f32, panel_h as f32),
        1.0,
        Color::new(0, 135, 200, border_a),
    );
    for (i, line) in lines.iter().enumerate() {
        d.draw_text(
            line,
            px + pad,
            py + pad + i as i32 * line_h,
            font_size,
            Color::new(210, 225, 255, text_a),
        );
    }
}
