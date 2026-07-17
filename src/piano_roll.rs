use kazumidiparser::{KazuMIDIParserData, MIDIPlayerData};
use raylib::prelude::*;
use rustc_hash::FxHashMap;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::kazumidiparser_cursor::{
    cursor_next, keyframe_tick_for, ns_to_tick, seek_track_cursor_with_held, TrackCursor,
};
use crate::note_index::NoteIndex;
use crate::ui_components::piano_keyboard::get_keyboard_note_regions;
use crate::utils::get_note_color_by_index;

/// How much of the future is visible, when the vertical axis is time based.
const VISIBLE_SPAN_NS: u64 = 3_000_000_000; // 3 seconds
/// How much of the future is visible, when the vertical axis is tick based.
/// Expressed in quarter notes (multiplied by `division` at runtime).
const VISIBLE_SPAN_QUARTERS: u32 = 8;
/// Reference roll height the visible spans above are tuned for. The visible span
/// scales with `height / REFERENCE_HEIGHT`, so pixels-per-time stays constant and
/// resizing the window reveals more/less future instead of stretching notes.
const REFERENCE_HEIGHT: f32 = 150.0;

/// Floor for `note_speed`. Not a limit on how far you can zoom out so much as a
/// guard for the arithmetic: the speed divides the visible span, and the zoom step
/// multiplies it, so a speed of exactly zero would both blow up the span and be
/// impossible to zoom back out of (0 * anything == 0).
pub const MIN_NOTE_SPEED: f32 = 0.0001;

/// Ceiling for `note_speed`. The speed divides the visible span, so this is how far
/// you can zoom in: at 128 a single note of a dense stretch is tens of pixels tall,
/// enough to read one note apart from the next.
pub const MAX_NOTE_SPEED: f32 = 128.0;

/// `end_tick` sentinel for a note whose note-on has been scanned but whose
/// note-off has not yet been seen. Such a note is drawn growing from its start up
/// to the current reveal frontier, and its real end is patched in once the
/// note-off is found.
const OPEN_END: u32 = u32::MAX;

/// A single note rectangle in tick space (resolved to pixels at draw time).
/// `color` is the palette index for the per-voice color (see
/// `utils::note_color_index`). `end_tick == OPEN_END` means the note is still being
/// held (note-off not scanned yet).
///
/// Packed to 10 bytes (vs 12 with the full track/channel + a redundant note byte)
/// to keep dense-MIDI buckets small; the key is the bucket index, so it isn't
/// stored per note. Fields are only ever read/written by value, never by
/// reference, which is sound for `repr(packed)`.
///
/// `color` needs both its bytes, and shrinking it back to one costs more than the
/// byte is worth: a one-byte index runs out after sixteen tracks and paints the rest
/// in an earlier track's color. Only a window around the playhead is resident, so
/// the byte does not scale with the file.
#[derive(Clone, Copy)]
#[repr(packed)]
pub struct RollNote {
    pub start_tick: u32,
    pub end_tick: u32,
    pub color: u16,
}

/// A sliding window of piano-roll draw data, filled in by the worker as playback
/// approaches and drained from behind as playback passes. Notes are bucketed per
/// key and, within a bucket, kept in start-tick order (the scan appends in global
/// tick order, so this holds naturally). `scan_frontier_tick` marks how far ahead
/// the scan has reached; `window_start_tick` is where this generation's scan began.
///
/// Only the window is ever resident, so memory is bounded by how much of the roll
/// is on screen, not by the length or note count of the MIDI.
pub struct RollChunk {
    pub generation: u64,
    pub scan_frontier_tick: u32,
    pub window_start_tick: u32,
    pub done: bool,
    /// Per-key note queues, in start-tick order. A note may have `end_tick ==
    /// OPEN_END` while it is still held; its end is patched in on note-off. Notes
    /// that playback has passed are popped off the front by `retire`.
    pub notes_by_key: [VecDeque<RollNote>; 128],
    /// Per-key count of notes popped off the front so far. The scan addresses notes
    /// by an absolute index (total ever appended to that key), so subtracting this
    /// maps an absolute index to the current queue position.
    retired: [u64; 128],
}

impl Default for RollChunk {
    fn default() -> Self {
        Self {
            generation: 0,
            scan_frontier_tick: 0,
            window_start_tick: 0,
            done: false,
            notes_by_key: std::array::from_fn(|_| VecDeque::new()),
            retired: [0; 128],
        }
    }
}

