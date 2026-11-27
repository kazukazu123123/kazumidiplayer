//! Piano roll.
//!
//! Premise: work out only what can be seen, do it off the render thread on every
//! core, and never make the render thread wait for anything.
//!
//! - **Removal at scan time.** Each key shows exactly one note at a time: the
//!   newest one still sounding, newest in event order (tick, then track index,
//!   then position within the track -- FIFO event-order layering, the order the
//!   old roll painted in). The scan keeps every key's sounding notes on a stack
//!   and stores only the top note's visible stretches ("segments"). That is
//!   overlap removal (covered notes cost nothing), sustain overlap removal (a
//!   re-struck key splits into visible stretches instead of piling up) and
//!   polyphony removal (each key's stack is capped at `MAX_SOUNDING_PER_KEY`).
//!
//! - **Parallel scan.** A note-on and its note-off always belong to the same
//!   track, so tracks are scanned independently on all cores, slice by slice of
//!   song time. Their events are then merged per key (in parallel, 128 ways) in
//!   event order and run through the key stacks.
//!
//! - **Lock-free handoff.** Each scanned slice is immutable. The worker
//!   publishes the visible set as one `Window` through an atomic pointer swap;
//!   the render thread swaps it out without ever taking a lock, and hands the
//!   window it replaces back to the worker, so it never frees memory either.
//!
//! - **Render cost is a function of the screen, not the notes.** Per frame, the
//!   segments in view are resolved to one colour per key per pixel row
//!   (128 x height, outlines included), uploaded as one small texture, and drawn
//!   as 384 textured quads in a single batch: each key's column, then its two
//!   1px side outlines. White keys first, black keys on top, as before.

use kazumidiparser::{KazuMIDIParserData, MIDIPlayerData};
use raylib::prelude::*;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::kazumidiparser_cursor::{
    HeldCheckpoint, HeldNote, TrackCursor, cursor_next, ns_to_tick,
    seek_track_cursor_with_held_ckpt, tick_to_ns,
};
use crate::ui_components::piano_keyboard::{get_keyboard_note_regions, is_black_key};
use crate::utils::{get_note_color_by_index, note_color_index};

// ---------------------------------------------------------------------------
// View constants (unchanged from the old roll, so it looks and zooms the same)
// ---------------------------------------------------------------------------

/// How much of the future is visible, when the vertical axis is time based.
const VISIBLE_SPAN_NS: u64 = 3_000_000_000; // 3 seconds
/// How much of the future is visible, when the vertical axis is tick based.
/// Expressed in quarter notes (multiplied by `division` at runtime).
const VISIBLE_SPAN_QUARTERS: u32 = 8;
/// Reference roll height the visible spans above are tuned for. The span scales
/// with `height / REFERENCE_HEIGHT`, so pixels-per-time stays constant and a
/// taller window shows more future instead of stretching notes.
const REFERENCE_HEIGHT: f32 = 150.0;

/// Floor for `note_speed` (guards the arithmetic: speed divides the span).
pub const MIN_NOTE_SPEED: f32 = 0.0001;
/// Ceiling for `note_speed`: how far you can zoom in.
pub const MAX_NOTE_SPEED: f32 = 128.0;
/// Floor for the scan lookahead past the top edge, as a duration. At maximum
/// zoom a purely span-relative margin would shrink to milliseconds.
pub const MARGIN_FLOOR_NS: u64 = 500_000_000; // 0.5 seconds

/// How dark a note's 1px outline is, as a percentage of its own colour.
const BORDER_SHADE: u32 = 45;

/// Polyphony removal: the most notes a key keeps sounding. The oldest goes
/// first; it is buried under every other one, so it could only show again if
/// all of them ended before it did.
const MAX_SOUNDING_PER_KEY: usize = 128;

/// Events the scan aims to process per slice. Big enough that the per-slice
/// overhead (touching every track and key) is noise, small enough that a seek
/// or a new file is answered within a few milliseconds of scanning.
const SLICE_TARGET_EVENTS: u64 = 1_500_000;

/// A forward jump past the scanned range rebuilds only beyond this much song
/// time; anything nearer is scanned through (see `PianoRoll::update`).
const FORWARD_SEEK_NS: u64 = 2_000_000_000;
// ---------------------------------------------------------------------------
// Data published to the renderer
// ---------------------------------------------------------------------------

/// The piece started in an earlier slice (its `start` is the slice start).
const CONT_BELOW: u8 = 1;
/// The piece continues into the next slice (its `end` is the slice end).
const CONT_ABOVE: u8 = 2;

/// One visible stretch of one note on one key, clipped to its slice. Pieces of
/// one stretch in consecutive slices carry CONT_ flags and are joined back up at
/// draw time, so slice boundaries never show as outlines.
#[derive(Clone, Copy)]
struct Seg {
    start: u32,
    end: u32,
    color: u16,
    flags: u8,
}

/// Every key's segments for song ticks [t0, t1). Immutable once published.
struct Slice {
    t0: u32,
    t1: u32,
    /// Key k's segments are `segs[offsets[k]..offsets[k + 1]]`, in start order,
    /// never overlapping.
    offsets: Box<[u32; 129]>,
    segs: Vec<Seg>,
}

impl Slice {
    #[inline]
    fn key(&self, k: usize) -> &[Seg] {
        &self.segs[self.offsets[k] as usize..self.offsets[k + 1] as usize]
    }
}

/// What the renderer draws from: a run of consecutive slices.
struct Window {
    file_id: u64,
    generation: u64,
    /// Everything below this tick has been scanned (exclusive).
    frontier: u32,
    /// The scan reached the end of the file.
    done: bool,
    slices: Vec<Arc<Slice>>,
}

/// Single-slot, latest-value mailbox between the worker (producer) and the
/// render thread (consumer), built on one atomic pointer exchange. Whoever
/// swaps a pointer out owns it, so there is no lock and nothing to wait on.
struct Mailbox {
    ptr: AtomicPtr<Window>,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// Worker side. A window the renderer never picked up is dropped here, on
    /// the worker.
    fn put(&self, w: Arc<Window>) {
        let new = Arc::into_raw(w) as *mut Window;
        let old = self.ptr.swap(new, Ordering::AcqRel);
        if !old.is_null() {
            // Safety: `old` came from `Arc::into_raw` in an earlier `put`, and
            // the swap made us its sole owner.
            drop(unsafe { Arc::from_raw(old) });
        }
    }

