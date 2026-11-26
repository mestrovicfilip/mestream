//! Sound output through WASAPI's low-latency shared mode (`IAudioClient3`).
//!
//! The ordinary shared mode hands audio to Windows in 10 ms blocks. This mode
//! uses the shortest block the sound driver supports (which on some drivers
//! is still 10 ms) without taking the device away from other programs, and
//! it carries on by itself when the output device changes. It only works in
//! the format Windows itself mixes in, so the caller falls back to the
//! ordinary output when that is not 48 kHz float.
//!
//! Exclusive mode would be faster still, but it bypasses the Windows mixer
//! and with it the speaker processing (Dolby, equalisers) that handhelds
//! rely on, so the sound loses more than the few milliseconds are worth.

use anyhow::{Context, Result, anyhow, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient3, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::w;

pub const SAMPLE_RATE: u32 = 48000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// How many device periods of sound to keep queued. One is being played;
/// the second covers this thread being scheduled a little late.
const QUEUED_PERIODS: u32 = 2;

/// Fills a buffer of interleaved stereo samples at 48 kHz.
pub trait Fill: FnMut(&mut [f32]) + Send + 'static {}
impl<F: FnMut(&mut [f32]) + Send + 'static> Fill for F {}

/// A running output. Sound stops when it is dropped.
pub struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// How much sound is handed to the device at a time, in milliseconds.
    pub block_ms: f32,
}

impl Output {
    /// Starts playing on the default output device, or says why this mode
    /// cannot be used there.
    pub fn start(fill: impl Fill) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (opened_tx, opened_rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || render_thread(fill, stop, opened_tx))
        };
        // The device is opened on the thread that will use it; wait to hear how it went.
        let block_ms = opened_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("the sound device did not answer"))
            .and_then(|opened| opened);
        match block_ms {
            Ok(block_ms) => Ok(Self { stop, thread: Some(thread), block_ms }),
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An open, started stream on one device.
struct Stream {
    client: IAudioClient3,
    render: IAudioRenderClient,
    /// Signalled by Windows each time it wants more sound.
    wake: HANDLE,
    channels: usize,
    period_frames: u32,
    buffer_frames: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.wake);
        }
    }
}

/// True if the format is 32-bit float at our sample rate with at least two channels.
unsafe fn is_mixable(format: *const WAVEFORMATEX) -> bool {
    unsafe {
        let basic = &*format;
        let is_float = match basic.wFormatTag as u32 {
            tag if tag == WAVE_FORMAT_EXTENSIBLE => {
                let extended = &*(format as *const WAVEFORMATEXTENSIBLE);
                let sub_format = std::ptr::addr_of!(extended.SubFormat).read_unaligned();
                sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            }
            tag => tag == u32::from(WAVE_FORMAT_IEEE_FLOAT),
        };
        let (rate, bits, channels) = (basic.nSamplesPerSec, basic.wBitsPerSample, basic.nChannels);
        is_float && rate == SAMPLE_RATE && bits == 32 && channels >= 2
    }
}

impl Stream {
    unsafe fn open() -> Result<Self> {
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no sound output device")?;
            let client: IAudioClient3 = device
                .Activate(CLSCTX_ALL, None)
                .context("the sound device does not offer low-latency output")?;

            let format = client.GetMixFormat()?;
            let initialised = Self::initialise(&client, format);
            CoTaskMemFree(Some(format as *const _));
            let (channels, period_frames) = initialised?;

            let stream = Self {
                render: client.GetService()?,
                buffer_frames: client.GetBufferSize()?,
                wake: CreateEventW(None, false, false, None)?,
                client,
                channels,
                period_frames,
            };
            stream.client.SetEventHandle(stream.wake)?;
            stream.client.Start()?;
            Ok(stream)
        }
    }

    /// Sets the stream up with the device's shortest block.
    /// Returns (channels, frames per block).
    unsafe fn initialise(client: &IAudioClient3, format: *const WAVEFORMATEX) -> Result<(usize, u32)> {
        unsafe {
            if !is_mixable(format) {
                bail!("Windows mixes this device in a format other than 48 kHz float");
            }
            let (mut usual, mut step, mut shortest, mut longest) = (0u32, 0u32, 0u32, 0u32);
            client.GetSharedModeEnginePeriod(format, &mut usual, &mut step, &mut shortest, &mut longest)?;
            eprintln!("[audio] device block sizes: shortest {shortest}, usual {usual}, longest {longest} samples");
            client
                .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, shortest, format, None)
                .context("the sound device refused a low-latency stream")?;
            Ok((usize::from((*format).nChannels), shortest))
        }
    }

    fn block_ms(&self) -> f32 {
        self.period_frames as f32 * 1000.0 / SAMPLE_RATE as f32
    }

    /// Waits until Windows wants more sound, then tops the queue up.
    unsafe fn serve(&self, fill: &mut impl Fill, stereo: &mut Vec<f32>) -> Result<()> {
        unsafe {
            if WaitForSingleObject(self.wake, 200) != WAIT_OBJECT_0 {
                return Ok(());
            }
            let queued = self.client.GetCurrentPadding()?;
            let wanted = (self.period_frames * QUEUED_PERIODS).min(self.buffer_frames);
            let frames = wanted.saturating_sub(queued);
            if frames == 0 {
                return Ok(());
            }
            let target = self.render.GetBuffer(frames)? as *mut f32;
            let device = std::slice::from_raw_parts_mut(target, frames as usize * self.channels);
            if self.channels == 2 {
                fill(device);
            } else {
                // Front left and right carry the sound; any further speakers stay silent.
                stereo.resize(frames as usize * 2, 0.0);
                fill(stereo);
                device.fill(0.0);
                for (frame, pair) in device.chunks_exact_mut(self.channels).zip(stereo.as_chunks::<2>().0) {
                    frame[..2].copy_from_slice(pair);
                }
            }
            self.render.ReleaseBuffer(frames, 0)?;
            Ok(())
        }
    }
}

fn render_thread(mut fill: impl Fill, stop: Arc<AtomicBool>, opened: mpsc::Sender<Result<f32>>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Ask Windows to schedule this thread like other audio threads.
        let mut task_index = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index);

        let mut stream = match Stream::open() {
            Ok(stream) => {
                let _ = opened.send(Ok(stream.block_ms()));
                Some(stream)
            }
            Err(error) => {
                let _ = opened.send(Err(error));
                return;
            }
        };
        let mut stereo = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let Some(current) = stream.as_ref() else {
                // The device went away (headphones unplugged, output switched):
                // carry on with whatever is the default now.
                std::thread::sleep(Duration::from_millis(500));
                stream = Stream::open().ok();
                if stream.is_some() {
                    crate::status!("[audio] continuing on the new output device");
                }
                continue;
            };
            if let Err(error) = current.serve(&mut fill, &mut stereo) {
                crate::status!("[audio] the output device stopped: {error}");
                stream = None;
            }
        }
    }
}
