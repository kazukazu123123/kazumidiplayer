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
    cursor_next, ns_to_tick, seek_track_cursor, seek_track_cursor_with_held, TrackCursor,
};
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

/// How dark a note's 1px outline is, as a percentage of the note's own color. A
/// shade of the note rather than flat black: on a dense MIDI the outlines meet, and
/// black ones add up to a black roll.
pub(crate) const BORDER_SHADE: u32 = 45;

/// Marks a packed column pixel as already outlined, so it is not darkened a second
/// time. Sits above the 24 color bits and the occupancy bit.
const EDGE_BIT: u32 = 1 << 25;

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

/// One key's notes, and the bookkeeping that has to stay in step with them.
///
/// `generation` lives here rather than on the chunk so that a publish's "is this
/// scan still current?" check and its append are one atomic step under this key's
/// lock. A chunk-level generation could go stale between the check and the append,
/// letting an abandoned scan write notes into a bucket `regenerate` had just
/// cleared.
#[derive(Default)]
pub struct KeyBucket {
    pub generation: u64,
    /// Notes in start-tick order. A note may have `end_tick == OPEN_END` while it is
    /// still held; its end is patched in on note-off. Notes that playback has passed
    /// are popped off the front by `retire`.
    pub notes: VecDeque<RollNote>,
    /// Count of notes popped off the front so far. The scan addresses notes by an
    /// absolute index (total ever appended to this key), so subtracting this maps an
    /// absolute index to the current queue position.
    retired: u64,
}

/// A sliding window of piano-roll draw data, filled in by the worker as playback
/// approaches and drained from behind as playback passes. Notes are bucketed per
/// key and, within a bucket, kept in start-tick order (the scan appends in global
/// tick order, so this holds naturally). `scan_frontier_tick` marks how far ahead
/// the scan has reached; `window_start_tick` is where this generation's scan began.
///
/// Only the window is ever resident, so memory is bounded by how much of the roll
/// is on screen, not by the length or note count of the MIDI.
///
/// Locked per key, not as a whole. One mutex over the entire chunk meant the scan
/// thread's every publish had to wait for the draw thread to finish a frame --
/// which is 128 columns of rasterization *and* every GPU rectangle call for them,
/// since the draw held the lock across all of it. Measured on a dense MIDI, that was
/// 9.6 seconds of lock wait against 0.5 seconds of actual scanning: the scan was
/// blocked ~95% of the time and only ran in the gaps between frames. Splitting the
/// lock per key means a publish for one key contends only with the draw of that same
/// key's column, and the draw releases each key's lock before it touches the GPU.
///
/// The scalar fields are atomics so that reading the frontier never takes a lock at
/// all.
pub struct RollChunk {
    /// Mirrors the per-bucket generation, so a superseded scan can bail out of a
    /// publish without taking 128 locks to discover it. The buckets' own copies stay
    /// authoritative for anything that writes notes.
    pub generation: AtomicU64,
    pub scan_frontier_tick: AtomicU32,
    pub window_start_tick: AtomicU32,
    pub done: AtomicBool,
    /// Draws currently reading this chunk. `commit` waits for this to fall to zero
    /// before recycling the chunk it swapped out, so a draw already in flight is
    /// never cleared out from under itself. Without it the per-key locks would let a
    /// commit blank the keys a mid-frame draw had not reached yet.
    readers: AtomicUsize,
    pub notes_by_key: [Mutex<KeyBucket>; 128],
}

impl Default for RollChunk {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            scan_frontier_tick: AtomicU32::new(0),
            window_start_tick: AtomicU32::new(0),
            done: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            notes_by_key: std::array::from_fn(|_| Mutex::new(KeyBucket::default())),
        }
    }
}

impl RollChunk {
    /// Reset every bucket to `generation` and hand back the memory they held.
    /// The buffers are freed after each key's lock is released: `free` takes the
    /// allocator's lock, and holding a bucket against that is exactly the kind of
    /// stall this split exists to avoid.
    fn reset(&self, generation: u64, start_tick: u32) {
        self.generation.store(generation, Ordering::SeqCst);
        for slot in self.notes_by_key.iter() {
            let mut b = slot.lock().unwrap();
            b.generation = generation;
            b.retired = 0;
            let old = std::mem::take(&mut b.notes);
            drop(b);
            drop(old);
        }
        self.scan_frontier_tick.store(start_tick, Ordering::SeqCst);
        self.window_start_tick.store(start_tick, Ordering::SeqCst);
        self.done.store(false, Ordering::SeqCst);
    }

