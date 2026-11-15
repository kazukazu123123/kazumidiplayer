use kazumidiparser::{KazuMIDIParserData, MIDIPlayerData};
use raylib::prelude::*;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::kazumidiparser_cursor::{
    HeldCheckpoint, TrackCursor, cursor_next, ns_to_tick, seek_track_cursor,
    seek_track_cursor_with_held_ckpt,
};
use crate::ui_components::piano_keyboard::{get_keyboard_note_regions, is_black_key};
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

/// Floor for the span-relative margins the main loop derives from the visible
/// span (the scan's lookahead past the top edge and the zoom rebuild tolerance),
/// expressed as a duration and converted to ticks where it is used. Both margins
/// scale with the span, so at maximum zoom they would otherwise shrink to a few
/// milliseconds: the scan could not stay ahead of the top edge, cutting the roll
/// off there, and any small zoom step would trip the rebuild test. Half a second
/// of music also bounds the extra resident notes to a fixed time window instead
/// of a number of screenfuls.
pub const MARGIN_FLOOR_NS: u64 = 500_000_000; // 0.5 seconds

/// `end_tick` sentinel for a note whose note-on has been scanned but whose
/// note-off has not yet been seen. Such a note is drawn growing from its start up
/// to the current reveal frontier, and its real end is patched in once the
/// note-off is found.
const OPEN_END: u32 = u32::MAX;

/// How dark a note's 1px outline is, as a percentage of the note's own color. A
/// shade of the note rather than flat black: on a dense MIDI the outlines meet, and
/// black ones add up to a black roll.
pub(crate) const BORDER_SHADE: u32 = 45;

/// Reuse raster buffers across frames instead of allocating and freeing three
/// large vectors on every draw. The renderer calls this on one thread, so TLS
/// avoids cross-thread locking and keeps capacity stable across frames.
#[derive(Default)]
struct DrawScratch {
    row_tick: Vec<u32>,
    cols: Vec<u32>,
    owners: Vec<u32>,
    winner_notes: Vec<RollNote>,
    /// The view the cached `cols`/`owners` were rasterized for:
    /// (generation, content_seq, draw_tick, top_tick, effective_top,
    /// height_px, is_tick_based). A frame whose key matches re-emits the
    /// cached rasterization instead of recomputing it; only the rectangle
    /// emit runs. Anything the key does not capture cannot change the
    /// picture: the note set is generation+content_seq, the geometry is
    /// the rest.
    cache_key: Option<(u64, u64, u32, u32, u32, usize, bool)>,
}

thread_local! {
    static DRAW_SCRATCH: RefCell<DrawScratch> = RefCell::new(DrawScratch::default());
}

/// ---------------------------------------------------------------------------
/// Tile-baked roll rendering.
///
/// The direct path re-rasterizes the visible rows and re-emits every note
/// rectangle on every frame, so its cost grows with the resident note count
/// (measured on a 100M-note file at low zoom: raster ~8 ms + emit ~6 ms per
/// frame -- over the 60 fps budget on their own). The tile path instead
/// rasterizes a fixed range of rows into a CPU pixel buffer ONCE, uploads
/// it as a texture, and per frame only blits the one or two tiles the view
/// overlaps: the per-frame cost is then a function of the screen size, not
/// of the note count.
///
/// Rows are anchored absolutely: absolute row A covers the tick range
/// [A * span, (A + 1) * span), where span is the row span of the current
/// zoom (ticks, or ns in time-based mode). Advancing the playhead moves
/// which rows are visible, never what a baked row contains, so playback
/// does not invalidate tiles. Content changes invalidate precisely: the
/// chunk tracks the tick range publishes and retires touched (see
/// `RollChunk::dirty`), and only tiles intersecting it are rebaked.
///
/// Baking is budgeted (a few ms per frame, columns in keyboard z-order);
/// while a visible tile is not ready the caller falls back to the direct
/// path, so the worst case at any moment is exactly the old behaviour.
/// ---------------------------------------------------------------------------

/// Pixel rows per tile. At the zooms where the direct path hurts (low
/// zoom, huge row spans) a tile covers minutes of playback; at high zoom
/// tiles are consumed quickly, but there the direct path is cheap and the
/// fallback covers it.
const TILE_ROWS: usize = 512;
const TILE_SLOTS: usize = 5;
/// Per-frame budget for baking, so a bake can never itself break 60 fps.
const TILE_BAKE_BUDGET_MS: u64 = 3;

/// Everything a baked tile depends on besides the chunk's notes: the note
/// set generation, the row grid (span per row, in ticks or ns according
/// to the mode), and the pixel width the key regions were computed for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TileKey {
    generation: u64,
    span_key: u64,
    is_tick_based: bool,
    width_px: u32,
}

struct RollTile {
    key: Option<TileKey>,
    /// Absolute row index of this tile's bottom row; the tile covers rows
    /// [start_row, start_row + TILE_ROWS), aligned to TILE_ROWS.
    start_row: u64,
    /// Bake progress in key steps (keyboard z-order, whites first).
    steps_done: usize,
    ready: bool,
    /// Absolute row band [lo, hi) of a ready tile that a chunk change
    /// has touched since the bake: only these rows are rebaked. Notes
    /// ending at the playhead dirty just its bottom row, so whole-tile
    /// invalidation would keep that tile rebaking forever.
    dirty_rows: Option<(u64, u64)>,
    /// Dirties that arrived while `dirty_rows` was being rebaked. They
    /// queue instead of merging into the in-flight band: merging reset
    /// its progress every frame, and a wide band (a still-sounding note
    /// rolled off the pinned set dirties everything up to the frontier)
    /// then never finished under per-frame dirties.
    pending_rows: Option<(u64, u64)>,
    /// Key-step progress of an in-flight band rebake.
    band_steps: usize,
    texture: Option<raylib::ffi::Texture2D>,
    /// RGBA pixels, row 0 = the tile's top row.
    pixels: Vec<u8>,
}

impl Default for RollTile {
    fn default() -> Self {
        Self {
            key: None,
            start_row: 0,
            steps_done: 0,
            ready: false,
            dirty_rows: None,
            pending_rows: None,
            band_steps: 0,
            texture: None,
            pixels: Vec::new(),
        }
    }
}

#[derive(Default)]
struct TileScratch {
    row_tick: Vec<u32>,
    cols: Vec<u32>,
    owners: Vec<u32>,
    winner_notes: Vec<RollNote>,
}

#[derive(Default)]
struct TileCache {
    tiles: Vec<RollTile>,
    scratch: TileScratch,
}

thread_local! {
    static TILE_CACHE: RefCell<TileCache> = RefCell::new(TileCache::default());
}

/// Key indices in painting order: every white key first, then every black
/// key on top, matching the direct emit pass (black-key regions overlap
/// their white neighbours).
fn bake_key_order() -> [usize; 128] {
    let mut order = [0usize; 128];
    let mut i = 0;
    for black in [false, true] {
        for n in 0..128 {
            if is_black_key(n as u8) == black {
                order[i] = n;
                i += 1;
            }
        }
    }
    order
}

fn fill_rect(pixels: &mut [u8], width_px: usize, x0: i32, y0: i32, x1: i32, y1: i32, rgba: [u8; 4]) {
    let x0 = x0.max(0);
    let y0 = y0.max(0);
    let x1 = x1.min(width_px as i32);
    let y1 = y1.min(TILE_ROWS as i32);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for y in y0..y1 {
        let row = &mut pixels[(y as usize * width_px + x0 as usize) * 4..(y as usize * width_px + x1 as usize) * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&rgba);
        }
    }
}

/// Paint one rasterized column into a tile's pixel buffer, mirroring the
/// direct emit pass exactly: one rect per contiguous same-colour run,
/// with the run's left and right pixel columns darkened as its edges.
fn paint_column(pixels: &mut [u8], width_px: usize, col: &[u32], rx: f32, rw: f32) {
    let x0 = rx.round() as i32;
    let x1 = (rx + rw).round() as i32;
    let mut yy = 0usize;
    while yy < col.len() {
        let p = col[yy];
        if p == 0 {
            yy += 1;
            continue;
        }
        let run_start = yy;
        while yy < col.len() && col[yy] == p {
            yy += 1;
        }
        let rgba = [(p >> 16) as u8, (p >> 8) as u8, p as u8, 255];
        fill_rect(pixels, width_px, x0, run_start as i32, x1, yy as i32, rgba);
        let e = darken(p);
        let edge = [(e >> 16) as u8, (e >> 8) as u8, e as u8, 255];
        fill_rect(pixels, width_px, x0, run_start as i32, x0 + 1, yy as i32, edge);
        fill_rect(pixels, width_px, x1 - 1, run_start as i32, x1, yy as i32, edge);
    }
}

