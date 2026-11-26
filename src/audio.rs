//! Audio path: Opus packets -> PCM -> default output device, through a
//! buffer (see `pacing::AudioBuffer`) that holds no more than the network
//! has shown it needs.

use crate::pacing::AudioBuffer;
use crate::settings;
use crate::wasapi::{self, SAMPLE_RATE};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};

const CHANNELS: usize = 2;
/// The reserve kept on a calm network, and the most it may grow to on a rough one.
const MIN_RESERVE_MS: usize = 10;
const MAX_RESERVE_MS: usize = 80;
/// Largest Opus frame: 120 ms at 48 kHz.
const MAX_FRAME: usize = 5760;

#[derive(Default)]
pub struct AudioStats {
    pub decoded_frames: AtomicU64,
    pub played_frames: AtomicU64,
    pub skipped_frames: AtomicU64,
    pub underruns: AtomicU64,
    /// The reserve the buffer currently aims to keep, in milliseconds.
    pub reserve_ms: AtomicU32,
    /// How much audio is waiting to be played right now, in milliseconds.
    pub buffered_ms: AtomicU32,
    /// Loudest sample since the last read, in thousandths of full scale.
    pub peak_milli: AtomicU32,
}

/// Scales the samples for the volume setting. The slider is squared because
/// loudness is heard roughly logarithmically: half-way should sound half as loud.
fn apply_volume(samples: &mut [f32], volume: f32) {
    if volume >= 1.0 {
        return;
    }
    let gain = volume * volume;
    for sample in samples {
        *sample *= gain;
    }
}

/// What the sound card calls to get its next samples: takes them from the
/// buffer and keeps the statistics up to date.
fn player(buffer: Arc<Mutex<AudioBuffer>>, stats: Arc<AudioStats>) -> impl FnMut(&mut [f32]) + Send + 'static {
    move |out| {
        let mut buffer = buffer.lock().unwrap();
        buffer.pull(out);
        stats.played_frames.fetch_add((out.len() / CHANNELS) as u64, Ordering::Relaxed);
        stats.underruns.store(buffer.underruns, Ordering::Relaxed);
        stats.reserve_ms.store(buffer.reserve_ms() as u32, Ordering::Relaxed);
        stats.buffered_ms.store(buffer.buffered_ms() as u32, Ordering::Relaxed);
    }
}

/// The open sound output. Dropping it stops the sound.
enum Output {
    LowLatency(#[allow(dead_code)] wasapi::Output),
    Standard(#[allow(dead_code)] cpal::Stream),
}

impl Output {
    /// Opens the low-latency output where the device allows it, and the
    /// ordinary one (10 ms blocks, any device format) everywhere else.
    fn open(buffer: &Arc<Mutex<AudioBuffer>>, stats: &Arc<AudioStats>) -> Result<Self> {
        match wasapi::Output::start(player(buffer.clone(), stats.clone())) {
            Ok(output) => {
                crate::status!("[audio] low-latency output, {:.1} ms blocks", output.block_ms);
                return Ok(Self::LowLatency(output));
            }
            Err(e) => crate::status!("[audio] standard output ({e:#})"),
        }
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let build = |buffer_size: cpal::BufferSize| {
            let mut play = player(buffer.clone(), stats.clone());
            device.build_output_stream(
                cpal::StreamConfig { channels: CHANNELS as u16, sample_rate: SAMPLE_RATE, buffer_size },
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| play(out),
                |e| crate::status!("[audio] output error: {e}"),
                None,
            )
        };
        // 10 ms device buffer if the driver allows it, otherwise whatever it prefers.
        let stream = build(cpal::BufferSize::Fixed(480))
            .or_else(|_| build(cpal::BufferSize::Default))
            .context("could not open the audio output at 48 kHz stereo")?;
        stream.play().context("could not start audio playback")?;
        Ok(Self::Standard(stream))
    }
}

/// Decodes and plays packets until `quit` is set or the sender is dropped. Call from a dedicated thread.
pub fn run(packets: Receiver<Bytes>, stats: Arc<AudioStats>, quit: Arc<AtomicBool>) -> Result<()> {
    let buffer = Arc::new(Mutex::new(AudioBuffer::new(
        SAMPLE_RATE as usize,
        CHANNELS,
        MIN_RESERVE_MS,
        MAX_RESERVE_MS,
    )));
    let _output = Output::open(&buffer, &stats)?;

    let mut decoder = opusic_c::Decoder::new(opusic_c::Channels::Stereo, opusic_c::SampleRate::Hz48000)
        .map_err(|e| anyhow!("could not create the Opus decoder: {e:?}"))?;
    let mut pcm = vec![0f32; MAX_FRAME * CHANNELS];

    while !quit.load(Ordering::Relaxed) {
        let packet = match packets.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A packet that does not decode is skipped; the next one stands on its own.
        let frames = match decoder.decode_float_to_slice(&packet, &mut pcm, false) {
            Ok(frames) if frames > 0 => frames,
            _ => continue,
        };
        let samples = &mut pcm[..frames * CHANNELS];
        apply_volume(samples, settings::get().volume);
        stats.decoded_frames.fetch_add(frames as u64, Ordering::Relaxed);
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        stats.peak_milli.fetch_max((peak * 1000.0) as u32, Ordering::Relaxed);

        let mut buffer = buffer.lock().unwrap();
        buffer.push(samples);
        stats
            .skipped_frames
            .store(buffer.skipped_samples / CHANNELS as u64, Ordering::Relaxed);
    }

    Ok(())
}