    /// Render side.
    fn take(&self) -> Option<Arc<Window>> {
        let p = self.ptr.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            None
        } else {
            // Safety: as in `put`.
            Some(unsafe { Arc::from_raw(p) })
        }
    }
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Counters the scan publishes for the debug overlay.
#[derive(Default)]
pub struct ScanStats {
    /// MIDI events processed, ever.
    pub events: AtomicU64,
    /// Rebuilds requested, ever (new file or seek).
    pub rebuilds: AtomicU64,
    /// True while the scan is ahead of what the view needs and waiting.
    pub idle: AtomicBool,
}

/// What one `draw` call did, for the debug overlay.
#[derive(Clone, Copy, Default)]
pub struct DrawStats {
    /// Resolving segments into the per-key pixel columns.
    pub fill_ms: f32,
    /// Uploading the column texture and queueing the 384 quads.
    pub upload_ms: f32,
    /// Segments looked at this frame.
    pub segments: u32,
}

struct Shared {
    mailbox: Mailbox,
    /// Lowest tick the renderer may still draw; slices wholly below it retire.
    floor_tick: AtomicU32,
    /// The scan must have reached at least this tick.
    target_tick: AtomicU32,
    stats: ScanStats,
    shutdown: AtomicBool,
}

struct Rebuild {
    data: Arc<MIDIPlayerData>,
    file_id: u64,
    generation: u64,
    start_tick: u32,
    /// Withhold the new window until it reaches this tick (the top edge), so
    /// a seek lands in one piece -- unless `progressive`, when there is nothing
    /// on screen to hold onto and filling in is better than blank.
    priority_tick: u32,
    progressive: bool,
}

// ---------------------------------------------------------------------------
// Render-side handle
// ---------------------------------------------------------------------------

pub struct PianoRoll {
    shared: Arc<Shared>,
    tx: Option<Sender<Rebuild>>,
    garbage: Option<Sender<Arc<Window>>>,
    handle: Option<JoinHandle<()>>,

    current: Option<Arc<Window>>,
    data: Option<Arc<MIDIPlayerData>>,
    file_id: u64,
    /// Generation of the newest rebuild requested.
    generation: u64,
    last_tick: u32,
    /// A rebuild was asked for during a seek drag and is owed once
    /// the drag ends (see `update`).
    rebuild_deferred: bool,

    // Render resources, reused across frames.
    texture: Option<raylib::ffi::Texture2D>,
    tex_h: usize,
    pixels: Vec<u32>,
    row_tick: Vec<u32>,
    col_owner: Vec<u32>,
    col_color: Vec<u16>,
    merged: Vec<Merged>,
    /// Packed RGBA (normal, darkened) per palette index, filled on first use.
    palette: Vec<(u32, u32)>,
}

/// A segment joined back up across slices, as the draw sees it.
#[derive(Clone, Copy)]
struct Merged {
    start: u32,
    /// `u32::MAX` while the note is still sounding at the scan frontier.
    end: u32,
    color: u16,
}