fn tile_texture_create(width_px: u32) -> raylib::ffi::Texture2D {
    unsafe {
        let img = raylib::ffi::GenImageColor(
            width_px as i32,
            TILE_ROWS as i32,
            raylib::ffi::Color { r: 0, g: 0, b: 0, a: 0 },
        );
        let tex = raylib::ffi::LoadTextureFromImage(img);
        raylib::ffi::UnloadImage(img);
        // Point sampling: the blit lands on whole pixels, and anything
        // else would smear the 1px note edges.
        raylib::ffi::SetTextureFilter(tex, 0);
        tex
    }
}

/// Pick the slot for (key, start): the one already holding it if any,
/// else the most evictable (stale key first, then furthest from the
/// playhead), never a slot holding a currently wanted tile.
fn find_slot(tiles: &[RollTile], key: &TileKey, start: u64, row0: u64, wanted: &[u64]) -> usize {
    if let Some(i) = tiles
        .iter()
        .position(|t| t.key == Some(*key) && t.start_row == start)
    {
        return i;
    }
    let mut best = 0usize;
    let mut best_score = i64::MIN;
    for (i, t) in tiles.iter().enumerate() {
        if t.key == Some(*key) && wanted.contains(&t.start_row) {
            continue;
        }
        let score = if t.key != Some(*key) {
            i64::MAX
        } else {
            (t.start_row as i64 - row0 as i64).abs()
        };
        if score > best_score {
            best_score = score;
            best = i;
        }
    }
    best
}

/// Advance one tile's bake by as many columns as the deadline allows.
/// Returns true when the budget ran out mid-bake.
#[allow(clippy::too_many_arguments)]
fn bake_tile(
    tile: &mut RollTile,
    scratch: &mut TileScratch,
    key: &TileKey,
    start: u64,
    chunk: &RollChunk,
    tick_at: &dyn Fn(u64) -> u32,
    regions: &[(f32, f32); 128],
    frontier: u32,
    deadline: std::time::Instant,
) -> bool {
    if tile.key != Some(*key) || tile.start_row != start {
        if let Some(old) = tile.texture.take() {
            unsafe { raylib::ffi::UnloadTexture(old) };
        }
        tile.key = Some(*key);
        tile.start_row = start;
        tile.steps_done = 0;
        tile.ready = false;
        tile.dirty_rows = None;
        tile.pending_rows = None;
        tile.band_steps = 0;
        tile.pixels.clear();
        tile.pixels.resize(key.width_px as usize * TILE_ROWS * 4, 0);
    }
    if tile.ready {
        return false;
    }
    if tile.steps_done == 0 {
        // Starting a fresh bake: every column reads state from now on,
        // so bands recorded before this point are moot. Bands recorded
        // during the bake survive completion (see below).
        tile.dirty_rows = None;
        tile.pending_rows = None;
        tile.band_steps = 0;
    }
    // Row ticks for the tile, top first: row_tick[j] is the top edge of
    // tile row j, and tile row 0 is the tile's highest absolute row.
    scratch.row_tick.resize(TILE_ROWS + 1, 0);
    for (j, slot) in scratch.row_tick.iter_mut().enumerate() {
        *slot = tick_at(start + (TILE_ROWS - j) as u64);
    }
    let bottom = tick_at(start);
    let top = tick_at(start + TILE_ROWS as u64);
    let effective_top = top.min(frontier).max(bottom);
    scratch.cols.resize(128 * TILE_ROWS, 0);
    scratch.owners.resize(128 * TILE_ROWS, u32::MAX);
    let order = bake_key_order();
    let width_px = key.width_px as usize;
    while tile.steps_done < 128 {
        let note = order[tile.steps_done];
        {
            let col = &mut scratch.cols[note * TILE_ROWS..(note + 1) * TILE_ROWS];
            let own = &mut scratch.owners[note * TILE_ROWS..(note + 1) * TILE_ROWS];
            rasterize_column(
                &chunk.notes_by_key[note],
                col,
                own,
                &scratch.row_tick,
                bottom,
                effective_top,
                &mut scratch.winner_notes,
            );
        }
        let (rx, rw) = regions[note];
        let col = &scratch.cols[note * TILE_ROWS..(note + 1) * TILE_ROWS];
        paint_column(&mut tile.pixels, width_px, col, rx, rw);
        tile.steps_done += 1;
        if std::time::Instant::now() >= deadline {
            return true;
        }
    }
    let tex = match tile.texture {
        Some(t) if t.width == key.width_px as i32 && t.height == TILE_ROWS as i32 => t,
        _ => {
            if let Some(old) = tile.texture.take() {
                unsafe { raylib::ffi::UnloadTexture(old) };
            }
            tile_texture_create(key.width_px)
        }
    };
    tile.texture = Some(tex);
    unsafe {
        raylib::ffi::UpdateTexture(tex, tile.pixels.as_ptr() as *const std::ffi::c_void);
    }
    // dirty_rows is deliberately kept: bands recorded during the bake
    // refer to columns that predate their change, and the band pass
    // fixes them before the tile may blit.
    tile.ready = true;
    false
}

/// The absolute row whose tick range contains `tick`, searched within
/// the tile starting at `start` (clamped to its row range).
fn row_containing(tick_at: &dyn Fn(u64) -> u32, start: u64, tick: u32) -> u64 {
    let mut lo_r = start;
    let mut hi_r = start + TILE_ROWS as u64;
    while lo_r < hi_r {
        let mid = (lo_r + hi_r + 1) / 2;
        if tick_at(mid) <= tick {
            lo_r = mid;
        } else {
            hi_r = mid - 1;
        }
    }
    lo_r
}

/// Rebake only a ready tile's dirty row band. The band is rasterized
/// with a one-row halo on each side (rasterized for context, never
/// painted) so run-edge capping inside the band matches a full bake
/// exactly, and with the tile's own bottom and top as the rasterizer's
/// floor and ceiling for the same reason. Returns true when the budget
/// ran out mid-band.
#[allow(clippy::too_many_arguments)]
fn bake_band(
    tile: &mut RollTile,
    scratch: &mut TileScratch,
    key: &TileKey,
    chunk: &RollChunk,
    tick_at: &dyn Fn(u64) -> u32,
    regions: &[(f32, f32); 128],
    frontier: u32,
    deadline: std::time::Instant,
) -> bool {
    let (b_lo, b_hi) = match tile.dirty_rows {
        Some(r) => r,
        None => return false,
    };
    let start = tile.start_row;
    let h_lo = if b_lo > start { b_lo - 1 } else { b_lo };
    let h_hi = if b_hi < start + TILE_ROWS as u64 { b_hi + 1 } else { b_hi };
    let rows = (h_hi - h_lo) as usize;
    scratch.row_tick.resize(rows + 1, 0);
    for (l, slot) in scratch.row_tick.iter_mut().enumerate() {
        *slot = tick_at(h_hi - l as u64);
    }
    let bottom = tick_at(start);
    let top = tick_at(start + TILE_ROWS as u64);
    let effective_top = top.min(frontier).max(bottom);
    scratch.cols.resize(128 * TILE_ROWS, 0);
    scratch.owners.resize(128 * TILE_ROWS, u32::MAX);
    // Tile-column offset of the halo's top row, and the band's offset
    // both within the halo raster and within the pixel buffer.
    let h_off = (start + TILE_ROWS as u64 - h_hi) as usize;
    let in_halo = (h_hi - b_hi) as usize;
    let b_off = (start + TILE_ROWS as u64 - b_hi) as usize;
    let band_rows = (b_hi - b_lo) as usize;
    let order = bake_key_order();
    let width_px = key.width_px as usize;
    while tile.band_steps < 128 {
        let note = order[tile.band_steps];
        {
            let col = &mut scratch.cols
                [note * TILE_ROWS + h_off..note * TILE_ROWS + h_off + rows];
            let own = &mut scratch.owners
                [note * TILE_ROWS + h_off..note * TILE_ROWS + h_off + rows];
            rasterize_column(
                &chunk.notes_by_key[note],
                col,
                own,
                &scratch.row_tick,
                bottom,
                effective_top,
                &mut scratch.winner_notes,
            );
        }
        let (rx, rw) = regions[note];
        let band = &scratch.cols[note * TILE_ROWS + h_off + in_halo
            ..note * TILE_ROWS + h_off + in_halo + band_rows];
        paint_column(&mut tile.pixels[b_off * width_px * 4..], width_px, band, rx, rw);
        tile.band_steps += 1;
        if std::time::Instant::now() >= deadline {
            return true;
        }
    }
    if let Some(tex) = tile.texture {
        unsafe {
            raylib::ffi::UpdateTexture(tex, tile.pixels.as_ptr() as *const std::ffi::c_void);
        }
    }
    // Promote whatever queued behind this band; it bakes next frame.
    tile.dirty_rows = tile.pending_rows.take();
    tile.band_steps = 0;
    false
}

