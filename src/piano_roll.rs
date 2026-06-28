use kazumidiparser::{KazuMIDIParserData, MIDIPlayerData};
use raylib::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::kazumidiparser_cursor::{
    cursor_next, ns_to_tick, seek_track_cursor_with_held, tick_to_ns, TrackCursor,
};
use crate::ui_components::piano_keyboard::get_keyboard_note_regions;
use crate::utils::get_note_color_by_track_channel;

/// How much of the future is visible, when the vertical axis is time based.
const VISIBLE_SPAN_NS: u64 = 3_000_000_000; // 3 seconds
/// How much of the future is visible, when the vertical axis is tick based.
/// Expressed in quarter notes (multiplied by `division` at runtime).
const VISIBLE_SPAN_QUARTERS: u32 = 8;
/// Reference roll height the visible spans above are tuned for. The visible span
/// scales with `height / REFERENCE_HEIGHT`, so pixels-per-time stays constant and
/// resizing the window reveals more/less future instead of stretching notes.
const REFERENCE_HEIGHT: f32 = 150.0;

/// `end_tick` sentinel for a note whose note-on has been scanned but whose
/// note-off has not yet been seen. Such a note is drawn growing from its start up
/// to the current reveal frontier, and its real end is patched in once the
/// note-off is found.
const OPEN_END: u32 = u32::MAX;

/// A single note rectangle in tick space (resolved to pixels at draw time).
/// `track`/`channel` drive the per-voice color. `end_tick == OPEN_END` means the
/// note is still being held (note-off not scanned yet).
#[derive(Clone, Copy)]
pub struct RollNote {
    pub note: u8,
    pub start_tick: u32,
    pub end_tick: u32,
    pub track: u16,
    pub channel: u8,
}

/// Generated piano-roll draw data (start of MIDI -> end), filled in progressively
/// by the worker. Notes are bucketed per key and, within a bucket, kept in
/// start-tick order (the scan appends in global tick order, so this holds
/// naturally). `scan_frontier_tick` marks how far the scan has reached so the draw
/// reveals it bottom-to-top.
pub struct RollChunk {
    pub generation: u64,
    pub scan_frontier_tick: u32,
    pub done: bool,
    /// Per-key note lists, in start-tick order. A note may have `end_tick ==
    /// OPEN_END` while it is still held; its end is patched in on note-off.
    pub notes_by_key: [Vec<RollNote>; 128],
}

impl Default for RollChunk {
    fn default() -> Self {
        Self {
            generation: 0,
            scan_frontier_tick: 0,
            done: false,
            notes_by_key: std::array::from_fn(|_| Vec::new()),
        }
    }
}

/// Per-key forward cursor for the draw, kept across frames. Each `cursor[key]` is
/// the lowest bucket index from which notes can still be visible; it advances past
/// notes that have fully ended so the draw doesn't re-scan the whole (possibly
/// huge) bucket from index 0 every frame. Reset on rewind or when the chunk is
/// regenerated (a new file changes the buckets/indices).
pub struct PianoRollDrawState {
    cursor: [u32; 128],
    last_current_tick: u32,
    last_generation: u64,
}

impl Default for PianoRollDrawState {
    fn default() -> Self {
        Self {
            cursor: [0; 128],
            last_current_tick: 0,
            last_generation: u64::MAX,
        }
    }
}

