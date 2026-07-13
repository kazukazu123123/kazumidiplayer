use kazumidiparser::KazuMIDIParserData;

#[derive(Debug, Clone)]
pub struct TrackCursor {
    pub track_index: u16,
    pub p: *const u8,
    pub end: *const u8,
    pub abs_tick: u32,
    pub running_status: u8,
    pub status: u8,
    pub param1: u8,
    pub param2: u8,
    pub is_tempo: bool,
    pub tempo_val: u32,
    pub has_event: bool,
    pub skip_next_delta: bool,
    /// Whether the optional "MTrk" header at the cursor's start position has been
    /// looked for yet. It can only sit at the very first read, but the check used to
    /// run on every event, costing a 4-byte compare per event across the whole file. It
    /// also risked a false positive: MIDI data that happens to read as "MTrk"
    /// mid-track would have skipped 8 bytes of real events.
    pub header_checked: bool,
}

/// Read a variable-length value from a MIDI byte stream.
///
/// # Safety
/// - `p` must point to a valid read pointer within `end`.
/// - `end` must be a valid pointer to the end of the buffer.
/// - The caller ensures `*p <= end`.
#[inline]
pub unsafe fn read_varlen(p: &mut *const u8, end: *const u8) -> Option<u32> {
    debug_assert!(*p <= end);
    if *p >= end {
        return None;
    }
    unsafe {
        let mut b = *(*p);
        *p = (*p).add(1);
        if (b & 0x80) == 0 {
            return Some(b as u32);
        }
        let mut val = (b & 0x7F) as u32;
        loop {
            if *p >= end {
                return None;
            }
            b = *(*p);
            *p = (*p).add(1);
            val = (val << 7) | ((b & 0x7F) as u32);
            if (b & 0x80) == 0 {
                break;
            }
        }
        Some(val)
    }
}

/// Initialize a track cursor for a given track index.
///
/// # Safety
/// - `data` must be a valid reference to a properly initialized KazuMIDIParserData.
/// - `track_idx` must be less than `data.header.tracks`.
#[inline]
pub unsafe fn init_track_cursor(data: &KazuMIDIParserData, track_idx: u16) -> Option<TrackCursor> {
    let ti = track_idx as usize;
    let track_count = data.header.tracks as usize;
    if ti >= track_count {
        return None;
    }

    unsafe {
        let raw = data.raw;
        let track_start_offset = *data.track_index.add(ti);
        let start = raw.add(track_start_offset);
        let len = if !data.track_lengths.is_null() {
            *data.track_lengths.add(ti)
        } else if ti + 1 < data.track_index_count {
            *data.track_index.add(ti + 1) - track_start_offset
        } else {
            data.raw_size - track_start_offset
        };

        Some(TrackCursor {
            track_index: track_idx,
            p: start,
            end: start.add(len),
            abs_tick: 0,
            running_status: 0,
            status: 0,
            param1: 0,
            param2: 0,
            is_tempo: false,
            tempo_val: 0,
            has_event: false,
            skip_next_delta: false,
            header_checked: false,
        })
    }
}

/// Advance a track cursor to the next MIDI event.
///
/// # Safety
/// - `c` must be a valid, initialized TrackCursor.
/// - The cursor's `p` and `end` pointers must point to valid MIDI data.
#[inline]
pub unsafe fn cursor_next(c: &mut TrackCursor) -> bool {
    unsafe {
        // Skip the MTrk header if the cursor starts on one. Once only: see
        // `header_checked`.
        if !c.header_checked {
            c.header_checked = true;
            if c.p.add(4) <= c.end {
                let sig = std::slice::from_raw_parts(c.p, 4);
                if sig == b"MTrk" && c.p.add(8) <= c.end {
                    c.p = c.p.add(8);
                }
            }
        }
        while c.p < c.end {
            if !c.skip_next_delta {
                let delta = match read_varlen(&mut c.p, c.end) {
                    Some(d) => d,
                    None => return false,
                };
                c.abs_tick = c.abs_tick.saturating_add(delta);
            } else {
                c.skip_next_delta = false;
                let _ = read_varlen(&mut c.p, c.end);
            }

            if c.p >= c.end {
                return false;
            }

            let mut status = *c.p;
            if status < 0x80 {
                status = c.running_status;
            } else {
                c.p = c.p.add(1);
                if status < 0xF0 {
                    c.running_status = status;
                }
            }

            if status >= 0x80 && status < 0xF0 {
                if status >= 0xC0 && status < 0xE0 {
                    if c.p >= c.end {
                        return false;
                    }
                    let p1 = *c.p;
                    c.p = c.p.add(1);
                    c.status = status;
                    c.param1 = p1;
                    c.param2 = 0;
                    c.is_tempo = false;
                    c.has_event = true;
                    return true;
                } else {
                    if c.p.add(1) >= c.end {
                        return false;
                    }
                    let p1 = *c.p;
                    c.p = c.p.add(1);
                    let p2 = *c.p;
                    c.p = c.p.add(1);
                    c.status = status;
                    c.param1 = p1;
                    c.param2 = p2;
                    c.is_tempo = false;
                    c.has_event = true;
                    return true;
                }
            } else if status == 0xFF {
                c.running_status = 0;
                if c.p >= c.end {
                    return false;
                }
                let mtype = *c.p;
                c.p = c.p.add(1);
                let mlen = match read_varlen(&mut c.p, c.end) {
                    Some(d) => d,
                    None => return false,
                };
                if mtype == 0x51 && mlen >= 3 {
                    c.is_tempo = true;
                    c.status = 0xFF;
                    c.tempo_val = ((*c.p as u32) << 16)
                        | ((*c.p.add(1) as u32) << 8)
                        | (*c.p.add(2) as u32);
                    c.has_event = true;
                    c.p = c.p.add(mlen as usize);
                    return true;
                }
                c.p = c.p.add(mlen as usize);
            } else if status == 0xF0 || status == 0xF7 {
                c.running_status = 0;
                let slen = match read_varlen(&mut c.p, c.end) {
                    Some(d) => d,
                    None => return false,
                };
                c.p = c.p.add(slen as usize);
            } else {
                c.p = c.p.add(1);
            }
        }
        false
    }
}

