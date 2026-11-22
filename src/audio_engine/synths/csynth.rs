//! CSynth v2 output: MIDI short messages from the player are queued to the audio
//! callback, which schedules them on CSynth's stream clock and renders through
//! cpal. CSynth itself opens no device, so this module owns the cpal stream.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use csynth::limiter::Limiter;
use csynth::{PianoProcessor, QUANTUM};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

/// Where the MPP piano samples come from when there is no local or cached copy.
const SAMPLE_SERVER: &str = "wss://mpp.c30.life";
const CACHE_APP_NAME: &str = "kazumidiplayer";

/// A MIDI short message decoded into what CSynth needs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SynthEvent {
    Press {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    Release {
        channel: u8,
        note: u8,
    },
    Control {
        channel: u8,
        controller: u8,
        value: u8,
    },
    Bend {
        channel: u8,
        value: u16,
    },
    Program {
        channel: u8,
        program: u8,
    },
    /// 0xFF: the player's reset before loading another file.
    Reset,
}

/// Decode a packed short message (`status | p1 << 8 | p2 << 16`), as sent to KDMAPI.
fn decode(msg: u32) -> Option<SynthEvent> {
    let status = (msg & 0xFF) as u8;
    let p1 = ((msg >> 8) & 0x7F) as u8;
    let p2 = ((msg >> 16) & 0x7F) as u8;
    let channel = status & 0x0F;
    match status & 0xF0 {
        0x90 if p2 > 0 => Some(SynthEvent::Press {
            channel,
            note: p1,
            velocity: p2,
        }),
        0x80 | 0x90 => Some(SynthEvent::Release { channel, note: p1 }),
        0xB0 => Some(SynthEvent::Control {
            channel,
            controller: p1,
            value: p2,
        }),
        0xC0 => Some(SynthEvent::Program {
            channel,
            program: p1,
        }),
        0xE0 => Some(SynthEvent::Bend {
            channel,
            value: (p1 as u16) | ((p2 as u16) << 7),
        }),
        0xF0 if status == 0xFF => Some(SynthEvent::Reset),
        _ => None,
    }
}

/// CSynth's MPP piano shares a key across channels. Keep every outstanding
/// press until its matching note-off, including releases held by the pedal.
struct MidiState {
    pressed: [[u32; 128]; 16],
    sustained: [[u32; 128]; 16],
    key_down: [u32; 128],
    sustain: [bool; 16],
}

impl Default for MidiState {
    fn default() -> Self {
        Self {
            pressed: [[0; 128]; 16],
            sustained: [[0; 128]; 16],
            key_down: [0; 128],
            sustain: [false; 16],
        }
    }
}

impl MidiState {
    fn key_up(&mut self, piano: &mut PianoProcessor, time: f64, channel: u8, note: u8, count: u32) {
        if count == 0 {
            return;
        }
        let ch = channel as usize;
        let key = note as usize;
        self.key_down[key] -= count;
        let released = if piano.plays_fonts() {
            self.pressed[ch][key] == 0 && self.sustained[ch][key] == 0
        } else {
            self.key_down[key] == 0
        };
        if released {
            piano.release_on_channel(time, note, 0, channel);
        }
    }

    fn lift_pedal(&mut self, piano: &mut PianoProcessor, time: f64, channel: u8) {
        let ch = channel as usize;
        self.sustain[ch] = false;
        for note in 0..128u8 {
            let count = std::mem::take(&mut self.sustained[ch][note as usize]);
            self.key_up(piano, time, channel, note, count);
        }
    }

