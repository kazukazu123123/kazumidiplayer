use std::sync::Mutex;
use std::sync::MutexGuard;

pub fn lock_mutex<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// CSS `cubic-bezier(x1, y1, x2, y2)`: maps progress `t` (0..1) to an eased value. The
/// curve's endpoints are fixed at (0,0) and (1,1); the two control points shape it.
/// `y` may leave 0..1 (that is what makes a curve overshoot), but `x` must stay in it,
/// so x1/x2 are clamped: a curve that doubles back in x has no single value per t.
///
/// x(t) is not invertible in closed form, so the parameter for a given progress is found
/// with a few Newton steps (the curve is monotonic in x, so this converges fast), then y
/// is evaluated at it.
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let x1 = x1.clamp(0.0, 1.0);
    let x2 = x2.clamp(0.0, 1.0);

    // B(u) for control points (0, p1, p2, 1), expanded into polynomial coefficients.
    let curve = |p1: f32, p2: f32, u: f32| {
        let a = 3.0 * p1 - 3.0 * p2 + 1.0;
        let b = -6.0 * p1 + 3.0 * p2;
        let c = 3.0 * p1;
        ((a * u + b) * u + c) * u
    };
    let slope = |p1: f32, p2: f32, u: f32| {
        let a = 3.0 * p1 - 3.0 * p2 + 1.0;
        let b = -6.0 * p1 + 3.0 * p2;
        let c = 3.0 * p1;
        (3.0 * a * u + 2.0 * b) * u + c
    };

    let mut u = t;
    for _ in 0..5 {
        let dx = curve(x1, x2, u) - t;
        let d = slope(x1, x2, u);
        if d.abs() < 1e-6 {
            break;
        }
        u -= dx / d;
    }

    curve(y1, y2, u.clamp(0.0, 1.0))
}

// Palette index reserved for "no track": color by channel alone. Real tracks index
// below it, so it doubles as the modulus that keeps them out of its range.
const NO_TRACK: u16 = 0xFFF0;

// Fold a (track, channel) pair into a color-palette index: the low 4 bits are the
// channel, the rest the track.
//
// The index must not be narrowed to a byte. A byte fits sixteen tracks before
// `track * 16 + channel` wraps and tracks begin sharing a color, and MIDIs here run
// to thousands of tracks. Notes stacked on one key are told apart by nothing but
// their color, so a shared one makes a dense file unreadable rather than merely
// drab.
pub fn note_color_index(track_index: u16, channel: u8) -> u16 {
    let channel = (channel & 0x0F) as u32;
    if track_index == u16::MAX {
        return NO_TRACK | channel as u16;
    }
    ((track_index as u32 * 16 + channel) % NO_TRACK as u32) as u16
}

// Get note color from a palette index (see `note_color_index`).
pub fn get_note_color_by_index(index: u16) -> (u8, u8, u8) {
    let channel = (index & 0x0F) as usize;

    if index >= NO_TRACK {
        // No track to distinguish, so the channel gets the hue to itself.
        return hsv_to_rgb(channel as f32 * 360.0 / 16.0, 0.8, 0.95);
    }

    // Golden-angle hue stepping: consecutive tracks land far apart on the wheel, and
    // no two repeat within a file's worth of them. Stepping evenly instead gives
    // neighbouring tracks neighbouring hues, which reads as one gradient rather than
    // as separate voices. Channels are shades of their track's hue rather than hues
    // of their own, so a track still reads as one voice.
    let track = (index >> 4) as f32;
    let hue = (track * 137.507_76) % 360.0;
    let saturation = 0.75 + (channel % 3) as f32 * 0.08;
    let value = 0.78 + (channel % 4) as f32 * 0.06;

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

/// `MM:SS.mmm`, or `H:MM:SS.mmm` once past an hour. The hour field only appears when
/// there is one, so the common case is not padded with a leading `00:`, and minutes
/// stay two digits rather than running to `61:` and beyond.
pub fn format_time(ns: u64) -> String {
    let total_seconds = ns / 1_000_000_000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let millis = (ns % 1_000_000_000) / 1_000_000;
    if hours > 0 {
        format!("{}:{:02}:{:02}.{:03}", hours, minutes, seconds, millis)
    } else {
        format!("{:02}:{:02}.{:03}", minutes, seconds, millis)
    }
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
///
/// Counts use K/M/B, not the K/M/G of `format_bytes`: these are events, and "1.23G
/// events" reads like a data size.
pub fn format_count_short(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")];
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the wrap a one-byte index brings back: `track * 16 + channel` modulo
    /// 256 lands track 16 on track 0's color, 17 on 1's, and so on.
    #[test]
    fn tracks_past_the_sixteenth_keep_their_own_color() {
        for track in 1..64u16 {
            assert_ne!(
                note_color_index(track, 0),
                note_color_index(0, 0),
                "track {track} collides with track 0"
            );
        }
    }

    #[test]
    fn every_track_gets_a_distinct_index() {
        let mut seen = std::collections::HashSet::new();
        for track in 0..4095u16 {
            assert!(
                seen.insert(note_color_index(track, 0)),
                "track {track} reuses an index"
            );
        }
    }

    /// Channels are shades of their track's hue, so they must not collide either.
    #[test]
    fn channels_within_a_track_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for channel in 0..16u8 {
            assert!(seen.insert(note_color_index(7, channel)));
        }
    }

    #[test]
    fn no_track_sentinel_colors_by_channel_alone() {
        // Distinct per channel, and never confusable with a real track's index.
        let mut seen = std::collections::HashSet::new();
        for channel in 0..16u8 {
            let idx = note_color_index(u16::MAX, channel);
            assert!(seen.insert(idx));
            for track in 0..4095u16 {
                assert_ne!(idx, note_color_index(track, channel));
            }
        }
    }
}
