use kazumidiparser::KazuMIDIParserData;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

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
}

/// Safety: the pointers are into the parser's raw MIDI buffer, which is immutable
/// for as long as the `MIDIPlayerData` behind them is alive, and every caller holds
/// an `Arc` to it. A cursor is a read-only position in that buffer, so handing one
/// to another thread only moves where it is read from, never what it reads. The
/// piano roll's setup seeks its tracks in parallel and returns the cursors, which is
/// what needs this.
///
/// `Send` only, deliberately: two threads sharing *one* cursor would race on
/// `cursor_next`, which mutates it.
unsafe impl Send for TrackCursor {}

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
        // track_index points at the first event (past the 8-byte "MTrk" chunk
        // header) and track_lengths is the event byte count; the parser always
        // fills both and keeps the range inside raw.
        let start = data.raw.add(*data.track_index.add(ti));
        let len = *data.track_lengths.add(ti);

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

            // Same rules as the parser's own cursor, so events and
            // counts agree: running status survives meta / SysEx, and a data byte
            // with no status ever set is desync (stop rather than resync by luck).
            let mut status = *c.p;
            if status < 0x80 {
                if c.running_status == 0 {
                    return false;
                }
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
                if c.p >= c.end {
                    return false;
                }
                let mtype = *c.p;
                c.p = c.p.add(1);
                let mlen = match read_varlen(&mut c.p, c.end) {
                    Some(d) => d,
                    None => return false,
                };
                // Never step past `end`, not even to form the pointer.
                if mlen as usize > c.end.offset_from(c.p) as usize {
                    return false;
                }
                // End of track: the rest of the chunk is padding, not music.
                if mtype == 0x2F {
                    c.p = c.end;
                    return false;
                }
                if mtype == 0x51 && mlen >= 3 {
                    c.is_tempo = true;
                    c.status = 0xFF;
                    c.tempo_val =
                        ((*c.p as u32) << 16) | ((*c.p.add(1) as u32) << 8) | (*c.p.add(2) as u32);
                    c.has_event = true;
                    c.p = c.p.add(mlen as usize);
                    return true;
                }
                c.p = c.p.add(mlen as usize);
            } else if status == 0xF0 || status == 0xF7 {
                let slen = match read_varlen(&mut c.p, c.end) {
                    Some(d) => d,
                    None => return false,
                };
                if slen as usize > c.end.offset_from(c.p) as usize {
                    return false;
                }
                c.p = c.p.add(slen as usize);
            } else {
                // System common: 0xF2 carries two data bytes, 0xF1 / 0xF3 one,
                // the rest none.
                let skip = match status {
                    0xF2 => 2,
                    0xF1 | 0xF3 => 1,
                    _ => 0,
                };
                if skip > c.end.offset_from(c.p) as usize {
                    return false;
                }
                c.p = c.p.add(skip);
            }
        }
        false
    }
}