    fn apply(&mut self, piano: &mut PianoProcessor, time: f64, event: SynthEvent) {
        match event {
            SynthEvent::Press {
                channel,
                note,
                velocity,
            } => {
                self.pressed[channel as usize][note as usize] += 1;
                self.key_down[note as usize] += 1;
                let gain = csynth::velocity_gain(velocity as f32 / 127.0);
                piano.press_on_channel(time, note, gain, 0, channel);
            }
            SynthEvent::Release { channel, note } => {
                let ch = channel as usize;
                let key = note as usize;
                if self.pressed[ch][key] == 0 {
                    return;
                }
                self.pressed[ch][key] -= 1;
                if self.sustain[ch] {
                    self.sustained[ch][key] += 1;
                } else {
                    self.key_up(piano, time, channel, note, 1);
                }
            }
            SynthEvent::Control {
                channel,
                controller,
                value,
            } => {
                let ch = channel as usize;
                match controller {
                    64 => {
                        if value >= 64 {
                            self.sustain[ch] = true;
                        } else {
                            self.lift_pedal(piano, time, channel);
                        }
                    }
                    120 | 123 => {
                        for note in 0..128u8 {
                            let key = note as usize;
                            let mut count = std::mem::take(&mut self.pressed[ch][key]);
                            if controller == 123 && self.sustain[ch] {
                                self.sustained[ch][key] += count;
                            } else {
                                if controller == 120 {
                                    count += std::mem::take(&mut self.sustained[ch][key]);
                                }
                                self.key_up(piano, time, channel, note, count);
                            }
                        }
                        if controller == 120 {
                            piano.control(time, channel, controller, value);
                        }
                    }
                    121 => {
                        self.lift_pedal(piano, time, channel);
                        piano.control(time, channel, controller, value);
                    }
                    _ => piano.control(time, channel, controller, value),
                }
            }
            SynthEvent::Bend { channel, value } => piano.bend(time, channel, value),
            SynthEvent::Program { channel, program } => piano.program(time, channel, program),
            SynthEvent::Reset => {
                *self = Self::default();
                for channel in 0..16 {
                    piano.control(time, channel, 120, 0);
                }
                piano.reset_channels();
            }
        }
    }
}

/// What the audio callback publishes for the handle: voices sounding
/// after the last quantum, and the smoothed share of one quantum's
/// playback time its rendering took (1.0 = the whole quantum, i.e. 100%).
#[derive(Default)]
struct StreamStats {
    polyphony: AtomicU32,
    load: AtomicU32,
}

/// Lives in the audio callback: drains the queue at each quantum and renders.
struct Renderer {
    piano: PianoProcessor,
    midi: MidiState,
    limiter: Limiter,
    rx: Receiver<u32>,
    stats: Arc<StreamStats>,
    /// Smoothed render load, published into `stats` after every quantum.
    load: f32,
    /// Playback time of one quantum, in seconds (the load's denominator).
    block_secs: f32,
    left: [f32; QUANTUM],
    right: [f32; QUANTUM],
    interleaved: Vec<f32>,
    pos: usize,
}

impl Renderer {
    fn render_quantum(&mut self) {
        let now = self.piano.frame as f64;
        for msg in self.rx.try_iter() {
            if let Some(event) = decode(msg) {
                self.midi.apply(&mut self.piano, now, event);
            }
        }
        let started = std::time::Instant::now();
        self.piano.process(&mut self.left, &mut self.right);
        let took = started.elapsed().as_secs_f32();
        self.load += (took / self.block_secs - self.load) * 0.05;
        self.stats
            .polyphony
            .store(self.piano.stats.active, Ordering::Relaxed);
        self.stats.load.store(self.load.to_bits(), Ordering::Relaxed);
        self.interleaved.clear();
        for i in 0..QUANTUM {
            self.interleaved.push(self.left[i]);
            self.interleaved.push(self.right[i]);
        }
        self.limiter.process(&mut self.interleaved);
        self.pos = 0;
    }

    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        for frame in out.chunks_mut(channels) {
            if self.pos == QUANTUM {
                self.render_quantum();
            }
            let l = self.interleaved[self.pos * 2];
            let r = self.interleaved[self.pos * 2 + 1];
            self.pos += 1;
            if channels == 1 {
                frame[0] = T::from_sample((l + r) * 0.5);
                continue;
            }
            for (i, sample) in frame.iter_mut().enumerate() {
                let v = match i {
                    0 => l,
                    1 => r,
                    _ => 0.0,
                };
                *sample = T::from_sample(v);
            }
        }
    }
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut renderer: Renderer,
) -> Result<cpal::Stream, String> {
    let channels = config.channels as usize;
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| renderer.fill(out, channels),
            |err| eprintln!("[WARN] CSynth output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to open audio output: {e}"))
}

pub struct CSynthStream {
    /// Messages for the audio callback, applied at the next quantum.
    tx: Sender<u32>,
    /// Stats the audio callback publishes (see [`StreamStats`]).
    stats: Arc<StreamStats>,
    /// Dropping this ends the thread that owns the (non-Send) cpal stream.
    _shutdown: Sender<()>,
}

