use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::audio_engine::OutputStream;
use crate::kazumidiparser_cursor::{
    TrackCursor, ns_to_tick, seek_track_cursor, tick_to_ns,
};
use crate::midi_parser_worker::{MIDIParserWorker, ParserFeedbackCommand};
use crate::utils::lock_mutex;
mod player_thread;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
pub enum PlayerState {
    Stopped = 0,
    Playing = 1,
    Paused = 2,
    Loading = 3,
}

/// Longest the player thread spends walking events for the display of a
/// prerender engine in one frame, checked every `DISPLAY_BUDGET_CHECK` events.
const DISPLAY_BUDGET: Duration = Duration::from_millis(12);
const DISPLAY_BUDGET_CHECK: u32 = 1024;

pub const DEFAULT_MIDI_EV_BUFFER_SIZE: u32 = 8192;
pub const MIN_MIDI_EV_BUFFER_SIZE: u32 = 1;
pub const MAX_MIDI_EV_BUFFER_SIZE: u32 = 16_384;
const MIDI_EV_FRAME_DURATION: Duration = Duration::from_nanos(1_000_000_000 / 60);

pub enum PlayerCommand {
    Play,
    Pause,
    Stop,
    Seek(u64),
    LoadFile(String),
    SetPitchBendOverride(bool),
    SetPitchBendMultiplier(u8),
    SetUnlimitedOutput(bool),
    SetMidiEvBufferSize(u32),
    SetMidiEvCatchUp(bool),
    SetLegacyMidiOutput(bool),
    SetSpeed(f32),
    SetParseThreads(u32),
    /// Replace the output (audio engine switch).
    SetOutput(Option<OutputStream>),
    /// Longest prerendered audio buffer, in seconds.
    SetAudioBufferSecs(u32),
    /// S16 Clipping: no limiter, hard-clipped peaks instead.
    SetS16Clipping(bool),
    /// Built-in piano synth: most voices at once, 0 = unlimited.
    SetEngineSetting { key: String, value: crate::audio_engine::EngineSettingValue },
    /// Opus bitrate for the prerender buffer, in kbps.
    SetOpusBitrate(u32),
}

pub enum PlayerFeedback {
    Loaded(String, std::time::Duration),
    Progress(crate::midi_parser_worker::ParseProgress),

    ParseFailed(String),
    ParseCancelled,
}

pub(crate) struct TrackState {
    cursor: TrackCursor,
    pending: bool,
    exhausted: bool,
    pend_tick: u32,
    pend_status: u8,
    pend_p1: u8,
    pend_p2: u8,
}

#[derive(Clone, Copy)]
struct BufferedMidiEvent {
    tick: u32,
    track_index: u16,
    status: u8,
    p1: u8,
    p2: u8,
}

#[allow(clippy::too_many_arguments)]
fn dispatch_midi_event(
    event: BufferedMidiEvent,
    stream: Option<&mut OutputStream>,
    channel_rpn_msb: &mut [u8; 16],
    channel_rpn_lsb: &mut [u8; 16],
    channel_pitch_bend_range: &mut [f32; 16],
    pitch_bend_override: bool,
    pitch_bend_multiplier: u8,
    midi_original_pb_range: &mut [u8; 16],
    blocking_output: bool,
    active_notes: &std::sync::Mutex<[Option<(u16, u8)>; 128]>,
    active_count: &mut [u16; 128],
    notes_played_count: &mut u64,
    pitch_bend: &std::sync::Mutex<[u16; 16]>,
) {
    send_midi_event_with_override(
        stream,
        event.status,
        event.p1,
        event.p2,
        channel_rpn_msb,
        channel_rpn_lsb,
        channel_pitch_bend_range,
        pitch_bend_override,
        pitch_bend_multiplier,
        midi_original_pb_range,
        blocking_output,
    );

    let msg_type = event.status & 0xF0;
    if event.p1 < 128 {
        let key = event.p1 as usize;
        let mut guard = lock_mutex(active_notes);
        if msg_type == 0x90 && event.p2 > 0 {
            active_count[key] = active_count[key].saturating_add(1);
            *notes_played_count += 1;
            guard[key] = Some((event.track_index, event.status & 0x0F));
        } else if msg_type == 0x80 || (msg_type == 0x90 && event.p2 == 0) {
            active_count[key] = active_count[key].saturating_sub(1);
            if active_count[key] == 0 {
                guard[key] = None;
            }
        }
    }
    if msg_type == 0xE0 {
        let channel = (event.status & 0x0F) as usize;
        let bend = (event.p1 as u16) | ((event.p2 as u16) << 7);
        lock_mutex(pitch_bend)[channel] = bend;
    }
}