    /// Block until no draw is reading this chunk. Only called by `commit`, on the
    /// chunk it has just swapped *out*, so the wait is at most the tail of one frame.
    fn wait_for_readers(&self) {
        while self.readers.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
            thread::yield_now();
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
    chunks: [Arc<RollChunk>; 2],
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
}

/// Holds a chunk's reader registration for as long as a draw is reading it.
pub struct ReaderGuard<'a> {
    chunk: &'a RollChunk,
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        self.chunk.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PianoRollWorker {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<RollRequest>();
        let chunks = [
            Arc::new(RollChunk::default()),
            Arc::new(RollChunk::default()),
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
        }
    }

    /// The chunk the draw reads, registered as a reader so `commit` cannot recycle
    /// it mid-frame. Re-reads `live` after registering: a commit that flips `live`
    /// between the two reads would otherwise have already checked `readers` and
    /// found it zero, and would clear the chunk under us.
    fn acquire_live(&self) -> ReaderGuard<'_> {
        loop {
            let slot = self.live.load(Ordering::SeqCst);
            let chunk = &self.chunks[slot];
            chunk.readers.fetch_add(1, Ordering::SeqCst);
            if self.live.load(Ordering::SeqCst) == slot {
                return ReaderGuard { chunk };
            }
            chunk.readers.fetch_sub(1, Ordering::SeqCst);
        }
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
        let guard = self.acquire_live();
        for slot in guard.chunk.notes_by_key.iter() {
            let mut b = slot.lock().unwrap();
            let mut popped = 0u64;
            while let Some(front) = b.notes.front() {
                // Copy out of the packed struct before comparing.
                let end = front.end_tick;
                if end >= current_tick {
                    break;
                }
                b.notes.pop_front();
                popped += 1;
            }
            b.retired += popped;

            // A VecDeque keeps its high-water capacity forever, and it grows by
            // doubling. On a dense MIDI that slack ran to a hundred megabytes of
            // untouched memory. Hand it back once a bucket is holding more than
            // twice what it uses. The 4096 floor keeps this from reallocating over
            // and over on the small, busy buckets.
            //
            // `shrink_to_fit` allocates a new buffer, copies into it, and frees the
            // old one. All three take time, and the first and last take the
            // allocator's lock, so none of it belongs under a bucket the scan is
            // trying to publish into: this key's queue can hold a dense screenful.
            //
            // So only the copy stays. The replacement buffer is allocated before the
            // lock is taken (from the length seen last frame -- if it moved, the copy
            // below just grows it again, which is a plain `extend`), and the old
            // buffer is dropped after it is released.
            let (len, cap) = (b.notes.len(), b.notes.capacity());
            let shrink = cap > 4096 && cap > len.saturating_mul(2);
            drop(b);

            if shrink {
                let mut fresh = VecDeque::with_capacity(len.next_power_of_two().max(4096));
                let mut b = slot.lock().unwrap();
                fresh.extend(b.notes.iter().copied());
                let discarded = std::mem::replace(&mut b.notes, fresh);
                drop(b);
                drop(discarded);
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
        let guard = self.acquire_live();
        f(guard.chunk)
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
            // No scan ever writes the live slot (the new one fills `build_slot`), so
            // the generation stamped here is inert; it only has to not be an older
            // one an abandoned scan might still be holding.
            self.chunks[live_slot].reset(generation, start_tick);
        }

        // Empty the build slot for the new generation. Anything in it is either an
        // abandoned build or the window we swapped out last time. Stamping the new
        // generation into every bucket is also what stops a superseded scan from
        // publishing into it: each publish re-checks under the same bucket lock.
        self.chunks[build_slot].reset(generation, start_tick);

        if let Some(tx) = &self.tx {
            let _ = tx.send(RollRequest {
                data,
                start_tick,
                priority_tick,
                generation,
                build_slot,
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
    latest: &AtomicU64,
    chunks: &[Arc<RollChunk>; 2],
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

        // Recycle the window just swapped out. A draw that started before the store
        // above is still walking it key by key, and clearing under it would blank
        // whichever keys it had not reached; wait for it to finish first. Any draw
        // starting after the store picks up the new slot, so this wait is bounded by
        // one frame.
        let old = &chunks[1 - build_slot];
        old.wait_for_readers();
        old.reset(generation, start_tick);
    };

    // Whether to show this window as it is built rather than only once its visible
    // range is complete. Withholding is what makes a seek land in one piece instead
    // of filling in bottom-to-top, but it only pays when there is already a window on
    // screen to hold onto. With nothing to show (a fresh file), withholding leaves
    // the roll blank and inert for as long as the scan takes, which on a dense MIDI is
    // seconds. Then it is better to let it fill: the filling *is* the progress bar.
    let reveal_progressively = chunks[1 - build_slot]
        .notes_by_key
        .iter()
        .all(|b| b.lock().unwrap().notes.is_empty());

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
    // Patches are staged per key for the same reason as appends: publish walks the
    // keys once, locking each bucket a single time for both.
    let mut patches: [Vec<(u64, u32)>; 128] = std::array::from_fn(|_| Vec::new());
    let mut patch_count = 0usize;

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

    // Seek every track to `start_tick`, in parallel. This is the one part of a scan
    // that splits cleanly: a track's seek reads only its own events and the shared
    // (immutable) index, so nothing here depends on another track. On a 4393-track
    // file it was ~50 ms of the ~300 ms a seek took to show anything, and it is a
    // fixed cost paid on every single seek.
    //
    // Stride rather than block: track lengths vary wildly, and interleaving keeps one
    // thread from drawing all the long ones.
    let worker_count = (crate::app_state::cpu_threads().max(1) as usize).min(num_tracks.max(1));
    let seeded = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for w in 0..worker_count {
            let data = data.clone();
            handles.push(scope.spawn(move || {
                // Safety: the Arc keeps the C MIDI struct alive for this call.
                let parser = unsafe { &*data.as_ptr() };
                let mut out: Vec<(usize, TrackCursor, Vec<crate::kazumidiparser_cursor::HeldNote>)> =
                    Vec::new();
                let mut ti = w;
                while ti < num_tracks {
                    // Safety: ti < num_tracks; parser valid for this call. Each track
                    // also reports notes already held at `start_tick`, so notes
                    // spanning the seek point still render -- except those that began
                    // before the keyframe the walk starts from, whose note-on it never
                    // reads. Those are simply missing from the roll until playback
                    // reaches a rebuild that starts before them.
                    let (cur, held) = unsafe {
                        seek_track_cursor_with_held(
                            parser,
                            ti as u16,
                            start_tick,
                            MAX_SOUNDING_PER_KEY,
                        )
                    };
                    out.push((ti, cur, held));
                    ti += worker_count;
                }
                out
            }));
        }
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect::<Vec<_>>()
    });

    // Fold the seeks back together in track order. `sounding` keeps the newest 128
    // starts per key, which is the same set whatever order they arrive in, but the
    // heap merge below wants `cursors` indexed by track, so put them back in order.
    let mut per_track: Vec<Option<(TrackCursor, Vec<crate::kazumidiparser_cursor::HeldNote>)>> =
        (0..num_tracks).map(|_| None).collect();
    for chunk in seeded {
        for (ti, cur, held) in chunk {
            per_track[ti] = Some((cur, held));
        }
    }

    for (ti, entry) in per_track.into_iter().enumerate() {
        let Some((cur, held)) = entry else {
            // A worker panicked; treat the track as empty rather than mis-indexing
            // every track after it.
            cursors.push(unsafe { seek_track_cursor(parser, ti as u16, start_tick) });
            continue;
        };
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
    //
    // One key at a time: each bucket is locked, given its appends and its patches
    // together, and released before the next is touched. A key with no work is not
    // locked at all. The draw holds the same locks, but only one column's worth and
    // only while rasterizing it, so the two threads now interleave instead of the
    // scan waiting out whole frames.
    let publish = |batch: &mut [Vec<RollNote>; 128],
                   staged: &mut usize,
                   patches: &mut [Vec<(u64, u32)>; 128],
                   patch_count: &mut usize,
                   frontier: u32|
     -> bool {
        // Cheap early-out on a superseded scan, before taking any lock.
        if chunk.generation.load(Ordering::SeqCst) != generation {
            return false;
        }
        *staged = 0;
        *patch_count = 0;

        for (key, (run, key_patches)) in batch.iter_mut().zip(patches.iter_mut()).enumerate() {
            if run.is_empty() && key_patches.is_empty() {
                continue;
            }
            let mut b = chunk.notes_by_key[key].lock().unwrap();

            // Re-checked under the bucket's own lock: this and the writes below have
            // to be one step, or a `regenerate` landing between them would have its
            // freshly cleared bucket refilled by this abandoned scan.
            if b.generation != generation {
                return false;
            }
            b.notes.extend(run.drain(..));
            for (idx, end) in key_patches.drain(..) {
                // Absolute index -> current queue position. A note whose index is
                // below the retired count has already scrolled out of the window; its
                // end no longer exists to patch, so the patch is simply dropped.
                let Some(pos) = idx.checked_sub(b.retired) else {
                    continue;
                };
                if let Some(slot) = b.notes.get_mut(pos as usize) {
                    slot.end_tick = end;
                }
            }
        }

        // The frontier is only a reveal hint, so it is not worth a lock: a
        // `regenerate` racing the check above can leave one stale value here, on a
        // chunk that now holds no notes (so nothing is drawn) and whose new scan
        // overwrites it on its very first publish.
        chunk.scan_frontier_tick.store(frontier, Ordering::SeqCst);
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
            if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, frontier) {
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
                    if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, last_tick) {
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
                    if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, last_tick) {
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
                        if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, last_tick) {
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
                        if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, last_tick) {
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
                                patches[key].push((idx, tick));
                                patch_count += 1;
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
        if staged >= MAX_STAGED || patch_count >= MAX_STAGED {
            // Hold the frontier at the window start while the reveal is being withheld;
            // publishing `last_tick` there would leak a partial visible window, which is
            // exactly what the withholding exists to prevent.
            let frontier = if priority_done || reveal_progressively {
                last_tick.saturating_sub(1)
            } else {
                start_tick
            };
            events_since_publish = 0;
            if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, frontier) {
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
            patches[*key as usize].push((*idx, end_tick));
            patch_count += 1;
        }
    }

    // Final publish: flush the rest and mark done. The scan can reach the end of the
    // MIDI without ever passing the priority tick (a short tail, or a seek near the
    // end), so this is also the last chance to show what was built.
    if !publish(&mut batch, &mut staged, &mut patches, &mut patch_count, end_tick) {
        return;
    }
    if chunk.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    chunk.done.store(true, Ordering::SeqCst);
    commit(&mut committed);
}

#[inline]
fn pack(c: (u8, u8, u8)) -> u32 {
    (1u32 << 24) | ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | (c.2 as u32)
}

/// A note's outline: its own color, darkened, and flagged as already darkened so the
/// side pass leaves it alone. Without the flag the corners, where a top or bottom
/// edge meets a side, get darkened twice.
#[inline]
fn darken(packed: u32) -> u32 {
    if packed & EDGE_BIT != 0 {
        return packed;
    }
    let ch = |shift: u32| ((packed >> shift) & 0xFF) * BORDER_SHADE / 100;
    (1u32 << 24) | EDGE_BIT | (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Index of the first note starting above `limit`, in the deque's logical order.
/// The bucket is start-sorted, so every note from here on is off the top of the view.
fn first_note_above(notes: &VecDeque<RollNote>, limit: u32) -> usize {
    // A VecDeque is a ring, so its contents are up to two slices; each is sorted and
    // the head's notes all precede the tail's.
    let (head, tail) = notes.as_slices();
    let in_head = head.partition_point(|n| n.start_tick <= limit);
    if in_head < head.len() {
        in_head
    } else {
        head.len() + tail.partition_point(|n| n.start_tick <= limit)
    }
}

/// How far back a row's lookup will walk before giving up on finding a note that
/// covers it. The walk only continues while the notes it passes end *below* the row,
/// which in a dense stretch means it stops on the first one; the cap is there for a
/// key holding a rest, where nothing covers the row and the walk would otherwise
/// scan the whole bucket for every row of the gap.
const MAX_LOOKBACK: usize = 64;

/// Fill one key's column by asking each pixel which note it shows, rather than by
/// painting every note and letting them cover each other up.
///
/// Painting was measured at ~4.9M notes a frame to produce ~14k rectangles: a pixel
/// row holds about 285 notes' worth of starts, and 284 of them are painted over. The
/// waste is not spread across the column, it is *within a row* -- which matters,
/// because the bucket is start-sorted, so the notes landing in one row are a
/// contiguous run of it. That makes the winner findable instead of paintable:
///
/// - a note reaches this row only if it starts at or before the row's top tick, and
///   those notes are a prefix of the bucket (binary search, `first_note_above`);
/// - the roll paints later notes over earlier ones, so the winner is the *last* note
///   of that prefix which also still ends at or after the row's bottom tick.
///
/// The cost stops scaling with the note count and starts scaling with the pixel
/// count. Measured on the frame that used to touch 4.9M notes: 35k lookups, and every
/// single one of them found its winner on the first note it checked.
///
/// `owner` records which note won each row, so the emit pass can still tell two
/// touching notes of the same colour apart and outline them separately -- reading the
/// colour alone would merge them into one block.
fn rasterize_column(
    bucket: &Mutex<KeyBucket>,
    col: &mut [u32],
    owner: &mut [u32],
    row_tick: &[u32],
    current_tick: u32,
    effective_top: u32,
) {
    let b = bucket.lock().unwrap();
    if b.notes.is_empty() {
        return;
    }
    for slot in col.iter_mut() {
        *slot = 0;
    }
    for slot in owner.iter_mut() {
        *slot = u32::MAX;
    }

    for r in 0..col.len() {
        // The ticks this pixel row covers. Clamped to the window: above the frontier
        // nothing has been generated, below the playhead nothing is drawn.
        let t_hi = row_tick[r].min(effective_top);
        let t_lo = row_tick[r + 1].max(current_tick);
        if t_hi < t_lo {
            continue;
        }

        let prefix = first_note_above(&b.notes, t_hi);
        if prefix == 0 {
            continue;
        }

        // Walk back from the newest note that could reach this row. Each one passed
        // over is a note that ended before the row starts, i.e. one that is genuinely
        // not here -- not one being covered up.
        for (k, n) in b.notes.range(..prefix).rev().enumerate() {
            if k >= MAX_LOOKBACK {
                break;
            }
            if n.end_tick >= t_lo {
                col[r] = pack(get_note_color_by_index(n.color));
                owner[r] = (prefix - 1 - k) as u32;
                break;
            }
        }

        // A very long note can sit at the front of the queue and keep all the
        // shorter notes after it from being retired. Once more than MAX_LOOKBACK
        // of those later notes have ended, the bounded reverse search above no
        // longer reaches the long note and it appears to vanish. Check the oldest
        // edge as a bounded fallback; held notes reconstructed by a seek also live
        // here, so this covers both normal playback and roll regeneration without
        // turning a dense rest into an unbounded scan on every pixel.
        if owner[r] == u32::MAX && prefix > MAX_LOOKBACK {
            let oldest_end = prefix.min(MAX_LOOKBACK);
            for (index, n) in b.notes.range(..oldest_end).enumerate().rev() {
                if n.end_tick >= t_lo {
                    col[r] = pack(get_note_color_by_index(n.color));
                    owner[r] = index as u32;
                    break;
                }
            }
        }
    }

    // Outline each note's visible extent, top and bottom. Per run of one owner rather
    // than per note: a note that is partly covered only shows where it won, and that
    // is the edge worth drawing.
    let mut r = 0usize;
    while r < col.len() {
        if owner[r] == u32::MAX {
            r += 1;
            continue;
        }
        let run_start = r;
        let who = owner[r];
        while r < col.len() && owner[r] == who {
            r += 1;
        }
        let edge = darken(col[run_start]);
        col[run_start] = edge;
        col[r - 1] = edge;
    }
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
    let height = area.height;
    // Scale the visible span with the window height so pixels-per-time stays constant:
    // a taller window shows more future, it does not stretch notes.
    let height_scale = (height / REFERENCE_HEIGHT).max(0.01);
    let height_px = (area.height.ceil() as usize).max(1);
    let top_y = area.y;

    // The tick at each pixel row's top edge; row 0 is the top of the view (furthest
    // future) and row `height_px` is the playhead, so this is non-increasing. Row r
    // therefore covers ticks [row_tick[r + 1], row_tick[r]].
    //
    // This is the inverse of the old tick -> y mapping, and it is the only direction
    // the rasterizer needs now that pixels look their note up instead of notes
    // painting themselves onto pixels. Both axis modes build it, so there is one path
    // rather than a closure per mode.
    let mut row_tick = vec![0u32; height_px + 1];
    let top_tick = if is_tick_based {
        let division = (data.header.division as u32).max(1);
        let span =
            (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
        for (r, slot) in row_tick.iter_mut().enumerate() {
            let progress = (1.0 - r as f32 / height_px as f32).clamp(0.0, 1.0);
            *slot = if r == height_px {
                current_tick
            } else {
                current_tick.saturating_add((progress * span as f32) as u32)
            };
        }
        current_tick.saturating_add(span)
    } else {
        let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
        for (r, slot) in row_tick.iter_mut().enumerate() {
            let progress = (1.0 - r as f32 / height_px as f32).clamp(0.0, 1.0);
            let ns = if r == height_px {
                playback_position_ns
            } else {
                playback_position_ns.saturating_add((progress * span_ns as f32) as u64)
            };
            *slot = ns_to_tick(data, ns);
        }
        ns_to_tick(data, playback_position_ns.saturating_add(span_ns))
    };

    // Reveal only up to the scan frontier; notes above it are not generated yet.
    let effective_top = top_tick
        .min(chunk.scan_frontier_tick.load(Ordering::SeqCst))
        .max(current_tick);

    let regions = get_keyboard_note_regions(area.width);

    // All 128 columns, back to back. Packed color per pixel: 0 = empty, else
    // (1<<24) | rgb. `owners` runs alongside so the emit pass can separate two
    // touching notes that happen to share a colour.
    let mut cols: Vec<u32> = vec![0u32; 128 * height_px];
    let mut owners: Vec<u32> = vec![u32::MAX; 128 * height_px];

    let worker_count = (crate::app_state::cpu_threads().max(1) as usize).min(128);
    let mut assignments: Vec<Vec<(usize, &mut [u32], &mut [u32])>> =
        (0..worker_count).map(|_| Vec::new()).collect();
    for (key, (col, own)) in cols
        .chunks_mut(height_px)
        .zip(owners.chunks_mut(height_px))
        .enumerate()
    {
        assignments[key % worker_count].push((key, col, own));
    }

    let row_tick_ref = &row_tick[..];
    thread::scope(|scope| {
        for mine in assignments {
            scope.spawn(move || {
                for (key, col, own) in mine {
                    rasterize_column(
                        &chunk.notes_by_key[key],
                        col,
                        own,
                        row_tick_ref,
                        current_tick,
                        effective_top,
                    );
                }
            });
        }
    });

    // Emit one rectangle per contiguous same-color run.
    //
    // The left and right edges are drawn per run rather than per note: every note on
    // a key spans the key's full width, so those two edges are the same two columns
    // of pixels for all of them.
    for (note, col) in cols.chunks(height_px).enumerate() {
        let (rx, rw) = regions[note];
        let x = area.x + rx;
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
            let unpack = |v: u32| Color::new((v >> 16) as u8, (v >> 8) as u8, v as u8, 255);
            let y = top_y + run_start as f32;
            let h = (yy - run_start) as f32;
            d.draw_rectangle_rec(Rectangle::new(x, y, rw, h), unpack(p));
            let edge = unpack(darken(p));
            d.draw_rectangle_rec(Rectangle::new(x, y, 1.0, h), edge);
            d.draw_rectangle_rec(Rectangle::new(x + rw - 1.0, y, 1.0, h), edge);
        }
    }
}