impl PianoRoll {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            mailbox: Mailbox::new(),
            floor_tick: AtomicU32::new(0),
            target_tick: AtomicU32::new(0),
            stats: ScanStats::default(),
            shutdown: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel::<Rebuild>();
        let (gtx, grx) = mpsc::channel::<Arc<Window>>();
        let s = shared.clone();
        let handle = thread::Builder::new()
            .name("piano-roll".into())
            .spawn(move || worker_main(s, rx, grx))
            .expect("spawn piano roll worker");
        Self {
            shared,
            tx: Some(tx),
            garbage: Some(gtx),
            handle: Some(handle),
            current: None,
            data: None,
            file_id: 0,
            generation: 0,
            last_tick: 0,
            rebuild_deferred: false,
            texture: None,
            tex_h: 0,
            pixels: Vec::new(),
            row_tick: Vec::new(),
            col_owner: Vec::new(),
            col_color: Vec::new(),
            merged: Vec::new(),
            palette: vec![(0, 0); 65536],
        }
    }

    /// True while the newest rebuild has not produced a window yet.
    pub fn rebuilding(&self) -> bool {
        self.current.as_ref().map(|w| w.generation) != Some(self.generation)
    }

    /// The scan frontier of the window on screen.
    pub fn frontier_tick(&self) -> u32 {
        self.current.as_ref().map(|w| w.frontier).unwrap_or(0)
    }

    pub fn scan_done(&self) -> bool {
        self.current.as_ref().is_some_and(|w| w.done)
    }

    fn replace_current(&mut self, w: Option<Arc<Window>>) {
        if let Some(old) = std::mem::replace(&mut self.current, w) {
            // Freed on the worker, never here.
            if let Some(g) = &self.garbage {
                let _ = g.send(old);
            }
        }
    }

    /// Ask the worker for a fresh window at `current_tick`.
    fn request_rebuild(&mut self, data: &Arc<MIDIPlayerData>, current_tick: u32, top_tick: u32) {
        self.generation += 1;
        self.shared.stats.rebuilds.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = &self.tx {
            let _ = tx.send(Rebuild {
                data: data.clone(),
                file_id: self.file_id,
                generation: self.generation,
                start_tick: current_tick,
                priority_tick: top_tick,
                progressive: self.current.is_none(),
            });
        }
    }

    /// Per-frame bookkeeping, before drawing: pick up the newest window, ask
    /// for a rebuild on a new file or a seek, and tell the scan how far ahead
    /// to stay. `top_tick` is the top edge of the view, `target_tick` how far
    /// the scan must have reached. While `seek_dragging`, rebuilds are
    /// deferred to a single one when the drag ends.
    pub fn update(
        &mut self,
        data: &Arc<MIDIPlayerData>,
        parser: &KazuMIDIParserData,
        current_tick: u32,
        top_tick: u32,
        target_tick: u32,
        seek_dragging: bool,
    ) {
        while let Some(w) = self.shared.mailbox.take() {
            if w.file_id == self.file_id {
                self.replace_current(Some(w));
            } else if let Some(g) = &self.garbage {
                let _ = g.send(w);
            }
        }

        let data_changed = match &self.data {
            Some(d) => !Arc::ptr_eq(d, data),
            None => true,
        };
        // Backwards is always a seek. Forwards only when playback is well past
        // everything scanned (FORWARD_SEEK_NS of song): anything closer is
        // cheaper to scan through than to rebuild, since a rebuild has to work
        // out every note held at the new position first. Rebuilding on any
        // gap at all looped on polyphony bombs: the rebuild's setup took long
        // enough that its window landed behind the playhead again. And never
        // while a rebuild is already on its way, whose window has yet to land.
        let frontier = self.frontier_tick();
        let seeked = current_tick < self.last_tick
            || (!self.rebuilding()
                && !self.scan_done()
                && current_tick > frontier
                && current_tick < parser.max_tick
                && tick_to_ns(parser, current_tick).saturating_sub(tick_to_ns(parser, frontier))
                    > FORWARD_SEEK_NS);
        self.last_tick = current_tick;

        if data_changed || seeked {
            if data_changed {
                // The notes on screen belong to a file that is gone.
                self.file_id += 1;
                self.data = Some(data.clone());
                self.replace_current(None);
            }
            if seek_dragging {
                // A drag asks for a rebuild per preview step, and the
                // setup costs hundreds of ms on a dense file, so
                // nearly every window was discarded unseen (measured:
                // 21 of 24 rebuilds in one drag). Owe one instead and
                // pay it at the release position below.
                self.rebuild_deferred = true;
            } else {
                self.rebuild_deferred = false;
                self.request_rebuild(data, current_tick, top_tick);
            }
        } else if !seek_dragging && self.rebuild_deferred {
            // The drag ended: rebuild once at the settled position.
            self.rebuild_deferred = false;
            self.request_rebuild(data, current_tick, top_tick);
        }
        self.shared.target_tick.store(target_tick, Ordering::Relaxed);
    }

    /// Colour of the note showing at `tick` on each key, for the keyboard
    /// highlight. The same note the roll shows at the playhead.
    pub fn sounding_colors(&self, tick: u32) -> [Option<u16>; 128] {
        let mut out = [None; 128];
        let Some(w) = &self.current else {
            return out;
        };
        // The slice holding `tick`: the last one starting at or before it.
        let i = w.slices.partition_point(|s| s.t0 <= tick);
        if i == 0 {
            return out;
        }
        let slice = &w.slices[i - 1];
        if tick >= slice.t1 {
            return out;
        }
        for (k, slot) in out.iter_mut().enumerate() {
            let segs = slice.key(k);
            let j = segs.partition_point(|s| s.start <= tick);
            if j > 0 {
                let s = segs[j - 1];
                if s.end > tick || s.flags & CONT_ABOVE != 0 {
                    *slot = Some(s.color);
                }
            }
        }
        out
    }

    fn colors(&mut self, idx: u16) -> (u32, u32) {
        let e = self.palette[idx as usize];
        if e.0 != 0 {
            return e;
        }
        let (r, g, b) = get_note_color_by_index(idx);
        let pack = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16) | (255 << 24);
        let (r, g, b) = (r as u32, g as u32, b as u32);
        let e = (
            pack(r, g, b),
            pack(
                r * BORDER_SHADE / 100,
                g * BORDER_SHADE / 100,
                b * BORDER_SHADE / 100,
            ),
        );
        self.palette[idx as usize] = e;
        e
    }

    /// Draw the roll into `area`. The bottom edge is "now", the top edge the
    /// furthest visible future point.
    pub fn draw(
        &mut self,
        _d: &mut impl RaylibDraw,
        parser: &KazuMIDIParserData,
        playback_position_ns: u64,
        area: Rectangle,
        is_tick_based: bool,
        note_speed: f32,
    ) -> DrawStats {
        let mut stats = DrawStats::default();
        let t_fill = Instant::now();

        let current_tick = ns_to_tick(parser, playback_position_ns);
        let speed = note_speed.max(MIN_NOTE_SPEED);
        let height_scale = (area.height / REFERENCE_HEIGHT).max(0.01);
        let h = (area.height.ceil() as usize).max(1);

        // Row grid, bottom-up: row b covers ticks [row_tick[b], row_tick[b+1]].
        // Anchored absolutely and snapped to whole rows, as before, so notes
        // move in whole pixel steps.
        self.row_tick.resize(h + 1, 0);
        let draw_tick = if is_tick_based {
            let division = (parser.header.division as u32).max(1);
            let span = (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32)
                .max(1);
            let row_span = (span / h as u32).max(1);
            let draw_tick = current_tick - current_tick % row_span;
            for (b, slot) in self.row_tick.iter_mut().enumerate() {
                *slot = draw_tick.saturating_add((b as u32).saturating_mul(row_span));
            }
            draw_tick
        } else {
            let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
            let row_span_ns = (span_ns / h as u64).max(1);
            let draw_ns = playback_position_ns - playback_position_ns % row_span_ns;
            for (b, slot) in self.row_tick.iter_mut().enumerate() {
                *slot = ns_to_tick(parser, draw_ns.saturating_add(b as u64 * row_span_ns));
            }
            ns_to_tick(parser, draw_ns)
        };
        self.shared.floor_tick.store(draw_tick, Ordering::Relaxed);

        // Column texture: x 0..128 = key colours, x 128..256 = the same
        // darkened (side outlines). Row 0 = top of the view.
        if self.tex_h != h || self.texture.is_none() {
            if let Some(t) = self.texture.take() {
                unsafe { raylib::ffi::UnloadTexture(t) };
            }
            self.texture = Some(column_texture_create(h));
            self.tex_h = h;
        }
        self.pixels.clear();
        self.pixels.resize(256 * h, 0);

        if let Some(win) = self.current.clone() {
            // Highest tick with scanned data. Rows above it stay empty.
            let data_top = if win.done {
                u32::MAX
            } else {
                win.frontier.saturating_sub(1)
            };
            let view_top = self.row_tick[h];
            // Rows that can show anything: lo <= hi after clamping to the
            // playhead and the frontier (the old rasterizer's clamps).
            let mut rows = 0usize;
            while rows < h {
                let lo = self.row_tick[rows].max(current_tick);
                let hi = self.row_tick[rows + 1].min(data_top);
                if hi < lo {
                    break;
                }
                rows += 1;
            }
            let first = win.slices.partition_point(|s| s.t1 <= draw_tick);
            self.col_owner.resize(h, u32::MAX);
            self.col_color.resize(h, 0);

            for k in 0..128usize {
                // 1. Join this key's pieces in view back into whole segments.
                self.merged.clear();
                let mut pending: Option<(Merged, bool)> = None; // (seg, open above)
                for slice in &win.slices[first..] {
                    if slice.t0 > view_top {
                        break;
                    }
                    let segs = slice.key(k);
                    // Pieces ending below the view are of no interest; ends
                    // ascend, since segments never overlap.
                    let from = segs.partition_point(|s| s.end < draw_tick);
                    for s in &segs[from..] {
                        if s.start > view_top {
                            break;
                        }
                        stats.segments += 1;
                        if s.flags & CONT_BELOW != 0 {
                            match pending.as_mut() {
                                Some((m, open)) => {
                                    m.end = s.end;
                                    *open = s.flags & CONT_ABOVE != 0;
                                }
                                // Its start lies below the view.
                                None => {
                                    pending = Some((
                                        Merged {
                                            start: 0,
                                            end: s.end,
                                            color: s.color,
                                        },
                                        s.flags & CONT_ABOVE != 0,
                                    ))
                                }
                            }
                            continue;
                        }
                        if let Some((mut m, open)) = pending.take() {
                            if open {
                                m.end = u32::MAX;
                            }
                            self.merged.push(m);
                        }
                        pending = Some((
                            Merged {
                                start: s.start,
                                end: s.end,
                                color: s.color,
                            },
                            s.flags & CONT_ABOVE != 0,
                        ));
                    }
                }
                if let Some((mut m, open)) = pending.take() {
                    if open {
                        m.end = u32::MAX;
                    }
                    self.merged.push(m);
                }
                if self.merged.is_empty() {
                    continue;
                }

                // 2. Resolve rows. A row shows the last segment that starts at
                //    or before its top and ends at or after its bottom; later
                //    segments overwrite earlier ones on a shared row. Both
                //    bounds only move up, so two cursors replace a search.
                for b in 0..rows {
                    self.col_owner[b] = u32::MAX;
                }
                let mut b_lo = 0usize;
                let mut b_hi = 0usize;
                for (i, m) in self.merged.iter().enumerate() {
                    // First row whose top reaches the segment's start.
                    while b_lo < rows && self.row_tick[b_lo + 1].min(data_top) < m.start {
                        b_lo += 1;
                    }
                    // One past the last row whose bottom the segment reaches.
                    if b_hi < b_lo {
                        b_hi = b_lo;
                    }
                    while b_hi < rows && self.row_tick[b_hi].max(current_tick) <= m.end {
                        b_hi += 1;
                    }
                    for b in b_lo..b_hi {
                        self.col_owner[b] = i as u32;
                        self.col_color[b] = m.color;
                    }
                }

                // 3. Write the column, outlining each run of one segment at its
                //    top and bottom -- except where the run is cut off by the
                //    view's own edge rather than ending there.
                let mut b = 0usize;
                while b < rows {
                    let who = self.col_owner[b];
                    if who == u32::MAX {
                        b += 1;
                        continue;
                    }
                    let run_lo = b;
                    while b < rows && self.col_owner[b] == who {
                        b += 1;
                    }
                    let run_hi = b - 1;
                    let m = self.merged[who as usize];
                    let (normal, dark) = self.colors(self.col_color[run_lo]);
                    let edge_top = run_hi + 1 < h || m.end <= data_top.min(view_top);
                    let edge_bottom = run_lo > 0 || m.start >= current_tick;
                    for r in run_lo..=run_hi {
                        let edge = (r == run_hi && edge_top) || (r == run_lo && edge_bottom);
                        let y = h - 1 - r;
                        self.pixels[y * 256 + k] = if edge { dark } else { normal };
                        self.pixels[y * 256 + 128 + k] = dark;
                    }
                }
            }
        }
        stats.fill_ms = t_fill.elapsed().as_secs_f32() * 1000.0;

        let t_up = Instant::now();
        let tex = self.texture.unwrap();
        unsafe {
            raylib::ffi::UpdateTexture(tex, self.pixels.as_ptr() as *const std::ffi::c_void);
        }
        let regions = get_keyboard_note_regions(area.width);
        let hf = h as f32;
        let quad = |src_x: f32, x: f32, w: f32| unsafe {
            raylib::ffi::DrawTexturePro(
                tex,
                raylib::ffi::Rectangle {
                    x: src_x,
                    y: 0.0,
                    width: 1.0,
                    height: hf,
                },
                raylib::ffi::Rectangle {
                    x,
                    y: area.y,
                    width: w,
                    height: hf,
                },
                raylib::ffi::Vector2 { x: 0.0, y: 0.0 },
                0.0,
                raylib::ffi::Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
            );
        };
        // White keys first, black keys on top: black regions overlap their
        // white neighbours, as on a real keyboard.
        for black in [false, true] {
            for (k, &(rx, rw)) in regions.iter().enumerate() {
                if is_black_key(k as u8) != black {
                    continue;
                }
                let x = area.x + rx;
                quad(k as f32, x, rw);
                quad(128.0 + k as f32, x, 1.0);
                quad(128.0 + k as f32, x + rw - 1.0, 1.0);
            }
        }
        stats.upload_ms = t_up.elapsed().as_secs_f32() * 1000.0;
        stats
    }

    /// Stop the worker and free the GPU texture. Call before the window closes.
    pub fn shutdown(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.tx.take();
        self.replace_current(None);
        self.garbage.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(t) = self.texture.take() {
            unsafe { raylib::ffi::UnloadTexture(t) };
        }
    }
}

