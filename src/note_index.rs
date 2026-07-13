//! An index of the notes a seek cannot see on its own.
//!
//! Seeking a track jumps to the last keyframe at or before the target and walks from
//! there, tracking note-ons and note-offs to learn what is sounding. A note that
//! started *before* that keyframe is invisible to such a walk: its note-on was never
//! read. Those notes then vanish from the piano roll after a seek.
//!
//! A note is missable exactly when it spans a keyframe (`start < keyframe <= end`),
//! since the keyframe a seek lands on always sits at or before the target. So indexing
//! only the notes that span a keyframe is enough to close the hole completely, and
//! those are rare: they are the notes longer than the gap between keyframes, while a
//! dense MIDI is overwhelmingly short ones.
//!
//! Building the index means reading the whole file once, which takes seconds on a
//! large MIDI. It runs in the background, split across tracks, and seeks keep their
//! old (lossy) behaviour until it lands.

use kazumidiparser::MIDIPlayerData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

use crate::kazumidiparser_cursor::{cursor_next, init_track_cursor, keyframe_ticks};

/// A note that spans at least one keyframe, and so has to be remembered explicitly.
#[derive(Clone, Copy)]
pub struct SpanningNote {
    pub start_tick: u32,
    pub end_tick: u32,
    pub key: u8,
    pub channel: u8,
}

/// How many spanning notes the index may keep, across all tracks. Some MIDIs have
/// tens of millions of them (they sustain everything), and holding every one would
/// cost hundreds of megabytes -- more than the piano roll itself is allowed. Past
/// this the longest notes are kept and the rest dropped: length is what makes a note
/// both visible (it reaches furthest up the roll) and worth recovering, and the ones
/// dropped are simply missing, which is what they were before this index existed.
const MAX_INDEX_NOTES: usize = 2_000_000;

/// Per track, the spanning notes in start-tick order.
pub struct NoteIndex {
    by_track: Vec<Vec<SpanningNote>>,
}

impl NoteIndex {
    /// The notes sounding at `target_tick` that a walk starting at `keyframe_tick`
    /// would miss: those that began before that keyframe and have not ended yet.
    ///
    /// Notes starting at or after the keyframe are deliberately excluded. The walk
    /// reads their note-ons itself, and reporting them here as well would draw every
    /// one of them twice.
    pub fn missed_at(&self, track: u16, keyframe_tick: u32, target_tick: u32) -> Vec<SpanningNote> {
        let Some(notes) = self.by_track.get(track as usize) else {
            return Vec::new();
        };

        // Notes are start-sorted, so everything that could have begun before the
        // keyframe is a prefix; the ends within it are in no particular order, hence
        // the scan.
        let cut = notes.partition_point(|n| n.start_tick < keyframe_tick);
        notes[..cut]
            .iter()
            .filter(|n| n.end_tick > target_tick)
            .copied()
            .collect()
    }
}

pub struct NoteIndexWorker {
    rx_done: Receiver<(usize, Arc<NoteIndex>)>,
    /// Cancels the build in flight. A new file supersedes the old one's index, which
    /// would otherwise keep several threads busy reading a MIDI nobody is playing.
    cancel: Arc<AtomicBool>,
    /// Identifies which build a finished index belongs to, so a result that arrives
    /// after the file has changed again can be dropped.
    generation: usize,
}

impl NoteIndexWorker {
    pub fn new() -> Self {
        let (_tx, rx_done) = mpsc::channel();
        Self {
            rx_done,
            cancel: Arc::new(AtomicBool::new(false)),
            generation: 0,
        }
    }