/// Seek a track cursor to the first event at or after the given tick,
/// walking from the track start. Playback seeks use the block index
/// ([`NoteOnIndex::seek_cursor`]) instead; this is the reference walk.
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
            };
        }
    };

    if cursor.p.is_null() {
        return cursor;
    }

    unsafe {
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

/// Count note-ons (velocity above zero) with a tick strictly before
/// `target_tick`, across all tracks, by scanning each track from its
/// start. This deliberately mirrors the parser's own note-on
/// definition, so the count at the end of the song equals
/// `total_note_on_events`.
///
/// The played-notes counter normally just counts dispatched events,
/// which drifts whenever playback teleports: a seek discards or
/// replays a whole section, a stop rewinds to zero, and the display
/// reseek in prerender mode skips events the audio still played.
/// Callers re-derive the counter with this after any of those jumps
/// so it keeps meaning "note-ons up to the current position".
///
/// # Safety
/// - `data` must be a valid KazuMIDIParserData reference.
pub unsafe fn count_note_ons_before(data: &KazuMIDIParserData, target_tick: u32) -> u64 {
    let mut total = 0u64;
    for t in 0..data.header.tracks {
        let Some(mut cursor) = (unsafe { init_track_cursor(data, t) }) else {
            continue;
        };
        while unsafe { cursor_next(&mut cursor) } {
            if cursor.is_tempo {
                continue;
            }
            if cursor.abs_tick >= target_tick {
                break;
            }
            if cursor.status & 0xF0 == 0x90 && cursor.param2 > 0 {
                total += 1;
            }
        }
    }
    total
}

/// A per-track block index over the song's note-ons, built once per
/// load so recounting them does not rescan from the start of every
/// track on each seek. Every `BLOCK_EVENTS` events, a block records
/// the cursor state (byte offset, tick, running status) and the
/// number of note-ons before it; counting then resumes from the
/// nearest block below the target and scans about one block of
/// events per track.
pub struct NoteOnIndex {
    tracks: Vec<Vec<NoteBlock>>,
}

#[derive(Clone, Copy)]
struct NoteBlock {
    /// Byte offset of the block's first event within its track.
    byte_offset: u32,
    /// Tick of the event just before the block: the cumulative tick
    /// the block's first event's delta is added to.
    tick: u32,
    /// Running status in effect for the block's first event.
    running_status: u8,
    /// Note-ons in this track before the block's first event.
    note_ons_before: u64,
}

impl NoteOnIndex {
    // Small blocks: with many-track files the per-track tail scans
    // add up (measured: 8192-event blocks still cost ~116 ms per
    // recount on a 100+ track file), so cap the tail at 256.
    const BLOCK_EVENTS: u32 = 256;

    /// Scan every track once and record the blocks. A pure read over
    /// the parser's buffer; the player thread runs it as part of
    /// load completion.
    ///
    /// # Safety
    /// - `data` must be valid parser data whose raw buffer stays
    ///   alive and immutable for the scan (the owning `Arc` does).
    pub unsafe fn build(data: &KazuMIDIParserData) -> NoteOnIndex {
        let mut tracks = Vec::with_capacity(data.header.tracks as usize);
        for t in 0..data.header.tracks {
            let mut blocks = Vec::new();
            let Some(mut cursor) = (unsafe { init_track_cursor(data, t) }) else {
                tracks.push(blocks);
                continue;
            };
            let track_start = cursor.p;
            let mut note_ons = 0u64;
            // The first event opens a block.
            let mut since_block = Self::BLOCK_EVENTS;
            loop {
                if since_block >= Self::BLOCK_EVENTS {
                    blocks.push(NoteBlock {
                        byte_offset: (cursor.p as usize - track_start as usize) as u32,
                        tick: cursor.abs_tick,
                        running_status: cursor.running_status,
                        note_ons_before: note_ons,
                    });
                    since_block = 0;
                }
                if !unsafe { cursor_next(&mut cursor) } {
                    break;
                }
                since_block += 1;
                if !cursor.is_tempo && cursor.status & 0xF0 == 0x90 && cursor.param2 > 0 {
                    note_ons += 1;
                }
            }
            tracks.push(blocks);
        }
        NoteOnIndex { tracks }
    }

    /// Seek a cursor to the first event at or after `target_tick`
    /// on `track`, resuming from this index's nearest block instead
    /// of walking from the track start. Lands exactly where
    /// `seek_track_cursor` lands; falls back to it when the track
    /// has no blocks.
    ///
    /// # Safety
    /// - Same requirements as `seek_track_cursor`, and the index
    ///   must have been built from this same `data`.
    pub unsafe fn seek_cursor(
        &self,
        data: &KazuMIDIParserData,
        track_idx: u16,
        target_tick: u32,
    ) -> TrackCursor {
        let Some(blocks) = self.tracks.get(track_idx as usize).filter(|b| !b.is_empty()) else {
            return unsafe { seek_track_cursor(data, track_idx, target_tick) };
        };
        let Some(mut cursor) = (unsafe { init_track_cursor(data, track_idx) }) else {
            return unsafe { seek_track_cursor(data, track_idx, target_tick) };
        };
        // Last block whose pre-tick is at or below the target. Block
        // 0 is the track head, so targets before the first block use
        // it too.
        let past = blocks.partition_point(|b| b.tick <= target_tick);
        let block = blocks[past.saturating_sub(1)];
        cursor.p = unsafe { cursor.p.add(block.byte_offset as usize) };
        cursor.abs_tick = block.tick;
        cursor.running_status = block.running_status;
        unsafe {
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

    /// Note-ons with a tick strictly before `target_tick`. Agrees
    /// with `count_note_ons_before`; it is just faster.
    ///
    /// # Safety
    /// - Same requirements as `count_note_ons_before`, and the index
    ///   must have been built from this same `data`.
    pub unsafe fn count_before(&self, data: &KazuMIDIParserData, target_tick: u32) -> u64 {
        let mut total = 0u64;
        for (t, blocks) in self.tracks.iter().enumerate() {
            // The last block whose pre-tick is below the target; the
            // scan from there covers everything up to the target.
            let past = blocks.partition_point(|b| b.tick < target_tick);
            if past == 0 {
                continue;
            }
            let block = blocks[past - 1];
            let Some(mut cursor) = (unsafe { init_track_cursor(data, t as u16) }) else {
                continue;
            };
            cursor.p = unsafe { cursor.p.add(block.byte_offset as usize) };
            cursor.abs_tick = block.tick;
            cursor.running_status = block.running_status;
            let mut count = block.note_ons_before;
            while unsafe { cursor_next(&mut cursor) } {
                if cursor.is_tempo {
                    continue;
                }
                if cursor.abs_tick >= target_tick {
                    break;
                }
                if cursor.status & 0xF0 == 0x90 && cursor.param2 > 0 {
                    count += 1;
                }
            }
            total += count;
        }
        total
    }
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
/// of those before any note-off arrives, which is routine in dense MIDI, where a key is
/// struck again while it still rings. Each note-off closes the oldest note still
/// open on its (channel, key), so overlapping notes survive as separate notes
/// instead of truncating each other.
///
/// This deliberately scans from the track start instead of resuming from a
/// checkpoint. Checkpoints do not contain held-note state, so resuming can lose
/// a long note whose note-on lies before the checkpoint and make it disappear
/// after a roll rebuild.
///
/// # Safety
/// Same requirements as `seek_track_cursor`.
#[allow(dead_code)] // reference implementation; production uses the checkpointed variant
pub unsafe fn seek_track_cursor_with_held(
    data: &KazuMIDIParserData,
    track_idx: u16,
    target_tick: u32,
    max_per_key: usize,
) -> (TrackCursor, Vec<HeldNote>) {
    // Open notes per (channel, key), oldest first. Keyed by channel as well as key so a
    // note-off closes a note on its own channel, and a queue rather than a list so it
    // closes the oldest in O(1): a MIDI that sustains heavily can leave millions of
    // notes open on one track, and searching a list for each note-off would be
    // quadratic.
    let mut open: FxHashMap<(u8, u8), VecDeque<u32>> = FxHashMap::default();

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
                },
                Vec::new(),
            );
        }
    };

    if cursor.p.is_null() {
        return (cursor, Vec::new());
    }

    unsafe {
        // Walk from the track start to the target, tracking note on/off so the
        // held-note set is complete even when a note spans one or more checkpoints.
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
                    let queue = open.entry((channel, key as u8)).or_default();
                    queue.push_back(cursor.abs_tick);
                    // The roll can only draw a handful of notes per key anyway: they
                    // are all painted over each other in one column. Bound the queue so
                    // a MIDI that leaves millions of notes ringing does not build a
                    // multi-hundred-megabyte list here on every seek. The oldest go,
                    // since the newest are the ones drawn on top.
                    if queue.len() > max_per_key {
                        queue.pop_front();
                    }
                } else if msg_type == 0x80 || (msg_type == 0x90 && cursor.param2 == 0) {
                    // Close the oldest note still open on this channel and key. A
                    // note-off carries no way to say which of several stacked notes
                    // it ends, so oldest-first is the convention.
                    if let Some(queue) = open.get_mut(&(channel, key as u8)) {
                        queue.pop_front();
                    }
                }
            }
        }
        if cursor.abs_tick < target_tick {
            cursor.has_event = false;
        }
    }

    // Flatten, oldest note-on first, so the caller can append them in start order.
    (cursor, flatten_held(&open))
}

