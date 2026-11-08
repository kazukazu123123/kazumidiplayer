//! CSynth v2 output: MIDI short messages from the player are queued to the audio
//! callback, which schedules them on CSynth's stream clock and renders through
//! cpal. CSynth itself opens no device, so this module owns the cpal stream.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use csynth::limiter::Limiter;
use csynth::{PianoProcessor, QUANTUM};
use std::sync::mpsc::{self, Receiver, Sender};
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

fn apply(piano: &mut PianoProcessor, time: f64, event: SynthEvent) {
    match event {
        SynthEvent::Press {
            channel,
            note,
            velocity,
        } => {
            let gain = csynth::velocity_gain(velocity as f32 / 127.0);
            piano.press_on_channel(time, note, gain, 0, channel);
        }
        SynthEvent::Release { channel, note } => piano.release_on_channel(time, note, 0, channel),
        SynthEvent::Control {
            channel,
            controller,
            value,
        } => piano.control(time, channel, controller, value),
        SynthEvent::Bend { channel, value } => piano.bend(time, channel, value),
        SynthEvent::Program { channel, program } => piano.program(time, channel, program),
        SynthEvent::Reset => {
            for channel in 0..16 {
                piano.control(time, channel, 120, 0);
            }
            piano.reset_channels();
        }
    }
}

/// Lives in the audio callback: drains the queue at each quantum and renders.
struct Renderer {
    piano: PianoProcessor,
    limiter: Limiter,
    rx: Receiver<u32>,
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
                apply(&mut self.piano, now, event);
            }
        }
        self.piano.process(&mut self.left, &mut self.right);
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
    let renderer = Renderer {
        piano: PianoProcessor::new(samples, rate),
        limiter: csynth::output_limiter(rate),
        rx,
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
            _shutdown: shutdown,
        },
    ))
}

/// CSynth without a device, for prerendered playback.
pub struct CSynthPrerender {
    piano: PianoProcessor,
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
            apply(&mut self.piano, now, event);
        }
    }

    /// Apply a message `frame` frames after the current render position.
    pub fn send_at(&mut self, frame: u32, msg: u32) {
        if let Some(event) = decode(msg) {
            let time = (self.piano.frame + frame as u64) as f64;
            apply(&mut self.piano, time, event);
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