struct RollRequest {
    data: Arc<MIDIPlayerData>,
    start_tick: u32,
    /// The visible top tick at request time. The scan publishes everything up to
    /// here in one shot first (so a seek shows the visible window instantly), then
    /// streams the rest above it.
    priority_tick: u32,
    generation: u64,
    /// Which chunk slot to fill. It is the one the draw is *not* reading; the scan
    /// flips `live` to it once the visible range is complete.
    build_slot: usize,
    /// Absent until the background index is built. Without it the scan cannot see
    /// notes that began before the keyframe it seeks from, and they go missing.
    index: Option<Arc<NoteIndex>>,
}

/// Compute the topmost visible tick (the future point shown at the top edge),
/// used to decide how much to generate up-front for an instant seek.
pub fn future_tick(
    data: &KazuMIDIParserData,
    playback_position_ns: u64,
    is_tick_based: bool,
    note_speed: f32,
    height: f32,
) -> u32 {
    let speed = note_speed.max(MIN_NOTE_SPEED);
    let height_scale = (height / REFERENCE_HEIGHT).max(0.01);
    if is_tick_based {
        let division = (data.header.division as u32).max(1);
        let span = (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
        let current = ns_to_tick(data, playback_position_ns);
        current.saturating_add(span)
    } else {
        let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
        ns_to_tick(data, playback_position_ns.saturating_add(span_ns))
    }
}

/// Generates piano-roll draw data on a single reused worker thread. Each
/// `regenerate` call cancels any in-flight scan (via the generation counter) and
/// hands the thread a fresh request to stream a window forward from the current
/// tick, publishing progress incrementally.
///
/// The chunks are double-buffered: a rebuild fills the *other* slot while the draw
/// keeps reading the current one, and the two are swapped only once the new
/// window's visible range is complete. Rebuilding in place would blank the roll
/// until the scan caught up, which on a dense MIDI is plainly visible.
pub struct PianoRollWorker {
    tx: Option<Sender<RollRequest>>,
    handle: Option<JoinHandle<()>>,
    chunks: [Arc<Mutex<RollChunk>>; 2],
    /// Which of `chunks` the draw reads. The scan fills the other one and flips
    /// this when the new window is ready to show.
    live: Arc<AtomicUsize>,
    /// Bumped on every `regenerate`; the running scan compares against it and
    /// bails when superseded.
    latest_generation: Arc<AtomicU64>,
    /// Debug knob: per-tick scan delay in milliseconds. The scan pauses this long
    /// each time it crosses to a new tick, so a slow scan is observable.
    scan_delay_ms: Arc<AtomicU32>,
    /// How far ahead of playback the scan is allowed to run. The scan idles once
    /// its frontier reaches this and resumes when the main loop raises it, which is
    /// what keeps the resident window (and thus memory) bounded.
    target_tick: Arc<AtomicU32>,
    /// Set on shutdown so a running scan bails and the thread can be joined before
    /// the process tears down (avoids killing the thread mid-allocation).
    shutdown: Arc<AtomicBool>,
    /// The background note index, once built. Handed to each scan so it can restore
    /// the notes a keyframe seek cannot see.
    note_index: Mutex<Option<Arc<NoteIndex>>>,
}

impl PianoRollWorker {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<RollRequest>();
        let chunks = [
            Arc::new(Mutex::new(RollChunk::default())),
            Arc::new(Mutex::new(RollChunk::default())),
        ];
        let live = Arc::new(AtomicUsize::new(0));
        let latest_generation = Arc::new(AtomicU64::new(0));
        let scan_delay_ms = Arc::new(AtomicU32::new(0));
        let target_tick = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let chunks_for_thread = chunks.clone();
        let live_for_thread = live.clone();
        let latest_for_thread = latest_generation.clone();
        let scan_delay_for_thread = scan_delay_ms.clone();
        let target_for_thread = target_tick.clone();
        let shutdown_for_thread = shutdown.clone();

        let handle = thread::spawn(move || {
            // One reused worker. A running scan bails promptly when a newer
            // request bumps the generation, then we pick up the newest request.
            while let Ok(mut req) = rx.recv() {
                if shutdown_for_thread.load(Ordering::SeqCst) {
                    break;
                }
                while let Ok(next) = rx.try_recv() {
                    req = next;
                }
                if req.generation != latest_for_thread.load(Ordering::SeqCst) {
                    continue;
                }
                run_scan(
                    &req.data,
                    req.start_tick,
                    req.priority_tick,
                    req.generation,
                    req.index.clone(),
                    &latest_for_thread,
                    &chunks_for_thread,
                    req.build_slot,
                    &live_for_thread,
                    &scan_delay_for_thread,
                    &target_for_thread,
                    &shutdown_for_thread,
                );
            }
        });

        Self {
            tx: Some(tx),
            handle: Some(handle),
            chunks,
            live,
            latest_generation,
            scan_delay_ms,
            target_tick,
            shutdown,
            note_index: Mutex::new(None),
        }
    }

    /// Hand the worker the finished note index. Scans started after this see the notes
    /// that a keyframe seek would otherwise miss; the caller regenerates to apply it.
    pub fn set_note_index(&self, index: Arc<NoteIndex>) {
        *self.note_index.lock().unwrap() = Some(index);
    }

    /// The chunk the draw reads. The scan may be filling the other one.
    fn live_chunk(&self) -> &Mutex<RollChunk> {
        &self.chunks[self.live.load(Ordering::SeqCst)]
    }

    pub fn set_scan_delay_ms(&self, ms: u32) {
        self.scan_delay_ms.store(ms, Ordering::Relaxed);
    }

    /// Set the lookahead ceiling. Call every frame with the topmost visible tick
    /// plus some slack: the scan generates up to here and no further. Lowering it
    /// (zooming in shrinks the visible span) stops the scan early; the notes already
    /// generated above it are dropped by rebuilding the window.
    pub fn set_target_tick(&self, tick: u32) {
        self.target_tick.store(tick, Ordering::Relaxed);
    }

    /// Notes currently resident in the window, and the bytes their queues occupy
    /// (allocated capacity, not just the used part). For the stats panel: this is
    /// the piano roll's share of the heap, to be compared against the total.
    /// Drop notes that playback has passed, freeing the memory behind the window.
    /// Pops from the front of each key queue while the front note has fully ended;
    /// the queues are start-sorted, so the first note still alive stops the pop and
    /// a long note holds back everything behind it (exactly what the draw needs).
    pub fn retire(&self, current_tick: u32) {
        let mut g = self.live_chunk().lock().unwrap();
        for key in 0..128usize {
            let bucket = &mut g.notes_by_key[key];
            let mut popped = 0u64;
            while let Some(front) = bucket.front() {
                // Copy out of the packed struct before comparing.
                let end = front.end_tick;
                if end >= current_tick {
                    break;
                }
                bucket.pop_front();
                popped += 1;
            }
            g.retired[key] += popped;

            // A VecDeque keeps its high-water capacity forever, and it grows by
            // doubling. On a dense MIDI that slack ran to a hundred megabytes of
            // untouched memory. Hand it back once a bucket is holding more than
            // twice what it uses. The 4096 floor keeps this from reallocating over
            // and over on the small, busy buckets.
            let bucket = &mut g.notes_by_key[key];
            let (len, cap) = (bucket.len(), bucket.capacity());
            if cap > 4096 && cap > len.saturating_mul(2) {
                bucket.shrink_to_fit();
            }
        }
    }

    /// Stop the worker and join it. Call before the process exits so the thread
    /// is never force-killed mid-allocation (which can deadlock teardown).
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.tx.take(); // drop the sender so a blocked recv() returns and exits
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn with_chunk<R>(&self, f: impl FnOnce(&RollChunk) -> R) -> R {
        let guard = self.live_chunk().lock().unwrap();
        f(&guard)
    }

    /// Cancel any running scan and hand the worker a fresh request to stream a
    /// window forward from `start_tick`. Call on load, on a seek that lands outside
    /// the resident window, and on a zoom that shrinks it.
    ///
    /// The visible chunk normally keeps drawing while the new window is built in the
    /// other slot, and is swapped only once the new one's visible range is complete,
    /// so a rebuild never blanks the roll. `drop_visible` overrides that: the notes on
    /// screen belong to a file that is no longer loaded, and showing another MIDI's
    /// roll for the seconds the scan takes is worse than showing none. Dropping them
    /// also leaves the scan nothing to protect, so it reveals its window as it fills.
    pub fn regenerate(
        &self,
        data: Arc<MIDIPlayerData>,
        start_tick: u32,
        priority_tick: u32,
        drop_visible: bool,
    ) {
        let generation = self.latest_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.target_tick.store(priority_tick, Ordering::Relaxed);

        let live_slot = self.live.load(Ordering::SeqCst);
        let build_slot = 1 - live_slot;

        if drop_visible {
            let mut g = self.chunks[live_slot].lock().unwrap();
            for bucket in g.notes_by_key.iter_mut() {
                bucket.clear();
                bucket.shrink_to_fit();
            }
            g.retired = [0; 128];
            g.done = false;
            g.scan_frontier_tick = start_tick;
            g.window_start_tick = start_tick;
        }

        {
            // Empty the build slot for the new generation. Anything in it is either
            // an abandoned build or the window we swapped out last time.
            let mut g = self.chunks[build_slot].lock().unwrap();
            g.generation = generation;
            g.scan_frontier_tick = start_tick;
            g.window_start_tick = start_tick;
            g.done = false;
            for bucket in g.notes_by_key.iter_mut() {
                bucket.clear();
                bucket.shrink_to_fit();
            }
            g.retired = [0; 128];
        }

        if let Some(tx) = &self.tx {
            let _ = tx.send(RollRequest {
                data,
                start_tick,
                priority_tick,
                generation,
                build_slot,
                index: self.note_index.lock().unwrap().clone(),
            });
        }
    }
}