/// How many stepped events apart held-note checkpoints are taken while
/// walking a track. The interval bounds one track's replay, but a seek
/// re-seeds every track, so the total replay is the sum over tracks:
/// on NECROFANTASIA (6,541 tracks) a parallel seek of every track to
/// the midpoint measured 1,290 ms uncheckpointed, 892 ms at a 1M
/// interval (8.3 MB of checkpoints), 525 ms at 250k (37.5 MB) and
/// 262 ms at this 100k interval (105 MB), all with the per-key cap at
/// 128 (measured 2026-11-15). Ordinary files are far smaller: their
/// tracks are short, so most never reach even one checkpoint.
pub const HELD_CKPT_INTERVAL: u64 = 100_000;

/// A resumable held-note state partway through a track: the cursor
/// just after `event_count` stepped events, plus every note still
/// sounding there, in start-tick order, the per-(channel, key) queues
/// capped exactly as `seek_track_cursor_with_held` caps them.
/// Restoring one and walking on reproduces the from-start walk
/// exactly: the queues are rebuilt in start order, and note-offs
/// close the oldest note on their (channel, key).
#[derive(Clone)]
pub struct HeldCheckpoint {
    pub event_count: u64,
    pub tick: u32,
    pub cursor: TrackCursor,
    pub held: Vec<HeldNote>,
}

