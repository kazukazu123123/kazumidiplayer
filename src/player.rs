use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::kdmapi::KDMAPIStream;
use crate::kazumidiparser_cursor::{cursor_next, ns_to_tick, seek_track_cursor, tick_to_ns, TrackCursor};
use crate::midi_parser_worker::{MIDIParserWorker, ParserCommand, ParserFeedbackCommand};
use crate::utils::lock_mutex;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
pub enum PlayerState {
    Stopped = 0,
    Playing = 1,
    Paused = 2,
    Loading = 3,
}

const MAX_EVENTS_PER_FRAME: usize = 8192;

pub enum PlayerCommand {
    Play,
    Pause,
    Stop,
    Seek(u64),
    LoadFile(String),
    SetPitchBendOverride(bool),
    SetPitchBendMultiplier(u8),
    SetUnlimitedOutput(bool),
    SetSpeed(f32),
    SetParseThreads(u32),
}

pub enum PlayerFeedback {
    Loaded(String),
    Progress(crate::midi_parser_worker::ParseProgress),

    ParseFailed(String),
    ParseCancelled,
}

pub(crate) struct TrackState {
    cursor: TrackCursor,
    pending: bool,
    pend_tick: u32,
    pend_status: u8,
    pend_p1: u8,
    pend_p2: u8,
}

// Safety: TrackState contains only u8 fields, a bool, and a TrackCursor (C struct).
// All fields are trivially Copy/Send.
unsafe impl Send for TrackState {}

pub struct PlayerHandle {
    tx_command: Sender<PlayerCommand>,
    state: Arc<AtomicU32>,
    position_ns: Arc<AtomicU64>,
    duration_ns: Arc<AtomicU64>,
    current_midi_data: Arc<std::sync::Mutex<Option<Arc<kazumidiparser::MIDIPlayerData>>>>,
    active_notes: Arc<std::sync::Mutex<[Option<(u16, u8)>; 128]>>,
    pitch_bend: Arc<std::sync::Mutex<[u16; 16]>>,
    pitch_bend_range: Arc<std::sync::Mutex<[f32; 16]>>,
    pitch_bend_override: Arc<AtomicBool>,
    pitch_bend_multiplier: Arc<AtomicU8>,
    unlimited_output: Arc<AtomicBool>,
    // Playback speed as f32 bits (1.0 = normal). Scales wall-clock -> song time.
    playback_speed: Arc<AtomicU32>,
    // Running note-on count, used to derive NPS in the stats overlay.
    notes_played: Arc<AtomicU64>,
}

impl Clone for PlayerHandle {
    fn clone(&self) -> Self {
        Self {
            tx_command: self.tx_command.clone(),
            state: self.state.clone(),
            position_ns: self.position_ns.clone(),
            duration_ns: self.duration_ns.clone(),
            current_midi_data: self.current_midi_data.clone(),
            active_notes: self.active_notes.clone(),
            pitch_bend: self.pitch_bend.clone(),
            pitch_bend_range: self.pitch_bend_range.clone(),
            pitch_bend_override: self.pitch_bend_override.clone(),
            pitch_bend_multiplier: self.pitch_bend_multiplier.clone(),
            unlimited_output: self.unlimited_output.clone(),
            playback_speed: self.playback_speed.clone(),
            notes_played: self.notes_played.clone(),
        }
    }
}