/// Blit the part of one ready tile that overlaps the visible rows.
fn blit_tile(tile: &RollTile, row0: u64, height_px: usize, area: &Rectangle) {
    let tex = match tile.texture {
        Some(t) => t,
        None => return,
    };
    let ts = tile.start_row;
    let vis_lo = row0.max(ts);
    let vis_hi = (row0 + height_px as u64).min(ts + TILE_ROWS as u64);
    if vis_hi <= vis_lo {
        return;
    }
    // Highest visible absolute row in this tile, and where it lands both
    // in the texture (row 0 = tile top) and on screen (row 0 = view top).
    let a_hi = vis_hi - 1;
    let src_y = (ts + TILE_ROWS as u64 - 1 - a_hi) as f32;
    let src_h = (vis_hi - vis_lo) as f32;
    let dest_y = area.y + (row0 + height_px as u64 - 1 - a_hi) as f32;
    unsafe {
        raylib::ffi::DrawTextureRec(
            tex,
            raylib::ffi::Rectangle {
                x: 0.0,
                y: src_y,
                width: tile.key.map(|k| k.width_px).unwrap_or(0) as f32,
                height: src_h,
            },
            raylib::ffi::Vector2 { x: area.x, y: dest_y },
            raylib::ffi::Color { r: 255, g: 255, b: 255, a: 255 },
        );
    }
}

/// One frame of the tile path: invalidate from the chunk's dirty range,
/// bake within budget, and blit if every visible tile is ready. Returns
/// false when the caller should draw the direct path instead.
fn tile_frame(
    key: &TileKey,
    row0: u64,
    height_px: usize,
    area: &Rectangle,
    regions: &[(f32, f32); 128],
    data: &KazuMIDIParserData,
    chunk: &RollChunk,
    frontier: u32,
) -> bool {
    let tick_at = |row: u64| -> u32 {
        if key.is_tick_based {
            row.saturating_mul(key.span_key).min(u32::MAX as u64) as u32
        } else {
            ns_to_tick(data, row.saturating_mul(key.span_key))
        }
    };
    TILE_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.tiles.is_empty() {
            cache.tiles = (0..TILE_SLOTS).map(|_| RollTile::default()).collect();
        }
        let TileCache { tiles, scratch } = &mut *cache;

        // Invalidate what the dirty range touches: only the rows
        // the change can reach are marked, which for a note ending at
        // the playhead is a single bottom row.
        if let Some((lo, hi)) = chunk.take_dirty() {
            for t in tiles.iter_mut() {
                if t.key != Some(*key) {
                    continue;
                }
                let tlo = tick_at(t.start_row);
                let thi = tick_at(t.start_row + TILE_ROWS as u64);
                if lo >= thi || tlo >= hi {
                    continue;
                }
                // A tile mid-bake is NOT restarted: at low-zoom density
                // a bake spans many frames and dirties land every
                // frame, so restarting would pin it at step zero
                // forever. The band is recorded like a ready tile's
                // and fixed by the band pass once the bake completes;
                // columns baked after the change are just rebaked
                // once more in their band rows, which is cheap.
                // Clip to the visible rows: rows below the playhead
                // cannot be seen again without a seek, and a seek
                // regenerates under a new generation (new tiles).
                // Without this, a retired long note dirties its whole
                // span back to its start, and the playhead tile's band
                // never finishes under constant new dirties.
                let a_lo = row_containing(&tick_at, t.start_row, lo)
                    .max(t.start_row)
                    .max(row0);
                let a_hi = (row_containing(&tick_at, t.start_row, hi.saturating_sub(1)) + 1)
                    .min(t.start_row + TILE_ROWS as u64);
                if a_lo < a_hi {
                    if t.ready && t.dirty_rows.is_some() {
                        t.pending_rows = Some(match t.pending_rows {
                            Some((plo, phi)) => (plo.min(a_lo), phi.max(a_hi)),
                            None => (a_lo, a_hi),
                        });
                    } else {
                        t.dirty_rows = Some(match t.dirty_rows {
                            Some((plo, phi)) => (plo.min(a_lo), phi.max(a_hi)),
                            None => (a_lo, a_hi),
                        });
                        t.band_steps = 0;
                    }
                }
            }
        }

        // Visible tile starts, bottom first, capped so the ring can hold
        // them plus one prefetch; a taller view than that falls back.
        let first = row0 / TILE_ROWS as u64 * TILE_ROWS as u64;
        let last = (row0 + height_px as u64 - 1) / TILE_ROWS as u64 * TILE_ROWS as u64;
        let mut wanted: Vec<u64> = Vec::new();
        let mut cur = first;
        while cur <= last && wanted.len() < TILE_SLOTS - 1 {
            wanted.push(cur);
            cur += TILE_ROWS as u64;
        }
        let covered = wanted.len() == ((last - first) / (TILE_ROWS as u64)) as usize + 1;
        let prefetch = last + TILE_ROWS as u64;

        // Bake: band fixes, then visible tiles, then the tile above
        // the view. A tile may be baked before the scan frontier has
        // crossed it: the bake clips at the frontier, and the rows
        // revealed afterwards are precisely the rows the publishes
        // dirty, so they arrive as band rebakes. (Gating bakes on full
        // reveal could never engage during playback: the scan leads
        // the view by only a quarter span, far less than a tile.)
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(TILE_BAKE_BUDGET_MS);
        let mut exhausted = false;
        // Band fixes first: rebaking a ready tile's touched rows is
        // nearly free (usually the playhead's bottom row) and keeps the
        // tile blit-able this very frame.
        for &start in wanted.iter() {
            if exhausted {
                break;
            }
            let idx = match tiles.iter().position(|t| {
                t.ready && t.key == Some(*key) && t.start_row == start && t.dirty_rows.is_some()
            }) {
                Some(i) => i,
                None => continue,
            };
            exhausted = bake_band(
                &mut tiles[idx], scratch, key, chunk, &tick_at, regions, frontier, deadline,
            );
        }
        for &start in wanted.iter().chain(std::iter::once(&prefetch)) {
            if exhausted {
                break;
            }
            let idx = find_slot(tiles, key, start, row0, &wanted);
            let tile = &mut tiles[idx];
            if tile.key == Some(*key) && tile.start_row == start && tile.ready {
                continue;
            }
            exhausted = bake_tile(
                tile, scratch, key, start, chunk, &tick_at, regions, frontier, deadline,
            );
        }

        if !covered {
            return false;
        }
        for &start in wanted.iter() {
            let tile = tiles.iter().find(|t| {
                t.ready
                    && t.key == Some(*key)
                    && t.start_row == start
                    && t.dirty_rows.is_none()
                    && t.pending_rows.is_none()
            });
            match tile {
                Some(t) => blit_tile(t, row0, height_px, area),
                None => return false,
            }
        }
        true
    })
}


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

/// A still-sounding note moved out of `KeyBucket::notes` so it cannot hold back
/// retirement of the dead notes behind it. `idx` is the note's absolute append
/// index, which is how a note-off that arrives after the move finds it.
#[derive(Clone, Copy)]
struct PinnedNote {
    idx: u64,
    note: RollNote,
}