fn flatten_held(open: &FxHashMap<(u8, u8), VecDeque<u32>>) -> Vec<HeldNote> {
    let mut held: Vec<HeldNote> = Vec::new();
    for ((channel, key), starts) in open.iter() {
        for &start_tick in starts {
            held.push(HeldNote {
                key: *key,
                start_tick,
                channel: *channel,
            });
        }
    }
    // Canonical order: start tick, then channel, then key. The map's
    // iteration order depends on insertion history, which differs
    // between a from-start walk and a checkpoint-restored one; without
    // the tie-breakers, notes sharing a start tick would come out in
    // different orders from the two walks.
    held.sort_by(|a, b| {
        (a.start_tick, a.channel, a.key).cmp(&(b.start_tick, b.channel, b.key))
    });
    held
}

/// Like `seek_track_cursor_with_held`, but starts from `base` (a
/// checkpoint on the same track whose tick is before `target_tick`)
/// when one is given, and pushes a fresh checkpoint into `deposited`
/// each time the walk crosses a multiple of `HELD_CKPT_INTERVAL`
/// stepped events. The result is identical to the from-start seek:
/// the walk from a checkpoint processes the same events with the
/// same voice state.
///
/// # Safety
/// Same requirements as `seek_track_cursor`. `base` must come from a
/// walk over the same track of the same data.
pub unsafe fn seek_track_cursor_with_held_ckpt(
    data: &KazuMIDIParserData,
    track_idx: u16,
    target_tick: u32,
    max_per_key: usize,
    base: Option<&HeldCheckpoint>,
    deposited: &mut Vec<HeldCheckpoint>,
) -> (TrackCursor, Vec<HeldNote>) {
    let mut open: FxHashMap<(u8, u8), VecDeque<u32>> = FxHashMap::default();
    let (mut cursor, mut count) = match base {
        Some(b) => {
            debug_assert_eq!(b.cursor.track_index, track_idx);
            for n in &b.held {
                open.entry((n.channel, n.key))
                    .or_default()
                    .push_back(n.start_tick);
            }
            (b.cursor.clone(), b.event_count)
        }
        None => match unsafe { init_track_cursor(data, track_idx) } {
            Some(c) => (c, 0),
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
                    },
                    Vec::new(),
                );
            }
        },
    };

    if cursor.p.is_null() {
        return (cursor, Vec::new());
    }

    let mut next_ckpt = (count / HELD_CKPT_INTERVAL + 1) * HELD_CKPT_INTERVAL;
    unsafe {
        while cursor_next(&mut cursor) {
            count += 1;
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
                    let queue = open.entry((channel, key as u8)).or_default();
                    queue.push_back(cursor.abs_tick);
                    if queue.len() > max_per_key {
                        queue.pop_front();
                    }
                } else if msg_type == 0x80 || (msg_type == 0x90 && cursor.param2 == 0) {
                    if let Some(queue) = open.get_mut(&(channel, key as u8)) {
                        queue.pop_front();
                    }
                }
            }
            if count >= next_ckpt {
                deposited.push(HeldCheckpoint {
                    event_count: count,
                    tick: cursor.abs_tick,
                    cursor: cursor.clone(),
                    held: flatten_held(&open),
                });
                next_ckpt += HELD_CKPT_INTERVAL;
            }
        }
        if cursor.abs_tick < target_tick {
            cursor.has_event = false;
        }
    }

    (cursor, flatten_held(&open))
}

