
const NO_TRACK: u16 = u16::MAX;

use crate::kazumidiparser_cursor::{cursor_next, init_track_cursor, tick_to_ns};
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
pub struct TimedNote {
    /// Chunk-relative start time in microseconds
    pub start_rel_us: u32,
    /// Chunk-relative end time in microseconds
    pub end_rel_us: u32,
    pub note: u8,
    pub channel: u8,
    pub track_index: u16,
}

pub fn filter_overlapping_notes(notes: &mut Vec<TimedNote>) {
    if notes.is_empty() {
        return;
    }
    notes.sort_unstable_by(|a, b| {
        (a.track_index, a.channel, a.note, a.start_rel_us).cmp(&(
            b.track_index,
            b.channel,
            b.note,
            b.start_rel_us,
        ))
    });
    let mut last_key = (NO_TRACK, 0u8, 0u8);
    let mut last_end_rel_us = 0u32;
    let mut first = true;
    notes.retain(|note| {
        let key = (note.track_index, note.channel, note.note);
        if first || key != last_key {
            first = false;
            last_key = key;
            last_end_rel_us = note.end_rel_us;
            true
        } else {
            if note.start_rel_us >= last_end_rel_us {
                last_end_rel_us = note.end_rel_us;
                true
            } else {
                false
            }
        }
    });
}

// ─── Real-time streaming extractor (no pre-built chunked_notes) ───────────

use crate::kazumidiparser_cursor::TrackCursor;

struct TrackExtractorState {
    cursor: Option<TrackCursor>,
}

pub struct NoteExtractor {
    active_notes: FxHashMap<u64, (u64, u8, u16)>,
    track_states: Vec<TrackExtractorState>,
    last_end_ns: u64,
}

impl NoteExtractor {
    pub fn new() -> Self {
        Self {
            // Start with a modest capacity; the map grows as needed.
            // 8192 was always allocated even for tiny files.
            active_notes: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            track_states: Vec::new(),
            last_end_ns: 0,
        }
    }

    pub fn reset_with_tracks(&mut self, track_count: usize) {
        // Shrink the map if it grew very large, to reclaim memory between songs.
        if self.active_notes.capacity() > 1024 {
            self.active_notes = FxHashMap::with_capacity_and_hasher(256, Default::default());
        } else {
            self.active_notes.clear();
        }
        self.track_states.clear();
        self.track_states.resize_with(track_count, || TrackExtractorState { cursor: None });
        self.last_end_ns = 0;
    }