impl CSynthStream {
    /// Load the samples and open the default output on a background thread.
    /// The result arrives on the returned receiver.
    pub fn spawn() -> Receiver<Result<CSynthStream, String>> {
        let (tx_result, rx_result) = mpsc::channel();
        thread::spawn(move || {
            let (tx_shutdown, rx_shutdown) = mpsc::channel::<()>();
            let stream = match open(tx_shutdown) {
                Ok((stream, handle)) => {
                    if tx_result.send(Ok(handle)).is_err() {
                        return;
                    }
                    stream
                }
                Err(e) => {
                    let _ = tx_result.send(Err(e));
                    return;
                }
            };
            // Returns once the handle (and its shutdown sender) is dropped.
            let _ = rx_shutdown.recv();
            drop(stream);
        });
        rx_result
    }

    pub fn send(&self, data: u32) {
        let _ = self.tx.send(data);
    }

    /// Voices sounding after the last rendered quantum.
    pub fn polyphony(&self) -> u32 {
        self.stats.polyphony.load(Ordering::Relaxed)
    }

    /// Smoothed share of one quantum's playback time spent rendering it
    /// (1.0 = the whole quantum, i.e. 100%).
    pub fn render_load(&self) -> f32 {
        f32::from_bits(self.stats.load.load(Ordering::Relaxed))
    }
}

fn open(shutdown: Sender<()>) -> Result<(cpal::Stream, CSynthStream), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No audio output device found")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("No usable output format: {e}"))?;
    let rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();

    let bundle = csynth::find_bundle(
        SAMPLE_SERVER,
        csynth::cache_path(CACHE_APP_NAME).as_deref(),
        &|status| eprintln!("[CSynth] {status}"),
    )?;
    let samples = csynth::decode_bundle(&bundle, rate, &|_, _| {})?;
    drop(bundle);

    let (tx, rx) = mpsc::channel();
    let stats = Arc::new(StreamStats::default());
    let renderer = Renderer {
        piano: PianoProcessor::new(samples, rate),
        midi: MidiState::default(),
        limiter: csynth::output_limiter(rate),
        rx,
        stats: stats.clone(),
        load: 0.0,
        block_secs: QUANTUM as f32 / rate as f32,
        left: [0.0; QUANTUM],
        right: [0.0; QUANTUM],
        interleaved: Vec::with_capacity(QUANTUM * 2),
        pos: QUANTUM,
    };
    let stream = match format {
        SampleFormat::F32 => build_stream::<f32>(&device, config, renderer)?,
        SampleFormat::I16 => build_stream::<i16>(&device, config, renderer)?,
        SampleFormat::U16 => build_stream::<u16>(&device, config, renderer)?,
        SampleFormat::I32 => build_stream::<i32>(&device, config, renderer)?,
        other => return Err(format!("Unsupported output sample format: {other}")),
    };
    stream
        .play()
        .map_err(|e| format!("Failed to start audio output: {e}"))?;
    Ok((
        stream,
        CSynthStream {
            tx,
            stats,
            _shutdown: shutdown,
        },
    ))
}

/// CSynth without a device, for prerendered playback.
pub struct CSynthPrerender {
    piano: PianoProcessor,
    midi: MidiState,
    limiter: Limiter,
    limiter_bypassed: bool,
    rate: u32,
    interleaved: Vec<f32>,
}