/// The SMF default tempo, in microseconds per quarter note (120 BPM).
///
/// The tempo map holds only the 0x51 changes the file actually contains, so it says
/// nothing about the stretch before the first one -- that stretch runs at this, and
/// `tempo_ns[0]` is measured on that basis. A file whose first change sits at tick 0
/// (nearly all of them) never exposes the gap, which is why "2 million de touches",
/// whose first change is at tick 3386, was the one that broke: the searches below
/// clamp to entry 0, and `tick - tempo_tick[0]` then underflows a u32 into ~4.3e9,
/// which turned every tick before 3386 into a time of ~u64::MAX. Playback and the
/// roll both read these, so both went with it.
const DEFAULT_TEMPO_US: u64 = 500_000;

#[inline]
pub fn tick_to_ns(data: &KazuMIDIParserData, tick: u32) -> u64 {
    let division = (data.header.division as u64).max(1);
    // Before the first tempo change (or with no tempo at all), the default rate.
    let default_ns =
        |t: u32| -> u64 { (t as u128 * DEFAULT_TEMPO_US as u128 * 1000 / division as u128) as u64 };
    if data.tempo_count == 0 || data.tempo_tick.is_null() {
        return default_ns(tick);
    }
    unsafe {
        if tick < *data.tempo_tick {
            return default_ns(tick);
        }
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
        // Use u128 to avoid both overflow and truncation errors from integer division.
        // delta_tick can be ~4e9, tempo ~1.6e7, product * 1000 ~ 6.4e19 (> u64::MAX).
        let delta_tick = tick.saturating_sub(base_tick) as u128;
        let ns = delta_tick * (tempo as u128) * 1000 / division as u128;
        base_ns.saturating_add(ns as u64)
    }
}

/// The active tempo in microseconds per quarter note at the given absolute
/// playback time in nanoseconds (e.g. 500_000 = 120 BPM). Before the file's first
/// tempo event the SMF default rate applies, same as `tick_to_ns`/`ns_to_tick`
/// assume for the stretch before `tempo_ns[0]`.
#[inline]
pub fn tempo_us_at_ns(data: &KazuMIDIParserData, ns: u64) -> u32 {
    if data.tempo_count == 0 || data.tempo_ns.is_null() {
        return DEFAULT_TEMPO_US as u32;
    }
    unsafe {
        if ns < *data.tempo_ns {
            return DEFAULT_TEMPO_US as u32;
        }
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
        *data.tempo_val.add(lo)
    }
}

