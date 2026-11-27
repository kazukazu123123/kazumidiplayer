//! The player thread: owns the MIDIPlayer and the parser worker and
//! runs the command and playback loop that `PlayerHandle::spawn` starts.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio_engine::OutputStream;
use crate::kazumidiparser_cursor::{
    NoteOnIndex, count_note_ons_before, cursor_next, ns_to_tick, tick_to_ns,
};
use crate::midi_parser_worker::{MIDIParserWorker, ParserCommand, ParserFeedbackCommand};
use crate::utils::lock_mutex;

use super::{
    AliveGuard, BufferedMidiEvent, DEFAULT_MIDI_EV_BUFFER_SIZE, DISPLAY_BUDGET,
    DISPLAY_BUDGET_CHECK, DISPLAY_RESEEK_LAG, MAX_MIDI_EV_BUFFER_SIZE, MIDIPlayer,
    MIN_MIDI_EV_BUFFER_SIZE,
    MIDI_EV_FRAME_DURATION, PlayerCommand, PlayerFeedback, PlayerState, PositionAnchor,
    dispatch_midi_event, read_speed,
};

/// Note-ons before `tick` in `data`: via the block index when it
/// covers this song, and a full scan otherwise (a safety net; the
/// index is built during load, so this should not happen).
fn recount_notes(
    note_index: &Option<(Arc<kazumidiparser::MIDIPlayerData>, Arc<NoteOnIndex>)>,
    data: &Arc<kazumidiparser::MIDIPlayerData>,
    tick: u32,
) -> u64 {
    let raw = unsafe { &*data.as_ptr() };
    match index_for(note_index, data) {
        Some(index) => unsafe { index.count_before(raw, tick) },
        None => unsafe { count_note_ons_before(raw, tick) },
    }
}

/// The block index, when the one built at load covers `data`.
fn index_for<'a>(
    note_index: &'a Option<(Arc<kazumidiparser::MIDIPlayerData>, Arc<NoteOnIndex>)>,
    data: &Arc<kazumidiparser::MIDIPlayerData>,
) -> Option<&'a NoteOnIndex> {
    match note_index {
        Some((indexed, index)) if Arc::ptr_eq(indexed, data) => Some(&**index),
        _ => None,
    }
}