/// Jump a cursor to the last keyframe at or before `target_tick`, so the walk to
/// the target starts near it instead of at the head of the track. Keyframes are in
/// ascending tick order, so this is a binary search: a linear scan here would cost
/// (tracks x keyframes) per seek and get slower the later in the file you land.
///
/// No-op when the track has no keyframes (the cursor then walks from the start).
///
/// # Safety
/// - `data` must be valid and `track_idx` within bounds.
/// - `cursor` must belong to `track_idx` and be freshly initialized.
unsafe fn jump_to_keyframe(data: &KazuMIDIParserData, cursor: &mut TrackCursor, target_tick: u32) {
    unsafe {
        let ti = cursor.track_index as usize;
        let kc = *data.keyframe_counts.add(ti);
        if kc == 0 {
            return;
        }
        let kf_ptr = *data.keyframes.add(ti);

        // Largest j with keyframes[j].abs_tick <= target_tick. Keyframe 0 sits at
        // the track start, so when every keyframe is past the target, j = 0 is the
        // correct (and safe) place to start.
        let mut lo = 0usize;
        let mut hi = kc;
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if (*kf_ptr.add(mid)).abs_tick <= target_tick {
                lo = mid;
            } else {
                hi = mid;
            }
        }

        let kf = &*kf_ptr.add(lo);
        let track_start_offset = *data.track_index.add(ti);
        cursor.p = data.raw.add(track_start_offset + kf.raw_offset);
        cursor.abs_tick = kf.abs_tick;
        cursor.running_status = kf.running_status;
        cursor.skip_next_delta = true;
    }
}

/// Seek a track cursor to the first event at or after the given tick.
///
/// # Safety
/// - `data` must be a valid KazuMIDIParserData reference.
/// - `track_idx` must be within bounds.
/// - The returned cursor's pointers are valid for `data`'s raw memory.
pub unsafe fn seek_track_cursor(
    data: &KazuMIDIParserData,
    track_idx: u16,
    target_tick: u32,
) -> TrackCursor {
    let mut cursor = match unsafe { init_track_cursor(data, track_idx) } {
        Some(c) => c,
        None => {
            return TrackCursor {
                track_index: track_idx,
                p: std::ptr::null(),
                end: std::ptr::null(),
                abs_tick: 0,
                running_status: 0,
                status: 0,
                param1: 0,
                param2: 0,
                is_tempo: false,
                tempo_val: 0,
                has_event: false,
                skip_next_delta: false,
                header_checked: false,
            };
        }
    };

    if cursor.p.is_null() {
        return cursor;
    }

    unsafe {
        jump_to_keyframe(data, &mut cursor, target_tick);

        while cursor_next(&mut cursor) {
            if cursor.is_tempo {
                continue;
            }
            if cursor.abs_tick >= target_tick {
                break;
            }
        }
        if cursor.abs_tick < target_tick {
            cursor.has_event = false;
        }
    }

    cursor
}

/// A note that was still sounding when a seek landed mid-note: the key it is on,
/// the tick it started at, and its channel.
#[derive(Clone, Copy)]
pub struct HeldNote {
    pub key: u8,
    pub start_tick: u32,
    pub channel: u8,
}