/// How many still-sounding notes a key keeps after they leave the queue, at each
/// end. The rasterizer's oldest-edge fallback looks at the oldest ones and its
/// bounded reverse walk at the newest ones; the notes between are never reached,
/// so they are not worth holding.
const MAX_PINNED: usize = MAX_LOOKBACK;

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
    /// Count of notes removed from the front so far. The scan addresses notes by an
    /// absolute index (total ever appended to this key), so subtracting this maps an
    /// absolute index to the current queue position.
    retired: u64,
    /// Notes that started before the playhead but have not ended. They are what
    /// would otherwise block `retire` at the front of the queue while millions of
    /// already-ended notes pile up behind them. They are kept as a merged prefix by
    /// the draw (every pinned note starts no later than the first queued one).
    ///
    /// Two sets, because both ends of that prefix are visible: `pinned_old` holds
    /// the oldest notes (the rasterizer's oldest-edge fallback) and `pinned_new`
    /// rolls the newest ones (the notes just below the playhead). Everything in
    /// between is never reached by either bounded scan.
    pinned_old: VecDeque<PinnedNote>,
    pinned_new: VecDeque<PinnedNote>,
    /// Total notes ever appended to this bucket in this generation. Together
    /// with `retired` this is the scan's absolute index space: a note's queue
    /// position is its absolute index minus `retired`.
    appended_total: u64,
    /// The draw's winner index, over absolute append indices: entry `j` is
    /// the maximum `end_tick` over absolute indices `[j * WINNER_BLOCK,
    /// (j + 1) * WINNER_BLOCK)`. Anchored to absolute indices so `retire`,
    /// which only ever removes from the front, never shifts it. The draw
    /// used to rebuild this over merged positions for every column of every
    /// frame -- O(notes in view) per frame, which is what made low zoom
    /// crawl on dense files, the resident window there being the whole song.
    ///
    /// Entries for notes that have since been dropped (retired dead, or
    /// rolled off the pinned sets) go stale, but a stale entry can only
    /// *overstate* a block's maximum: the winner search validates the real
    /// notes it finds, so staleness costs an occasional wasted block scan,
    /// never a wrong winner. A patch (which only ever lowers an end, from
    /// OPEN_END to the real one) marks its block dirty instead, and the
    /// draw recomputes dirty blocks on demand.
    abs_block_max: Vec<u32>,
    abs_block_dirty: Vec<bool>,
    /// Start tick of the first note of each absolute block. Starts never
    /// change and appends are in start order, so this stays exact for the
    /// whole generation, dropped notes included -- which is what lets the
    /// draw find a row's boundary block by searching this small array
    /// instead of binary-searching the bucket for every pixel row.
    abs_block_start: Vec<u32>,
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
    /// Bumped when a publish or a retire changes something at or below the
    /// revealed frontier -- i.e. something a cached frame could already
    /// show. The draw keys its raster cache on this, so publishes that only
    /// extend the unrevealed lookahead do not invalidate it.
    pub content_seq: AtomicU64,
    pub scan_frontier_tick: AtomicU32,
    pub window_start_tick: AtomicU32,
    pub done: AtomicBool,
    /// Tick range touched by publishes and retires since the draw last
    /// took it: (min, max) with (u32::MAX, 0) meaning empty. The tile
    /// cache invalidates exactly the baked tiles this range intersects,
    /// where the global content_seq would take every tile down for a
    /// change anywhere in the window.
    pub dirty: Mutex<(u32, u32)>,
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
            content_seq: AtomicU64::new(0),
            scan_frontier_tick: AtomicU32::new(0),
            window_start_tick: AtomicU32::new(0),
            done: AtomicBool::new(false),
            dirty: Mutex::new((u32::MAX, 0)),
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
        self.content_seq.store(0, Ordering::SeqCst);
        for slot in self.notes_by_key.iter() {
            let mut b = slot.lock().unwrap();
            b.generation = generation;
            b.retired = 0;
            b.appended_total = 0;
            b.abs_block_max.clear();
            b.abs_block_dirty.clear();
            b.abs_block_start.clear();
            let old = std::mem::take(&mut b.notes);
            let old_pinned_old = std::mem::take(&mut b.pinned_old);
            let old_pinned_new = std::mem::take(&mut b.pinned_new);
            drop(b);
            drop(old);
            drop(old_pinned_old);
            drop(old_pinned_new);
        }
        self.scan_frontier_tick.store(start_tick, Ordering::SeqCst);
        self.window_start_tick.store(start_tick, Ordering::SeqCst);
        self.done.store(false, Ordering::SeqCst);
        *self.dirty.lock().unwrap() = (u32::MAX, 0);
    }

    /// Block until no draw is reading this chunk. Only called by `commit`, on the
    /// chunk it has just swapped *out*, so the wait is at most the tail of one frame.
    fn wait_for_readers(&self) {
        while self.readers.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
            thread::yield_now();
        }
    }

    /// Grow the dirty range by [lo, hi]. Called at most once per publish
    /// batch and once per retire, so the lock is never contended for long.
    pub fn mark_dirty(&self, lo: u32, hi: u32) {
        if lo > hi {
            return;
        }
        let mut d = self.dirty.lock().unwrap();
        if lo < d.0 {
            d.0 = lo;
        }
        if hi > d.1 {
            d.1 = hi;
        }
    }

    /// Take the accumulated dirty range, leaving it empty.
    pub fn take_dirty(&self) -> Option<(u32, u32)> {
        let mut d = self.dirty.lock().unwrap();
        if d.0 > d.1 {
            return None;
        }
        let out = *d;
        *d = (u32::MAX, 0);
        Some(out)
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
        let span =
            (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
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
            // Holds the data Arc itself, not just its address: while the
            // slot holds it, that allocation cannot be freed and reused
            // by a different file's Arc, so the ptr_eq check below can
            // never confuse two files.
            let mut ckpt_slot: Option<(Arc<MIDIPlayerData>, Arc<HeldCheckpointStore>)> = None;
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
                // One checkpoint store per loaded file, keyed by the data
                // Arc's identity. A new file cancels the old store's
                // background build and starts a fresh one.
                let store = match &ckpt_slot {
                    Some((d, s)) if Arc::ptr_eq(d, &req.data) => s.clone(),
                    _ => {
                        if let Some((_, old)) = &ckpt_slot {
                            old.cancelled.store(true, Ordering::SeqCst);
                        }
                        let n = unsafe { (&*req.data.as_ptr()).header.tracks } as usize;
                        let s = Arc::new(HeldCheckpointStore::new(n));
                        let bdata = req.data.clone();
                        let bstore = s.clone();
                        thread::spawn(move || build_held_checkpoints(bdata, bstore));
                        ckpt_slot = Some((req.data.clone(), s.clone()));
                        s
                    }
                };
                run_scan(
                    &req.data,
                    &store,
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

    /// Drop notes that playback has passed, freeing the memory behind the window.
    /// Notes that started in the past and are still sounding are moved to `pinned`
    /// instead of being left in the queue where they would stop every later note
    /// from ever being retired. A long note used to make a bucket grow for the whole
    /// song, because the dead notes behind it could never reach the front.
    pub fn retire(&self, current_tick: u32) {
        let guard = self.acquire_live();
        let mut dropped_pinned = false;
        let mut d_lo = u32::MAX;
        let mut d_hi = 0u32;
        let frontier_cap = guard.chunk.scan_frontier_tick.load(Ordering::SeqCst);
        for slot in guard.chunk.notes_by_key.iter() {
            let mut b = slot.lock().unwrap();

            // A pinned note dropped here may be the one holding its winner
            // block's maximum at OPEN_END, and its note-off patch will never
            // land now (the note is gone), so the block would overstate
            // forever and the winner search would keep scanning it. Mark
            // the blocks of everything about to be dropped dirty, so the
            // draw recomputes them.
            let mut stale: [u64; 2 * MAX_PINNED] = [0; 2 * MAX_PINNED];
            let mut stale_len = 0usize;
            for p in b.pinned_old.iter().chain(b.pinned_new.iter()) {
                if p.note.end_tick < current_tick {
                    stale[stale_len] = p.idx;
                    stale_len += 1;
                    d_lo = d_lo.min(p.note.start_tick);
                    d_hi = d_hi.max(p.note.end_tick);
                }
            }
            for &idx in stale.iter().take(stale_len) {
                mark_abs_block_dirty(&mut b, idx);
            }
            if stale_len > 0 {
                dropped_pinned = true;
            }

            // A pinned note whose note-off has since been patched can go.
            b.pinned_old.retain(|p| p.note.end_tick >= current_tick);
            b.pinned_new.retain(|p| p.note.end_tick >= current_tick);

            while let Some(front) = b.notes.front() {
                // Copy out of the packed struct before comparing.
                let end = front.end_tick;
                let start = front.start_tick;
                if end < current_tick {
                    b.notes.pop_front();
                    b.retired += 1;
                    d_lo = d_lo.min(start);
                    d_hi = d_hi.max(end);
                } else if start >= current_tick {
                    // Nothing at or before the playhead can follow: every remaining
                    // note starts now or later, so none can be dead yet.
                    break;
                } else {
                    // Started in the past and still ringing. It is exactly what used
                    // to block retirement, so take it out of the queue. Keep the
                    // oldest MAX_PINNED for the oldest-edge fallback and roll the
                    // newest MAX_PINNED for the lookups just under the playhead; the
                    // draw never reaches the rest, so they are dropped for good.
                    let idx = b.retired;
                    let note = b.notes.pop_front().unwrap();
                    b.retired += 1;
                    if b.pinned_new.is_empty() && b.pinned_old.len() < MAX_PINNED {
                        b.pinned_old.push_back(PinnedNote { idx, note });
                    } else {
                        if b.pinned_new.len() == MAX_PINNED {
                            // The rolled-off note is dropped for good, still
                            // sounding: same stale-block hazard as above.
                            if let Some(old) = b.pinned_new.pop_front() {
                                mark_abs_block_dirty(&mut b, old.idx);
                                dropped_pinned = true;
                                // A still-sounding note reaches the frontier;
                                // cap there like the publish side does.
                                d_lo = d_lo.min(old.note.start_tick);
                                d_hi = d_hi.max(old.note.end_tick.min(frontier_cap));
                            }
                        }
                        b.pinned_new.push_back(PinnedNote { idx, note });
                    }
                }
            }

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

        // A dropped pinned note was on screen (it reaches the playhead), so
        // any cached frame is stale now.
        if dropped_pinned {
            guard.chunk.content_seq.fetch_add(1, Ordering::SeqCst);
        }
        if d_lo <= d_hi {
            guard.chunk.mark_dirty(d_lo, d_hi);
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
/// How many of the newest sounding notes on each (channel, key) a seek
/// keeps. Shared by the scan's seed and the checkpoint builder, so a
/// checkpoint's stored state is capped exactly as a live seek's is.
const MAX_SOUNDING_PER_KEY: usize = 128;

/// Held-note checkpoints for one loaded file: one Vec per track, kept
/// sorted by event count (ticks are non-decreasing along it). Built in
/// the background when a file is loaded and topped up by every seed
/// seek that walks past a boundary, so a seek to anywhere replays at
/// most HELD_CKPT_INTERVAL events per track instead of walking from
/// the track start. Measured cost on the densest real files is
/// single-digit megabytes per file (NECROFANTASIA 8.9 MB).
pub struct HeldCheckpointStore {
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

    /// The newest checkpoint on `track` starting before `target_tick`.
    fn base_for(&self, track: usize, target_tick: u32) -> Option<HeldCheckpoint> {
        let v = self.per_track.get(track)?.lock().unwrap();
        let idx = v.partition_point(|c| c.tick < target_tick);
        if idx == 0 {
            None
        } else {
            Some(v[idx - 1].clone())
        }
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

/// Walk every track of a freshly loaded file once, depositing held-note
/// checkpoints. Runs on its own thread; `cancelled` is set when the
/// file is replaced, and the walk bails at the next track boundary.
fn build_held_checkpoints(data: Arc<MIDIPlayerData>, store: Arc<HeldCheckpointStore>) {
    let num_tracks = unsafe { (&*data.as_ptr()).header.tracks } as usize;
    let worker_count = (crate::app_state::cpu_threads().max(1) as usize).min(num_tracks.max(1));
    thread::scope(|scope| {
        for w in 0..worker_count {
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
                    ti += worker_count;
                }
            });
        }
    });
}

fn run_scan(
    data: &Arc<MIDIPlayerData>,
    ckpts: &HeldCheckpointStore,
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
    // Open notes of one voice, oldest first. Most voices hold a single ringing
    // note at a time, so the first index lives inline in the map value and the
    // heap-allocated queue only appears once notes actually stack. A
    // VecDeque per voice paid an allocation and a free for nearly every note
    // in a dense file, which dwarfed the cost of the merge itself.
    #[derive(Default)]
    enum VoiceQueue {
        #[default]
        Empty,
        Single(u64),
        Queue(VecDeque<u64>),
    }
    impl VoiceQueue {
        fn push_back(&mut self, idx: u64) {
            match self {
                VoiceQueue::Empty => *self = VoiceQueue::Single(idx),
                VoiceQueue::Single(first) => {
                    let mut q = VecDeque::with_capacity(4);
                    q.push_back(*first);
                    q.push_back(idx);
                    *self = VoiceQueue::Queue(q);
                }
                VoiceQueue::Queue(q) => q.push_back(idx),
            }
        }
        fn pop_front(&mut self) -> Option<u64> {
            match self {
                VoiceQueue::Empty => None,
                VoiceQueue::Single(first) => {
                    let idx = *first;
                    *self = VoiceQueue::Empty;
                    Some(idx)
                }
                VoiceQueue::Queue(q) => {
                    let idx = q.pop_front();
                    if q.is_empty() {
                        *self = VoiceQueue::Empty;
                    }
                    idx
                }
            }
        }
        fn is_empty(&self) -> bool {
            matches!(self, VoiceQueue::Empty)
        }
        fn iter(&self) -> Box<dyn Iterator<Item = &u64> + '_> {
            match self {
                VoiceQueue::Empty => Box::new(std::iter::empty()),
                VoiceQueue::Single(first) => Box::new(std::iter::once(first)),
                VoiceQueue::Queue(q) => Box::new(q.iter()),
            }
        }
    }
    let mut open: FxHashMap<(u16, u8, u8), VoiceQueue> = FxHashMap::default();
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
    // Stream seek results through a bounded channel instead of collecting every
    // worker's Vec and then building a second per-track staging Vec. On dense files,
    // each track can return many held notes; retaining all those result vectors until
    // every worker finishes needlessly raises the seek-time memory peak.
    // The bounded queue limits queued results to 2 * worker_count tracks (plus at
    // most one in-flight result per worker), while preserving the exact same set of
    // held notes and the track-indexed cursor order needed by the merge heap.
    let mut cursor_slots: Vec<Option<TrackCursor>> = (0..num_tracks).map(|_| None).collect();
    let queue_capacity = worker_count.saturating_mul(2).max(1);
    let (seed_tx, seed_rx) = mpsc::sync_channel::<(
        usize,
        TrackCursor,
        Vec<crate::kazumidiparser_cursor::HeldNote>,
    )>(queue_capacity);

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for w in 0..worker_count {
            let data = data.clone();
            let tx = seed_tx.clone();
            handles.push((
                w,
                scope.spawn(move || {
                    // Safety: the Arc keeps the C MIDI struct alive for this call.
                    let parser = unsafe { &*data.as_ptr() };
                    let mut ti = w;
                    while ti < num_tracks {
                        // Safety: ti < num_tracks; parser valid for this call.
                        // Start from the newest held-note checkpoint
                        // before the target when one exists, so the walk
                        // replays at most HELD_CKPT_INTERVAL events, and
                        // deposit any checkpoints this walk crosses.
                        let base = ckpts.base_for(ti, start_tick);
                        let mut deposited = Vec::new();
                        let (cur, held) = unsafe {
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
                        if tx.send((ti, cur, held)).is_err() {
                            break;
                        }
                        ti += worker_count;
                    }
                }),
            ));
        }
        // The receiver exits only after every worker drops its sender. This lets the
        // main scan thread consume results while workers are still seeking, keeping
        // the number of live HeldNote vectors bounded instead of proportional to the
        // total number of tracks.
        drop(seed_tx);
        while let Ok((ti, cur, held)) = seed_rx.recv() {
            for note in held {
                let newest = &mut sounding[note.key as usize];
                newest.push(Reverse((note.start_tick, ti as u16, note.channel)));
                if newest.len() > MAX_SOUNDING_PER_KEY {
                    if let Some(Reverse((_, old_ti, old_ch))) = newest.pop() {
                        *evicted.entry((old_ti, old_ch, note.key)).or_default() += 1;
                    }
                }
            }
            cursor_slots[ti] = Some(cur);
        }
        // Preserve the existing recovery behavior if a seek worker panicked: only
        // that worker's stride is retried synchronously below.
        for (w, handle) in handles {
            let _ = handle.join();
            let _ = w;
        }
    });

    for (ti, slot) in cursor_slots.into_iter().enumerate() {
        let cur = match slot {
            Some(cur) => cur,
            None => unsafe { seek_track_cursor(parser, ti as u16, start_tick) },
        };
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
            open.entry((ti, channel, key as u8))
                .or_default()
                .push_back(idx);
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
        // Only changes at or below the frontier as it stood when this
        // publish began can alter a frame the draw has already cached:
        // those notes are already revealed. Appends and patches above it
        // are new reveals, which the draw keys on the frontier itself, so
        // they must not invalidate the raster cache -- during playback the
        // scan publishes into the lookahead margin constantly, and taking
        // the cache down for those made the cache useless.
        let revealed = chunk.scan_frontier_tick.load(Ordering::SeqCst);
        let mut changed = false;
        let mut d_lo = u32::MAX;
        let mut d_hi = 0u32;

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
            for note in run.drain(..) {
                // Maintain the draw's absolute winner index as notes land:
                // an append can only raise its block's maximum, so this is
                // exact unless the block was already dirty from a patch.
                let idx = b.appended_total;
                b.appended_total += 1;
                let blk = (idx / WINNER_BLOCK as u64) as usize;
                if blk >= b.abs_block_max.len() {
                    b.abs_block_max.push(0);
                    b.abs_block_dirty.push(false);
                    b.abs_block_start.push(note.start_tick);
                }
                if note.end_tick > b.abs_block_max[blk] {
                    b.abs_block_max[blk] = note.end_tick;
                }
                b.notes.push_back(note);
                if note.start_tick <= revealed {
                    changed = true;
                }
                // An appended note is only drawable up to the frontier
                // this publish reveals, so cap the dirty range there; its
                // note-off patch dirties the rest when it lands.
                d_lo = d_lo.min(note.start_tick);
                d_hi = d_hi.max(note.end_tick.min(frontier));
            }
            for (idx, end) in key_patches.drain(..) {
                // A still-sounding note may have been moved out of the queue by
                // `retire` while its note-off was in flight. Pinned notes keep their
                // absolute index (ascending) so the patch still finds them.
                if let Ok(pos) = b.pinned_old.binary_search_by_key(&idx, |p| p.idx) {
                    b.pinned_old[pos].note.end_tick = end;
                    // A patch changes nothing below the new end (the
                    // note's body there is the same); it retracts the
                    // open-ended extent the note was drawn with, from
                    // the new end up to the frontier it reached.
                    d_lo = d_lo.min(end);
                    d_hi = d_hi.max(frontier);
                    let blk = (idx / WINNER_BLOCK as u64) as usize;
                    if blk < b.abs_block_dirty.len() {
                        b.abs_block_dirty[blk] = true;
                    }
                    if end <= revealed {
                        changed = true;
                    }
                    continue;
                }
                if let Ok(pos) = b.pinned_new.binary_search_by_key(&idx, |p| p.idx) {
                    b.pinned_new[pos].note.end_tick = end;
                    d_lo = d_lo.min(end);
                    d_hi = d_hi.max(frontier);
                    let blk = (idx / WINNER_BLOCK as u64) as usize;
                    if blk < b.abs_block_dirty.len() {
                        b.abs_block_dirty[blk] = true;
                    }
                    if end <= revealed {
                        changed = true;
                    }
                    continue;
                }
                // Absolute index -> current queue position. A note whose index is
                // below the retired count has already scrolled out of the window; its
                // end no longer exists to patch, so the patch is simply dropped.
                let Some(pos) = idx.checked_sub(b.retired) else {
                    continue;
                };
                if let Some(slot) = b.notes.get_mut(pos as usize) {
                    slot.end_tick = end;
                    d_lo = d_lo.min(end);
                    d_hi = d_hi.max(frontier);
                    let blk = (idx / WINNER_BLOCK as u64) as usize;
                    if blk < b.abs_block_dirty.len() {
                        b.abs_block_dirty[blk] = true;
                    }
                    if end <= revealed {
                        changed = true;
                    }
                }
            }
        }

        // The frontier is only a reveal hint, so it is not worth a lock: a
        // `regenerate` racing the check above can leave one stale value here, on a
        // chunk that now holds no notes (so nothing is drawn) and whose new scan
        // overwrites it on its very first publish.
        chunk.scan_frontier_tick.store(frontier, Ordering::SeqCst);
        if changed {
            chunk.content_seq.fetch_add(1, Ordering::SeqCst);
        }
        if d_lo <= d_hi {
            chunk.mark_dirty(d_lo, d_hi);
        }
        true
    };

    while let Some(mut top) = heap.peek_mut() {
        let (tick, ti) = (*top).0;
        let mut remove_top = false;
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
            if !publish(
                &mut batch,
                &mut staged,
                &mut patches,
                &mut patch_count,
                frontier,
            ) {
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
                    if !publish(
                        &mut batch,
                        &mut staged,
                        &mut patches,
                        &mut patch_count,
                        last_tick,
                    ) {
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
                    if !publish(
                        &mut batch,
                        &mut staged,
                        &mut patches,
                        &mut patch_count,
                        last_tick,
                    ) {
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
                        if !publish(
                            &mut batch,
                            &mut staged,
                            &mut patches,
                            &mut patch_count,
                            last_tick,
                        ) {
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
                        if !publish(
                            &mut batch,
                            &mut staged,
                            &mut patches,
                            &mut patch_count,
                            last_tick,
                        ) {
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
                    let voice = (ti as u16, channel, key as u8);
                    let mut emptied = false;
                    if let Some(queue) = open.get_mut(&voice) {
                        // A tombstone consumes its note-off and patches nothing: the
                        // note it stands for was never put in the roll.
                        if let Some(idx) = queue.pop_front() {
                            if idx != DROPPED {
                                patches[key].push((idx, tick));
                                patch_count += 1;
                            }
                        }
                        emptied = queue.is_empty();
                    }
                    // A voice with nothing left ringing must not keep its queue
                    // buffer alive for the rest of the scan. On a many-track MIDI
                    // this map is where most of the scan's memory goes: one entry
                    // per voice ever struck, each holding its peak stack size.
                    if emptied {
                        open.remove(&voice);
                    }
                }
            }
        }

        // Advance this track to its next non-tempo event and re-queue it. The
        // entry never leaves the heap: its key is replaced in place and the
        // peek guard's drop sifts it down once, instead of a pop (one sift
        // pass) plus a push (another) for every event.
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
                *top = Reverse((cur.abs_tick, ti));
            } else {
                remove_top = true;
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
            if !publish(
                &mut batch,
                &mut staged,
                &mut patches,
                &mut patch_count,
                frontier,
            ) {
                return;
            }
            if latest.load(Ordering::SeqCst) != generation || shutdown.load(Ordering::Relaxed) {
                return;
            }
        }

        if remove_top {
            std::collections::binary_heap::PeekMut::pop(top);
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
    if !publish(
        &mut batch,
        &mut staged,
        &mut patches,
        &mut patch_count,
        end_tick,
    ) {
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

/// Index of the first note starting above `limit` within the queue alone.
fn first_note_above_queue(notes: &VecDeque<RollNote>, limit: u32) -> usize {
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

/// Index of the first note starting above `limit`, in the draw's logical order:
/// pinned notes first, then the queue. Every pinned note starts no later than the
/// first queued one, so the merged sequence is still start-sorted. The bucket is
/// start-sorted, so every note from here on is off the top of the view.
fn first_note_above(
    pinned_old: &VecDeque<PinnedNote>,
    pinned_new: &VecDeque<PinnedNote>,
    notes: &VecDeque<RollNote>,
    limit: u32,
) -> usize {
    let in_old = pinned_old.partition_point(|p| p.note.start_tick <= limit);
    if in_old < pinned_old.len() {
        return in_old;
    }
    let in_new = pinned_new.partition_point(|p| p.note.start_tick <= limit);
    if in_new < pinned_new.len() {
        return pinned_old.len() + in_new;
    }
    pinned_old.len() + pinned_new.len() + first_note_above_queue(notes, limit)
}

/// The merged sequence's note at `i`.
#[inline]
fn note_at(
    pinned_old: &VecDeque<PinnedNote>,
    pinned_new: &VecDeque<PinnedNote>,
    notes: &VecDeque<RollNote>,
    i: usize,
) -> RollNote {
    let old_len = pinned_old.len();
    if i < old_len {
        pinned_old[i].note
    } else if i < old_len + pinned_new.len() {
        pinned_new[i - old_len].note
    } else {
        notes[i - old_len - pinned_new.len()]
    }
}

/// How many still-sounding notes `retire` keeps pinned at each end of a key's
/// queue (see `MAX_PINNED`). The rasterizer's winner search used to be capped at
/// this many notes from each edge of a row's prefix as well, but a long note
/// buried deeper than that in the middle was never found -- and then popped into
/// view mid-note once the notes ahead of it retired and it drifted into range.
/// The search is exact now (see `winner_in_prefix`); this constant only bounds
/// the pinning.
const MAX_LOOKBACK: usize = 64;

/// Block size of the per-column winner index (`build_block_max`). A row's winner
/// search skips whole blocks whose notes all end below the row, so it stays cheap
/// in a dense rest without giving up on notes buried in the middle of the prefix.
const WINNER_BLOCK: usize = 64;

/// Fill `out` with the maximum `end_tick` of each `WINNER_BLOCK`-sized block of
/// the bucket's merged note sequence (pinned first, then the queue), over the
/// first `limit` notes. Rebuilt per column per frame into scratch storage.
fn build_block_max(b: &KeyBucket, limit: usize, out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    while i < limit {
        let hi = (i + WINNER_BLOCK).min(limit);
        let mut m = 0u32;
        for j in i..hi {
            m = m.max(note_at(&b.pinned_old, &b.pinned_new, &b.notes, j).end_tick);
        }
        out.push(m);
        i = hi;
    }
}

/// The last note before `prefix` whose end reaches `min_end` -- the note a row
/// shows. Exact, unlike the bounded walks it replaces: blocks whose maximum end
/// falls short are skipped whole, and the first block from the top that can hold
/// a winner is scanned from its end, so the note found is the same one an
/// unbounded backward scan would find.
fn winner_in_prefix(b: &KeyBucket, block_max: &[u32], prefix: usize, min_end: u32) -> Option<usize> {
    if prefix == 0 {
        return None;
    }
    let mut block = (prefix - 1) / WINNER_BLOCK;
    loop {
        if block_max[block] >= min_end {
            let hi = prefix.min((block + 1) * WINNER_BLOCK);
            let lo = block * WINNER_BLOCK;
            for i in (lo..hi).rev() {
                if note_at(&b.pinned_old, &b.pinned_new, &b.notes, i).end_tick >= min_end {
                    return Some(i);
                }
            }
        }
        if block == 0 {
            return None;
        }
        block -= 1;
    }
}

/// Color of the topmost note sounding at `tick`, using the same exact winner
/// search as the roll rasterizer (see `winner_in_prefix`).
fn sounding_color_at_bucket(b: &KeyBucket, tick: u32) -> Option<u16> {
    let prefix = first_note_above(&b.pinned_old, &b.pinned_new, &b.notes, tick);
    let mut block_max = Vec::new();
    build_block_max(b, prefix, &mut block_max);
    let i = winner_in_prefix(b, &block_max, prefix, tick.saturating_add(1))?;
    Some(note_at(&b.pinned_old, &b.pinned_new, &b.notes, i).color)
}

impl KeyBucket {
    /// Color of the topmost note sounding at `tick`, for the keyboard highlight.
    pub fn sounding_color_at(&self, tick: u32) -> Option<u16> {
        sounding_color_at_bucket(self, tick)
    }
}

/// The note with absolute append index `idx`, wherever it currently lives:
/// the pinned sets (which keep their absolute index, ascending, so they
/// binary-search) or the queue (index minus the retired count). `None` if
/// the note has been dropped or not appended yet.
fn note_by_abs(b: &KeyBucket, idx: u64) -> Option<RollNote> {
    if idx < b.retired {
        if let Ok(pos) = b.pinned_old.binary_search_by_key(&idx, |p| p.idx) {
            return Some(b.pinned_old[pos].note);
        }
        if let Ok(pos) = b.pinned_new.binary_search_by_key(&idx, |p| p.idx) {
            return Some(b.pinned_new[pos].note);
        }
        return None;
    }
    let pos = (idx - b.retired) as usize;
    b.notes.get(pos).copied()
}

/// Recompute one block of the bucket's absolute winner index from the
/// notes that actually live in it now.
/// Mark the winner-index block holding absolute append index `idx` dirty,
/// so the draw recomputes it instead of trusting a maximum that a note
/// since dropped from the bucket may have set (an OPEN_END maximum whose
/// patch will never land would otherwise keep the block overstated -- and
/// scanned -- forever).
fn mark_abs_block_dirty(b: &mut KeyBucket, idx: u64) {
    let blk = (idx / WINNER_BLOCK as u64) as usize;
    if blk < b.abs_block_dirty.len() {
        b.abs_block_dirty[blk] = true;
    }
}

fn refresh_abs_block(b: &mut KeyBucket, blk: usize) {
    let lo = blk as u64 * WINNER_BLOCK as u64;
    let hi = (lo + WINNER_BLOCK as u64).min(b.appended_total);
    let mut m = 0u32;
    for idx in lo..hi {
        if let Some(n) = note_by_abs(b, idx) {
            m = m.max(n.end_tick);
        }
    }
    b.abs_block_max[blk] = m;
    b.abs_block_dirty[blk] = false;
}

/// The note a pixel row shows: the last note (in append order) that
/// starts at or before `t_hi` and still ends at or after `t_lo`.
///
/// The search starts from the row's boundary block -- the last block
/// whose first note starts at or before `t_hi`, found by searching the
/// small `abs_block_start` array rather than the bucket. Notes in blocks
/// below the boundary all start at or before `t_hi`, so only the
/// boundary block's own scan checks starts; everywhere else the block
/// maximum prunes whole blocks and the first note found reaching `t_lo`,
/// scanning backwards, is the winner.
fn winner_for_row(b: &KeyBucket, t_hi: u32, t_lo: u32) -> Option<(u64, RollNote)> {
    let le = b.abs_block_start.partition_point(|&s| s <= t_hi);
    if le == 0 {
        return None;
    }
    let boundary = le - 1;
    let queue_lo = b.retired;
    let queue_hi = b.retired + b.notes.len() as u64;
    let (head, tail) = b.notes.as_slices();
    for block in (0..=boundary).rev() {
        if b.abs_block_max[block] < t_lo {
            continue;
        }
        let hi = ((block as u64 + 1) * WINNER_BLOCK as u64).min(b.appended_total);
        let lo = block as u64 * WINNER_BLOCK as u64;
        let check_start = block == boundary;
        if lo >= queue_lo && hi <= queue_hi {
            // The whole block sits in the queue: scan its storage
            // directly instead of resolving every index through
            // `note_by_abs`.
            let qhi = (hi - queue_lo) as usize;
            let qlo = (lo - queue_lo) as usize;
            for pos in (qlo..qhi).rev() {
                let n = if pos < head.len() {
                    head[pos]
                } else {
                    tail[pos - head.len()]
                };
                if n.end_tick >= t_lo && (!check_start || n.start_tick <= t_hi) {
                    return Some((queue_lo + pos as u64, n));
                }
            }
        } else {
            for idx in (lo..hi).rev() {
                if let Some(n) = note_by_abs(b, idx) {
                    if n.end_tick >= t_lo && (!check_start || n.start_tick <= t_hi) {
                        return Some((idx, n));
                    }
                }
            }
        }
    }
    None
}

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
/// `owner` records which note won each row (an index into `winner_notes`,
/// the column's distinct winners in the order they first appear), so the
/// emit pass can still tell two touching notes of the same colour apart
/// and outline them separately -- reading the colour alone would merge
/// them into one block.
///
/// The winner search runs on the bucket's persistent absolute index
/// (`abs_block_max`) instead of an index rebuilt per frame: at low zoom
/// the resident window is the whole song, and rebuilding cost a full pass
/// over every note in it, every column, every frame.
fn rasterize_column(
    bucket: &Mutex<KeyBucket>,
    col: &mut [u32],
    owner: &mut [u32],
    row_tick: &[u32],
    current_tick: u32,
    effective_top: u32,
    winner_notes: &mut Vec<RollNote>,
) {
    // Clear the column before the empty-bucket fast path so a key that was active
    // in the previous frame cannot leave stale pixels behind. Doing this here also
    // avoids clearing every pixel globally and then clearing active columns again.
    for slot in col.iter_mut() {
        *slot = 0;
    }
    for slot in owner.iter_mut() {
        *slot = u32::MAX;
    }
    winner_notes.clear();
    let mut b = bucket.lock().unwrap();
    if b.notes.is_empty() && b.pinned_old.is_empty() && b.pinned_new.is_empty() {
        return;
    }

    // The bucket's winner index lives in absolute append indices (see
    // `KeyBucket::abs_block_max`), so the merged prefix the rows count to
    // is converted by adding back the notes that have been dropped: every
    // one of them started before the playhead, hence before any row's top
    // tick, so each sits inside every prefix this column computes.
    let dropped = b.appended_total
        - (b.pinned_old.len() + b.pinned_new.len() + b.notes.len()) as u64;
    let limit = first_note_above(&b.pinned_old, &b.pinned_new, &b.notes, effective_top);
    let limit_abs = limit as u64 + dropped;

    // Recompute only the blocks a patch has dirtied since they were last
    // exact. In steady playback nothing is dirty and this costs a flag
    // scan; the per-frame full rebuild it replaces cost a pass over
    // every note in the window.
    let top_block = ((limit_abs + WINNER_BLOCK as u64 - 1) / WINNER_BLOCK as u64) as usize;
    for blk in 0..top_block {
        if b.abs_block_dirty[blk] {
            refresh_abs_block(&mut b, blk);
        }
    }


    let mut last_winner: Option<u64> = None;
    for r in 0..col.len() {
        // The ticks this pixel row covers. Clamped to the window: above the frontier
        // nothing has been generated, below the playhead nothing is drawn.
        let t_hi = row_tick[r].min(effective_top);
        let t_lo = row_tick[r + 1].max(current_tick);
        if t_hi < t_lo {
            last_winner = None;
            continue;
        }

        // The last note that starts at or before this row's top and still
        // reaches its bottom wins; it paints over every earlier note. The
        // search is exact (see `winner_for_row`): a long note buried in
        // the middle of a dense stretch is found, instead of vanishing
        // until the notes ahead of it retire.
        if let Some((idx, n)) = winner_for_row(&b, t_hi, t_lo) {
            col[r] = pack(get_note_color_by_index(n.color));
            if last_winner == Some(idx) {
                owner[r] = winner_notes.len() as u32 - 1;
            } else {
                owner[r] = winner_notes.len() as u32;
                winner_notes.push(n);
                last_winner = Some(idx);
            }
        } else {
            last_winner = None;
        }
    }

    // Outline each note's visible extent, top and bottom. Per run of one owner rather
    // than per note: a note that is partly covered only shows where it won, and that
    // is the edge worth drawing.
    //
    // A run clipped by the column's own edge is not an edge of the note: a note
    // longer than the view fills the whole column, and capping its run would draw
    // a fake end at the top of the screen and a fake start at the playhead. Cap a
    // clipped run only where the note's own end (top) or start (bottom) falls
    // inside the view.
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
        let n = winner_notes[who as usize];
        let edge = darken(col[run_start]);
        if run_start > 0 || n.end_tick <= effective_top {
            col[run_start] = edge;
        }
        if r < col.len() || n.start_tick >= current_tick {
            col[r - 1] = edge;
        }
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

    // Geometry shared by the tile path and the direct path. The row grid
    // is anchored absolutely -- row A starts at A * row_span -- so a
    // baked tile stays valid as the playhead advances; only its blit
    // offset changes. (The direct path used to interpolate each row's
    // tick with a float over the screen height; the grid below agrees
    // with it to well under a pixel row and, unlike it, can be
    // reproduced for any sub-range of rows.)
    let (draw_tick, top_tick, span_key, draw_ns, row0) = if is_tick_based {
        let division = (data.header.division as u32).max(1);
        let span =
            (((division * VISIBLE_SPAN_QUARTERS) as f32 / speed * height_scale) as u32).max(1);
        let row_span = (span / height_px as u32).max(1);
        let draw_tick = current_tick - current_tick % row_span;
        (
            draw_tick,
            draw_tick.saturating_add(row_span.saturating_mul(height_px as u32)),
            row_span as u64,
            0u64,
            (draw_tick / row_span) as u64,
        )
    } else {
        let span_ns = (VISIBLE_SPAN_NS as f32 / speed * height_scale).max(1.0) as u64;
        let row_span_ns = (span_ns / height_px as u64).max(1);
        let draw_ns = playback_position_ns - playback_position_ns % row_span_ns;
        (
            ns_to_tick(data, draw_ns),
            ns_to_tick(data, draw_ns.saturating_add(span_ns)),
            row_span_ns,
            draw_ns,
            draw_ns / row_span_ns,
        )
    };
    let regions = get_keyboard_note_regions(area.width);

    // Tile path: blit pre-rasterized tiles when they cover the view.
    // Falls through to the direct path while any visible tile is still
    // baking, or when the view spans more tiles than the cache holds.
    let frontier = chunk.scan_frontier_tick.load(Ordering::SeqCst);
    let tile_key = TileKey {
        generation: chunk.generation.load(Ordering::SeqCst),
        span_key,
        is_tick_based,
        width_px: area.width.ceil().max(1.0) as u32,
    };
    if tile_frame(&tile_key, row0, height_px, &area, &regions, data, chunk, frontier) {
        return;
    }

    // The tick at each pixel row's top edge; row 0 is the top of the view (furthest
    // future) and row `height_px` is the playhead, so this is non-increasing. Row r
    // therefore covers ticks [row_tick[r + 1], row_tick[r]].
    //
    // This is the inverse of the old tick -> y mapping, and it is the only direction
    // the rasterizer needs now that pixels look their note up instead of notes
    // painting themselves onto pixels. Both axis modes build it, so there is one path
    // rather than a closure per mode.
    DRAW_SCRATCH.with(|scratch_cell| {
        let mut scratch = scratch_cell.borrow_mut();
        let pixel_count = 128 * height_px;
        scratch.row_tick.resize(height_px + 1, 0);
        scratch.cols.resize(pixel_count, 0);
        scratch.owners.resize(pixel_count, u32::MAX);
        let DrawScratch {
            row_tick,
            cols,
            owners,
            winner_notes,
            cache_key,
        } = &mut *scratch;
        row_tick.fill(0);
        // Snap the playhead down to a whole pixel row. While playback moves
        // within one row the rasterized picture cannot change, so the draw
        // can reuse the cached columns (see `cache_key` below) instead of
        // re-rasterizing all 128 columns every frame. The picture lags the
        // true playhead by less than one row -- under a pixel.
        // Row boundaries on the same absolute grid the tiles use: row
        // r's top edge is (height_px - r) rows above the snapped
        // playhead, so this path and the tile path agree pixel for pixel.
        if is_tick_based {
            let row_span = span_key as u32;
            for (r, slot) in row_tick.iter_mut().enumerate() {
                *slot = draw_tick.saturating_add(((height_px - r) as u32).saturating_mul(row_span));
            }
        } else {
            for (r, slot) in row_tick.iter_mut().enumerate() {
                let ns = draw_ns.saturating_add(((height_px - r) as u64).saturating_mul(span_key));
                *slot = ns_to_tick(data, ns);
            }
        }

        // Reveal only up to the scan frontier; notes above it are not generated yet.
        let effective_top = top_tick
            .min(chunk.scan_frontier_tick.load(Ordering::SeqCst))
            .max(draw_tick);


        // All 128 columns, back to back. Packed color per pixel: 0 = empty, else
        // (1<<24) | rgb. `owners` runs alongside so the emit pass can separate two
        // touching notes that happen to share a colour.

        // This is only 128 * window-height pixels. Spawning a full set of scoped OS
        // threads every frame cost more than the rasterization itself, caused scheduler
        // spikes, and made every thread reserve a stack. Keep the hot path on the render
        // thread; the expensive MIDI scan remains on its persistent worker.
        let frame_key = (
            chunk.generation.load(Ordering::SeqCst),
            chunk.content_seq.load(Ordering::SeqCst),
            draw_tick,
            top_tick,
            effective_top,
            height_px,
            is_tick_based,
        );
        if *cache_key != Some(frame_key) {
            for (key, (col, own)) in cols
                .chunks_mut(height_px)
                .zip(owners.chunks_mut(height_px))
                .enumerate()
            {
                rasterize_column(
                    &chunk.notes_by_key[key],
                    col,
                    own,
                    &row_tick,
                    draw_tick,
                    effective_top,
                    winner_notes,
                );
            }
            *cache_key = Some(frame_key);
        }

        // Emit one rectangle per contiguous same-color run.
        //
        // The left and right edges are drawn per run rather than per note: every note on
        // a key spans the key's full width, so those two edges are the same two columns
        // of pixels for all of them.
        // White-key regions overlap the neighbouring black-key regions. MIDI note order
        // alternates between them, so a single ascending pass lets the next white key
        // paint over the preceding black key. Match a physical keyboard's z-order:
        // render every white-key column first, then all black-key columns on top.
        for black_pass in [false, true] {
            for (note, col) in cols.chunks(height_px).enumerate() {
                if is_black_key(note as u8) != black_pass {
                    continue;
                }
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
    });
}