impl PlayerHandle {
    pub fn spawn(
        rx_main: Receiver<PlayerCommand>,
        tx_main_feedback: Sender<PlayerFeedback>,
        kdmapi: KDMAPIStream<'static>,
    ) -> Self {
        let state = Arc::new(AtomicU32::new(PlayerState::Stopped as u32));
        let position_ns = Arc::new(AtomicU64::new(0));
        let duration_ns = Arc::new(AtomicU64::new(1));
        let current_midi_data =
            Arc::new(std::sync::Mutex::new(None::<Arc<kazumidiparser::MIDIPlayerData>>));
        let active_notes = Arc::new(std::sync::Mutex::new([None; 128]));
        let pitch_bend = Arc::new(std::sync::Mutex::new([8192u16; 16]));
        let pitch_bend_range = Arc::new(std::sync::Mutex::new([2.0f32; 16]));
        let pitch_bend_override = Arc::new(AtomicBool::new(false));
        let pitch_bend_multiplier = Arc::new(AtomicU8::new(12));
        let unlimited_output = Arc::new(AtomicBool::new(false));
        let playback_speed = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let notes_played = Arc::new(AtomicU64::new(0));

        let (tx_command_to_thread, rx_thread_commands) = mpsc::channel::<PlayerCommand>();
        let tx_forward_main_channel = tx_command_to_thread.clone();

        thread::spawn(move || {
            while let Ok(cmd) = rx_main.recv() {
                tx_forward_main_channel.send(cmd).ok();
            }
        });

        let (tx_parser_feedback, rx_parser_feedback) = mpsc::channel::<ParserFeedbackCommand>();
        let parser_worker = MIDIParserWorker::spawn(tx_parser_feedback);
        let current_parse_cancellation_token =
            Arc::new(std::sync::Mutex::new(None::<kazumidiparser::CancelToken>));

        let state_for_thread = state.clone();
        let position_ns_for_thread = position_ns.clone();
        let duration_ns_for_thread = duration_ns.clone();
        let tx_main_feedback_for_thread = tx_main_feedback.clone();
        let parser_worker_for_thread = parser_worker.clone();
        let current_parse_cancellation_token_for_thread = current_parse_cancellation_token.clone();
        let current_midi_data_for_thread = current_midi_data.clone();
        let active_notes_for_thread = active_notes.clone();
        let pitch_bend_for_thread = pitch_bend.clone();
        let pitch_bend_range_for_thread = pitch_bend_range.clone();
        let kdmapi_for_thread = kdmapi;
        let pitch_bend_override_for_thread = pitch_bend_override.clone();
        let pitch_bend_multiplier_for_thread = pitch_bend_multiplier.clone();
        let unlimited_output_for_thread = unlimited_output.clone();
        let playback_speed_for_thread = playback_speed.clone();
        let notes_played_for_thread = notes_played.clone();

        thread::spawn(move || {
            let pitch_bend_override_clone = pitch_bend_override_for_thread;
            let pitch_bend_multiplier_clone = pitch_bend_multiplier_for_thread;
            let pitch_bend_clone = pitch_bend_for_thread;
            let pitch_bend_range_clone = pitch_bend_range_for_thread;
            let state_clone = state_for_thread;
            let position_clone = position_ns_for_thread;
            let duration_clone = duration_ns_for_thread;
            let current_parse_cancellation_token_clone = current_parse_cancellation_token_for_thread;
            let active_notes_clone = active_notes_for_thread;
            let playback_speed_clone = playback_speed_for_thread;
            let notes_played_clone = notes_played_for_thread;
            // Running count of every note-on triggered, used to derive NPS.
            let mut notes_played_count: u64 = 0;
            // Wall-clock -> song-time speed actually applied to `playback_start`.
            // When the requested speed changes we re-anchor `playback_start`.
            let mut applied_speed = 1.0f32;
            // Per-key count of currently-held note-ons (across all tracks/channels).
            // A key clears only when this drops to 0, so overlapping notes on the
            // same key (unisons/doublings) don't blank it out on the first note-off.
            let mut active_count = [0u16; 128];

            let mut player = MIDIPlayer::new(
                kdmapi_for_thread,
                pitch_bend_override_clone,
                pitch_bend_multiplier_clone,
                unlimited_output_for_thread,
            );
            let mut playback_start = Instant::now();

            loop {
                let mut cmd_iter = rx_thread_commands.try_iter().peekable();
                while let Some(cmd) = cmd_iter.next() {
                    match cmd {
                        PlayerCommand::Play => {
                            player.state = PlayerState::Playing;
                            applied_speed = read_speed(&playback_speed_clone);
                            playback_start = anchor_start(player.seek_position_ns, applied_speed);
                        }
                        PlayerCommand::Pause => {
                            player.send_all_notes_off();
                            player.state = PlayerState::Paused;
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                        }
                        PlayerCommand::Stop => {
                            player.reset_playback_state();
                            player.state = PlayerState::Stopped;
                            player.seek_position_ns = 0;
                            player.reset_tracks();
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            *lock_mutex(&pitch_bend_clone) = [8192; 16];
                        }
                        PlayerCommand::Seek(pos) => {
                            let mut latest_pos = pos;
                            while let Some(PlayerCommand::Seek(_)) = cmd_iter.peek() {
                                if let Some(PlayerCommand::Seek(next_pos)) = cmd_iter.next() {
                                    latest_pos = next_pos;
                                }
                            }

                            player.reset_playback_state();
                            player.seek_position_ns = latest_pos;
                            applied_speed = read_speed(&playback_speed_clone);
                            playback_start = anchor_start(latest_pos, applied_speed);
                            player.seek_tracks(latest_pos);
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            *lock_mutex(&pitch_bend_clone) = [8192; 16];

                            position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                            state_clone.store(player.state as u32, Ordering::SeqCst);
                        }
                        PlayerCommand::LoadFile(path) => {
                            player.send_all_notes_off();
                            player.stream.send_direct_data(0xFF);
                            player.data = None;
                            player.tracks.clear();
                            player.state = PlayerState::Loading;
                            state_clone.store(PlayerState::Loading as u32, Ordering::SeqCst);
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            notes_played_count = 0;
                            notes_played_clone.store(0, Ordering::Relaxed);
                            *lock_mutex(&pitch_bend_clone) = [8192; 16];
                            // 古い MIDI データを即座に解放
                            *lock_mutex(&current_midi_data_for_thread) = None;

                            let new_cancel_token = kazumidiparser::CancelToken::new();
                            {
                                let mut current_token_guard =
                                    lock_mutex(&current_parse_cancellation_token_clone);
                                if let Some(prev_token) = current_token_guard.take() {
                                    prev_token.cancel();
                                }
                                *current_token_guard = Some(new_cancel_token.clone());
                            }

                            parser_worker_for_thread
                                .send(ParserCommand::Load(path, new_cancel_token));
                        }
                        PlayerCommand::SetPitchBendOverride(enabled) => {
                            let prev = player.pitch_bend_override.load(Ordering::SeqCst);
                            player.pitch_bend_override.store(enabled, Ordering::SeqCst);
                            if enabled && !prev {
                                player.send_pitch_bend_range_override();
                            } else if !enabled && prev {
                                player.send_pitch_bend_range_restore();
                            }
                        }
                        PlayerCommand::SetPitchBendMultiplier(multiplier) => {
                            let prev = player.pitch_bend_multiplier.load(Ordering::SeqCst);
                            player.pitch_bend_multiplier.store(multiplier, Ordering::SeqCst);
                            if player.pitch_bend_override.load(Ordering::SeqCst) && multiplier != prev {
                                player.send_pitch_bend_range_override();
                            }
                        }
                        PlayerCommand::SetUnlimitedOutput(enabled) => {
                            player.unlimited_output.store(enabled, Ordering::SeqCst);
                        }
                        PlayerCommand::SetSpeed(speed) => {
                            let clamped = speed.clamp(0.05, 4.0);
                            playback_speed_clone.store(clamped.to_bits(), Ordering::Relaxed);
                        }
                        PlayerCommand::SetParseThreads(threads) => {
                            parser_worker_for_thread.set_threads(threads);
                        }
                    }
                }

                while let Ok(feedback_cmd) = rx_parser_feedback.try_recv() {
                    match feedback_cmd {
                        ParserFeedbackCommand::ParseComplete(data, path, token) => {
                            let is_current = {
                                let mut current_token_guard =
                                    lock_mutex(&current_parse_cancellation_token_clone);
                                if let Some(stored_token) = current_token_guard.as_ref() {
                                    if *stored_token == token {
                                        *current_token_guard = None;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };

                            if !is_current {
                                continue;
                            }

                            *lock_mutex(&current_midi_data_for_thread) =
                                Some(data.clone());

                            player.load_parsed_data(Some(data));
                            duration_clone.store(player.get_duration_ns(), Ordering::SeqCst);

                            *lock_mutex(&pitch_bend_clone) = [8192; 16];

                            playback_start = Instant::now();
                            player.state = PlayerState::Paused;

                            position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                            state_clone.store(player.state as u32, Ordering::SeqCst);

                            let _ = tx_main_feedback_for_thread
                                .send(PlayerFeedback::Loaded(path));
                        }
                        ParserFeedbackCommand::ParseCancelled(token) => {
                            let is_current = {
                                let current_token_guard =
                                    lock_mutex(&current_parse_cancellation_token_clone);
                                if let Some(stored_token) = current_token_guard.as_ref() {
                                    *stored_token == token
                                } else {
                                    false
                                }
                            };

                            if !is_current {
                                continue;
                            }

                            {
                                let mut current_token_guard =
                                    lock_mutex(&current_parse_cancellation_token_clone);
                                *current_token_guard = None;
                            }

                            player.state = PlayerState::Stopped;
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            let _ = tx_main_feedback_for_thread
                                .send(PlayerFeedback::ParseCancelled);
                        }
                        ParserFeedbackCommand::ParseFailed(path, token) => {
                            let is_current = {
                                let mut current_token_guard =
                                    lock_mutex(&current_parse_cancellation_token_clone);
                                if let Some(stored_token) = current_token_guard.as_ref() {
                                    if *stored_token == token {
                                        *current_token_guard = None;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };

                            if !is_current {
                                continue;
                            }

                            player.state = PlayerState::Stopped;
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            let _ = tx_main_feedback_for_thread
                                .send(PlayerFeedback::ParseFailed(path));
                        }
                        ParserFeedbackCommand::Progress(progress) => {
                            let _ = tx_main_feedback_for_thread
                                .send(PlayerFeedback::Progress(progress));
                        }
                    }
                }

                if player.state == PlayerState::Playing {
                    if let Some(data) = &player.data {
                        // If the requested speed changed, re-anchor playback_start so the
                        // current song position stays continuous, then apply the new rate.
                        let requested_speed = read_speed(&playback_speed_clone);
                        if requested_speed != applied_speed {
                            playback_start = anchor_start(player.seek_position_ns, requested_speed);
                            applied_speed = requested_speed;
                        }
                        let elapsed_ns =
                            (playback_start.elapsed().as_nanos() as f64 * applied_speed as f64) as u64;
                        player.seek_position_ns = elapsed_ns;

                        let cur_tick =
                            // Safety: data.as_ptr() returns a valid pointer to the C MIDI struct
                            ns_to_tick(unsafe { &*data.as_ptr() }, player.seek_position_ns);

                        let stream = &mut player.stream;
                        let rpn_msb = &mut player.channel_rpn_msb;
                        let rpn_lsb = &mut player.channel_rpn_lsb;
                        let pb_range = &mut player.channel_pitch_bend_range;
                        let pb_override = player.pitch_bend_override.load(Ordering::Relaxed);
                        let pb_multiplier = player.pitch_bend_multiplier.load(Ordering::Relaxed);

                        let mut events_processed = 0usize;
                        let unlimited = player.unlimited_output.load(Ordering::Relaxed);

                        for track in &mut player.tracks {
                            // Process pending event from previous frame
                            if track.pending {
                                if track.pend_tick <= cur_tick {
                                    if track.pend_status != 0xFF {
                                        send_midi_event_with_override(
                                            stream,
                                            track.pend_status,
                                            track.pend_p1,
                                            track.pend_p2,
                                            rpn_msb,
                                            rpn_lsb,
                                            pb_range,
                                            pb_override,
                                            pb_multiplier,
                                            &mut player.midi_original_pb_range,
                                        );
                                        {
                                            let mut guard = lock_mutex(&active_notes_clone);
                                            if track.pend_p1 < 128 {
                                                let key = track.pend_p1 as usize;
                                                let msg_type = track.pend_status & 0xF0;
                                                if msg_type == 0x90 && track.pend_p2 > 0 {
                                                    active_count[key] += 1;
                                                    notes_played_count += 1;
                                                    guard[key] =
                                                        Some((track.cursor.track_index, track.pend_status & 0x0F));
                                                } else if msg_type == 0x80
                                                    || (msg_type == 0x90 && track.pend_p2 == 0)
                                                {
                                                    if active_count[key] > 0 {
                                                        active_count[key] -= 1;
                                                    }
                                                    if active_count[key] == 0 {
                                                        guard[key] = None;
                                                    }
                                                }
                                            }
                                        }
                                        {
                                            let msg_type = track.pend_status & 0xF0;
                                            if msg_type == 0xE0 {
                                                let channel = (track.pend_status & 0x0F) as usize;
                                                let pb = (track.pend_p1 as u16) | ((track.pend_p2 as u16) << 7);
                                                lock_mutex(&pitch_bend_clone)[channel.clamp(0, 15)] = pb;
                                            }
                                        }
                                    }
                                    track.pending = false;
                                    events_processed += 1;
                                }
                            }

                            while !track.pending && (unlimited || events_processed < MAX_EVENTS_PER_FRAME) {
                                if !unsafe { cursor_next(&mut track.cursor) } {
                                    break;
                                }
                                events_processed += 1;
                                if track.cursor.is_tempo {
                                    continue;
                                }
                                if track.cursor.abs_tick <= cur_tick {
                                    send_midi_event_with_override(
                                        stream,
                                        track.cursor.status,
                                        track.cursor.param1,
                                        track.cursor.param2,
                                        rpn_msb,
                                        rpn_lsb,
                                        pb_range,
                                        pb_override,
                                        pb_multiplier,
                                        &mut player.midi_original_pb_range,
                                    );
                                    {
                                        let mut guard = lock_mutex(&active_notes_clone);
                                        if track.cursor.param1 < 128 {
                                            let key = track.cursor.param1 as usize;
                                            let msg_type = track.cursor.status & 0xF0;
                                            if msg_type == 0x90 && track.cursor.param2 > 0 {
                                                active_count[key] += 1;
                                                notes_played_count += 1;
                                                guard[key] =
                                                    Some((track.cursor.track_index, track.cursor.status & 0x0F));
                                            } else if msg_type == 0x80
                                                || (msg_type == 0x90 && track.cursor.param2 == 0)
                                            {
                                                if active_count[key] > 0 {
                                                    active_count[key] -= 1;
                                                }
                                                if active_count[key] == 0 {
                                                    guard[key] = None;
                                                }
                                            }
                                        }
                                    }
                                    {
                                        let msg_type = track.cursor.status & 0xF0;
                                        if msg_type == 0xE0 {
                                            let channel = (track.cursor.status & 0x0F) as usize;
                                            let pb = (track.cursor.param1 as u16) | ((track.cursor.param2 as u16) << 7);
                                            lock_mutex(&pitch_bend_clone)[channel.clamp(0, 15)] = pb;
                                        }
                                    }
                                } else {
                                    track.pending = true;
                                    track.pend_tick = track.cursor.abs_tick;
                                    track.pend_status = track.cursor.status;
                                    track.pend_p1 = track.cursor.param1;
                                    track.pend_p2 = track.cursor.param2;
                                    break;
                                }
                            }
                        }

                        let max_tick = unsafe { (*data.as_ptr()).max_tick };
                        // Safety: data.as_ptr() is valid, non-null, with properly initialized fields
                        let max_ns = tick_to_ns(unsafe { &*data.as_ptr() }, max_tick);
                        if player.seek_position_ns >= max_ns {
                            player.state = PlayerState::Stopped;
                            player.reset_playback_state();
                            *lock_mutex(&active_notes_clone) = [None; 128];
                            active_count = [0u16; 128];
                            *lock_mutex(&pitch_bend_clone) = [8192; 16];
                        }
                    }
                }

                state_clone.store(player.state as u32, Ordering::SeqCst);
                position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                notes_played_clone.store(notes_played_count, Ordering::Relaxed);
                *lock_mutex(&pitch_bend_range_clone) = player.channel_pitch_bend_range;

                if player.state != PlayerState::Playing {
                    thread::sleep(Duration::from_millis(10));
                } else {
                    std::thread::yield_now();
                }
            }
        });

        Self {
            tx_command: tx_command_to_thread,
            state,
            position_ns,
            duration_ns,
            current_midi_data,
            active_notes,
            pitch_bend,
            pitch_bend_range,
            pitch_bend_override,
            pitch_bend_multiplier,
            unlimited_output,
            playback_speed,
            notes_played,
        }
    }

    pub fn send(&self, cmd: PlayerCommand) {
        self.tx_command.send(cmd).ok();
    }

    pub fn set_pitch_bend_override(&self, enabled: bool) {
        let _ = self.tx_command.send(PlayerCommand::SetPitchBendOverride(enabled));
    }

    pub fn set_pitch_bend_multiplier(&self, multiplier: u8) {
        let _ = self.tx_command.send(PlayerCommand::SetPitchBendMultiplier(multiplier));
    }

    pub fn set_unlimited_output(&self, enabled: bool) {
        let _ = self.tx_command.send(PlayerCommand::SetUnlimitedOutput(enabled));
    }

    pub fn set_speed(&self, speed: f32) {
        let _ = self.tx_command.send(PlayerCommand::SetSpeed(speed));
    }

    /// Takes effect on the next load; a parse already in flight keeps its count.
    pub fn set_parse_threads(&self, threads: u32) {
        let _ = self.tx_command.send(PlayerCommand::SetParseThreads(threads));
    }

    /// Running total of note-ons triggered for the current file.
    pub fn notes_played(&self) -> u64 {
        self.notes_played.load(Ordering::Relaxed)
    }

    pub fn get_midi_data(&self) -> Option<Arc<kazumidiparser::MIDIPlayerData>> {
        lock_mutex(&self.current_midi_data).clone()
    }

    pub fn get_state(&self) -> PlayerState {
        match self.state.load(Ordering::SeqCst) {
            0 => PlayerState::Stopped,
            1 => PlayerState::Playing,
            2 => PlayerState::Paused,
            3 => PlayerState::Loading,
            _ => PlayerState::Stopped,
        }
    }

    pub fn position_ns(&self) -> u64 {
        self.position_ns.load(Ordering::SeqCst)
    }

    pub fn duration_ns(&self) -> u64 {
        self.duration_ns.load(Ordering::SeqCst)
    }

    pub fn get_active_notes(&self) -> [Option<(u16, u8)>; 128] {
        *lock_mutex(&self.active_notes)
    }

    pub fn get_pitch_bend_normalized(&self) -> f32 {
        let values = lock_mutex(&self.pitch_bend);
        let mut result = 0.0f32;
        for v in values.iter() {
            let normalized = (*v as f32 - 8192.0) / 8192.0;
            if normalized.abs() > result.abs() {
                result = normalized;
            }
        }
        result.clamp(-1.0, 1.0)
    }

    pub fn get_pitch_bend_range(&self) -> f32 {
        let ranges = lock_mutex(&self.pitch_bend_range);
        let sum: f32 = ranges.iter().sum();
        sum / ranges.len() as f32
    }
}

pub struct MIDIPlayer {
    pub stream: KDMAPIStream<'static>,
    pub data: Option<Arc<kazumidiparser::MIDIPlayerData>>,
    pub state: PlayerState,
    pub seek_position_ns: u64,
    pub tracks: Vec<TrackState>,
    pitch_bend_override: Arc<AtomicBool>,
    pitch_bend_multiplier: Arc<AtomicU8>,
    unlimited_output: Arc<AtomicBool>,
    channel_rpn_msb: [u8; 16],
    channel_rpn_lsb: [u8; 16],
    channel_pitch_bend_range: [f32; 16],
    midi_original_pb_range: [u8; 16],
}

impl MIDIPlayer {
    fn send_message(&mut self, msg: u32) {
        self.stream.send_direct_data(msg);
    }

    fn send_all_notes_off(&mut self) {
        for ch in 0..16u32 {
            self.stream.send_direct_data((0xB0 | ch) | (123 << 8) | (0 << 16));
            self.stream.send_direct_data((0xB0 | ch) | (120 << 8) | (0 << 16));
        }
    }

    /// Bring the synth back to a neutral state after a stop or a seek: silence
    /// everything, reset controllers, and force the pitch wheel back to center.
    /// All-notes-off alone leaves the wheel wherever the song last bent it, so
    /// the next notes would sound detuned until that channel's next 0xE0 event.
    fn reset_playback_state(&mut self) {
        self.send_all_notes_off();
        for ch in 0..16u32 {
            self.stream.send_direct_data((0xB0 | ch) | (121 << 8) | (0 << 16));
            self.stream.send_direct_data((0xE0 | ch) | (0 << 8) | (64 << 16));
        }
        // Reset-all-controllers leaves RPN selection undefined and may drop the
        // pitch bend sensitivity we programmed, so re-establish both.
        self.reset_channel_states();
        if self.pitch_bend_override.load(Ordering::SeqCst) {
            self.send_pitch_bend_range_override();
        }
    }

    pub fn send_pitch_bend_range_override(&mut self) {
        let multiplier = self.pitch_bend_multiplier.load(Ordering::SeqCst).clamp(1, 127);
        for ch in 0..16 {
            self.send_message((0xB0 | ch as u32) | (101 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (100 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (6 << 8) | ((multiplier as u32) << 16));
            self.send_message((0xB0 | ch as u32) | (38 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (101 << 8) | (127 << 16));
            self.send_message((0xB0 | ch as u32) | (100 << 8) | (127 << 16));
            self.channel_rpn_msb[ch] = 127;
            self.channel_rpn_lsb[ch] = 127;
            self.channel_pitch_bend_range[ch] = multiplier as f32;
        }
    }

    fn send_pitch_bend_range_restore(&mut self) {
        for ch in 0..16 {
            let semitones = self.midi_original_pb_range[ch];
            self.send_message((0xB0 | ch as u32) | (101 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (100 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (6 << 8) | ((semitones as u32) << 16));
            self.send_message((0xB0 | ch as u32) | (38 << 8) | (0 << 16));
            self.send_message((0xB0 | ch as u32) | (101 << 8) | (127 << 16));
            self.send_message((0xB0 | ch as u32) | (100 << 8) | (127 << 16));
            self.channel_rpn_msb[ch] = 127;
            self.channel_rpn_lsb[ch] = 127;
            self.channel_pitch_bend_range[ch] = semitones as f32;
        }
    }

    pub fn new(
        kdmapi: KDMAPIStream<'static>,
        pitch_bend_override: Arc<AtomicBool>,
        pitch_bend_multiplier: Arc<AtomicU8>,
        unlimited_output: Arc<AtomicBool>,
    ) -> Self {
        Self {
            stream: kdmapi,
            data: None,
            state: PlayerState::Stopped,
            seek_position_ns: 0,
            tracks: Vec::new(),
            pitch_bend_override,
            pitch_bend_multiplier,
            unlimited_output,
            channel_rpn_msb: [255; 16],
            channel_rpn_lsb: [255; 16],
            channel_pitch_bend_range: [2.0; 16],
            midi_original_pb_range: [2; 16],
        }
    }

    fn reset_channel_states(&mut self) {
        self.channel_rpn_msb = [255; 16];
        self.channel_rpn_lsb = [255; 16];
        self.channel_pitch_bend_range = [2.0; 16];
        self.midi_original_pb_range = [2; 16];
    }

    pub fn load_parsed_data(&mut self, data: Option<Arc<kazumidiparser::MIDIPlayerData>>) {
        self.data = data;
        self.state = PlayerState::Stopped;
        self.seek_position_ns = 0;
        self.reset_channel_states();
        self.reset_tracks();
        if self.pitch_bend_override.load(Ordering::SeqCst) {
            self.send_pitch_bend_range_override();
        }
    }

    pub fn get_duration_ns(&self) -> u64 {
        self.data
            .as_ref()
            .map(|d| {
                let data = unsafe { &*d.as_ptr() };
                // Safety: d is Arc<MIDIPlayerData> from the C FFI parser;
                // as_ptr() returns a valid, non-null pointer to the underlying C struct.
                tick_to_ns(data, data.max_tick)
            })
            .unwrap_or(1)
    }

    pub fn reset_tracks(&mut self) {
        self.tracks.clear();
        if let Some(data) = &self.data {
            let num_tracks = data.header().tracks as usize;
            for i in 0..num_tracks {
                // Safety: data.as_ptr() returns a valid C struct pointer
                let cursor = unsafe { seek_track_cursor(&*data.as_ptr(), i as u16, 0) };
                self.tracks.push(TrackState {
                    pending: cursor.has_event && !cursor.is_tempo,
                    pend_tick: cursor.abs_tick,
                    pend_status: cursor.status,
                    pend_p1: cursor.param1,
                    pend_p2: cursor.param2,
                    cursor,
                });
            }
        }
    }

    pub fn seek_tracks(&mut self, target_ns: u64) {
        self.tracks.clear();
        if let Some(data) = &self.data {
            let target_tick = ns_to_tick(unsafe { &*data.as_ptr() }, target_ns);
            // Safety: data.as_ptr() valid for the lifetime of this call
            let num_tracks = data.header().tracks as usize;
            for i in 0..num_tracks {
                let cursor =
                    unsafe { seek_track_cursor(&*data.as_ptr(), i as u16, target_tick) };
                self.tracks.push(TrackState {
                    pending: cursor.has_event && !cursor.is_tempo,
                    pend_tick: cursor.abs_tick,
                    pend_status: cursor.status,
                    pend_p1: cursor.param1,
                    pend_p2: cursor.param2,
                    cursor,
                });
            }
        }
    }
}

fn send_midi_event_with_override(
    stream: &mut KDMAPIStream<'static>,
    status: u8,
    p1: u8,
    p2: u8,
    rpn_msb: &mut [u8; 16],
    rpn_lsb: &mut [u8; 16],
    _pb_range: &mut [f32; 16],
    pb_override: bool,
    pb_multiplier: u8,
    midi_original_pb_range: &mut [u8; 16],
) {
    let msg_type = status & 0xF0;
    let channel = (status & 0x0F) as usize;

    let out_p1 = p1;
    let mut out_p2 = p2;

    if msg_type == 0xB0 {
        match p1 {
            101 => rpn_msb[channel] = p2,
            100 => rpn_lsb[channel] = p2,
            6 => {
                let rpn = ((rpn_msb[channel] as u16) << 7) | (rpn_lsb[channel] as u16);
                if rpn == 0 {
                    // Pitch bend range (semitones)
                    if pb_override {
                        midi_original_pb_range[channel] = p2;
                        out_p2 = pb_multiplier;
                        _pb_range[channel] = pb_multiplier as f32;
                    } else {
                        _pb_range[channel] = p2 as f32;
                    }
                }
            }
            38 => {
                let rpn = ((rpn_msb[channel] as u16) << 7) | (rpn_lsb[channel] as u16);
                if pb_override && rpn == 0 {
                    out_p2 = 0;
                }
            }
            _ => {}
        }
    }

    let midi_msg = status as u32 | ((out_p1 as u32) << 8) | ((out_p2 as u32) << 16);
    stream.send_direct_data(midi_msg);
}

/// Read the requested playback speed, clamped to the supported range.
fn read_speed(speed: &Arc<AtomicU32>) -> f32 {
    f32::from_bits(speed.load(Ordering::Relaxed)).clamp(0.05, 4.0)
}

/// Wall-clock anchor such that `anchor.elapsed() * speed == position_ns`, i.e. the
/// instant that maps to song position 0 at the given speed.
fn anchor_start(position_ns: u64, speed: f32) -> Instant {
    let back_ns = (position_ns as f64 / speed as f64) as u64;
    Instant::now() - Duration::from_nanos(back_ns)
}