/// Like `seek_track_cursor`, but also reports every note still held at
/// `target_tick`, in note-on order.
///
/// Held notes are tracked per (channel, key), and several note-ons may stack on one
/// of those before any note-off arrives -- routine in dense MIDI, where a key is
/// struck again while it still rings. Each note-off closes the oldest note still
/// open on its (channel, key), so overlapping notes survive as separate notes
/// instead of truncating each other.
///
/// Notes whose note-on lies before the keyframe used for the jump are not detected,
/// so a very long note spanning a keyframe boundary can still be missed.
///
/// # Safety
/// Same requirements as `seek_track_cursor`.
pub unsafe fn seek_track_cursor_with_held(
    data: &KazuMIDIParserData,
    track_idx: u16,
    target_tick: u32,
) -> (TrackCursor, Vec<HeldNote>) {
    // Open notes per key, in note-on order, each with the channel that opened it.
    let mut open: [Vec<(u32, u8)>; 128] = std::array::from_fn(|_| Vec::new());

    let mut cursor = match unsafe { init_track_cursor(data, track_idx) } {
        Some(c) => c,
        None => {
            return (
                TrackCursor {
                    track_index: track_idx,
                    p: std::ptr::null(),
                    end: std::ptr::null(),
                    abs_tick: 0,
                    running_status: 0,
                    status: 0,
                    param1: 0,
                    param2: 0,
                    is_tempo: false,
                    tempo_val: 0,
                    has_event: false,
                    skip_next_delta: false,
                    header_checked: false,
                },
                Vec::new(),
            );
        }
    };

    if cursor.p.is_null() {
        return (cursor, Vec::new());
    }

    unsafe {
        jump_to_keyframe(data, &mut cursor, target_tick);

        // Walk to the target, tracking note on/off so we know what is held.
        while cursor_next(&mut cursor) {
            if cursor.is_tempo {
                continue;
            }
            if cursor.abs_tick >= target_tick {
                break;
            }
            let msg_type = cursor.status & 0xF0;
            let key = cursor.param1 as usize;
            if key < 128 {
                let channel = cursor.status & 0x0F;
                if msg_type == 0x90 && cursor.param2 > 0 {
                    open[key].push((cursor.abs_tick, channel));
                } else if msg_type == 0x80 || (msg_type == 0x90 && cursor.param2 == 0) {
                    // Close the oldest note still open on this channel and key. A
                    // note-off carries no way to say which of several stacked notes
                    // it ends, so oldest-first is the convention.
                    if let Some(pos) = open[key].iter().position(|&(_, ch)| ch == channel) {
                        open[key].remove(pos);
                    }
                }
            }
        }
        if cursor.abs_tick < target_tick {
            cursor.has_event = false;
        }
    }

    // Flatten, oldest note-on first, so the caller can append them in start order.
    let mut held: Vec<HeldNote> = Vec::new();
    for (key, notes) in open.iter().enumerate() {
        for &(start_tick, channel) in notes {
            held.push(HeldNote {
                key: key as u8,
                start_tick,
                channel,
            });
        }
    }
    held.sort_by_key(|n| n.start_tick);

    (cursor, held)
}

#[inline]
pub fn tick_to_ns(data: &KazuMIDIParserData, tick: u32) -> u64 {
    if data.tempo_count == 0 {
        return 0;
    }
    unsafe {
        // Largest i with tempo_tick[i] <= tick (tempo_tick is ascending).
        let mut lo = 0usize;
        let mut hi = data.tempo_count;
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if *data.tempo_tick.add(mid) <= tick {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let i = lo;
        let base_ns = *data.tempo_ns.add(i);
        let base_tick = *data.tempo_tick.add(i);
        let tempo = *data.tempo_val.add(i);
        let division = data.header.division as u64;
        // Use u128 to avoid both overflow and truncation errors from integer division.
        // delta_tick can be ~4e9, tempo ~1.6e7, product * 1000 ~ 6.4e19 (> u64::MAX).
        let delta_tick = (tick - base_tick) as u128;
        let ns = delta_tick * (tempo as u128) * 1000 / division.max(1) as u128;
        base_ns.saturating_add(ns as u64)
    }
}

#[inline]
pub fn ns_to_tick(data: &KazuMIDIParserData, ns: u64) -> u32 {
    if data.tempo_count == 0 || ns == 0 {
        return 0;
    }
    unsafe {
        // Largest i with tempo_ns[i] <= ns (tempo_ns is ascending).
        let mut lo = 0usize;
        let mut hi = data.tempo_count;
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if *data.tempo_ns.add(mid) <= ns {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let i = lo;
        let base_ns = *data.tempo_ns.add(i);
        let base_tick = *data.tempo_tick.add(i);
        let tempo = *data.tempo_val.add(i);
        let delta_ns = ns - base_ns;
        // Use u128 to avoid overflow and truncation.
        let division = data.header.division as u128;
        let delta_tick = (delta_ns as u128 * division / ((tempo as u128).saturating_mul(1000)))
            .min(u32::MAX as u128) as u32;
        base_tick.saturating_add(delta_tick)
    }
}