fn column_texture_create(h: usize) -> raylib::ffi::Texture2D {
    unsafe {
        let img = raylib::ffi::GenImageColor(
            256,
            h as i32,
            raylib::ffi::Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
        );
        let tex = raylib::ffi::LoadTextureFromImage(img);
        raylib::ffi::UnloadImage(img);
        // Point sampling: each quad must read exactly its own column.
        raylib::ffi::SetTextureFilter(tex, 0);
        tex
    }
}

/// The topmost visible tick, used to size the scan's lookahead.
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
        let span =
            (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
        let current = ns_to_tick(data, playback_position_ns);
        current.saturating_add(span)
    } else {
        let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
        ns_to_tick(data, playback_position_ns.saturating_add(span_ns))
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// A note event bound for its key's stack. `track` + `id` identify the note
/// (ids are per track), so a note-off finds exactly the note it closes.
#[derive(Clone, Copy)]
struct Ev {
    tick: u32,
    id: u32,
    track: u16,
    color: u16,
    key: u8,
    on: bool,
}

/// Ringing notes of one (channel, key) voice in a track, oldest first. Most
/// voices hold one note at a time, so that one lives inline.
#[derive(Default)]
enum VoiceQueue {
    #[default]
    Empty,
    Single(u32),
    Queue(VecDeque<u32>),
}

impl VoiceQueue {
    fn push_back(&mut self, id: u32) {
        match self {
            VoiceQueue::Empty => *self = VoiceQueue::Single(id),
            VoiceQueue::Single(first) => {
                let mut q = VecDeque::with_capacity(4);
                q.push_back(*first);
                q.push_back(id);
                *self = VoiceQueue::Queue(q);
            }
            VoiceQueue::Queue(q) => q.push_back(id),
        }
    }

    fn pop_front(&mut self) -> Option<u32> {
        match self {
            VoiceQueue::Empty => None,
            VoiceQueue::Single(first) => {
                let id = *first;
                *self = VoiceQueue::Empty;
                Some(id)
            }
            VoiceQueue::Queue(q) => {
                let id = q.pop_front();
                if q.is_empty() {
                    *self = VoiceQueue::Empty;
                }
                id
            }
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, VoiceQueue::Empty)
    }
}

/// One (channel, key) voice of a track: its ringing notes, oldest first.
#[derive(Default)]
struct Voice {
    /// Notes held at the seek point that are buried too deep on their key to
    /// ever show (outside the key's newest MAX_SOUNDING_PER_KEY). They are the
    /// voice's oldest, so they sit at the front: their note-offs come first
    /// and are consumed here, keeping FIFO pairing exact for the notes behind
    /// them, without the scan carrying an id for each. On a polyphony bomb
    /// that is millions of notes the seek no longer has to materialize.
    phantoms: u32,
    q: VoiceQueue,
}

struct TrackState {
    cursor: TrackCursor,
    /// The cursor holds an unprocessed event at or below the end tick.
    live: bool,
    /// Open notes per (channel, key). A note-off closes the oldest one (FIFO).
    voices: FxHashMap<(u8, u8), Voice>,
    next_id: u32,
    /// This slice's events, in track order.
    out: Vec<Ev>,
}

/// One track's events for the current slice, grouped by key. Kept apart from
/// `TrackState` so the per-key pass can read every track's output from many
/// threads without touching (non-`Sync`) cursors.
struct TrackOut {
    by_key: Vec<Ev>,
    /// Key k's events are `by_key[offsets[k]..offsets[k + 1]]`.
    offsets: [u32; 129],
}

impl TrackState {
    /// Advance past tempo events to the next note-relevant one.
    fn advance(&mut self, end_tick: u32) {
        loop {
            // Safety: the cursor stays valid until it reports no more events.
            if !unsafe { cursor_next(&mut self.cursor) } {
                self.live = false;
                return;
            }
            if !self.cursor.is_tempo {
                break;
            }
        }
        self.live = self.cursor.abs_tick <= end_tick;
    }

    /// Process this track's events in [.., t1) into `out`, then group by key.
    fn scan(&mut self, dst: &mut TrackOut, track: u16, t1: u32, end_tick: u32) -> u64 {
        self.out.clear();
        let mut events = 0u64;
        while self.live && self.cursor.abs_tick < t1 {
            let c = &self.cursor;
            let tick = c.abs_tick;
            let msg = c.status & 0xF0;
            let key = c.param1;
            if key < 128 {
                let channel = c.status & 0x0F;
                if msg == 0x90 && c.param2 > 0 {
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1);
                    self.voices.entry((channel, key)).or_default().q.push_back(id);
                    self.out.push(Ev {
                        tick,
                        id,
                        track,
                        color: note_color_index(track, channel),
                        key,
                        on: true,
                    });
                } else if msg == 0x80 || (msg == 0x90 && c.param2 == 0) {
                    let voice = (channel, key);
                    if let Some(v) = self.voices.get_mut(&voice) {
                        if v.phantoms > 0 {
                            // Closes a buried note: nothing visible changes.
                            v.phantoms -= 1;
                        } else if let Some(id) = v.q.pop_front() {
                            self.out.push(Ev {
                                tick,
                                id,
                                track,
                                color: 0,
                                key,
                                on: false,
                            });
                        }
                        if v.phantoms == 0 && v.q.is_empty() {
                            self.voices.remove(&voice);
                        }
                    }
                }
            }
            events += 1;
            self.advance(end_tick);
        }

        // Counting sort by key; stable, so each key's events keep their order.
        let mut counts = [0u32; 129];
        for e in &self.out {
            counts[e.key as usize + 1] += 1;
        }
        for k in 0..128 {
            counts[k + 1] += counts[k];
        }
        dst.offsets = counts;
        dst.by_key.clear();
        dst.by_key.resize(
            self.out.len(),
            Ev {
                tick: 0,
                id: 0,
                track: 0,
                color: 0,
                key: 0,
                on: false,
            },
        );
        let mut pos = counts;
        for e in &self.out {
            let p = &mut pos[e.key as usize];
            dst.by_key[*p as usize] = *e;
            *p += 1;
        }
        events
    }
}