#[inline]
pub fn ns_to_tick(data: &KazuMIDIParserData, ns: u64) -> u32 {
    if ns == 0 {
        return 0;
    }
    let division = (data.header.division as u128).max(1);
    // The inverse of `tick_to_ns`'s default stretch, and it has to cover the same
    // range: `tempo_ns[0]` is where the map starts, and below it `ns - tempo_ns[0]`
    // would underflow exactly as the tick side did.
    let default_tick = |n: u64| -> u32 {
        (n as u128 * division / (DEFAULT_TEMPO_US as u128 * 1000)).min(u32::MAX as u128) as u32
    };
    if data.tempo_count == 0 || data.tempo_ns.is_null() {
        return default_tick(ns);
    }
    unsafe {
        if ns < *data.tempo_ns {
            return default_tick(ns);
        }
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
        let delta_ns = ns.saturating_sub(base_ns);
        // Use u128 to avoid overflow and truncation.
        let delta_tick = (delta_ns as u128 * division / (tempo as u128).saturating_mul(1000).max(1))
            .min(u32::MAX as u128) as u32;
        base_tick.saturating_add(delta_tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kazumidiparser::MIDIPlayerData;

    /// The block index must agree with the plain scan, including
    /// across block boundaries (the song below has 20,000 note-ons,
    /// so it spans many 256-event blocks).
    #[test]
    fn note_on_index_agrees_with_plain_scan() {
        const NOTES: u32 = 20_000;
        let mut track: Vec<u8> = Vec::new();
        for i in 0..NOTES {
            // Note-on at tick i * 10, its note-off 5 ticks later.
            if i == 0 {
                track.extend_from_slice(&[0x00, 0x90, 60, 100]);
            } else {
                track.extend_from_slice(&[0x05, 0x90, 60, 100]);
            }
            track.extend_from_slice(&[0x05, 0x80, 60, 0]);
        }
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let mut file: Vec<u8> = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&6u32.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes());
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(track.len() as u32).to_be_bytes());
        file.extend_from_slice(&track);
        let path = std::env::temp_dir().join("kazumidiplayer_cursor_index_test.mid");
        std::fs::write(&path, &file).unwrap();
        let data = MIDIPlayerData::parse(&path, 1, None, None).unwrap();
        let _ = std::fs::remove_file(&path);
        let raw = unsafe { &*data.as_ptr() };
        let index = unsafe { NoteOnIndex::build(raw) };
        for target in [0, 1, 5, 10, 50_000, 100_005, 199_995, u32::MAX] {
            assert_eq!(
                unsafe { index.count_before(raw, target) },
                unsafe { count_note_ons_before(raw, target) },
                "target {target}"
            );
        }
        assert_eq!(unsafe { index.count_before(raw, u32::MAX) }, NOTES as u64);
    }

    /// Seeking through the block index must land on the same event
    /// as the plain from-start seek, and the streams must agree
    /// afterwards. Same song shape as the count test above (40,000
    /// events, so the targets cross many 256-event blocks).
    #[test]
    fn index_seek_lands_where_plain_seek_lands() {
        const NOTES: u32 = 20_000;
        let mut track: Vec<u8> = Vec::new();
        for i in 0..NOTES {
            if i == 0 {
                track.extend_from_slice(&[0x00, 0x90, 60, 100]);
            } else {
                track.extend_from_slice(&[0x05, 0x90, 60, 100]);
            }
            track.extend_from_slice(&[0x05, 0x80, 60, 0]);
        }
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let mut file: Vec<u8> = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&6u32.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes());
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(track.len() as u32).to_be_bytes());
        file.extend_from_slice(&track);
        let path = std::env::temp_dir().join("kazumidiplayer_cursor_seek_test.mid");
        std::fs::write(&path, &file).unwrap();
        let data = MIDIPlayerData::parse(&path, 1, None, None).unwrap();
        let _ = std::fs::remove_file(&path);
        let raw = unsafe { &*data.as_ptr() };
        let index = unsafe { NoteOnIndex::build(raw) };
        fn landing(c: &TrackCursor) -> (bool, u32, u8, u8, u8, bool) {
            (
                c.has_event,
                c.abs_tick,
                c.status,
                c.param1,
                c.param2,
                c.is_tempo,
            )
        }
        for target in [0, 1, 5, 10, 50_000, 100_005, 199_995, u32::MAX] {
            let walked = unsafe { seek_track_cursor(raw, 0, target) };
            let indexed = unsafe { index.seek_cursor(raw, 0, target) };
            assert_eq!(landing(&walked), landing(&indexed), "target {target}");
            let (mut a, mut b) = (walked, indexed);
            for _ in 0..8 {
                let ea = unsafe { cursor_next(&mut a) };
                let eb = unsafe { cursor_next(&mut b) };
                assert_eq!(ea, eb, "target {target}");
                if !ea {
                    break;
                }
                assert_eq!(landing(&a), landing(&b), "target {target}");
            }
        }
    }

    /// `count_note_ons_before` must count exactly the note-ons the
    /// parser counted, split at the target tick. The velocity-0
    /// note-on at tick 720 is a note-off in disguise and never counts.
    #[test]
    fn count_note_ons_before_splits_at_target_tick() {
        let track: &[u8] = &[
            0x00, 0x90, 60, 100, // tick 0: note-on 60
            0x83, 0x60, 0x80, 60, 0, // tick 480: note-off 60
            0x81, 0x70, 0x90, 62, 0, // tick 720: note-on 62, velocity 0
            0x81, 0x70, 0x90, 64, 100, // tick 960: note-on 64
            0x00, 0xFF, 0x2F, 0x00, // end of track
        ];
        let mut file: Vec<u8> = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&6u32.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&480u16.to_be_bytes());
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(track.len() as u32).to_be_bytes());
        file.extend_from_slice(track);
        let path = std::env::temp_dir().join("kazumidiplayer_cursor_count_test.mid");
        std::fs::write(&path, &file).unwrap();
        let data = MIDIPlayerData::parse(&path, 1, None, None).unwrap();
        let _ = std::fs::remove_file(&path);
        let raw = unsafe { &*data.as_ptr() };
        assert_eq!(data.total_note_on_events(), 2);
        assert_eq!(unsafe { count_note_ons_before(raw, 0) }, 0);
        assert_eq!(unsafe { count_note_ons_before(raw, 1) }, 1);
        assert_eq!(unsafe { count_note_ons_before(raw, 960) }, 1);
        assert_eq!(unsafe { count_note_ons_before(raw, 961) }, 2);
        assert_eq!(unsafe { count_note_ons_before(raw, u32::MAX) }, 2);
    }

    /// Walking every track with `cursor_next` from the start must agree with
    /// the parser's own counts for every file in `KZMP_TEST_MIDIS` (a
    /// directory). Skipped when it is unset.
    #[test]
    fn cursor_matches_parser_counts() {
        let Ok(dir) = std::env::var("KZMP_TEST_MIDIS") else {
            return;
        };
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let Ok(midi) = MIDIPlayerData::parse(&path, 0, None, None) else {
                continue;
            };
            let data = unsafe { &*midi.as_ptr() };
            let (mut channel, mut note_on) = (0u64, 0u64);
            for t in 0..data.header.tracks {
                let mut c = unsafe { init_track_cursor(data, t) }.unwrap();
                while unsafe { cursor_next(&mut c) } {
                    if c.is_tempo {
                        continue;
                    }
                    channel += 1;
                    if c.status & 0xF0 == 0x90 && c.param2 > 0 {
                        note_on += 1;
                    }
                }
            }
            println!(
                "{}: {note_on} / {}",
                path.display(),
                midi.total_note_on_events()
            );
            assert_eq!(channel, midi.total_channel_events(), "{}", path.display());
            assert_eq!(note_on, midi.total_note_on_events(), "{}", path.display());
        }
    }

    /// Note-ons `cursor_next` sees in a one-track file with these event bytes.
    fn note_ons(events: &[u8]) -> (u64, u64) {
        let mut file = b"MThd       `MTrk".to_vec();
        file.extend_from_slice(&(events.len() as u32).to_be_bytes());
        file.extend_from_slice(events);
        let path = std::env::temp_dir().join(format!("kzmp_cursor_{}.mid", events.len()));
        std::fs::write(&path, &file).unwrap();
        let midi = MIDIPlayerData::parse(&path, 1, None, None).unwrap();
        let data = unsafe { &*midi.as_ptr() };
        let mut c = unsafe { init_track_cursor(data, 0) }.unwrap();
        let mut n = 0;
        while unsafe { cursor_next(&mut c) } {
            if c.status & 0xF0 == 0x90 && c.param2 > 0 {
                n += 1;
            }
        }
        let expected = midi.total_note_on_events();
        drop(midi);
        let _ = std::fs::remove_file(&path);
        (n, expected)
    }

    #[test]
    fn running_status_survives_meta_and_sysex() {
        let events = [
            0x00, 0x90, 0x3C, 0x40, // note on
            0x00, 0xFF, 0x01, 0x01, b'x', // text meta
            0x00, 0x3E, 0x40, // note on, running status
            0x00, 0xF0, 0x01, 0xF7, // SysEx
            0x00, 0x40, 0x40, // note on, running status
            0x00, 0xFF, 0x2F, 0x00,
        ];
        assert_eq!(note_ons(&events), (3, 3));
    }

    #[test]
    fn stops_at_end_of_track() {
        let events = [
            0x00, 0x90, 0x3C, 0x40, // note on
            0x00, 0xFF, 0x2F, 0x00, // end of track
            0x00, 0x90, 0x3E, 0x40, // padding that reads as a note
        ];
        assert_eq!(note_ons(&events), (1, 1));
    }

    #[test]
    fn oversized_meta_stops_inside_chunk() {
        // Tempo meta claiming 0x7F bytes in a chunk that has 3.
        let events = [0x00, 0xFF, 0x51, 0x7F, 0x07, 0xA1, 0x20];
        let mut file = b"MThd       `MTrk".to_vec();
        file.extend_from_slice(&(events.len() as u32).to_be_bytes());
        file.extend_from_slice(&events);
        let path = std::env::temp_dir().join("kzmp_cursor_oversized.mid");
        std::fs::write(&path, &file).unwrap();
        let midi = MIDIPlayerData::parse(&path, 1, None, None).unwrap();
        let data = unsafe { &*midi.as_ptr() };
        let mut c = unsafe { init_track_cursor(data, 0) }.unwrap();
        assert!(!unsafe { cursor_next(&mut c) });
        assert!(c.p <= c.end);
        drop(midi);
        let _ = std::fs::remove_file(&path);
    }

    /// Checkpointed seeks must reproduce from-start seeks exactly (same
    /// held notes in the same order, same final cursor position), for
    /// every file listed in `KZMP_CKPT_FILES` (semicolon-separated).
    /// Skipped when it is unset.
    #[test]
    fn ckpt_seek_matches_plain_seek() {
        let Ok(list) = std::env::var("KZMP_CKPT_FILES") else {
            return;
        };
        for path in list.split(';') {
            let Ok(midi) = MIDIPlayerData::parse(std::path::Path::new(path), 0, None, None) else {
                continue;
            };
            let data = unsafe { &*midi.as_ptr() };
            for t in 0..data.header.tracks {
                let mut deposited = Vec::new();
                let (end_cursor, _) = unsafe {
                    seek_track_cursor_with_held_ckpt(data, t, u32::MAX, 128, None, &mut deposited)
                };
                let end_tick = end_cursor.abs_tick;
                let mut targets = vec![
                    0,
                    1,
                    end_tick / 4,
                    end_tick / 2,
                    (end_tick / 4) * 3,
                    end_tick,
                ];
                targets.sort_unstable();
                targets.dedup();
                for target in targets {
                    let (c0, h0) = unsafe { seek_track_cursor_with_held(data, t, target, 128) };
                    let base = deposited.iter().filter(|c| c.tick < target).last();
                    let mut scratch = Vec::new();
                    let (c1, h1) = unsafe {
                        seek_track_cursor_with_held_ckpt(data, t, target, 128, base, &mut scratch)
                    };
                    assert_eq!(h0.len(), h1.len(), "{path} track {t} target {target} held count");
                    for (a, b) in h0.iter().zip(h1.iter()) {
                        assert!(
                            a.key == b.key && a.start_tick == b.start_tick && a.channel == b.channel,
                            "{path} track {t} target {target} held note"
                        );
                    }
                    assert_eq!(c0.abs_tick, c1.abs_tick, "{path} track {t} target {target} tick");
                    assert_eq!(c0.has_event, c1.has_event, "{path} track {t} target {target} has_event");
                    assert_eq!(c0.p as usize, c1.p as usize, "{path} track {t} target {target} pos");
                }
            }
            println!("ckpt seek equivalence ok: {path}");
        }
    }
}
