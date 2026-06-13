use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::kazumidiparser_cursor::tick_to_ns;
use crate::kazumidiparser_note_extractor::{NoteExtractor, TimedNote};
use crate::utils::{get_note_color_by_track_channel, lock_mutex};

const TEXTURE_WIDTH: usize = 128;
const TEXTURE_HEIGHT: usize = 16384;
const CHUNK_DURATION_NS: u64 = 5_000_000_000;
const DEFAULT_MAX_CHUNKS: usize = 10;

#[derive(Clone)]
pub struct NoteChunkTexture {
    pub start_time_ns: u64,
    pub end_time_ns: u64,
    pub width: usize,
    pub height: usize,
    pub rgba_data: Option<Vec<u8>>,
    pub data_version: u64,
}

pub enum TextureGenCommand {
    UpdatePosition(u64),
    UpdateData(Arc<kazumidiparser::MIDIPlayerData>),
    SetMaxChunks(usize),
    SetTextureHeight(usize),
    CancelAll,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChunkStatus {
    Todo,
    InProgress,
    Done,
}

struct GenerationState {
    chunks: Vec<ChunkStatus>,
    /// Cached Arc snapshot; rebuilt lazily when `chunks_dirty` is true.
    chunks_arc: Arc<Vec<ChunkStatus>>,
    chunks_dirty: bool,
    data_version: u64,
    max_chunks: usize,
    current_position_ns: u64,
    current_midi: Option<Arc<kazumidiparser::MIDIPlayerData>>,
    texture_height: usize,
}

pub struct NoteTextureGenerator {
    tx_command: Sender<TextureGenCommand>,
    rx_texture: mpsc::Receiver<NoteChunkTexture>,
    shutdown: Arc<AtomicBool>,
    task_queue_cv: Arc<Condvar>,
    state: Arc<Mutex<GenerationState>>,
    generated_count: Arc<AtomicU64>,
    expected_version: Arc<AtomicU64>,
}

impl NoteTextureGenerator {
    pub fn spawn(worker_count: usize) -> Self {
        let (tx_command, rx_command) = mpsc::channel::<TextureGenCommand>();
        // Each slot can hold up to TEXTURE_WIDTH * TEXTURE_HEIGHT * 4 bytes (~8 MB at default height).
        const TEXTURE_QUEUE_CAP: usize = 8;
        let (tx_texture, rx_texture) = mpsc::sync_channel::<NoteChunkTexture>(TEXTURE_QUEUE_CAP);
        let shutdown = Arc::new(AtomicBool::new(false));
        let task_queue_cv = Arc::new(Condvar::new());
        let generated_count = Arc::new(AtomicU64::new(0));
        let expected_version = Arc::new(AtomicU64::new(0));

        let state = Arc::new(Mutex::new(GenerationState {
            chunks: vec![ChunkStatus::Todo; DEFAULT_MAX_CHUNKS],
            chunks_arc: Arc::new(vec![ChunkStatus::Todo; DEFAULT_MAX_CHUNKS]),
            chunks_dirty: false,
            data_version: 0,
            max_chunks: DEFAULT_MAX_CHUNKS,
            current_position_ns: 0,
            current_midi: None,
            texture_height: TEXTURE_HEIGHT,
        }));

        let worker_count = worker_count.max(1);

        // ─── Worker threads ──────────────────────────────────────────────
        for _ in 0..worker_count {
            let tx_texture = tx_texture.clone();
            let gen_count = generated_count.clone();
            let state = state.clone();
            let task_queue_cv = task_queue_cv.clone();
            let shutdown = shutdown.clone();

            thread::spawn(move || {
                let mut extractor = NoteExtractor::new();
                let mut local_data_version = 0;
                let mut chunk_notes = Vec::with_capacity(128);

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    let (maybe_chunk_idx, current_ver, midi_data, tex_height) = {
                        let mut s = lock_mutex(&state);

                        if s.data_version != local_data_version {
                            local_data_version = s.data_version;
                            if let Some(ref data) = s.current_midi {
                                extractor.reset_with_tracks(
                                    // Safety: data.as_ptr() returns a valid C struct pointer; the header field is properly initialized
                                    unsafe { (*data.as_ptr()).header.tracks as usize },
                                );
                            }
                        }

                        if s.current_midi.is_none() {
                            (None, s.data_version, None, 0)
                        } else {
                            // Pick the first Todo chunk in order (0 → max)
                            let best_idx = s.chunks.iter().position(|&status| status == ChunkStatus::Todo);

                            if let Some(idx) = best_idx {
                                s.chunks[idx] = ChunkStatus::InProgress;
                                (Some(idx), s.data_version, s.current_midi.clone(), s.texture_height)
                            } else {
                                (None, s.data_version, None, 0)
                            }
                        }
                    };

                    if let (Some(idx), ver, Some(data)) = (maybe_chunk_idx, current_ver, midi_data) {
                        let chunk_start = idx as u64 * CHUNK_DURATION_NS;
                        let chunk_end = chunk_start + CHUNK_DURATION_NS;

                        extractor.extract_notes_in_range(
                            // Safety: data is Arc<MIDIPlayerData>, as_ptr() is valid for the duration of this scope
                            unsafe { &*data.as_ptr() },
                            chunk_start,
                            chunk_end,
                            &mut chunk_notes,
                        );

                        let texture =
                            generate_chunk_texture(&chunk_notes, chunk_start, chunk_end, tex_height, ver);

                        let should_send = {
                            let mut s = lock_mutex(&state);
                            if s.data_version == ver && idx < s.chunks.len() {
                                s.chunks[idx] = ChunkStatus::Done;
                                s.chunks_dirty = true; // rebuilt lazily on next get_chunks_status()
                                gen_count.fetch_add(1, Ordering::SeqCst);
                                true
                            } else {
                                // If the data version changed while we were generating, revert
                                // this chunk to Todo so another worker (or retry) can pick it up.
                                if idx < s.chunks.len() && s.chunks[idx] == ChunkStatus::InProgress {
                                    s.chunks[idx] = ChunkStatus::Todo;
                                    s.chunks_dirty = true;
                                }
                                false
                            }
                        };
                        if should_send {
                            // Non-blocking send to avoid deadlock on shutdown
                            if tx_texture.try_send(texture).is_err() {
                                // Queue full: revert chunk to Todo so another worker can retry.
                                let mut s = lock_mutex(&state);
                                if idx < s.chunks.len() && s.chunks[idx] == ChunkStatus::Done {
                                    s.chunks[idx] = ChunkStatus::Todo;
                                    s.chunks_dirty = true;
                                    gen_count.fetch_sub(1, Ordering::SeqCst);
                                }
                            }
                        }
                    } else {
                        // Use a thread-local static to avoid allocating a Mutex on every idle iteration.
                        thread_local! {
                            static IDLE_MUTEX: Mutex<()> = Mutex::new(());
                        }
                        IDLE_MUTEX.with(|m| {
                            let lock = lock_mutex(m);
                            let _ = task_queue_cv
                                .wait_timeout(lock, std::time::Duration::from_millis(100))
                                .unwrap();
                        });
                    }
                }
            });
        }