// Safety: TrackState contains only u8 fields, a bool, and a TrackCursor (C struct).
// All fields are trivially Copy/Send.
unsafe impl Send for TrackState {}

/// Clears its flag when dropped, which for the player thread happens only if that
/// thread unwinds: its loop never returns. A dead player thread silently swallows
/// every command, so without this the app just stops responding to play, seek and
/// even file loads with no indication why.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

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
    // False once the player thread has unwound; see `AliveGuard`.
    alive: Arc<AtomicBool>,
    // Prerendered audio waiting to play; `u64::MAX` for realtime outputs.
    audio_buffer_ns: Arc<AtomicU64>,
    // Voices sounding in the output engine; `u32::MAX` when it does not
    // report a count.
    polyphony: Arc<AtomicU32>,
    // Smoothed share of the realtime budget the engine spends rendering
    // (f32 bits; 1.0 = 100%); `u32::MAX` when it does not report one.
    render_load: Arc<AtomicU32>,
    // Note-on velocity a prerender engine drops up to; 0 when not skipping.
    drop_velocity: Arc<AtomicU32>,
    // Prerender buffer allocation in bytes and output rate; `u64::MAX` / 0
    // for realtime outputs. `buffer_compressed` is false when the buffer
    // is raw PCM (a rate Opus does not support).
    buffer_memory: Arc<AtomicU64>,
    output_rate: Arc<AtomicU32>,
    buffer_compressed: Arc<AtomicBool>,
    // Debug builds: cumulative ViSQOL series of the prerender Opus stream
    // ((MOS-LQO, bitrate_kbps) per 1-second window scored every half second),
    // mirrored for the debug window.
    #[cfg(debug_assertions)]
    visqol_series: Arc<std::sync::Mutex<Vec<(f32, u32)>>>,
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
            alive: self.alive.clone(),
            audio_buffer_ns: self.audio_buffer_ns.clone(),
            polyphony: self.polyphony.clone(),
            render_load: self.render_load.clone(),
            drop_velocity: self.drop_velocity.clone(),
            buffer_memory: self.buffer_memory.clone(),
            output_rate: self.output_rate.clone(),
            buffer_compressed: self.buffer_compressed.clone(),
            #[cfg(debug_assertions)]
            visqol_series: self.visqol_series.clone(),
        }
    }
}

impl PlayerHandle {
    pub fn spawn(
        rx_main: Receiver<PlayerCommand>,
        tx_main_feedback: Sender<PlayerFeedback>,
        kdmapi: Option<OutputStream>,
    ) -> Self {
        let state = Arc::new(AtomicU32::new(PlayerState::Stopped as u32));
        let position_ns = Arc::new(AtomicU64::new(0));
        let duration_ns = Arc::new(AtomicU64::new(1));
        let current_midi_data = Arc::new(std::sync::Mutex::new(
            None::<Arc<kazumidiparser::MIDIPlayerData>>,
        ));
        let active_notes = Arc::new(std::sync::Mutex::new([None; 128]));
        let pitch_bend = Arc::new(std::sync::Mutex::new([8192u16; 16]));
        let pitch_bend_range = Arc::new(std::sync::Mutex::new([2.0f32; 16]));
        let pitch_bend_override = Arc::new(AtomicBool::new(false));
        let pitch_bend_multiplier = Arc::new(AtomicU8::new(12));
        let unlimited_output = Arc::new(AtomicBool::new(false));
        let playback_speed = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let notes_played = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let audio_buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
        let audio_buffer_ns_for_thread = audio_buffer_ns.clone();
        let polyphony = Arc::new(AtomicU32::new(u32::MAX));
        let polyphony_for_thread = polyphony.clone();
        let render_load = Arc::new(AtomicU32::new(u32::MAX));
        let render_load_for_thread = render_load.clone();
        let drop_velocity = Arc::new(AtomicU32::new(0));
        let drop_velocity_for_thread = drop_velocity.clone();
        let buffer_memory = Arc::new(AtomicU64::new(u64::MAX));
        let buffer_memory_for_thread = buffer_memory.clone();
        let output_rate = Arc::new(AtomicU32::new(0));
        let output_rate_for_thread = output_rate.clone();
        let buffer_compressed = Arc::new(AtomicBool::new(false));
        let buffer_compressed_for_thread = buffer_compressed.clone();
        #[cfg(debug_assertions)]
        let visqol_series = Arc::new(std::sync::Mutex::new(Vec::<(f32, u32)>::new()));
        #[cfg(debug_assertions)]
        let visqol_series_for_thread = visqol_series.clone();

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
        let alive_for_thread = alive.clone();

        thread::spawn(move || {
            player_thread::run(
                pitch_bend_override_for_thread,
                pitch_bend_multiplier_for_thread,
                pitch_bend_for_thread,
                pitch_bend_range_for_thread,
                state_for_thread,
                position_ns_for_thread,
                duration_ns_for_thread,
                tx_main_feedback_for_thread,
                parser_worker_for_thread,
                current_parse_cancellation_token_for_thread,
                current_midi_data_for_thread,
                active_notes_for_thread,
                kdmapi_for_thread,
                unlimited_output_for_thread,
                playback_speed_for_thread,
                notes_played_for_thread,
                alive_for_thread,
                audio_buffer_ns_for_thread,
                polyphony_for_thread,
                render_load_for_thread,
                drop_velocity_for_thread,
                buffer_memory_for_thread,
                output_rate_for_thread,
                buffer_compressed_for_thread,
                #[cfg(debug_assertions)]
                visqol_series_for_thread,
                rx_thread_commands,
                rx_parser_feedback,
            );
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
            alive,
            audio_buffer_ns,
            polyphony,
            render_load,
            drop_velocity,
            buffer_memory,
            output_rate,
            buffer_compressed,
            #[cfg(debug_assertions)]
            visqol_series,
        }
    }

    /// False once the player thread has died. Commands sent to a dead player are
    /// silently dropped, so the UI must say so rather than appear merely stuck.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn send(&self, cmd: PlayerCommand) {
        self.tx_command.send(cmd).ok();
    }