/// A note's identity: (track, per-track id).
type NoteId = (u16, u32);

/// The key's current top segment, not yet handed over.
#[derive(Clone, Copy)]
struct Cur {
    start: u32,
    color: u16,
    /// Continues a piece from the previous slice.
    from_prev: bool,
    note: NoteId,
}

#[derive(Default)]
struct KeyState {
    /// Sounding notes, oldest first; the back one is the one on top.
    stack: VecDeque<(u16, u32, u16)>, // (track, id, colour)
    cur: Option<Cur>,
    /// The segment last closed: whose it was, the tick it closed at, and its
    /// index in this slice's output. If the same note is back on top at the
    /// same tick (something covered it for zero time), that segment is
    /// reopened rather than split in two, which would draw an outline across
    /// a note that never visibly changed.
    last_closed: Option<(NoteId, u32, usize)>,
}

impl KeyState {
    /// The top changes at `tick`: close the current segment, open `new`'s.
    fn switch(&mut self, tick: u32, new: Option<(u16, u32, u16)>, out: &mut Vec<Seg>) {
        if let Some(c) = self.cur.take() {
            // A segment covered on its own start tick was never visible, so
            // it is dropped -- unless it continues an earlier piece, which
            // needs this zero-length close to end where it really ends.
            if c.from_prev || c.start < tick {
                out.push(Seg {
                    start: c.start,
                    end: tick,
                    color: c.color,
                    flags: if c.from_prev { CONT_BELOW } else { 0 },
                });
                self.last_closed = Some((c.note, tick, out.len() - 1));
            }
        }
        let Some((track, id, color)) = new else {
            return;
        };
        if let Some((note, at, idx)) = self.last_closed {
            if note == (track, id) && at == tick && idx + 1 == out.len() {
                let s = out.pop().unwrap();
                self.last_closed = None;
                self.cur = Some(Cur {
                    start: s.start,
                    color: s.color,
                    from_prev: s.flags & CONT_BELOW != 0,
                    note,
                });
                return;
            }
        }
        self.cur = Some(Cur {
            start: tick,
            color,
            from_prev: false,
            note: (track, id),
        });
    }