    /// Start indexing `data`, cancelling any build already running.
    pub fn start(&mut self, data: Arc<MIDIPlayerData>, threads: u32) {
        self.cancel.store(true, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel();
        self.rx_done = rx;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        let generation = self.generation;
        let cancel = self.cancel.clone();

        thread::spawn(move || {
            if let Some(index) = build(&data, threads.max(1), &cancel) {
                let _ = tx.send((generation, Arc::new(index)));
            }
        });
    }

    /// The finished index, once it is ready. Returns it only for the build that is
    /// still current, and only once.
    pub fn take_ready(&self) -> Option<Arc<NoteIndex>> {
        let (generation, index) = self.rx_done.try_recv().ok()?;
        (generation == self.generation).then_some(index)
    }
}

/// Read every track and collect the notes that span a keyframe. Tracks are independent
/// (a note-off only ever closes a note in its own track), so they split cleanly across
/// threads.
fn build(data: &Arc<MIDIPlayerData>, threads: u32, cancel: &AtomicBool) -> Option<NoteIndex> {
    // Safety: the Arc keeps the C MIDI struct alive for this call.
    let parser = unsafe { &*data.as_ptr() };
    let num_tracks = parser.header.tracks as usize;
    let end_tick = parser.max_tick;

    let mut by_track: Vec<Vec<SpanningNote>> = vec![Vec::new(); num_tracks];

    let worker_count = (threads as usize).min(num_tracks.max(1));
    let results = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for w in 0..worker_count {
            let data = data.clone();
            handles.push(scope.spawn(move || {
                let mut out: Vec<(usize, Vec<SpanningNote>)> = Vec::new();
                // Stride rather than block: track lengths vary wildly, and interleaving
                // keeps one thread from drawing all the long ones.
                let mut ti = w;
                while ti < num_tracks {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    out.push((ti, index_track(&data, ti as u16, end_tick)));
                    ti += worker_count;
                }
                Some(out)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(None))
            .collect::<Option<Vec<_>>>()
    })?;

    for chunk in results {
        for (ti, notes) in chunk {
            by_track[ti] = notes;
        }
    }

    let total: usize = by_track.iter().map(|t| t.len()).sum();
    if total > MAX_INDEX_NOTES {
        // Spend the budget evenly across tracks, and within a track keep the longest
        // notes. A short spanning note is one that barely crosses a keyframe, so it is
        // only ever missing from a narrow band of seek positions, and it is also the
        // one that covers the least of the roll.
        let per_track = (MAX_INDEX_NOTES / num_tracks.max(1)).max(64);
        for notes in by_track.iter_mut() {
            if notes.len() > per_track {
                notes.select_nth_unstable_by_key(per_track, |n| {
                    std::cmp::Reverse(n.end_tick.saturating_sub(n.start_tick))
                });
                notes.truncate(per_track);
                notes.sort_by_key(|n| n.start_tick);
            }
        }
    }

    Some(NoteIndex { by_track })
}

/// Walk one track start to end, emitting the notes that span a keyframe.
fn index_track(data: &Arc<MIDIPlayerData>, track: u16, end_tick: u32) -> Vec<SpanningNote> {
    // Safety: the Arc keeps the C MIDI struct alive for this call, and `track` is
    // within the track count the caller read from the same struct.
    let parser = unsafe { &*data.as_ptr() };
    let keyframes = unsafe { keyframe_ticks(parser, track) };
    if keyframes.is_empty() {
        // No keyframe means a seek walks this track from its start and misses nothing.
        return Vec::new();
    }

    let Some(mut cursor) = (unsafe { init_track_cursor(parser, track) }) else {
        return Vec::new();
    };

    // Open notes per key, in note-on order, each carrying the channel that opened it.
    // Note-ons stack on a key rather than replacing each other, and a note-off closes
    // the oldest note on its channel: same rule the piano-roll scan follows.
    let mut open: [Vec<(u32, u8)>; 128] = std::array::from_fn(|_| Vec::new());
    let mut spanning: Vec<SpanningNote> = Vec::new();

    // Does a keyframe fall in (start, end]? That is exactly when a seek landing on it
    // would start its walk after this note began, and so never see the note-on.
    let spans_keyframe = |start: u32, end: u32| -> bool {
        let i = keyframes.partition_point(|&k| k <= start);
        keyframes.get(i).is_some_and(|&k| k <= end)
    };

    while unsafe { cursor_next(&mut cursor) } {
        if cursor.is_tempo || cursor.abs_tick > end_tick {
            continue;
        }
        let msg_type = cursor.status & 0xF0;
        let key = cursor.param1 as usize;
        if key >= 128 {
            continue;
        }
        let channel = cursor.status & 0x0F;
        let tick = cursor.abs_tick;

        if msg_type == 0x90 && cursor.param2 > 0 {
            open[key].push((tick, channel));
        } else if msg_type == 0x80 || (msg_type == 0x90 && cursor.param2 == 0) {
            if let Some(pos) = open[key].iter().position(|&(_, ch)| ch == channel) {
                let (start, ch) = open[key].remove(pos);
                if spans_keyframe(start, tick) {
                    spanning.push(SpanningNote {
                        start_tick: start,
                        end_tick: tick,
                        key: key as u8,
                        channel: ch,
                    });
                }
            }
        }
    }

    // Notes never closed run to the end of the MIDI, as the roll draws them.
    for (key, notes) in open.iter().enumerate() {
        for &(start, ch) in notes {
            if spans_keyframe(start, end_tick) {
                spanning.push(SpanningNote {
                    start_tick: start,
                    end_tick: end_tick,
                    key: key as u8,
                    channel: ch,
                });
            }
        }
    }

    spanning.sort_by_key(|n| n.start_tick);
    spanning
}