    pub fn set_pitch_bend_override(&self, enabled: bool) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetPitchBendOverride(enabled));
    }

    pub fn set_pitch_bend_multiplier(&self, multiplier: u8) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetPitchBendMultiplier(multiplier));
    }

    pub fn set_unlimited_output(&self, enabled: bool) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetUnlimitedOutput(enabled));
    }

    /// S16 Clipping: no limiter, samples past full scale wrap around.
    /// Only prerender engines honor it.
    pub fn set_s16_clipping(&self, enabled: bool) {
        let _ = self.tx_command.send(PlayerCommand::SetS16Clipping(enabled));
    }

    /// Apply one of the selected engine's published settings (see
    /// `EngineSettingDef`); engines that do not publish the key ignore it.
    pub fn set_engine_setting(
        &self,
        key: &str,
        value: crate::audio_engine::EngineSettingValue,
    ) {
        let _ = self.tx_command.send(PlayerCommand::SetEngineSetting {
            key: key.to_string(),
            value,
        });
    }

    /// Set the Opus bitrate of the prerender buffer.
    pub fn set_opus_bitrate(&self, kbps: u32) {
        let _ = self.tx_command.send(PlayerCommand::SetOpusBitrate(kbps));
    }

    /// Debug builds: snapshot of the prerender Opus ViSQOL series.
    #[cfg(debug_assertions)]
    pub fn visqol_series(&self) -> Vec<(f32, u32)> {
        lock_mutex(&self.visqol_series).clone()
    }

    pub fn set_midi_ev_buffer_size(&self, size: u32) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetMidiEvBufferSize(size));
    }

    pub fn set_midi_ev_catch_up(&self, enabled: bool) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetMidiEvCatchUp(enabled));
    }

    pub fn set_legacy_midi_output(&self, enabled: bool) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetLegacyMidiOutput(enabled));
    }

    pub fn set_speed(&self, speed: f32) {
        let _ = self.tx_command.send(PlayerCommand::SetSpeed(speed));
    }

    pub fn set_audio_buffer_secs(&self, secs: u32) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetAudioBufferSecs(secs));
    }

    /// Prerendered audio waiting to play, or `None` for a realtime output.
    pub fn audio_buffer_ns(&self) -> Option<u64> {
        Some(self.audio_buffer_ns.load(Ordering::Relaxed)).filter(|&ns| ns != u64::MAX)
    }

    /// Voices sounding in the output engine, or `None` when it does not
    /// report a count.
    pub fn polyphony(&self) -> Option<u32> {
        Some(self.polyphony.load(Ordering::Relaxed)).filter(|&n| n != u32::MAX)
    }

    /// Smoothed share of the realtime budget the engine spends rendering
    /// (1.0 = 100%), or `None` when it does not report one.
    pub fn render_load(&self) -> Option<f32> {
        let bits = self.render_load.load(Ordering::Relaxed);
        (bits != u32::MAX).then(|| f32::from_bits(bits))
    }

    /// Velocity notes are being skipped up to, or `None` when not skipping.
    pub fn skipping_velocity(&self) -> Option<u32> {
        Some(self.drop_velocity.load(Ordering::Relaxed)).filter(|&v| v > 0)
    }

    /// Prerender buffer allocation in bytes, output rate and whether the
    /// buffer is Opus-compressed, or `None` for a realtime output.
    pub fn buffer_memory(&self) -> Option<(u64, u32, bool)> {
        let bytes = self.buffer_memory.load(Ordering::Relaxed);
        (bytes != u64::MAX).then(|| {
            (
                bytes,
                self.output_rate.load(Ordering::Relaxed),
                self.buffer_compressed.load(Ordering::Relaxed),
            )
        })
    }

    pub fn set_output(&self, stream: Option<OutputStream>) {
        let _ = self.tx_command.send(PlayerCommand::SetOutput(stream));
    }

    /// Takes effect on the next load; a parse already in flight keeps its count.
    pub fn set_parse_threads(&self, threads: u32) {
        let _ = self
            .tx_command
            .send(PlayerCommand::SetParseThreads(threads));
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
    pub stream: Option<OutputStream>,
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
        if let Some(ref stream) = self.stream {
            stream.send_direct_data(msg);
        }
    }

    fn send_all_notes_off(&mut self) {
        let stream = match self.stream.as_ref() {
            Some(s) => s,
            None => return,
        };
        for ch in 0..16u32 {
            stream.send_direct_data((0xB0 | ch) | (123 << 8) | (0 << 16));
            stream.send_direct_data((0xB0 | ch) | (120 << 8) | (0 << 16));
        }
    }

    /// Bring the synth back to a neutral state after a stop or a seek: silence
    /// everything, reset controllers, and force the pitch wheel back to center.
    /// All-notes-off alone leaves the wheel wherever the song last bent it, so
    /// the next notes would sound detuned until that channel's next 0xE0 event.
    fn reset_playback_state(&mut self) {
        self.send_all_notes_off();
        if let Some(ref stream) = self.stream {
            for ch in 0..16u32 {
                stream.send_direct_data((0xB0 | ch) | (121 << 8) | (0 << 16));
                stream.send_direct_data((0xE0 | ch) | (0 << 8) | (64 << 16));
            }
        }
        // Reset-all-controllers leaves RPN selection undefined and may drop the
        // pitch bend sensitivity we programmed, so re-establish both.
        self.reset_channel_states();
        if self.pitch_bend_override.load(Ordering::SeqCst) {
            self.send_pitch_bend_range_override();
        }
    }

    pub fn send_pitch_bend_range_override(&mut self) {
        let multiplier = self
            .pitch_bend_multiplier
            .load(Ordering::SeqCst)
            .clamp(1, 127);
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

    /// Forced bend range for prerender sessions, if Pitch Bend Override is on.
    fn bend_override(&self) -> Option<u8> {
        self.pitch_bend_override
            .load(Ordering::SeqCst)
            .then(|| self.pitch_bend_multiplier.load(Ordering::SeqCst))
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
        kdmapi: Option<OutputStream>,
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
                    exhausted: !cursor.has_event,
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
                let cursor = unsafe { seek_track_cursor(&*data.as_ptr(), i as u16, target_tick) };
                self.tracks.push(TrackState {
                    pending: cursor.has_event && !cursor.is_tempo,
                    exhausted: !cursor.has_event,
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
    stream: Option<&mut OutputStream>,
    status: u8,
    p1: u8,
    p2: u8,
    rpn_msb: &mut [u8; 16],
    rpn_lsb: &mut [u8; 16],
    _pb_range: &mut [f32; 16],
    pb_override: bool,
    pb_multiplier: u8,
    midi_original_pb_range: &mut [u8; 16],
    blocking_output: bool,
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

    if let Some(stream) = stream {
        let midi_msg = status as u32 | ((out_p1 as u32) << 8) | ((out_p2 as u32) << 16);
        if blocking_output {
            stream.send_direct_data_blocking(midi_msg);
        } else {
            stream.send_direct_data(midi_msg);
        }
    }
}

/// Read the requested playback speed, clamped to the supported range.
fn read_speed(speed: &Arc<AtomicU32>) -> f32 {
    f32::from_bits(speed.load(Ordering::Relaxed)).clamp(0.05, 4.0)
}