    /// Run one slice of this key's events (in event order) through the stack.
    fn process(&mut self, evs: &[Ev], t1: u32, last: bool, end_tick: u32, out: &mut Vec<Seg>) {
        for e in evs {
            if e.on {
                self.stack.push_back((e.track, e.id, e.color));
                if self.stack.len() > MAX_SOUNDING_PER_KEY {
                    self.stack.pop_front();
                }
                self.switch(e.tick, Some((e.track, e.id, e.color)), out);
            } else if let Some(pos) = self
                .stack
                .iter()
                .position(|s| s.0 == e.track && s.1 == e.id)
            {
                // Ending a covered note changes nothing on screen; ending the
                // top one uncovers the next newest, resuming from here.
                let was_top = pos + 1 == self.stack.len();
                self.stack.remove(pos);
                if was_top {
                    let next = self.stack.back().copied();
                    self.switch(e.tick, next, out);
                }
            }
        }
        if last {
            // End of file: whatever still rings ends at the final tick.
            self.switch(end_tick, None, out);
            self.stack.clear();
        } else if let Some(c) = self.cur.as_mut() {
            out.push(Seg {
                start: c.start,
                end: t1,
                color: c.color,
                flags: CONT_ABOVE | if c.from_prev { CONT_BELOW } else { 0 },
            });
            c.start = t1;
            c.from_prev = true;
        }
    }
}

struct Scan {
    /// Never read: held so the parser data the track cursors point into
    /// outlives them.
    #[allow(dead_code)]
    data: Arc<MIDIPlayerData>,
    file_id: u64,
    generation: u64,
    priority_tick: u32,
    revealed: bool,
    end_tick: u32,
    frontier: u32,
    done: bool,
    tracks: Vec<TrackState>,
    outs: Vec<TrackOut>,
    keys: Vec<KeyState>,
    key_bufs: Vec<Vec<Ev>>,
    key_out: Vec<Vec<Seg>>,
    slices: Vec<Arc<Slice>>,
    /// Ticks per slice, adapted to aim at `SLICE_TARGET_EVENTS`.
    span: u32,
}