/// Walk every track in global tick order (k-way merge), emitting a note into its
/// key bucket the moment its note-on is seen (with `end_tick == OPEN_END`), then
/// patching its end when the note-off arrives. Because notes appear at note-on,
/// long notes are visible immediately and simply grow until they end. Publishes
/// progress as the tick frontier advances; bails if a newer generation supersedes
/// this one.
///
/// The scan runs only as far ahead of playback as `target`, then idles. Together
/// with `PianoRollWorker::retire` draining from behind, that makes the resident
/// note set a sliding window rather than the whole MIDI.
#[allow(clippy::too_many_arguments)]
fn run_scan(
    data: &Arc<MIDIPlayerData>,
    start_tick: u32,
    priority_tick: u32,
    generation: u64,
    index: Option<Arc<NoteIndex>>,
    latest: &AtomicU64,
    chunks: &[Arc<Mutex<RollChunk>>; 2],
    build_slot: usize,
    live: &AtomicUsize,
    scan_delay: &AtomicU32,
    target: &AtomicU32,
    shutdown: &AtomicBool,
) {
    let chunk = &*chunks[build_slot];

    // Show the window we have been building and release the one it replaces. Called
    // the moment the visible range is complete, so the roll cuts straight from the
    // old window to the new one with nothing blank in between.
    let mut committed = false;
    let commit = |committed: &mut bool| {
        if *committed {
            return;
        }
        *committed = true;
        live.store(build_slot, Ordering::SeqCst);

        let mut old = chunks[1 - build_slot].lock().unwrap();
        for bucket in old.notes_by_key.iter_mut() {
            bucket.clear();
            bucket.shrink_to_fit();
        }
        old.retired = [0; 128];
        old.done = false;
    };

    // Whether to show this window as it is built rather than only once its visible
    // range is complete. Withholding is what makes a seek land in one piece instead
    // of filling in bottom-to-top, but it only pays when there is already a window on
    // screen to hold onto. With nothing to show (a fresh file), withholding leaves
    // the roll blank and inert for as long as the scan takes, which on a dense MIDI is
    // seconds. Then it is better to let it fill: the filling *is* the progress bar.
    let reveal_progressively = {
        let live_chunk = chunks[1 - build_slot].lock().unwrap();
        live_chunk.notes_by_key.iter().all(|b| b.is_empty())
    };

    // Safety: the Arc keeps the C MIDI struct alive for this call.
    let parser = unsafe { &*data.as_ptr() };
    let end_tick = parser.max_tick;
    let num_tracks = parser.header.tracks as usize;

    // Notes that are open (note-on seen, note-off not yet), keyed by the triple that
    // identifies a voice: track, channel, key. The value is a queue of the absolute
    // indices of every note currently ringing on that voice, oldest first, so a
    // note-off can patch the right one's end.
    //
    // Several note-ons stack on one voice all the time in dense MIDI: a key is struck
    // again while it still rings, and the note-offs arrive later. Keeping only the
    // latest (and closing the previous one on re-trigger) chopped those notes into
    // stubs, which is what made the roll disagree with what was actually playing.
    //
    // A map rather than a track-by-key table: the table ignored channel entirely, and
    // sizing one per (track, channel, key) would run to tens of megabytes on a MIDI
    // with thousands of tracks. Only open notes are held here, of which there are at
    // most a few tens of thousands.
    //
    // A queue entry of `DROPPED` is a note that exists in the MIDI but was left out of
    // the roll (see the per-key cap below). It holds the note's place so that the
    // note-off that closes it consumes it, rather than closing the next note on the
    // voice and cutting that one short.
    const DROPPED: u64 = u64::MAX;
    let mut open: FxHashMap<(u16, u8, u8), VecDeque<u64>> = FxHashMap::default();
    // Running count of notes ever appended to each key (published + queued in the
    // current batch), i.e. the absolute index the next appended note will take.
    // Absolute, not a queue position: retiring pops notes off the front, so queue
    // positions shift and only an absolute index stays valid across that.
    let mut appended = [0u64; 128];

    // One cursor per track; a min-heap keyed by the next event tick merges them.
    let mut cursors: Vec<TrackCursor> = Vec::with_capacity(num_tracks);
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = BinaryHeap::new();

    // Pending appends and end-patches, flushed together on publish (appends first,
    // so a patch can target a note appended in the same batch). Appends are staged
    // per key so publish can move each key's run into its queue in one bulk extend,
    // rather than pushing a note at a time and re-checking capacity on each.
    let mut batch: [Vec<RollNote>; 128] = std::array::from_fn(|_| Vec::new());
    let mut staged = 0usize;
    let mut patches: Vec<(u8, u64, u32)> = Vec::new();

    // What is sounding at `start_tick`, kept to the newest MAX_SOUNDING_PER_KEY notes
    // on each key as the tracks are walked.
    //
    // Some MIDIs sustain absurdly: one MIDI here leaves ten million notes ringing at
    // once, eighty thousand of them on a single key. They are all drawn from the
    // playhead upwards, over each other, in the same one-key-wide column, so all but a
    // handful are painted over and never seen. Collecting them first and trimming after
    // would mean building a hundred-megabyte list on every seek, so each key holds a
    // min-heap on start tick instead and the oldest falls out as newer ones arrive.
    // What survives is what is drawn on top.
    const MAX_SOUNDING_PER_KEY: usize = 128;
    let mut sounding: [BinaryHeap<Reverse<(u32, u16, u8)>>; 128] =
        std::array::from_fn(|_| BinaryHeap::new());
    // Notes evicted above, counted per voice. They still have note-offs coming, and a
    // note-off closes the oldest note on its voice, so each one has to leave a
    // tombstone in the voice's queue or it would close a note that is still drawn and
    // cut it short. Only the count is needed: every evicted note is older than every
    // note kept on that key, so all the tombstones sit at the front of the queue.
    let mut evicted: FxHashMap<(u16, u8, u8), u64> = FxHashMap::default();

    for ti in 0..num_tracks {
        // Safety: ti < num_tracks; parser valid for this call. Each track also
        // reports notes already held at `start_tick`, so notes spanning the seek
        // point still render.
        let (cur, mut held) = unsafe {
            seek_track_cursor_with_held(parser, ti as u16, start_tick, MAX_SOUNDING_PER_KEY)
        };
        // The walk begins at a keyframe and so cannot see notes that began before it.
        // The index knows those; merge them in, keeping start order.
        if let Some(index) = &index {
            let kf = unsafe { keyframe_tick_for(parser, ti as u16, start_tick) };
            let missed = index.missed_at(ti as u16, kf, start_tick);
            if !missed.is_empty() {
                held.extend(missed.into_iter().map(|n| {
                    crate::kazumidiparser_cursor::HeldNote {
                        key: n.key,
                        start_tick: n.start_tick,
                        channel: n.channel,
                    }
                }));
            }
        }

        for note in held {
            let newest = &mut sounding[note.key as usize];
            newest.push(Reverse((note.start_tick, ti as u16, note.channel)));
            if newest.len() > MAX_SOUNDING_PER_KEY {
                if let Some(Reverse((_, old_ti, old_ch))) = newest.pop() {
                    *evicted.entry((old_ti, old_ch, note.key)).or_default() += 1;
                }
            }
        }
        if cur.has_event && !cur.is_tempo && cur.abs_tick <= end_tick {
            heap.push(Reverse((cur.abs_tick, ti)));
        }
        cursors.push(cur);
    }

    // Tombstones first: every evicted note is older than every note kept on its key, so
    // they all sit at the front of their voice's queue.
    for (&(ti, channel, key), &count) in evicted.iter() {
        let queue = open.entry((ti, channel, key)).or_default();
        for _ in 0..count {
            queue.push_back(DROPPED);
        }
    }

    // Then the survivors, oldest first, so each key's notes stay in start order.
    for (key, newest) in sounding.iter_mut().enumerate() {
        let mut kept: Vec<(u32, u16, u8)> = newest.drain().map(|Reverse(n)| n).collect();
        kept.sort_unstable();

        for (start, ti, channel) in kept {
            let idx = appended[key];
            appended[key] += 1;
            batch[key].push(RollNote {
                start_tick: start,
                end_tick: OPEN_END,
                color: crate::utils::note_color_index(ti, channel),
            });
            staged += 1;
            open.entry((ti, channel, key as u8)).or_default().push_back(idx);
        }
    }

    let mut events_since_publish = 0u32;
    let mut last_tick = start_tick;
    // Hold the *reveal* until the visible window is fully scanned, so a seek shows
    // the whole visible area at once instead of filling in gradually. Only the
    // frontier is held back, not the notes: they are flushed into the chunk as they
    // are found (see the `staged` flush below). Staging the entire visible
    // window in `batch` before handing it over would mean holding two copies of a
    // dense screenful at once, which is what made a seek spike the memory.
    let mut priority_done = false;
    /// Flush the staging buffers once they hold this many notes, to cap the
    /// transient copy at a fixed size instead of one screenful.
    const MAX_STAGED: usize = 8192;

    // Flush queued appends + patches and advance the frontier. Returns false if a
    // newer generation took over (caller should abandon the scan).
    let publish = |batch: &mut [Vec<RollNote>; 128],
                   staged: &mut usize,
                   patches: &mut Vec<(u8, u64, u32)>,
                   frontier: u32|
     -> bool {
        let mut g = chunk.lock().unwrap();
        if g.generation != generation {
            return false;
        }
        *staged = 0;
        for (key, run) in batch.iter_mut().enumerate() {
            if !run.is_empty() {
                g.notes_by_key[key].extend(run.drain(..));
            }
        }
        for (key, idx, end) in patches.drain(..) {
            let key = key as usize;
            // Absolute index -> current queue position. A note whose index is below
            // the retired count has already scrolled out of the window; its end no
            // longer exists to patch, so the patch is simply dropped.
            let Some(pos) = idx.checked_sub(g.retired[key]) else {
                continue;
            };
            if let Some(slot) = g.notes_by_key[key].get_mut(pos as usize) {
                slot.end_tick = end;
            }
        }
        g.scan_frontier_tick = frontier;
        true
    };

    while let Some(Reverse((tick, ti))) = heap.pop() {
        if tick > end_tick {
            break;
        }

        // Lookahead ceiling: this event is further ahead than the draw needs, so
        // stop here and idle until playback (or a resize/speed change) raises the
        // target. Everything up to `target` is scanned by definition, because the next
        // event lies above it, so the frontier can be published all the way there even
        // though no event landed on it.
        while tick > target.load(Ordering::Relaxed) {
            let frontier = target.load(Ordering::Relaxed);
            events_since_publish = 0;
            if !publish(&mut batch, &mut staged, &mut patches, frontier) {
                return;
            }
            priority_done = true;
            commit(&mut committed);
            if latest.load(Ordering::SeqCst) != generation || shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(4));
        }

        if tick != last_tick {
            if !priority_done {
                // Visible window: withhold the reveal until it is fully scanned, then
                // publish it in one shot, so a seek shows the whole visible area at
                // once. Unless there is nothing on screen to withhold it for, in which
                // case let it fill in bottom-to-top as it is found.
                if last_tick >= priority_tick {
                    events_since_publish = 0;
                    if !publish(&mut batch, &mut staged, &mut patches, last_tick) {
                        return;
                    }
                    if latest.load(Ordering::SeqCst) != generation
                        || shutdown.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    priority_done = true;
                    commit(&mut committed);
                } else if reveal_progressively && events_since_publish >= 2048 {
                    events_since_publish = 0;
                    if !publish(&mut batch, &mut staged, &mut patches, last_tick) {
                        return;
                    }
                    // Swap this half-built window in: it is replacing an empty one, so
                    // there is nothing to lose by showing it early.
                    commit(&mut committed);
                    if latest.load(Ordering::SeqCst) != generation
                        || shutdown.load(Ordering::Relaxed)
                    {
                        return;
                    }
                }
                last_tick = tick;
            } else {
                // Above the visible window: stream the rest. The debug scan delay
                // (per-tick crawl) only applies here, off-screen.
                let delay = scan_delay.load(Ordering::Relaxed);
                if delay > 0 {
                    let sleep = Duration::from_millis(delay as u64);
                    while last_tick < tick {
                        last_tick += 1;
                        if !publish(&mut batch, &mut staged, &mut patches, last_tick) {
                            return;
                        }
                        if latest.load(Ordering::SeqCst) != generation
                            || shutdown.load(Ordering::Relaxed)
                        {
                            return;
                        }
                        thread::sleep(sleep);
                    }
                } else {
                    // Publish only at tick boundaries, throttled by accumulated
                    // events.
                    if events_since_publish >= 2048 {
                        events_since_publish = 0;
                        if !publish(&mut batch, &mut staged, &mut patches, last_tick) {
                            return;
                        }
                        if latest.load(Ordering::SeqCst) != generation
                            || shutdown.load(Ordering::Relaxed)
                        {
                            return;
                        }
                    }
                    last_tick = tick;
                }
            }
        }

        // Process the current event of this track's cursor.
        {
            let cur = &cursors[ti];
            let msg_type = cur.status & 0xF0;
            let key = cur.param1 as usize;
            if key < 128 {
                let channel = cur.status & 0x0F;
                if msg_type == 0x90 && cur.param2 > 0 {
                    // A re-trigger does not end the note already ringing: it stacks on
                    // top of it. Both stay open until their own note-offs arrive.
                    let idx = appended[key];
                    appended[key] += 1;
                    batch[key].push(RollNote {
                        start_tick: tick,
                        end_tick: OPEN_END,
                        color: crate::utils::note_color_index(ti as u16, channel),
                    });
                    staged += 1;
                    open.entry((ti as u16, channel, key as u8))
                        .or_default()
                        .push_back(idx);
                } else if msg_type == 0x80 || (msg_type == 0x90 && cur.param2 == 0) {
                    // Close the oldest note still ringing on this voice. A note-off
                    // carries no way to say which of several stacked notes it ends,
                    // so oldest-first is the convention.
                    if let Some(queue) = open.get_mut(&(ti as u16, channel, key as u8)) {
                        // A tombstone consumes its note-off and patches nothing: the
                        // note it stands for was never put in the roll.
                        if let Some(idx) = queue.pop_front() {
                            if idx != DROPPED {
                                patches.push((key as u8, idx, tick));
                            }
                        }
                    }
                }
            }
        }

        // Advance this track to its next non-tempo event and re-queue it.
        {
            let cur = &mut cursors[ti];
            let mut advanced = false;
            loop {
                // Safety: cursor stays valid until it reports no further events.
                if !unsafe { cursor_next(cur) } {
                    break;
                }
                if !cur.is_tempo {
                    advanced = true;
                    break;
                }
            }
            if advanced && cur.abs_tick <= end_tick {
                heap.push(Reverse((cur.abs_tick, ti)));
            }
        }

        events_since_publish += 1;

        // Hand the staged notes over as soon as they pile up, without waiting for a
        // tick boundary or for the visible window to finish: a dense chord can carry
        // millions of notes on a single tick, and staging them all would duplicate a
        // screenful in memory. The frontier is only advanced to the last *complete*
        // tick, so this never reveals a half-scanned one.
        if staged >= MAX_STAGED || patches.len() >= MAX_STAGED {
            // Hold the frontier at the window start while the reveal is being withheld;
            // publishing `last_tick` there would leak a partial visible window, which is
            // exactly what the withholding exists to prevent.
            let frontier = if priority_done || reveal_progressively {
                last_tick.saturating_sub(1)
            } else {
                start_tick
            };
            events_since_publish = 0;
            if !publish(&mut batch, &mut staged, &mut patches, frontier) {
                return;
            }
            if latest.load(Ordering::SeqCst) != generation || shutdown.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    // Notes still ringing at the end extend to the final tick.
    for ((_, _, key), queue) in open.iter() {
        for idx in queue.iter().filter(|&&idx| idx != DROPPED) {
            patches.push((*key, *idx, end_tick));
        }
    }

    // Final publish: flush the rest and mark done. The scan can reach the end of the
    // MIDI without ever passing the priority tick (a short tail, or a seek near the
    // end), so this is also the last chance to show what was built.
    if !publish(&mut batch, &mut staged, &mut patches, end_tick) {
        return;
    }
    {
        let mut g = chunk.lock().unwrap();
        if g.generation != generation {
            return;
        }
        g.done = true;
    }
    commit(&mut committed);
}

/// Draw a scrolling piano roll from the (possibly still-generating) chunk.
///
/// The bottom edge of `area` is "now"; the top edge is the furthest visible
/// future point. Notes are only revealed up to the scan frontier, so a slow scan
/// fills in bottom-to-top. Dense same-key runs are merged in pixel space.
pub fn draw_piano_roll_chunk(
    d: &mut impl RaylibDraw,
    data: &KazuMIDIParserData,
    playback_position_ns: u64,
    area: Rectangle,
    is_tick_based: bool,
    note_speed: f32,
    chunk: &RollChunk,
) {
    let current_tick = ns_to_tick(data, playback_position_ns);

    let speed = note_speed.max(MIN_NOTE_SPEED);
    let bottom = area.y + area.height;
    let height = area.height;
    // Scale the visible span with the window height so pixels-per-time stays
    // constant: a taller window shows more future, it does not stretch notes.
    let height_scale = (height / REFERENCE_HEIGHT).max(0.01);

    // tick -> y mapping; y(now) = bottom, y(top) = top of the area.
    let (top_tick, tick_to_y): (u32, Box<dyn Fn(u32) -> f32>) = if is_tick_based {
        let division = (data.header.division as u32).max(1);
        let span = (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
        let start = current_tick;
        (
            current_tick.saturating_add(span),
            Box::new(move |tick: u32| {
                let progress = tick.saturating_sub(start) as f32 / span as f32;
                bottom - progress.min(1.0) * height
            }),
        )
    } else {
        let current_ns = playback_position_ns;
        let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
        let top_tick = ns_to_tick(data, current_ns.saturating_add(span_ns));
        // Invert tick->ns once per pixel row instead of per note edge. row_tick[r]
        // is the tick at the top boundary of local pixel row r (r=0 is the top /
        // furthest future, larger r is lower / closer to now); the array is
        // non-increasing. Mapping a note tick to y is then a binary search with no
        // per-note tempo-map walk or 128-bit division.
        let top_y = area.y;
        let rows = (height.ceil() as usize).max(1) + 1;
        let mut row_tick = vec![0u32; rows];
        for (r, slot) in row_tick.iter_mut().enumerate() {
            let y = top_y + r as f32;
            let progress = ((bottom - y) / height).clamp(0.0, 1.0);
            let ns = current_ns.saturating_add((progress * span_ns as f32) as u64);
            *slot = ns_to_tick(data, ns);
        }
        (
            top_tick,
            Box::new(move |tick: u32| {
                // First row whose tick boundary is <= tick (row_tick is
                // non-increasing): the pixel where this tick lands.
                let mut lo = 0usize;
                let mut hi = row_tick.len();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if row_tick[mid] > tick {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                top_y + lo as f32
            }),
        )
    };

    // Reveal only up to the scan frontier; notes above it are not generated yet.
    let effective_top = top_tick.min(chunk.scan_frontier_tick).max(current_tick);

    let regions = get_keyboard_note_regions(area.width);

    // Per-key column rasterization: composite each key's notes (in start order,
    // later-on-top painter's order) into a 1-pixel-wide color column on the CPU,
    // then emit one rectangle per contiguous same-color run. This caps GPU draw
    // calls at ~screen-height per key instead of one per note, so density (black
    // MIDI) no longer explodes the draw-call count.
    let height_px = (area.height.ceil() as usize).max(1);
    let top_y = area.y;
    // Packed color: 0 = empty, else (1<<24) | rgb.
    let mut col: Vec<u32> = vec![0u32; height_px];

    let pack = |c: (u8, u8, u8)| -> u32 {
        (1u32 << 24) | ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | (c.2 as u32)
    };
    // Writes a tick-space note span into the column.
    let write = |col: &mut [u32], y_top: f32, y_bottom: f32, packed: u32| {
        let a = (y_top - top_y).floor().max(0.0) as usize;
        let b = ((y_bottom - top_y).ceil() as usize).min(col.len());
        let a = a.min(col.len());
        for slot in &mut col[a..b.max(a)] {
            *slot = packed;
        }
    };

    for note in 0..128usize {
        let bucket = &chunk.notes_by_key[note];
        if bucket.is_empty() {
            continue;
        }
        let (rx, rw) = regions[note];
        let x = area.x + rx;

        for slot in col.iter_mut() {
            *slot = 0;
        }

        // The queue front is already the first note that can still be visible:
        // `retire` pops fully-ended notes off the front as playback passes them,
        // so the draw starts scanning right at the live range.
        for n in bucket.iter() {
            // The bucket is start-sorted, so once a note starts above the view the
            // rest do too.
            if n.start_tick > effective_top {
                break;
            }
            // A short note after a long one may still be past; skip it. Held notes
            // carry OPEN_END; clamping below grows them to the frontier.
            if n.end_tick < current_tick {
                continue;
            }
            // Copy out of the packed struct by value before use.
            let start_tick = n.start_tick;
            let end_tick = n.end_tick;
            let color = n.color;
            let s = start_tick.max(current_tick);
            let e = end_tick.min(effective_top);
            if e <= s {
                continue;
            }
            let y_bottom = tick_to_y(s);
            let y_top = tick_to_y(e);
            write(
                &mut col,
                y_top,
                y_bottom,
                pack(get_note_color_by_index(color)),
            );
        }

        // Emit one rectangle per contiguous same-color run.
        let mut yy = 0usize;
        while yy < height_px {
            let p = col[yy];
            if p == 0 {
                yy += 1;
                continue;
            }
            let run_start = yy;
            while yy < height_px && col[yy] == p {
                yy += 1;
            }
            let color = Color::new((p >> 16) as u8, (p >> 8) as u8, p as u8, 255);
            d.draw_rectangle_rec(
                Rectangle::new(x, top_y + run_start as f32, rw, (yy - run_start) as f32),
                color,
            );
        }
    }
}