/// The player thread's whole life: respond to commands, fold in parser
/// feedback, and keep due MIDI events flowing to the output. Every
/// argument is one of the shared cells or channels `PlayerHandle::spawn`
/// set up; the `_for_thread` names are the thread's own handles to them.
pub(super) fn run(
    pitch_bend_override_for_thread: Arc<AtomicBool>,
    pitch_bend_multiplier_for_thread: Arc<AtomicU8>,
    pitch_bend_for_thread: Arc<Mutex<[u16; 16]>>,
    pitch_bend_range_for_thread: Arc<Mutex<[f32; 16]>>,
    state_for_thread: Arc<AtomicU32>,
    position_ns_for_thread: Arc<AtomicU64>,
    position_anchor_for_thread: Arc<Mutex<Option<PositionAnchor>>>,
    duration_ns_for_thread: Arc<AtomicU64>,
    tx_main_feedback_for_thread: Sender<PlayerFeedback>,
    parser_worker_for_thread: MIDIParserWorker,
    current_parse_cancellation_token_for_thread: Arc<Mutex<Option<kazumidiparser::CancelToken>>>,
    current_midi_data_for_thread: Arc<Mutex<Option<Arc<kazumidiparser::MIDIPlayerData>>>>,
    active_notes_for_thread: Arc<Mutex<[Option<(u16, u8)>; 128]>>,
    kdmapi_for_thread: Option<OutputStream>,
    unlimited_output_for_thread: Arc<AtomicBool>,
    playback_speed_for_thread: Arc<AtomicU32>,
    notes_played_for_thread: Arc<AtomicU64>,
    alive_for_thread: Arc<AtomicBool>,
    audio_buffer_ns_for_thread: Arc<AtomicU64>,
    polyphony_for_thread: Arc<AtomicU32>,
    render_load_for_thread: Arc<AtomicU32>,
    drop_velocity_for_thread: Arc<AtomicU32>,
    buffer_memory_for_thread: Arc<AtomicU64>,
    output_rate_for_thread: Arc<AtomicU32>,
    buffer_compressed_for_thread: Arc<AtomicBool>,
    #[cfg(debug_assertions)]
    visqol_series_for_thread: Arc<Mutex<Vec<(f32, u32)>>>,
    rx_thread_commands: Receiver<PlayerCommand>,
    rx_parser_feedback: Receiver<ParserFeedbackCommand>,
) {
    let pitch_bend_override_clone = pitch_bend_override_for_thread;
    let pitch_bend_multiplier_clone = pitch_bend_multiplier_for_thread;
    let pitch_bend_clone = pitch_bend_for_thread;
    let pitch_bend_range_clone = pitch_bend_range_for_thread;
    let state_clone = state_for_thread;
    let position_clone = position_ns_for_thread;
    let position_anchor_clone = position_anchor_for_thread;
    let duration_clone = duration_ns_for_thread;
    let current_parse_cancellation_token_clone =
        current_parse_cancellation_token_for_thread;
    let active_notes_clone = active_notes_for_thread;
    let playback_speed_clone = playback_speed_for_thread;
    let notes_played_clone = notes_played_for_thread;
    // Dropped only if this thread panics, flagging the handle as dead.
    let _alive_guard = AliveGuard(alive_for_thread);
    // Running count of every note-on triggered, used to derive NPS.
    let mut notes_played_count: u64 = 0;
    // Note-on block index for the current song, built synchronously
    // when a load completes so the recounts on seeks stay cheap.
    let mut note_index: Option<(Arc<kazumidiparser::MIDIPlayerData>, Arc<NoteOnIndex>)> = None;
    // Song time runs from an anchor: `playback_base_ns` is the position the
    // anchor instant maps to, and time since then is scaled by the speed.
    //
    //     position = playback_base_ns + playback_start.elapsed() * applied_speed
    //
    // Re-anchoring (on play, seek, or a speed change) sets the base to the
    // current position and restarts the clock. It used to instead back-date the
    // anchor to the instant that mapped to position 0, but on Windows an
    // `Instant` cannot precede boot, so seeking deep into a long file at a slow
    // speed would push the anchor before it and panic.
    let mut playback_base_ns: u64 = 0;
    // Wall-clock -> song-time speed actually applied since the anchor.
    let mut applied_speed = 1.0f32;
    // Per-key count of currently-held note-ons (across all tracks/channels).
    // A key clears only when this drops to 0, so overlapping notes on the
    // same key (unisons/doublings) don't blank it out on the first note-off.
    let mut active_count = [0u16; 128];
    // WinMM-like bounded submission FIFO. Events enter in global
    // (tick, track) order and state changes happen only when they leave it.
    let mut midi_ev_buffer_size = DEFAULT_MIDI_EV_BUFFER_SIZE as usize;
    let mut midi_ev_catch_up = false;
    let mut legacy_midi_output = true;
    let mut midi_ev_fifo = VecDeque::<BufferedMidiEvent>::new();
    let mut audio_buffer_secs = crate::audio_engine::prerender::DEFAULT_MAX_BUFFER_SECS;
    let mut s16_clipping = false;
    let mut engine_settings: Vec<(String, crate::audio_engine::EngineSettingValue)> = Vec::new();
    let mut opus_bitrate_kbps = 256u32;
    #[cfg(debug_assertions)]
    let mut visqol_consumed: usize = 0;
    // The prerender session of the current output, if any.
    macro_rules! prerender {
        ($player:ident) => {
            $player.stream.as_ref().and_then(|s| s.prerender())
        };
    }

    let mut player = MIDIPlayer::new(
        kdmapi_for_thread,
        pitch_bend_override_clone,
        pitch_bend_multiplier_clone,
        unlimited_output_for_thread,
    );
    let mut playback_start = Instant::now();

    loop {
        let frame_start = Instant::now();
        let mut cmd_iter = rx_thread_commands.try_iter().peekable();
        while let Some(cmd) = cmd_iter.next() {
            match cmd {
                PlayerCommand::Play => {
                    player.state = PlayerState::Playing;
                    applied_speed = read_speed(&playback_speed_clone);
                    playback_base_ns = player.seek_position_ns;
                    playback_start = Instant::now();
                    if let Some(p) = prerender!(player) {
                        p.play(player.seek_position_ns, applied_speed);
                    }
                    // A stale anchor from before a pause would extrapolate
                    // from the old position; the playing pass sets a fresh
                    // one for prerendered outputs.
                    *lock_mutex(&position_anchor_clone) = None;
                }
                PlayerCommand::Pause => {
                    if let Some(p) = prerender!(player) {
                        // Resume exactly where the kept buffer begins.
                        player.seek_position_ns = p.pause();
                    }
                    midi_ev_fifo.clear();
                    player.send_all_notes_off();
                    player.state = PlayerState::Paused;
                    *lock_mutex(&active_notes_clone) = [None; 128];
                    active_count = [0u16; 128];
                }
                PlayerCommand::Stop => {
                    if let Some(p) = prerender!(player) {
                        p.pause();
                        p.seek(0);
                    }
                    midi_ev_fifo.clear();
                    player.reset_playback_state();
                    player.state = PlayerState::Stopped;
                    player.seek_position_ns = 0;
                    player.reset_tracks();
                    notes_played_count = 0;
                    notes_played_clone.store(0, Ordering::Relaxed);
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

                    midi_ev_fifo.clear();
                    player.reset_playback_state();
                    player.seek_position_ns = latest_pos;
                    applied_speed = read_speed(&playback_speed_clone);
                    playback_base_ns = latest_pos;
                    playback_start = Instant::now();
                    player.seek_tracks(latest_pos);
                    if let Some(p) = prerender!(player) {
                        // May land a few frames off when skipping
                        // inside the buffer; follow the audio.
                        player.seek_position_ns = p.seek(latest_pos);
                    }
                    // The counter counts dispatched note-ons, so a
                    // jump leaves it describing the old position:
                    // re-derive it from the new one.
                    if let Some(data) = &player.data {
                        let tick =
                            ns_to_tick(unsafe { &*data.as_ptr() }, player.seek_position_ns);
                        notes_played_count = recount_notes(&note_index, data, tick);
                        notes_played_clone.store(notes_played_count, Ordering::Relaxed);
                    }
                    *lock_mutex(&active_notes_clone) = [None; 128];
                    active_count = [0u16; 128];
                    *lock_mutex(&pitch_bend_clone) = [8192; 16];

                    // The position jumped; drop the extrapolation anchor.
                    *lock_mutex(&position_anchor_clone) = None;
                    position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                    state_clone.store(player.state as u32, Ordering::SeqCst);
                }
                PlayerCommand::LoadFile(path) => {
                    if let Some(p) = prerender!(player) {
                        p.pause();
                        p.set_data(None, None);
                    }
                    midi_ev_fifo.clear();
                    if let Some(ref stream) = player.stream {
                        // Do not let output queued for the old file delay its
                        // replacement after a blocked KDMAPI call recovers.
                        stream.invalidate_pending();
                    }
                    player.send_all_notes_off();
                    if let Some(ref stream) = player.stream {
                        stream.send_direct_data(0xFF);
                    }
                    player.data = None;
                    player.tracks.clear();
                    player.state = PlayerState::Loading;
                    state_clone.store(PlayerState::Loading as u32, Ordering::SeqCst);
                    *lock_mutex(&active_notes_clone) = [None; 128];
                    active_count = [0u16; 128];
                    notes_played_count = 0;
                    notes_played_clone.store(0, Ordering::Relaxed);
                    note_index = None;
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
                    if let Some(p) = prerender!(player) {
                        p.set_bend_override(player.bend_override());
                    }
                }
                PlayerCommand::SetPitchBendMultiplier(multiplier) => {
                    let prev = player.pitch_bend_multiplier.load(Ordering::SeqCst);
                    player
                        .pitch_bend_multiplier
                        .store(multiplier, Ordering::SeqCst);
                    if player.pitch_bend_override.load(Ordering::SeqCst)
                        && multiplier != prev
                    {
                        player.send_pitch_bend_range_override();
                    }
                    if let Some(p) = prerender!(player) {
                        p.set_bend_override(player.bend_override());
                    }
                }
                PlayerCommand::SetUnlimitedOutput(enabled) => {
                    player.unlimited_output.store(enabled, Ordering::SeqCst);
                }
                PlayerCommand::SetS16Clipping(enabled) => {
                    s16_clipping = enabled;
                    if let Some(ref stream) = player.stream {
                        stream.set_limiter_bypassed(enabled);
                    }
                    if let Some(p) = prerender!(player) {
                        p.set_limiter_bypassed(enabled);
                    }
                }
                PlayerCommand::SetEngineSetting { key, value } => {
                    engine_settings.retain(|(k, _)| *k != key);
                    engine_settings.push((key.clone(), value));
                    if let Some(ref stream) = player.stream {
                        stream.set_setting(&key, value);
                    }
                }
                PlayerCommand::SetOpusBitrate(kbps) => {
                    opus_bitrate_kbps = kbps;
                    if let Some(p) = prerender!(player) {
                        p.set_opus_bitrate(kbps);
                    }
                }
                PlayerCommand::SetMidiEvBufferSize(size) => {
                    midi_ev_buffer_size = size
                        .clamp(MIN_MIDI_EV_BUFFER_SIZE, MAX_MIDI_EV_BUFFER_SIZE)
                        as usize;
                }
                PlayerCommand::SetMidiEvCatchUp(enabled) => {
                    if !enabled && midi_ev_catch_up {
                        // Slowdown mode adopts the currently visible delayed
                        // position instead of retaining the wall-clock target.
                        playback_base_ns = player.seek_position_ns;
                        playback_start = Instant::now();
                    }
                    midi_ev_catch_up = enabled;
                }
                PlayerCommand::SetLegacyMidiOutput(enabled) => {
                    legacy_midi_output = enabled;
                    midi_ev_fifo.clear();
                    if enabled {
                        // Legacy output must start without asynchronous buffered
                        // sends left in front of its first blocking event.
                        if let Some(ref stream) = player.stream {
                            stream.invalidate_pending();
                        }
                    }
                }
                PlayerCommand::SetSpeed(speed) => {
                    let clamped = speed.clamp(0.05, 4.0);
                    playback_speed_clone.store(clamped.to_bits(), Ordering::Relaxed);
                    if let Some(p) = prerender!(player) {
                        p.set_speed(clamped);
                    }
                }
                PlayerCommand::SetParseThreads(threads) => {
                    parser_worker_for_thread.set_threads(threads);
                }
                PlayerCommand::SetOutput(stream) => {
                    // Silence the old output, then bring the new one to the
                    // same neutral state a seek would leave behind.
                    midi_ev_fifo.clear();
                    player.send_all_notes_off();
                    player.stream = stream;
                    player.reset_playback_state();
                    // The clock source changed with the output.
                    *lock_mutex(&position_anchor_clone) = None;
                    // Re-apply S16 Clipping and the engine settings to the new output.
                    if let Some(ref stream) = player.stream {
                        stream.set_limiter_bypassed(s16_clipping);
                        for (key, value) in &engine_settings {
                            stream.set_setting(key, *value);
                        }
                    }
                    if let Some(p) = prerender!(player) {
                        p.set_bend_override(player.bend_override());
                        p.set_max_buffer_secs(audio_buffer_secs);
                        p.set_opus_bitrate(opus_bitrate_kbps);
                        p.set_limiter_bypassed(s16_clipping);
                        p.set_data(player.data.clone(), player.note_index.clone());
                        if player.state == PlayerState::Playing {
                            applied_speed = read_speed(&playback_speed_clone);
                            p.play(player.seek_position_ns, applied_speed);
                        } else {
                            p.seek(player.seek_position_ns);
                        }
                    } else if player.state == PlayerState::Playing {
                        // Leaving prerender: continue on the wall clock.
                        playback_base_ns = player.seek_position_ns;
                        playback_start = Instant::now();
                    }
                }
                PlayerCommand::SetAudioBufferSecs(secs) => {
                    audio_buffer_secs = secs;
                    if let Some(p) = prerender!(player) {
                        p.set_max_buffer_secs(secs);
                    }
                }
            }
        }

        while let Ok(feedback_cmd) = rx_parser_feedback.try_recv() {
            match feedback_cmd {
                ParserFeedbackCommand::ParseComplete(data, path, token, parse_duration) => {
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

                    *lock_mutex(&current_midi_data_for_thread) = Some(data.clone());

                    // Build the note-on block index first: load
                    // completion blocks on it, so seeks and recounts
                    // resume from it instead of walking from the
                    // track start.
                    let built_index =
                        Arc::new(unsafe { NoteOnIndex::build(&*data.as_ptr()) });
                    note_index = Some((data.clone(), built_index.clone()));
                    if let Some(p) = prerender!(player) {
                        p.set_data(Some(data.clone()), Some(built_index.clone()));
                        p.seek(0);
                    }
                    player.load_parsed_data(Some(data), Some(built_index));
                    duration_clone.store(player.get_duration_ns(), Ordering::SeqCst);

                    *lock_mutex(&pitch_bend_clone) = [8192; 16];

                    playback_base_ns = player.seek_position_ns;
                    playback_start = Instant::now();
                    player.state = PlayerState::Paused;

                    position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                    state_clone.store(player.state as u32, Ordering::SeqCst);

                    let _ = tx_main_feedback_for_thread.send(PlayerFeedback::Loaded(path, parse_duration));
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
                    let _ =
                        tx_main_feedback_for_thread.send(PlayerFeedback::ParseCancelled);
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
                    let _ =
                        tx_main_feedback_for_thread.send(PlayerFeedback::ParseFailed(path));
                }
                ParserFeedbackCommand::Progress(progress) => {
                    let _ = tx_main_feedback_for_thread
                        .send(PlayerFeedback::Progress(progress));
                }
            }
        }

        let mut display_reseek = None;
        if player.state == PlayerState::Playing {
            if let Some(data) = &player.data {
                // If the requested speed changed, re-anchor at the current position
                // so song time stays continuous, then apply the new rate.
                let requested_speed = read_speed(&playback_speed_clone);
                if requested_speed != applied_speed {
                    playback_base_ns = player.seek_position_ns;
                    playback_start = Instant::now();
                    applied_speed = requested_speed;
                }
                let position_before_fill = player.seek_position_ns;
                let since_anchor_ns = (playback_start.elapsed().as_nanos() as f64
                    * applied_speed as f64)
                    as u64;
                // Prerendered output is the clock: follow what is audible.
                let ideal_position_ns = match prerender!(player) {
                    Some(p) => p.position_ns(),
                    None => playback_base_ns.saturating_add(since_anchor_ns),
                };
                let cur_tick = ns_to_tick(unsafe { &*data.as_ptr() }, ideal_position_ns);
                player.seek_position_ns = ideal_position_ns;

                // Prerender outputs ignore the MIDI sent here (it only
                // drives the display), so pacing it would just make
                // the display lag the audio: always go direct.
                let is_prerender = prerender!(player).is_some();
                // Anchor the shown position to the heard position: the
                // walk below can take ~100 ms on dense files, and the UI
                // extrapolates from this anchor between passes instead of
                // stepping at the pass rate (see
                // `PlayerHandle::position_ns`). Realtime outputs follow
                // the wall clock directly, so they keep it cleared.
                *lock_mutex(&position_anchor_clone) = if is_prerender {
                    Some(PositionAnchor {
                        pos_ns: ideal_position_ns,
                        at: Instant::now(),
                        speed: applied_speed as f64,
                    })
                } else {
                    None
                };
                let unlimited =
                    player.unlimited_output.load(Ordering::Relaxed) || is_prerender;
                let mut display_behind = false;
                // Unlimited has one unambiguous path: globally ordered,
                // synchronous direct dispatch. Keep the selected EV mode only
                // for when Unlimited is turned off again.
                let use_legacy_traversal = legacy_midi_output && !unlimited;
                // Legacy mode reproduces the pre-buffer loop's hard-coded
                // 8192-event pass and continuous cadence. The temporary queue
                // is drained in this same pass, so no EV backlog is retained.
                let capacity = if use_legacy_traversal {
                    8192
                } else {
                    midi_ev_buffer_size
                };
                let mut last_sent_tick = None;

                // Buffered mode normalizes every cursor to a pending output event
                // before the global merge. Legacy mode must not do this: its old
                // per-track loop counted every cursor advance itself.
                if !use_legacy_traversal {
                    for track in &mut player.tracks {
                        if !track.pending && !track.exhausted {
                            loop {
                                if !unsafe { cursor_next(&mut track.cursor) } {
                                    track.exhausted = true;
                                    break;
                                }
                                if !track.cursor.is_tempo {
                                    track.pending = true;
                                    track.pend_tick = track.cursor.abs_tick;
                                    track.pend_status = track.cursor.status;
                                    track.pend_p1 = track.cursor.param1;
                                    track.pend_p2 = track.cursor.param2;
                                    break;
                                }
                            }
                        }
                    }
                }

                // Unlimited output bypasses the EV FIFO completely. The same
                // traversal/order rules still apply, but due events go straight
                // to KDMAPI instead of accumulating in an unbounded VecDeque.
                macro_rules! submit_event {
                    ($event:expr) => {{
                        let event = $event;
                        if unlimited {
                            last_sent_tick = Some(event.tick);
                            dispatch_midi_event(
                                event,
                                player.stream.as_mut(),
                                &mut player.channel_rpn_msb,
                                &mut player.channel_rpn_lsb,
                                &mut player.channel_pitch_bend_range,
                                player.pitch_bend_override.load(Ordering::Relaxed),
                                player.pitch_bend_multiplier.load(Ordering::Relaxed),
                                &mut player.midi_original_pb_range,
                                true,
                                &active_notes_clone,
                                &mut active_count,
                                &mut notes_played_count,
                                &pitch_bend_clone,
                            );
                        } else {
                            midi_ev_fifo.push_back(event);
                        }
                    }};
                }

                if use_legacy_traversal {
                    // Exact pre-buffer traversal: tracks are visited in file
                    // order, the 8192 ceiling counts cursor advances (including
                    // tempo/future events), and a future event is kept pending
                    // for the next tight-loop pass.
                    let mut events_processed = 0usize;
                    'display_walk: for track in &mut player.tracks {
                        if track.pending && track.pend_tick <= cur_tick {
                            submit_event!(BufferedMidiEvent {
                                tick: track.pend_tick,
                                track_index: track.cursor.track_index,
                                status: track.pend_status,
                                p1: track.pend_p1,
                                p2: track.pend_p2,
                            });
                            track.pending = false;
                            events_processed += 1;
                        }

                        while !track.pending
                            && (unlimited || events_processed < 8192)
                            && !track.exhausted
                        {
                            if !unsafe { cursor_next(&mut track.cursor) } {
                                track.exhausted = true;
                                break;
                            }
                            events_processed += 1;
                            // Prerender display walk: the same budget as
                            // the buffered traversal. The cursors keep
                            // their place when it trips, so the next pass
                            // resumes the walk instead of starting over.
                            if is_prerender
                                && events_processed % (DISPLAY_BUDGET_CHECK as usize) == 0
                                && frame_start.elapsed() > DISPLAY_BUDGET
                            {
                                display_behind = true;
                                break 'display_walk;
                            }
                            if track.cursor.is_tempo {
                                continue;
                            }
                            if track.cursor.abs_tick <= cur_tick {
                                submit_event!(BufferedMidiEvent {
                                    tick: track.cursor.abs_tick,
                                    track_index: track.cursor.track_index,
                                    status: track.cursor.status,
                                    p1: track.cursor.param1,
                                    p2: track.cursor.param2,
                                });
                            } else {
                                track.pending = true;
                                track.pend_tick = track.cursor.abs_tick;
                                track.pend_status = track.cursor.status;
                                track.pend_p1 = track.cursor.param1;
                                track.pend_p2 = track.cursor.param2;
                            }
                        }
                    }
                } else {
                    // Fill the bounded FIFO from all tracks in timestamp order.
                    // Track index is the stable tie-breaker for simultaneous events.
                    // A min-heap of due tracks keeps each pick O(log tracks):
                    // scanning every track per event stalls this thread for
                    // seconds on files with tens of thousands of tracks.
                    let mut due: BinaryHeap<Reverse<(u32, usize)>> = player
                        .tracks
                        .iter()
                        .enumerate()
                        .filter(|(_, track)| track.pending && track.pend_tick <= cur_tick)
                        .map(|(index, track)| Reverse((track.pend_tick, index)))
                        .collect();
                    let mut walked = 0u32;
                    while midi_ev_fifo.len() < capacity {
                        // Prerender: these events only drive the display.
                        // On files denser than this thread can walk, stop
                        // for this frame and jump to the audio below.
                        walked = walked.wrapping_add(1);
                        if is_prerender
                            && walked % DISPLAY_BUDGET_CHECK == 0
                            && frame_start.elapsed() > DISPLAY_BUDGET
                        {
                            display_behind = true;
                            break;
                        }
                        let Some(Reverse((_, track_index))) = due.pop() else {
                            break;
                        };
                        let track = &mut player.tracks[track_index];
                        submit_event!(BufferedMidiEvent {
                            tick: track.pend_tick,
                            track_index: track.cursor.track_index,
                            status: track.pend_status,
                            p1: track.pend_p1,
                            p2: track.pend_p2,
                        });

                        track.pending = false;
                        loop {
                            if !unsafe { cursor_next(&mut track.cursor) } {
                                track.exhausted = true;
                                break;
                            }
                            if !track.cursor.is_tempo {
                                track.pending = true;
                                track.pend_tick = track.cursor.abs_tick;
                                track.pend_status = track.cursor.status;
                                track.pend_p1 = track.cursor.param1;
                                track.pend_p2 = track.cursor.param2;
                                break;
                            }
                        }
                        if track.pending && track.pend_tick <= cur_tick {
                            due.push(Reverse((track.pend_tick, track_index)));
                        }
                    }
                }

                if display_behind {
                    // The walk ran out of budget this pass. Reseek only
                    // once it has fallen far enough behind the audio that
                    // catching up is hopeless; below that it resumes where
                    // it stopped on the next pass, so a dense passage
                    // costs display lag, not a reseek and a stretched pass
                    // on every frame.
                    let oldest_due = player
                        .tracks
                        .iter()
                        .filter(|track| track.pending && track.pend_tick <= cur_tick)
                        .map(|track| track.pend_tick)
                        .min();
                    let lag_ns = oldest_due.map_or(0, |tick| {
                        ideal_position_ns
                            .saturating_sub(tick_to_ns(unsafe { &*data.as_ptr() }, tick))
                    });
                    if lag_ns > DISPLAY_RESEEK_LAG.as_nanos() as u64 {
                        display_reseek = Some(ideal_position_ns);
                    }
                }

                // A full FIFO only means backpressure when at least one more due
                // event remains. Slowdown re-anchors song time here; Catch Up
                // deliberately leaves the ideal wall-clock anchor untouched.
                let backlog_due = player
                    .tracks
                    .iter()
                    .any(|track| track.pending && track.pend_tick <= cur_tick);
                let catch_up_backlog =
                    !use_legacy_traversal && midi_ev_catch_up && backlog_due;
                if !use_legacy_traversal
                    && !unlimited
                    && midi_ev_fifo.len() == capacity
                    && backlog_due
                {
                    // Both modes expose the delayed position while the FIFO is
                    // saturated. Slowdown also moves the clock anchor back;
                    // Catch Up keeps the original anchor so the position jumps
                    // to the wall-clock target as soon as the backlog is gone.
                    player.seek_position_ns = position_before_fill;
                    if !midi_ev_catch_up {
                        playback_base_ns = position_before_fill;
                        playback_start = Instant::now();
                    }
                }

                // Bounded modes drain their FIFO here. Unlimited events were
                // already dispatched directly by `submit_event!` above.
                while let Some(event) = midi_ev_fifo.pop_front() {
                    last_sent_tick = Some(event.tick);
                    dispatch_midi_event(
                        event,
                        player.stream.as_mut(),
                        &mut player.channel_rpn_msb,
                        &mut player.channel_rpn_lsb,
                        &mut player.channel_pitch_bend_range,
                        player.pitch_bend_override.load(Ordering::Relaxed),
                        player.pitch_bend_multiplier.load(Ordering::Relaxed),
                        &mut player.midi_original_pb_range,
                        use_legacy_traversal,
                        &active_notes_clone,
                        &mut active_count,
                        &mut notes_played_count,
                        &pitch_bend_clone,
                    );
                }

                if catch_up_backlog {
                    // Publish each submitted chunk's frontier immediately.
                    // Catch-up can run through many chunks before the normal
                    // end-of-loop store, which otherwise makes the UI look
                    // frozen until the entire backlog disappears.
                    if let Some(tick) = last_sent_tick {
                        player.seek_position_ns =
                            tick_to_ns(unsafe { &*data.as_ptr() }, tick)
                                .min(ideal_position_ns);
                        position_clone.store(player.seek_position_ns, Ordering::SeqCst);
                    }
                }

                let max_tick = unsafe { (*data.as_ptr()).max_tick };
                let max_ns = tick_to_ns(unsafe { &*data.as_ptr() }, max_tick);
                let drained = player
                    .tracks
                    .iter()
                    .all(|track| track.exhausted && !track.pending)
                    && midi_ev_fifo.is_empty();
                if player.seek_position_ns >= max_ns && (use_legacy_traversal || drained) {
                    player.seek_position_ns = max_ns;
                    player.state = PlayerState::Stopped;
                    if let Some(p) = prerender!(player) {
                        p.pause();
                    }
                    player.reset_playback_state();
                    *lock_mutex(&active_notes_clone) = [None; 128];
                    active_count = [0u16; 128];
                    *lock_mutex(&pitch_bend_clone) = [8192; 16];
                }
            }
        }

        if let Some(position) = display_reseek {
            // Skip the rest of what is due: the audio does not depend
            // on it, and catching up never would.
            player.seek_tracks(position);
            *lock_mutex(&active_notes_clone) = [None; 128];
            active_count = [0u16; 128];
            // The skipped note-ons were still heard (the prerender
            // played them), so re-derive the counter from the new
            // position instead of leaving it behind the song.
            if let Some(data) = &player.data {
                let tick = ns_to_tick(unsafe { &*data.as_ptr() }, position);
                notes_played_count = recount_notes(&note_index, data, tick);
            }
        }

        state_clone.store(player.state as u32, Ordering::SeqCst);
        position_clone.store(player.seek_position_ns, Ordering::SeqCst);
        notes_played_clone.store(notes_played_count, Ordering::Relaxed);
        audio_buffer_ns_for_thread.store(
            prerender!(player).map_or(u64::MAX, |p| p.buffered_ns()),
            Ordering::Relaxed,
        );
        polyphony_for_thread.store(
            match prerender!(player) {
                Some(p) => p.polyphony(),
                None => player
                    .stream
                    .as_ref()
                    .and_then(|s| s.polyphony())
                    .unwrap_or(u32::MAX),
            },
            Ordering::Relaxed,
        );
        render_load_for_thread.store(
            match prerender!(player) {
                Some(p) => p.render_load().to_bits(),
                None => player
                    .stream
                    .as_ref()
                    .and_then(|s| s.render_load())
                    .map_or(u32::MAX, f32::to_bits),
            },
            Ordering::Relaxed,
        );
        drop_velocity_for_thread.store(
            prerender!(player).map_or(0, |p| p.drop_velocity()),
            Ordering::Relaxed,
        );
        buffer_memory_for_thread.store(
            prerender!(player).map_or(u64::MAX, |p| p.memory_bytes()),
            Ordering::Relaxed,
        );
        output_rate_for_thread.store(
            prerender!(player).map_or(0, |p| p.rate()),
            Ordering::Relaxed,
        );
        buffer_compressed_for_thread.store(
            prerender!(player).map_or(false, |p| p.compressed()),
            Ordering::Relaxed,
        );
        #[cfg(debug_assertions)]
        {
            let series = prerender!(player).map_or_else(Vec::new, |p| p.visqol_series());
            if series.len() < visqol_consumed {
                visqol_consumed = 0;
            }
            if series.len() > visqol_consumed {
                let mut slot = lock_mutex(&visqol_series_for_thread);
                slot.extend_from_slice(&series[visqol_consumed..]);
                visqol_consumed = series.len();
            }
        }
        *lock_mutex(&pitch_bend_range_clone) = player.channel_pitch_bend_range;

        if player.state != PlayerState::Playing {
            thread::sleep(Duration::from_millis(10));
        } else if prerender!(player).is_some() {
            // Only the display follows this loop; once per frame is
            // enough, and the audio clock keeps it in step.
            if let Some(remaining) =
                MIDI_EV_FRAME_DURATION.checked_sub(frame_start.elapsed())
            {
                thread::sleep(remaining);
            }
        } else if legacy_midi_output {
            // This was the pre-buffer cadence: run continuously and let the
            // hard-coded per-pass event ceiling provide the only throttling.
            thread::yield_now();
        } else if player.unlimited_output.load(Ordering::Relaxed) {
            // Unlimited mode must not inherit the normal 60 Hz submission
            // cadence. Run continuously so events become due and are sent at
            // their actual timestamps instead of in 16.7 ms batches.
            thread::yield_now();
        } else if let Some(remaining) =
            MIDI_EV_FRAME_DURATION.checked_sub(frame_start.elapsed())
        {
            // Bounded modes get one FIFO submission window per output frame.
            // Catch Up keeps the ideal wall clock while these paced chunks
            // drain the backlog, then jumps to it once caught up.
            thread::sleep(remaining);
        }
    }
}