    pub fn extract_notes_in_range(
        &mut self,
        data: &kazumidiparser::KazuMIDIParserData,
        start_ns: u64,
        end_ns: u64,
        out_notes: &mut Vec<TimedNote>,
    ) {
        // Reset if this is the first chunk or if we're jumping to a non-contiguous chunk.
        // This prevents cursor state corruption when workers pick arbitrary chunks.
        let needs_rewind = start_ns == 0 || start_ns != self.last_end_ns;
        if needs_rewind {
            self.reset_with_tracks(data.header.tracks as usize);
        }

        out_notes.clear();
        let has_track_indices = data.header.tracks > 1;
        let num_tracks = data.header.tracks as usize;

        // Safety: data is a valid reference from Arc<MIDIPlayerData>; the C struct is fully initialized.
        // Track cursors are initialized by init_track_cursor and iterated via cursor_next.
        unsafe {
            for track_idx in 0..num_tracks {
                let state = &mut self.track_states[track_idx];
                if state.cursor.is_none() {
                    if let Some(cursor) = init_track_cursor(data, track_idx as u16) {
                        state.cursor = Some(cursor);
                    } else {
                        continue;
                    }
                }

                let cursor = state.cursor.as_mut().unwrap();

                // Pre-scan: when jumping to a non-contiguous chunk, rebuild active_notes
                // by replaying events from the track start up to start_ns.
                let mut cursor_at_start_event = false;
                if needs_rewind && start_ns > 0 {
                    while cursor.p < cursor.end {
                        if !cursor_next(cursor) {
                            break;
                        }
                        let time_ns = tick_to_ns(data, cursor.abs_tick);

                        let status = cursor.status;
                        let msg_type = status & 0xF0;
                        let note_val = cursor.param1;
                        let velocity = cursor.param2;
                        let channel = status & 0x0F;
                        let t_idx = cursor.track_index;
                        let slot_key = ((t_idx as u64) << 12)
                            | ((channel as u64) << 8)
                            | (note_val as u64);

                        if time_ns < start_ns {
                            if msg_type == 0x90 && velocity > 0 {
                                self.active_notes.insert(slot_key, (time_ns, channel, t_idx));
                            } else if msg_type == 0x80 || (msg_type == 0x90 && velocity == 0) {
                                self.active_notes.remove(&slot_key);
                            }
                        } else {
                            // cursor now points to the first event at or after start_ns.
                            // The main loop below must process this event without calling
                            // cursor_next first.
                            cursor_at_start_event = true;
                            break;
                        }
                    }
                }

                while cursor.p < cursor.end {
                    if !cursor_at_start_event {
                        if !cursor_next(cursor) {
                            break;
                        }
                    }
                    cursor_at_start_event = false;

                    let time_ns = tick_to_ns(data, cursor.abs_tick);

                    // Skip events past the chunk end first
                    if time_ns >= end_ns {
                        break;
                    }

                    // Events before this chunk must not affect active_notes,
                    // but cursor_next already advanced running_status etc.
                    if time_ns < start_ns {
                        continue;
                    }

                    let status = cursor.status;
                    let msg_type = status & 0xF0;
                    let note_val = cursor.param1;
                    let velocity = cursor.param2;
                    let channel = status & 0x0F;
                    let t_idx = cursor.track_index;
                    let slot_key = ((t_idx as u64) << 12)
                        | ((channel as u64) << 8)
                        | (note_val as u64);

                    if msg_type == 0x90 && velocity > 0 {
                        if let Some((prev_start, ch, _trk)) =
                            self.active_notes.insert(slot_key, (time_ns, channel, t_idx))
                        {
                            out_notes.push(TimedNote {
                                start_rel_us: ((prev_start.saturating_sub(start_ns)) / 1000) as u32,
                                end_rel_us: ((time_ns.saturating_sub(start_ns)) / 1000) as u32,
                                note: note_val,
                                channel: ch,
                                track_index: if has_track_indices { t_idx } else { u16::MAX },
                            });
                        }
                    } else if msg_type == 0x80 || (msg_type == 0x90 && velocity == 0) {
                        if let Some((note_start, ch, _trk)) =
                            self.active_notes.remove(&slot_key)
                        {
                            out_notes.push(TimedNote {
                                start_rel_us: ((note_start.saturating_sub(start_ns)) / 1000) as u32,
                                end_rel_us: ((time_ns.saturating_sub(start_ns)) / 1000) as u32,
                                note: note_val,
                                channel: ch,
                                track_index: if has_track_indices { t_idx } else { u16::MAX },
                            });
                        }
                    }
                }
            }

            // Notes still active at chunk end
            for (&slot_key, &(note_start, channel, t_idx)) in &self.active_notes {
                if note_start < end_ns {
                    let note_val = (slot_key & 0xFF) as u8;
                    out_notes.push(TimedNote {
                        start_rel_us: ((note_start.saturating_sub(start_ns)) / 1000) as u32,
                        end_rel_us: ((end_ns.saturating_sub(start_ns)) / 1000) as u32,
                        note: note_val,
                        channel,
                        track_index: if has_track_indices { t_idx } else { u16::MAX },
                    });
                }
            }
        }

        filter_overlapping_notes(out_notes);
        self.last_end_ns = end_ns;
    }
}