        // ─── Coordinator thread ──────────────────────────────────────────
        let state_coordinator = state.clone();
        let task_queue_cv_coordinator = task_queue_cv.clone();
        let shutdown_coordinator = shutdown.clone();
        let gen_count_coordinator = generated_count.clone();
        let expected_version_coordinator = expected_version.clone();

        thread::spawn(move || {
            loop {
                while let Ok(cmd) = rx_command.try_recv() {
                    match cmd {
                        TextureGenCommand::UpdatePosition(pos) => {
                            let mut s = lock_mutex(&state_coordinator);
                            s.current_position_ns = pos;
                        }
                        TextureGenCommand::UpdateData(data) => {
                            let midi_end_time_ns = unsafe {
                                // Safety: data.as_ptr() is valid; tick_to_ns only reads the struct
                                tick_to_ns(&*data.as_ptr(), (*data.as_ptr()).max_tick)
                            };
                            let max = ((midi_end_time_ns as f64 / CHUNK_DURATION_NS as f64).ceil()
                                as usize)
                                .max(1);

                            let mut s = lock_mutex(&state_coordinator);
                            s.data_version += 1;
                            expected_version_coordinator.store(s.data_version, Ordering::SeqCst);
                            s.current_midi = Some(data);
                            s.max_chunks = max;
                            s.chunks = vec![ChunkStatus::Todo; max];
                            s.chunks_arc = Arc::new(Vec::new()); // invalidate old snapshot
                            s.chunks_dirty = true;
                            s.current_position_ns = 0;
                            gen_count_coordinator.store(0, Ordering::SeqCst);
                            task_queue_cv_coordinator.notify_all();
                        }
                        TextureGenCommand::SetMaxChunks(max) => {
                            let mut s = lock_mutex(&state_coordinator);
                            if max > s.max_chunks {
                                s.chunks.resize(max, ChunkStatus::Todo);
                                s.max_chunks = max;
                                s.chunks_dirty = true;
                                task_queue_cv_coordinator.notify_all();
                            }
                        }
                        TextureGenCommand::SetTextureHeight(height) => {
                            let mut s = lock_mutex(&state_coordinator);
                            s.texture_height = height;
                            s.data_version += 1;
                            expected_version_coordinator.store(s.data_version, Ordering::SeqCst);
                            if s.current_midi.is_some() {
                                s.chunks = vec![ChunkStatus::Todo; s.max_chunks];
                            } else {
                                s.chunks = Vec::new();
                                s.max_chunks = 0;
                            }
                            s.chunks_dirty = true;
                            s.current_position_ns = 0;
                            gen_count_coordinator.store(0, Ordering::SeqCst);
                            task_queue_cv_coordinator.notify_all();
                        }
                        TextureGenCommand::CancelAll => {
                            let mut s = lock_mutex(&state_coordinator);
                            s.data_version += 1;
                            expected_version_coordinator.store(s.data_version, Ordering::SeqCst);
                            s.chunks = Vec::new();
                            s.chunks_dirty = true;
                            s.max_chunks = 0;
                            s.current_midi = None;
                            s.current_position_ns = 0;
                            gen_count_coordinator.store(0, Ordering::SeqCst);
                            task_queue_cv_coordinator.notify_all();
                        }
                    }
                }

                if shutdown_coordinator.load(Ordering::Relaxed) {
                    break;
                }

                thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        Self {
            tx_command,
            rx_texture,
            shutdown,
            task_queue_cv,
            state,
            generated_count,
            expected_version,
        }
    }

    pub fn try_recv_texture(&self) -> Option<NoteChunkTexture> {
        loop {
            let expected = self.expected_version.load(Ordering::SeqCst);
            match self.rx_texture.try_recv().ok() {
                Some(chunk) => {
                    if chunk.data_version == expected {
                        return Some(chunk);
                    }
                }
                None => return None,
            }
        }
    }

    pub fn send(&self, cmd: TextureGenCommand) {
        self.tx_command.send(cmd).ok();
    }

    pub fn get_current_texture_count(&self) -> usize {
        self.generated_count.load(Ordering::SeqCst) as usize
    }

    pub fn update_position(&self, position_ns: u64) {
        self.send(TextureGenCommand::UpdatePosition(position_ns));
    }

    pub fn cancel_all(&self) {
        self.send(TextureGenCommand::CancelAll);
        while self.try_recv_texture().is_some() {}
    }

    pub fn set_max_chunks(&self, max_chunks: usize) {
        self.send(TextureGenCommand::SetMaxChunks(max_chunks));
    }

    pub fn get_max_chunks(&self) -> usize {
        lock_mutex(&self.state).max_chunks
    }

    pub fn get_chunks_status(&self) -> Arc<Vec<ChunkStatus>> {
        let mut s = lock_mutex(&self.state);
        if s.chunks_dirty {
            s.chunks_arc = Arc::new(s.chunks.clone());
            s.chunks_dirty = false;
        }
        s.chunks_arc.clone()
    }

    pub fn get_chunk_duration_ns(&self) -> u64 {
        CHUNK_DURATION_NS
    }
    pub fn set_texture_height(&self, height: usize) {
        self.send(TextureGenCommand::SetTextureHeight(height));
    }

}

impl Drop for NoteTextureGenerator {
    fn drop(&mut self) {
        if !self.shutdown.load(Ordering::SeqCst) {
            self.shutdown.store(true, Ordering::SeqCst);
            self.task_queue_cv.notify_all();
        }
    }
}

fn generate_chunk_texture(
    notes: &[TimedNote],
    start_time_ns: u64,
    end_time_ns: u64,
    texture_height: usize,
    data_version: u64,
) -> NoteChunkTexture {
    if notes.is_empty() {
        // Generate a 1-pixel tall transparent texture so the chunk still exists
        // in the texture manager, preventing "gaps" in the piano roll.
        return NoteChunkTexture {
            start_time_ns,
            end_time_ns,
            width: TEXTURE_WIDTH,
            height: 1,
            rgba_data: Some(vec![0u8; TEXTURE_WIDTH * 4]),
            data_version,
        };
    }

    let duration_us = ((end_time_ns - start_time_ns) / 1000) as u32;
    let height = texture_height;
    let mut rgba_data = Vec::with_capacity(TEXTURE_WIDTH * height * 4);
    rgba_data.resize(TEXTURE_WIDTH * height * 4, 0);

    // Sort a Vec of indices instead of allocating a Vec<&TimedNote>.
    // This avoids an extra heap allocation per chunk; the index Vec is smaller and
    // freed immediately after the loop.
    let mut indices: Vec<usize> = (0..notes.len()).collect();
    indices.sort_unstable_by(|&a, &b| {
        notes[a].start_rel_us
            .cmp(&notes[b].start_rel_us)
            .then(notes[a].channel.cmp(&notes[b].channel))
            .then(notes[a].note.cmp(&notes[b].note))
    });

    for &i in &indices {
        let note = &notes[i];
        let x = note.note as usize;
        let x = x.min(TEXTURE_WIDTH - 1);

        let start_y_normal = ((note.start_rel_us as f64 / duration_us as f64) * height as f64)
            .round() as usize;
        let end_y_normal = ((note.end_rel_us as f64 / duration_us as f64) * height as f64)
            .round() as usize;

        let start_y = height.saturating_sub(end_y_normal.clamp(0, height));
        let end_y = height.saturating_sub(start_y_normal.clamp(0, height));

        if start_y >= end_y {
            continue;
        }

        let color = get_note_color_by_track_channel(note.track_index, note.channel);
        let pixel_x = x;
        let row_stride = TEXTURE_WIDTH * 4;
        let base_offset = pixel_x * 4;
        for y in start_y..end_y {
            let idx = y * row_stride + base_offset;
            // Safety: idx+3 is always within bounds given the loop ranges
            unsafe {
                *rgba_data.get_unchecked_mut(idx) = color.0;
                *rgba_data.get_unchecked_mut(idx + 1) = color.1;
                *rgba_data.get_unchecked_mut(idx + 2) = color.2;
                *rgba_data.get_unchecked_mut(idx + 3) = 255;
            }
        }
    }

    NoteChunkTexture {
        start_time_ns,
        end_time_ns,
        width: TEXTURE_WIDTH,
        height,
        rgba_data: Some(rgba_data),
        data_version,
    }
}
