//! An index of the notes a seek cannot see on its own.
//!
//! Seeking a track jumps to the last keyframe at or before the target and walks from
//! there, tracking note-ons and note-offs to learn what is sounding. A note that
//! started *before* that keyframe is invisible to such a walk: its note-on was never
//! read. Those notes then vanish from the piano roll after a seek.
//!
//! A note is missable exactly when it spans a keyframe (`start < keyframe <= end`),
//! since the keyframe a seek lands on always sits at or before the target. So indexing
//! only the notes that span a keyframe is enough to close the hole completely.
//!
//! Those notes are not rare. It is tempting to reason that they are the ones longer
//! than the gap between keyframes, and that a dense MIDI is overwhelmingly short
//! notes, so they must be few. That is backwards. Keyframes are placed every 4096
//! *events*, not every N ticks, so the denser the track the closer together they
//! land: on a real 4393-track file, one track had 4850 keyframes over 828k ticks
//! (~171 ticks apart, shorter than its notes) and 82% of its notes -- 8.1M of 9.9M --
//! spanned one. Density does not make this index small, it makes it bigger. Hence
//! `MAX_INDEX_NOTES`, enforced while each track is walked rather than after it.
//!
//! Building the index means reading the whole file once, which takes seconds on a
//! large MIDI. It runs in the background, split across tracks, and seeks keep their
//! old (lossy) behaviour until it lands.

use kazumidiparser::MIDIPlayerData;
use rustc_hash::FxHashMap;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
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
/// cost hundreds of megabytes, more than the piano roll itself is allowed. Past
/// this the longest notes are kept and the rest dropped: length is what makes a note
/// both visible (it reaches furthest up the roll) and worth recovering, and the ones
/// dropped are simply missing, which is what they were before this index existed.
///
/// It also caps what a single track's walk may hold, so a dense track stages 2M notes
/// rather than the 8.1M it really has. That bound costs nothing: no track is ever
/// allowed to keep more than the whole index anyway.
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
                    // The budget is the whole index's, not a share of it: no track can
                    // ever keep more than that anyway, so capping the walk here throws
                    // away nothing the split below could have kept, while still
                    // bounding what one track stages to 2M notes instead of the 8.1M a
                    // dense track really has.
                    out.push((ti, index_track(&data, ti as u16, end_tick, MAX_INDEX_NOTES)));
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

    // Only spend the budget if it is actually exceeded: a file that fits keeps every
    // note it found.
    let total: usize = by_track.iter().map(|t| t.len()).sum();
    if total > MAX_INDEX_NOTES {
        // Water-fill: each track gets what it asks for if that fits an equal share of
        // what is still unspent, and the tracks that want more divide the remainder.
        // A flat even split hands thousands of empty tracks a share they cannot use
        // while cutting the few dense ones to the same figure: on a 4393-track file
        // that is 455 notes each, including for the track with 8.1M to offer. Going
        // in ascending order of demand is what lets the unused shares fall through to
        // the tracks that can spend them.
        let mut order: Vec<usize> = (0..by_track.len()).collect();
        order.sort_unstable_by_key(|&i| by_track[i].len());

        let mut cap = vec![0usize; by_track.len()];
        let mut remaining = MAX_INDEX_NOTES;
        let mut unfilled = order.len();
        for &i in &order {
            let share = remaining / unfilled.max(1);
            let take = by_track[i].len().min(share);
            cap[i] = take;
            remaining -= take;
            unfilled -= 1;
        }

        // Within a track keep the longest notes. A short spanning note is one that
        // barely crosses a keyframe, so it is only ever missing from a narrow band of
        // seek positions, and it is also the one that covers the least of the roll.
        for (notes, &keep) in by_track.iter_mut().zip(cap.iter()) {
            if notes.len() > keep {
                notes.select_nth_unstable_by_key(keep, |n| {
                    Reverse(n.end_tick.saturating_sub(n.start_tick))
                });
                notes.truncate(keep);
                notes.sort_by_key(|n| n.start_tick);
            }
        }
    }

    Some(NoteIndex { by_track })
}

/// Walk one track start to end, keeping the longest `budget` notes that span a
/// keyframe.
fn index_track(
    data: &Arc<MIDIPlayerData>,
    track: u16,
    end_tick: u32,
    budget: usize,
) -> Vec<SpanningNote> {
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

    // Open notes per (channel, key), oldest first. Note-ons stack on a key rather than
    // replacing each other, and a note-off closes the oldest note on its channel: same
    // rule the piano-roll scan follows.
    //
    // A queue rather than a list, so a note-off closes the oldest in O(1). This was a
    // `Vec` searched with `position` and cut with `remove`, which is a scan plus a
    // shift of the tail on every single note-off: on a track that leaves tens of
    // thousands of notes ringing on one key that is quadratic, and it is what stopped
    // this build from ever finishing. `seek_track_cursor_with_held` keeps a queue for
    // exactly this reason.
    let mut open: FxHashMap<(u8, u8), VecDeque<u32>> = FxHashMap::default();

    // The longest spanning notes seen so far, shortest at the top so it is the one
    // evicted once the track is over budget: (duration, start_tick, key, channel).
    let mut longest: BinaryHeap<Reverse<(u32, u32, u8, u8)>> = BinaryHeap::new();

    // Does a keyframe fall in (start, end]? That is exactly when a seek landing on it
    // would start its walk after this note began, and so never see the note-on.
    let spans_keyframe = |start: u32, end: u32| -> bool {
        let i = keyframes.partition_point(|&k| k <= start);
        keyframes.get(i).is_some_and(|&k| k <= end)
    };

    // Offer a closed note to the index, keeping it only if it spans a keyframe and is
    // long enough to earn one of the track's slots.
    let keep = |start: u32,
                end: u32,
                key: u8,
                channel: u8,
                longest: &mut BinaryHeap<Reverse<(u32, u32, u8, u8)>>| {
        if !spans_keyframe(start, end) {
            return;
        }
        longest.push(Reverse((end.saturating_sub(start), start, key, channel)));
        if longest.len() > budget {
            longest.pop();
        }
    };

    while unsafe { cursor_next(&mut cursor) } {
        if cursor.is_tempo || cursor.abs_tick > end_tick {
            continue;
        }
        let msg_type = cursor.status & 0xF0;
        let key = cursor.param1;
        if key >= 128 {
            continue;
        }
        let channel = cursor.status & 0x0F;
        let tick = cursor.abs_tick;

        if msg_type == 0x90 && cursor.param2 > 0 {
            open.entry((channel, key)).or_default().push_back(tick);
        } else if msg_type == 0x80 || (msg_type == 0x90 && cursor.param2 == 0) {
            // Close the oldest note still open on this channel and key. A note-off
            // carries no way to say which of several stacked notes it ends, so
            // oldest-first is the convention.
            if let Some(start) = open.get_mut(&(channel, key)).and_then(|q| q.pop_front()) {
                keep(start, tick, key, channel, &mut longest);
            }
        }
    }

    // Notes never closed run to the end of the MIDI, as the roll draws them.
    for (&(channel, key), starts) in open.iter() {
        for &start in starts {
            keep(start, end_tick, key, channel, &mut longest);
        }
    }

    let mut spanning: Vec<SpanningNote> = longest
        .into_iter()
        .map(|Reverse((duration, start_tick, key, channel))| SpanningNote {
            start_tick,
            end_tick: start_tick.saturating_add(duration),
            key,
            channel,
        })
        .collect();
    spanning.sort_by_key(|n| n.start_tick);
    spanning
}