impl Scan {
    /// Seek every track to `start_tick` in parallel and seed the key stacks
    /// with the notes already sounding there.
    fn start(req: Rebuild, ckpts: &HeldCheckpointStore, pool: &rayon::ThreadPool) -> Self {
        // Safety: the Arc keeps the C MIDI struct alive for this scan.
        let parser = unsafe { &*req.data.as_ptr() };
        let end_tick = parser.max_tick;
        let num_tracks = parser.header.tracks as usize;
        let start_tick = req.start_tick;

        let data = &req.data;
        let seeded: Vec<(TrackCursor, Vec<HeldNote>)> = pool.install(|| {
            (0..num_tracks)
                .into_par_iter()
                .map(|ti| {
                    // Safety: the Arc keeps the C MIDI struct alive for this call.
                    let parser = unsafe { &*data.as_ptr() };
                    let base = ckpts.base_for(ti, start_tick);
                    let mut deposited = Vec::new();
                    // Safety: ti < num_tracks; parser valid for this call.
                    let r = unsafe {
                        seek_track_cursor_with_held_ckpt(
                            parser,
                            ti as u16,
                            start_tick,
                            MAX_SOUNDING_PER_KEY,
                            base.as_ref(),
                            &mut deposited,
                        )
                    };
                    ckpts.merge(ti, deposited);
                    r
                })
                .collect()
        });

        let (cursors, helds): (Vec<TrackCursor>, Vec<Vec<HeldNote>>) = seeded.into_iter().unzip();

        // Held notes are ordered by HeldKey = (start, track, index in the
        // track's held list): event order. Each key keeps only its newest
        // MAX_SOUNDING_PER_KEY (polyphony removal). Pass 1 finds, per key, the
        // oldest note that still makes the cut, in parallel; each partial list
        // is trimmed as it grows, so a polyphony bomb never builds a full one.
        type HeldKey = (u32, u16, u32);
        fn keep_newest(v: &mut Vec<HeldKey>) {
            if v.len() > MAX_SOUNDING_PER_KEY {
                v.sort_unstable();
                let cut = v.len() - MAX_SOUNDING_PER_KEY;
                v.drain(..cut);
            }
        }
        let empty = || (0..128).map(|_| Vec::<HeldKey>::new()).collect::<Vec<_>>();
        let newest: Vec<Vec<HeldKey>> = pool.install(|| {
            helds
                .par_iter()
                .enumerate()
                .fold(empty, |mut acc, (ti, held)| {
                    for (i, n) in held.iter().enumerate() {
                        let v = &mut acc[n.key as usize];
                        v.push((n.start_tick, ti as u16, i as u32));
                        if v.len() >= 8 * MAX_SOUNDING_PER_KEY {
                            keep_newest(v);
                        }
                    }
                    acc
                })
                .reduce(empty, |mut a, b| {
                    for (va, vb) in a.iter_mut().zip(b) {
                        va.extend(vb);
                        keep_newest(va);
                    }
                    a
                })
        });
        // A held note is kept iff it is at or above its key's cutoff.
        let cutoff: Vec<Option<HeldKey>> = newest
            .into_iter()
            .map(|mut v| {
                keep_newest(&mut v);
                if v.len() == MAX_SOUNDING_PER_KEY {
                    v.iter().min().copied()
                } else {
                    None
                }
            })
            .collect();

        // Pass 2, per track in parallel: kept notes get an id (and go to their
        // key's stack); buried ones only bump their voice's phantom count.
        // Within a voice the kept notes are its newest, so phantoms are always
        // the front of the queue, as FIFO pairing needs.
        type Kept = (HeldKey, u8, u32, u16); // (order, key, id, colour)
        let built: Vec<(TrackState, Vec<Kept>)> = pool.install(|| {
            cursors
                .into_par_iter()
                .zip(helds.into_par_iter())
                .enumerate()
                .map(|(ti, (cursor, held))| {
                    let mut voices: FxHashMap<(u8, u8), Voice> = FxHashMap::default();
                    let mut kept = Vec::new();
                    let mut next_id = 0u32;
                    // `held` is in note-on order, so each voice fills oldest first.
                    for (i, n) in held.iter().enumerate() {
                        let order = (n.start_tick, ti as u16, i as u32);
                        let voice = voices.entry((n.channel, n.key)).or_default();
                        if cutoff[n.key as usize].is_some_and(|c| order < c) {
                            voice.phantoms += 1;
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        voice.q.push_back(id);
                        kept.push((order, n.key, id, note_color_index(ti as u16, n.channel)));
                    }
                    let live =
                        cursor.has_event && !cursor.is_tempo && cursor.abs_tick <= end_tick;
                    (
                        TrackState {
                            cursor,
                            live,
                            voices,
                            next_id,
                            out: Vec::new(),
                        },
                        kept,
                    )
                })
                .collect()
        });

        // Pass 3: stack each key's kept notes in event order; the newest is
        // the one on top.
        let mut by_key: Vec<Vec<Kept>> = (0..128).map(|_| Vec::new()).collect();
        let mut tracks = Vec::with_capacity(num_tracks);
        for (ts, kept) in built {
            tracks.push(ts);
            for k in kept {
                by_key[k.1 as usize].push(k);
            }
        }
        let mut keys: Vec<KeyState> = (0..128).map(|_| KeyState::default()).collect();
        for (k, mut v) in by_key.into_iter().enumerate() {
            v.sort_unstable_by_key(|e| e.0);
            let ks = &mut keys[k];
            for &((_, track, _), _, id, color) in &v {
                ks.stack.push_back((track, id, color));
            }
            // Only the top one shows. It starts where it really started.
            if let Some(&((start, track, _), _, id, color)) = v.last() {
                ks.cur = Some(Cur {
                    start,
                    color,
                    from_prev: false,
                    note: (track, id),
                });
            }
        }

        let division = (parser.header.division as u32).max(1);
        Self {
            data: req.data,
            file_id: req.file_id,
            generation: req.generation,
            priority_tick: req.priority_tick,
            revealed: req.progressive,
            end_tick,
            frontier: start_tick,
            done: false,
            outs: (0..tracks.len())
                .map(|_| TrackOut {
                    by_key: Vec::new(),
                    offsets: [0; 129],
                })
                .collect(),
            tracks,
            keys,
            key_bufs: (0..128).map(|_| Vec::new()).collect(),
            key_out: (0..128).map(|_| Vec::new()).collect(),
            slices: Vec::new(),
            span: division,
        }
    }

    /// Scan the next slice, [frontier, t1).
    fn step(&mut self, goal: u32, pool: &rayon::ThreadPool, stats: &ScanStats) {
        let t0 = self.frontier;
        let mut t1 = t0.saturating_add(self.span).min(goal.max(t0.saturating_add(1)));
        let last = t1 > self.end_tick;
        if last {
            t1 = self.end_tick.saturating_add(1);
        }
        let end_tick = self.end_tick;

        // 1. Every track, in parallel: its events in [t0, t1), grouped by key.
        let tracks = &mut self.tracks;
        let outs = &mut self.outs;
        let events: u64 = pool.install(|| {
            tracks
                .par_iter_mut()
                .zip(outs.par_iter_mut())
                .enumerate()
                .map(|(ti, (ts, dst))| ts.scan(dst, ti as u16, t1, end_tick))
                .sum()
        });

        // 2. Every key, in parallel: gather its events from each track in
        //    track order, sort by tick (stable: ties keep track order, then the
        //    track's own order -- event order), and run them through the stack.
        let tracks = &self.outs;
        pool.install(|| {
            self.keys
                .par_iter_mut()
                .zip(self.key_bufs.par_iter_mut())
                .zip(self.key_out.par_iter_mut())
                .enumerate()
                .for_each(|(k, ((ks, buf), out))| {
                    buf.clear();
                    for ts in tracks.iter() {
                        let (a, b) = (ts.offsets[k] as usize, ts.offsets[k + 1] as usize);
                        if a < b {
                            buf.extend_from_slice(&ts.by_key[a..b]);
                        }
                    }
                    buf.sort_by_key(|e| e.tick);
                    out.clear();
                    ks.process(buf, t1, last, end_tick, out);
                });
        });

        // 3. Freeze the slice.
        let total: usize = self.key_out.iter().map(|o| o.len()).sum();
        let mut segs = Vec::with_capacity(total);
        let mut offsets = Box::new([0u32; 129]);
        for (k, o) in self.key_out.iter().enumerate() {
            segs.extend_from_slice(o);
            offsets[k + 1] = segs.len() as u32;
        }
        self.slices.push(Arc::new(Slice {
            t0,
            t1,
            offsets,
            segs,
        }));
        self.frontier = t1;
        // Tracks that run out early just leave empty slices until the end tick
        // is reached; the span grows 4x per empty slice, so that is quick.
        self.done = last;

        // Aim the next slice at the target event count.
        let ticks = (t1 - t0).max(1) as u64;
        let ideal = if events == 0 {
            self.span as u64 * 4
        } else {
            ticks * SLICE_TARGET_EVENTS / events
        };
        let ideal = ideal.clamp(1, self.span as u64 * 4).min(u32::MAX as u64);
        self.span = ((self.span as u64 + ideal) / 2).max(1) as u32;

        stats.events.fetch_add(events, Ordering::Relaxed);
    }

    /// Drop slices the view has passed. Keeps the one holding `floor`.
    fn retire(&mut self, floor: u32) -> bool {
        let n = self.slices.partition_point(|s| s.t1 <= floor);
        // Never retire the last slice: the next one continues from it.
        let n = n.min(self.slices.len().saturating_sub(1));
        if n > 0 {
            self.slices.drain(..n);
            true
        } else {
            false
        }
    }

    fn window(&self) -> Arc<Window> {
        Arc::new(Window {
            file_id: self.file_id,
            generation: self.generation,
            frontier: self.frontier,
            done: self.done,
            slices: self.slices.clone(),
        })
    }
}

fn worker_main(shared: Arc<Shared>, rx: Receiver<Rebuild>, garbage: Receiver<Arc<Window>>) {
    // Half the cores, at most 8. Measured at ~124M events/s on nearly every
    // core, against the ~3M/s a 1.4M NPS stretch needs: half of that is still
    // dozens of times enough, and it leaves the render and audio threads cores
    // of their own instead of a fight for them (a stolen time slice on the
    // render thread is a missed vsync, i.e. a stutter).
    let threads = (crate::app_state::cpu_threads() as usize / 2).clamp(1, 8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("piano-roll-scan-{i}"))
        .build()
        .expect("piano roll thread pool");

    let mut scan: Option<Scan> = None;
    let mut pending: Option<Rebuild> = None;
    // Held-note checkpoints for the loaded file, so a seek replays a bounded
    // stretch of each track instead of walking it from the start.
    let mut ckpt_slot: Option<(Arc<MIDIPlayerData>, Arc<HeldCheckpointStore>)> = None;

    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Windows the renderer replaced, freed here rather than on its thread.
        while garbage.try_recv().is_ok() {}
        // Only the newest request matters.
        loop {
            match rx.try_recv() {
                Ok(r) => pending = Some(r),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        if let Some(req) = pending.take() {
            drop(scan.take()); // free the old scan first, here on the worker
            let store = match &ckpt_slot {
                Some((d, s)) if Arc::ptr_eq(d, &req.data) => s.clone(),
                _ => {
                    if let Some((_, old)) = &ckpt_slot {
                        old.cancelled.store(true, Ordering::SeqCst);
                    }
                    let n = unsafe { (&*req.data.as_ptr()).header.tracks } as usize;
                    let s = Arc::new(HeldCheckpointStore::new(n));
                    let (bd, bs) = (req.data.clone(), s.clone());
                    thread::spawn(move || build_held_checkpoints(bd, bs));
                    ckpt_slot = Some((req.data.clone(), s.clone()));
                    s
                }
            };
            let s = Scan::start(req, &store, &pool);
            if s.revealed {
                shared.mailbox.put(s.window());
            }
            scan = Some(s);
            continue;
        }

        let mut worked = false;
        if let Some(s) = scan.as_mut() {
            let target = shared.target_tick.load(Ordering::Relaxed);
            let floor = shared.floor_tick.load(Ordering::Relaxed);
            let mut changed = s.retire(floor);
            if !s.done && (s.frontier <= target || !s.revealed) {
                // Behind: scan ahead past the target by as much again as the
                // view needs, so the next refill is a batch, not a trickle.
                let ahead = target.saturating_sub(floor).max(1);
                let goal = target.saturating_add(ahead);
                s.step(goal, &pool, &shared.stats);
                changed = true;
                worked = true;
            }
            // Hold a rebuilt window back until it covers the view as it is
            // *now* (the target follows playback), not just as it was when
            // the rebuild was asked for: setting up a seek on a polyphony bomb
            // takes long enough for playback to move on, and a window shown
            // behind the playhead read as yet another seek.
            if !s.revealed
                && (s.done || (s.frontier > s.priority_tick && s.frontier > target))
            {
                s.revealed = true;
                changed = true;
            }
            if changed && s.revealed {
                shared.mailbox.put(s.window());
            }
        }
        shared.stats.idle.store(!worked, Ordering::Relaxed);

        if !worked {
            // Nothing to do until playback moves or a request arrives.
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(r) => pending = Some(r),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Held-note checkpoints (seek acceleration)
// ---------------------------------------------------------------------------

/// Held-note checkpoints for one loaded file: one list per track, sorted by
/// event count. Built in the background on load and topped up by every seek
/// that walks past a boundary, so a seek replays a bounded number of events
/// per track instead of walking it from the start.
struct HeldCheckpointStore {
    per_track: Vec<Mutex<Vec<HeldCheckpoint>>>,
    cancelled: AtomicBool,
}

impl HeldCheckpointStore {
    fn new(num_tracks: usize) -> Self {
        Self {
            per_track: (0..num_tracks).map(|_| Mutex::new(Vec::new())).collect(),
            cancelled: AtomicBool::new(false),
        }
    }

    /// The newest checkpoint on `track` before `target_tick`.
    fn base_for(&self, track: usize, target_tick: u32) -> Option<HeldCheckpoint> {
        let v = self.per_track.get(track)?.lock().unwrap();
        let idx = v.partition_point(|c| c.tick < target_tick);
        if idx == 0 { None } else { Some(v[idx - 1].clone()) }
    }

    fn merge(&self, track: usize, new: Vec<HeldCheckpoint>) {
        if new.is_empty() {
            return;
        }
        let mut v = self.per_track[track].lock().unwrap();
        for c in new {
            if let Err(pos) = v.binary_search_by_key(&c.event_count, |x| x.event_count) {
                v.insert(pos, c);
            }
        }
    }
}

/// Walk every track of a freshly loaded file once, depositing checkpoints.
fn build_held_checkpoints(data: Arc<MIDIPlayerData>, store: Arc<HeldCheckpointStore>) {
    let num_tracks = unsafe { (&*data.as_ptr()).header.tracks } as usize;
    // Two threads. This walks the entire file once to speed up later seeks;
    // on a 1.6B-note file it used to run on every core for as long as that
    // took, which is what pinned the CPU after loading. Seeks before it
    // finishes still work, they just replay more events.
    let workers = 2usize.min(num_tracks.max(1));
    thread::scope(|scope| {
        for w in 0..workers {
            let data = &data;
            let store = &store;
            scope.spawn(move || {
                // Safety: the Arc keeps the C MIDI struct alive for this call.
                let parser = unsafe { &*data.as_ptr() };
                let mut ti = w;
                while ti < num_tracks {
                    if store.cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut deposited = Vec::new();
                    let _ = unsafe {
                        seek_track_cursor_with_held_ckpt(
                            parser,
                            ti as u16,
                            u32::MAX,
                            MAX_SOUNDING_PER_KEY,
                            None,
                            &mut deposited,
                        )
                    };
                    store.merge(ti, deposited);
                    ti += workers;
                }
            });
        }
    });
}