impl CSynthPrerender {
    /// Load the samples at the default output's rate.
    pub fn load() -> Result<Self, String> {
        let rate = cpal::default_host()
            .default_output_device()
            .ok_or("No audio output device found")?
            .default_output_config()
            .map_err(|e| format!("No usable output format: {e}"))?
            .sample_rate();
        let bundle = csynth::find_bundle(
            SAMPLE_SERVER,
            csynth::cache_path(CACHE_APP_NAME).as_deref(),
            &|status| eprintln!("[CSynth] {status}"),
        )?;
        let samples = csynth::decode_bundle(&bundle, rate, &|_, _| {})?;
        Ok(Self {
            piano: PianoProcessor::new(samples, rate),
            midi: MidiState::default(),
            limiter: csynth::output_limiter(rate),
            limiter_bypassed: false,
            rate,
            interleaved: Vec::with_capacity(QUANTUM * 2),
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// S16 Clipping: bypass this engine's own output limiter.
    pub fn set_limiter_bypassed(&mut self, bypassed: bool) {
        self.limiter_bypassed = bypassed;
    }

    /// Apply a message at the current render position.
    pub fn send(&mut self, msg: u32) {
        if let Some(event) = decode(msg) {
            let now = self.piano.frame as f64;
            self.midi.apply(&mut self.piano, now, event);
        }
    }

    /// Apply a message `frame` frames after the current render position.
    pub fn send_at(&mut self, frame: u32, msg: u32) {
        if let Some(event) = decode(msg) {
            let time = (self.piano.frame + frame as u64) as f64;
            self.midi.apply(&mut self.piano, time, event);
        }
    }

    /// Voices sounding (releasing included) after the last render.
    pub fn polyphony(&self) -> u32 {
        self.piano.stats.active
    }

    /// `left.len()` must be a multiple of `QUANTUM`.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.chunks_mut(QUANTUM).zip(right.chunks_mut(QUANTUM)) {
            self.piano.process(l, r);
            if self.limiter_bypassed {
                // S16 Clipping: no limiter; samples past full scale wrap
                // around to the other end, like the old s16 output.
                for (l, r) in l.iter_mut().zip(r.iter_mut()) {
                    *l = (*l + 1.0).rem_euclid(2.0) - 1.0;
                    *r = (*r + 1.0).rem_euclid(2.0) - 1.0;
                }
            } else {
                self.interleaved.clear();
                for i in 0..QUANTUM {
                    self.interleaved.push(l[i]);
                    self.interleaved.push(r[i]);
                }
                self.limiter.process(&mut self.interleaved);
                for i in 0..QUANTUM {
                    l[i] = self.interleaved[i * 2];
                    r[i] = self.interleaved[i * 2 + 1];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> CSynthPrerender {
        let sample = std::sync::Arc::new(csynth::Sample::new(vec![vec![0.25; 96_000]; 2]));
        CSynthPrerender {
            piano: PianoProcessor::new(vec![Some(sample); 128], 48_000),
            midi: MidiState::default(),
            limiter: csynth::output_limiter(48_000),
            limiter_bypassed: false,
            rate: 48_000,
            interleaved: Vec::with_capacity(QUANTUM * 2),
        }
    }

    fn render_quanta(engine: &mut CSynthPrerender, count: usize) -> f32 {
        let mut left = [0.0; QUANTUM];
        let mut right = [0.0; QUANTUM];
        for _ in 0..count {
            engine.render(&mut left, &mut right);
        }
        left.iter()
            .chain(&right)
            .map(|v| v.abs())
            .fold(0.0, f32::max)
    }

    fn on(channel: u32) -> u32 {
        0x90 | channel | 60 << 8 | 100 << 16
    }

    fn off(channel: u32) -> u32 {
        0x80 | channel | 60 << 8
    }

    fn cc(channel: u32, controller: u32, value: u32) -> u32 {
        0xB0 | channel | controller << 8 | value << 16
    }

    #[test]
    fn overlapping_notes_end_only_after_the_last_note_off() {
        // The same-channel case is common in yurikago_hzchop.mid. MPP keys
        // also stay down across channels, as in midi-blocks-rs.
        for other_channel in [0, 1] {
            let mut engine = test_engine();
            engine.send(on(0));
            render_quanta(&mut engine, 4);
            engine.send(on(other_channel));
            render_quanta(&mut engine, 4);
            engine.send(off(0));
            assert!(render_quanta(&mut engine, 120) > 0.01);
            engine.send(off(other_channel));
            assert_eq!(render_quanta(&mut engine, 120), 0.0);
        }
    }

    #[test]
    fn unmatched_note_off_does_not_end_another_channels_note() {
        let mut engine = test_engine();
        engine.send(on(1));
        render_quanta(&mut engine, 4);
        engine.send(off(0));
        assert!(render_quanta(&mut engine, 120) > 0.01);
    }

    #[test]
    fn pedal_holds_releases_including_repeated_presses() {
        let mut engine = test_engine();
        engine.send(cc(0, 64, 127));
        engine.send(on(0));
        render_quanta(&mut engine, 4);
        engine.send(off(0));
        assert!(render_quanta(&mut engine, 120) > 0.01);
        engine.send(on(0));
        render_quanta(&mut engine, 4);
        engine.send(off(0));
        assert!(render_quanta(&mut engine, 120) > 0.01);
        engine.send(cc(0, 64, 0));
        assert_eq!(render_quanta(&mut engine, 120), 0.0);
    }

    #[test]
    fn lifting_one_channels_pedal_keeps_other_channels_key() {
        let mut engine = test_engine();
        engine.send(cc(0, 64, 127));
        engine.send(on(0));
        engine.send(on(1));
        render_quanta(&mut engine, 4);
        engine.send(off(0));
        engine.send(cc(0, 64, 0));
        assert!(render_quanta(&mut engine, 120) > 0.01);
        engine.send(off(1));
        assert_eq!(render_quanta(&mut engine, 120), 0.0);
    }

    #[test]
    fn all_notes_off_respects_the_pedal_and_controller_reset_lifts_it() {
        let mut engine = test_engine();
        engine.send(cc(0, 64, 127));
        engine.send(on(0));
        render_quanta(&mut engine, 4);
        engine.send(cc(0, 123, 0));
        assert!(render_quanta(&mut engine, 120) > 0.01);
        engine.send(cc(0, 121, 0));
        assert_eq!(render_quanta(&mut engine, 120), 0.0);
    }

    #[test]
    fn sound_off_and_reset_do_not_leave_stale_presses() {
        for reset in [cc(0, 120, 0), 0xFF] {
            let mut engine = test_engine();
            engine.send(cc(0, 64, 127));
            engine.send(on(0));
            engine.send(on(0));
            engine.send(off(0));
            render_quanta(&mut engine, 4);
            engine.send(reset);
            assert_eq!(render_quanta(&mut engine, 120), 0.0);
            engine.send(cc(0, 64, 0));
            engine.send(on(0));
            render_quanta(&mut engine, 4);
            engine.send(off(0));
            assert_eq!(render_quanta(&mut engine, 120), 0.0);
        }
    }

    #[test]
    fn scheduled_pedal_release_keeps_its_render_frame() {
        let mut engine = test_engine();
        engine.send(cc(0, 64, 127));
        engine.send(on(0));
        render_quanta(&mut engine, 4);
        engine.send(off(0));
        // Schedule into the second quantum of a prerender chunk.
        engine.send_at(QUANTUM as u32, cc(0, 64, 0));
        render_quanta(&mut engine, 1);
        assert_eq!(engine.piano.stats.releasing, 0);
        render_quanta(&mut engine, 1);
        assert_eq!(engine.piano.stats.releasing, 1);
    }

    #[test]
    fn realtime_callback_holds_notes_until_pedal_up() {
        let engine = test_engine();
        let (tx, rx) = mpsc::channel();
        let mut renderer = Renderer {
            piano: engine.piano,
            midi: MidiState::default(),
            limiter: engine.limiter,
            rx,
            stats: Arc::new(StreamStats::default()),
            load: 0.0,
            block_secs: QUANTUM as f32 / 48_000.0,
            left: [0.0; QUANTUM],
            right: [0.0; QUANTUM],
            interleaved: Vec::with_capacity(QUANTUM * 2),
            pos: QUANTUM,
        };
        let mut out = [0.0; QUANTUM * 2];
        for msg in [cc(0, 64, 127), on(0), on(0), off(0), off(0)] {
            tx.send(msg).unwrap();
        }
        for _ in 0..120 {
            renderer.fill(&mut out, 2);
        }
        assert!(out.iter().any(|v: &f32| v.abs() > 0.01));
        tx.send(cc(0, 64, 0)).unwrap();
        for _ in 0..120 {
            renderer.fill(&mut out, 2);
        }
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn decodes_note_on_and_zero_velocity_release() {
        assert_eq!(
            decode(0x90 | 3 | (60 << 8) | (100 << 16)),
            Some(SynthEvent::Press {
                channel: 3,
                note: 60,
                velocity: 100
            })
        );
        assert_eq!(
            decode(0x90 | (60 << 8)),
            Some(SynthEvent::Release {
                channel: 0,
                note: 60
            })
        );
        assert_eq!(
            decode(0x8F | (61 << 8) | (64 << 16)),
            Some(SynthEvent::Release {
                channel: 15,
                note: 61
            })
        );
    }

    #[test]
    fn decodes_controllers_bend_program_and_reset() {
        assert_eq!(
            decode(0xB1 | (7 << 8) | (90 << 16)),
            Some(SynthEvent::Control {
                channel: 1,
                controller: 7,
                value: 90
            })
        );
        assert_eq!(
            decode(0xE2 | (0 << 8) | (64 << 16)),
            Some(SynthEvent::Bend {
                channel: 2,
                value: 8192
            })
        );
        assert_eq!(
            decode(0xC4 | (5 << 8)),
            Some(SynthEvent::Program {
                channel: 4,
                program: 5
            })
        );
        assert_eq!(decode(0xFF), Some(SynthEvent::Reset));
        assert_eq!(decode(0xA0 | (60 << 8)), None);
    }
}
