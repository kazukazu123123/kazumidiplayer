use std::sync::Mutex;
use std::sync::MutexGuard;

pub fn lock_mutex<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// Fold a (track, channel) pair into a single 0..255 color-palette index. This is
// all the color depends on, so it can be stored as one byte per note instead of
// keeping the full track (u16) + channel (u8).
pub fn note_color_index(track_index: u16, channel: u8) -> u8 {
    // 256-color palette (16 tracks x 16 channels = 256 combinations)
    let track_idx = if track_index == u16::MAX { 0 } else { track_index as usize };
    ((track_idx * 16 + channel as usize) % 256) as u8
}

// Get note color from a palette index (see `note_color_index`).
pub fn get_note_color_by_index(index: u8) -> (u8, u8, u8) {
    let combined_index = index as usize;

    // Generate color in HSV color space (vary hue)
    let hue = (combined_index as f32 * 360.0 / 256.0) % 360.0;
    let saturation = 0.7 + ((combined_index % 3) as f32 * 0.1);
    let value = 0.8 + ((combined_index % 2) as f32 * 0.15);

    hsv_to_rgb(hue, saturation, value)
}

// Get note color based on track + channel.
pub fn get_note_color_by_track_channel(track_index: u16, channel: u8) -> (u8, u8, u8) {
    get_note_color_by_index(note_color_index(track_index, channel))
}

// Convert HSV to RGB
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

pub fn format_time(ns: u64) -> String {
    let total_seconds = ns / 1_000_000_000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    let millis = (ns % 1_000_000_000) / 1_000_000;
    format!("{:02}:{:02}.{:03}", minutes, seconds, millis)
}

/// Group a count into thousands, e.g. `1234567` -> `1,234,567`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Abbreviate a count to 3 significant-ish digits, e.g. `1234567` -> `1.23M`.
pub fn format_count_short(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "K")];
    for (scale, suffix) in UNITS {
        if n >= scale {
            let v = n as f64 / scale as f64;
            let decimals = if v < 10.0 {
                2
            } else if v < 100.0 {
                1
            } else {
                0
            };
            return format!("{:.*}{}", decimals, v, suffix);
        }
    }
    n.to_string()
}

/// Format a byte count with binary units, e.g. `1536` -> `1.50KB`.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "KB"),
    ];
    for (scale, suffix) in UNITS {
        if n >= scale {
            let v = n as f64 / scale as f64;
            let decimals = if v < 10.0 {
                2
            } else if v < 100.0 {
                1
            } else {
                0
            };
            return format!("{:.*}{}", decimals, v, suffix);
        }
    }
    format!("{}B", n)
}

pub fn format_measures(weighted_ns: u64, total_weighted_ns: u64) -> String {
    let beat_duration_ns = 500_000_000; // 500ms (120 BPM weighted beat)
    let beats_per_measure = 4;
    let measure_duration_ns = beat_duration_ns * beats_per_measure;

    let current_measure = (weighted_ns / measure_duration_ns) + 1;
    let total_measures = (total_weighted_ns.saturating_sub(1) / measure_duration_ns) + 1;

    // Cap current_measure to total_measures to avoid "11/10" at the very end
    let current_measure = current_measure.min(total_measures);

    format!("{}/{}", current_measure, total_measures.max(1))
}