impl PianoRollDrawState {
    pub fn new() -> Self {
        Self::default()
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
    let speed = note_speed.max(0.01);
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
/// hands the thread a fresh request to walk from the current tick to the end of
/// the MIDI, publishing progress incrementally.
pub struct PianoRollWorker {
    tx: Option<Sender<RollRequest>>,
    handle: Option<JoinHandle<()>>,
    chunk: Arc<Mutex<RollChunk>>,
    /// Bumped on every `regenerate`; the running scan compares against it and
    /// bails when superseded.
    latest_generation: Arc<AtomicU64>,
    /// Debug knob: per-tick scan delay in milliseconds. The scan pauses this long
    /// each time it crosses to a new tick, so a slow scan is observable.
    scan_delay_ms: Arc<AtomicU32>,
    /// Set on shutdown so a running scan bails and the thread can be joined before
    /// the process tears down (avoids killing the thread mid-allocation).
    shutdown: Arc<AtomicBool>,
}

impl PianoRollWorker {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<RollRequest>();
        let chunk = Arc::new(Mutex::new(RollChunk::default()));
        let latest_generation = Arc::new(AtomicU64::new(0));
        let scan_delay_ms = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let chunk_for_thread = chunk.clone();
        let latest_for_thread = latest_generation.clone();
        let scan_delay_for_thread = scan_delay_ms.clone();
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
                    &chunk_for_thread,
                    &scan_delay_for_thread,
                    &shutdown_for_thread,
                );
            }
        });

        Self {
            tx: Some(tx),
            handle: Some(handle),
            chunk,
            latest_generation,
            scan_delay_ms,
            shutdown,
        }
    }

    pub fn set_scan_delay_ms(&self, ms: u32) {
        self.scan_delay_ms.store(ms, Ordering::Relaxed);
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
        let guard = self.chunk.lock().unwrap();
        f(&guard)
    }

    /// Note-generation progress as `(fraction, done)`: the furthest generated
    /// tick as a fraction of the whole MIDI (`scan_frontier_tick / max_tick`).
    pub fn gen_progress(&self, max_tick: u32) -> (f32, bool) {
        let g = self.chunk.lock().unwrap();
        if g.done {
            return (1.0, true);
        }
        let frac = if max_tick == 0 {
            1.0
        } else {
            g.scan_frontier_tick as f32 / max_tick as f32
        };
        (frac.clamp(0.0, 1.0), g.done)
    }

    /// Cancel any running scan, clear the cache, and hand the worker a fresh
    /// request to generate from `start_tick` to the end of the MIDI. Call once
    /// per seek / zoom change with the current visible tick.
    pub fn regenerate(&self, data: Arc<MIDIPlayerData>, start_tick: u32, priority_tick: u32) {
        let generation = self.latest_generation.fetch_add(1, Ordering::SeqCst) + 1;

        // Reset the chunk to the new (empty) generation so the draw immediately
        // shows a blank roll starting at `start_tick`; the worker then fills it in.
        {
            let mut g = self.chunk.lock().unwrap();
            g.generation = generation;
            g.scan_frontier_tick = start_tick;
            g.done = false;
            for bucket in g.notes_by_key.iter_mut() {
                bucket.clear();
            }
        }

        if let Some(tx) = &self.tx {
            let _ = tx.send(RollRequest {
                data,
                start_tick,
                priority_tick,
                generation,
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
fn run_scan(
    data: &Arc<MIDIPlayerData>,
    start_tick: u32,
    priority_tick: u32,
    generation: u64,
    latest: &AtomicU64,
    chunk: &Mutex<RollChunk>,
    scan_delay: &AtomicU32,
    shutdown: &AtomicBool,
) {
    // Safety: the Arc keeps the C MIDI struct alive for this call.
    let parser = unsafe { &*data.as_ptr() };
    let end_tick = parser.max_tick;
    let num_tracks = parser.header.tracks as usize;

    // Per-track, per-key: the index of the still-open note within that key's
    // bucket, so the note-off can patch its end. Independent per track so one
    // track's note-off never closes another track's note on the same key.
    let mut pending: Vec<[Option<u32>; 128]> = vec![[None; 128]; num_tracks];
    // Running count of notes appended to each key bucket (published + queued in
    // the current batch). This is exactly the index the next appended note will
    // occupy, so it stays in lockstep with the shared bucket's length.
    let mut appended = [0u32; 128];

    // One cursor per track; a min-heap keyed by the next event tick merges them.
    let mut cursors: Vec<TrackCursor> = Vec::with_capacity(num_tracks);
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = BinaryHeap::new();

    // Pending appends and end-patches, flushed together on publish (appends first,
    // so a patch can target a note appended in the same batch).
    let mut batch: Vec<RollNote> = Vec::new();
    let mut patches: Vec<(u8, u32, u32)> = Vec::new();

    for ti in 0..num_tracks {
        // Safety: ti < num_tracks; parser valid for this call. Each track also
        // reports notes already held at `start_tick`, so notes spanning the seek
        // point still render.
        let (cur, held) = unsafe { seek_track_cursor_with_held(parser, ti as u16, start_tick) };
        for (key, slot) in held.iter().enumerate() {
            if let Some((s, ch)) = slot {
                let idx = appended[key];
                appended[key] += 1;
                batch.push(RollNote {
                    note: key as u8,
                    start_tick: *s,
                    end_tick: OPEN_END,
                    track: ti as u16,
                    channel: *ch,
                });
                pending[ti][key] = Some(idx);
            }
        }
        if cur.has_event && !cur.is_tempo && cur.abs_tick <= end_tick {
            heap.push(Reverse((cur.abs_tick, ti)));
        }
        cursors.push(cur);
    }

    let mut events_since_publish = 0u32;
    let mut last_tick = start_tick;
    // Hold the first publish until the visible window is fully scanned, so a seek
    // reveals the whole visible area at once instead of filling in gradually.
    let mut priority_done = false;

    // Flush queued appends + patches and advance the frontier. Returns false if a
    // newer generation took over (caller should abandon the scan).
    let publish = |batch: &mut Vec<RollNote>,
                   patches: &mut Vec<(u8, u32, u32)>,
                   frontier: u32|
     -> bool {
        let mut g = chunk.lock().unwrap();
        if g.generation != generation {
            return false;
        }
        for n in batch.drain(..) {
            g.notes_by_key[n.note as usize].push(n);
        }
        for (key, idx, end) in patches.drain(..) {
            if let Some(slot) = g.notes_by_key[key as usize].get_mut(idx as usize) {
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

        if tick != last_tick {
            if !priority_done {
                // Visible window: withhold all output (regardless of scan delay)
                // until it is fully scanned, then publish it in one shot, so a
                // seek shows the whole visible area instantly.
                if last_tick >= priority_tick {
                    events_since_publish = 0;
                    if !publish(&mut batch, &mut patches, last_tick) {
                        return;
                    }
                    if latest.load(Ordering::SeqCst) != generation
                        || shutdown.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    priority_done = true;
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
                        if !publish(&mut batch, &mut patches, last_tick) {
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
                        if !publish(&mut batch, &mut patches, last_tick) {
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
                    // Re-trigger on a still-held key: close the previous note here.
                    if let Some(idx) = pending[ti][key].take() {
                        patches.push((key as u8, idx, tick));
                    }
                    let idx = appended[key];
                    appended[key] += 1;
                    batch.push(RollNote {
                        note: key as u8,
                        start_tick: tick,
                        end_tick: OPEN_END,
                        track: ti as u16,
                        channel,
                    });
                    pending[ti][key] = Some(idx);
                } else if msg_type == 0x80 || (msg_type == 0x90 && cur.param2 == 0) {
                    if let Some(idx) = pending[ti][key].take() {
                        patches.push((key as u8, idx, tick));
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
    }

    // Notes still held at the end extend to the final tick.
    for track_pending in pending.iter() {
        for (key, slot) in track_pending.iter().enumerate() {
            if let Some(idx) = slot {
                patches.push((key as u8, *idx, end_tick));
            }
        }
    }

    // Final publish: flush the rest and mark done.
    {
        let mut g = chunk.lock().unwrap();
        if g.generation != generation {
            return;
        }
        for n in batch.drain(..) {
            g.notes_by_key[n.note as usize].push(n);
        }
        for (key, idx, end) in patches.drain(..) {
            if let Some(slot) = g.notes_by_key[key as usize].get_mut(idx as usize) {
                slot.end_tick = end;
            }
        }
        g.scan_frontier_tick = end_tick;
        g.done = true;
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
    state: &mut PianoRollDrawState,
) {
    let current_tick = ns_to_tick(data, playback_position_ns);

    // Reset the per-key forward cursors on rewind or when the chunk was rebuilt
    // (new file -> different buckets/indices); otherwise they only advance.
    if current_tick < state.last_current_tick || chunk.generation != state.last_generation {
        state.cursor = [0; 128];
    }
    state.last_current_tick = current_tick;
    state.last_generation = chunk.generation;

    let speed = note_speed.max(0.01);
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
        let data_ptr = data as *const KazuMIDIParserData;
        (
            top_tick,
            Box::new(move |tick: u32| {
                // Safety: `data` outlives this closure (same call).
                let ns = tick_to_ns(unsafe { &*data_ptr }, tick);
                let progress = ns.saturating_sub(current_ns) as f32 / span_ns as f32;
                bottom - progress.min(1.0) * height
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

        // Advance this key's forward cursor past notes that have fully ended, so we
        // start scanning near the currently-visible range instead of from index 0.
        // We stop at the first note still alive (end >= now); every note before it
        // has ended, so none are visible and skipping them is safe (long notes are
        // preserved because their end keeps them alive). The cursor only moves
        // forward, so this is amortized cheap across frames.
        let mut lo = state.cursor[note] as usize;
        while lo < bucket.len() && bucket[lo].end_tick < current_tick {
            lo += 1;
        }
        state.cursor[note] = lo as u32;

        for n in &bucket[lo..] {
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
            let s = n.start_tick.max(current_tick);
            let e = n.end_tick.min(effective_top);
            if e <= s {
                continue;
            }
            let y_bottom = tick_to_y(s);
            let y_top = tick_to_y(e);
            write(
                &mut col,
                y_top,
                y_bottom,
                pack(get_note_color_by_track_channel(n.track, n.channel)),
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
