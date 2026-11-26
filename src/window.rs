//! The stream window together with the threads that serve it. It is opened
//! before anything else so the start-up screen is visible while connecting,
//! and it outlives individual streams: a reconnect draws into the same window.

use crate::options::PlayOptions;
use crate::{gamepad, input, lifecycle, stream, video};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub struct Window {
    /// Encoded pictures for the window to decode and show.
    pub frames: std::sync::mpsc::Sender<video::AccessUnit>,
    pub stats: Arc<video::VideoStats>,
    /// Controller states and frame timings on their way to the console.
    pub input_tx: tokio::sync::mpsc::UnboundedSender<input::InputEvent>,
    pub input_rx: stream::InputQueue,
    quit: Arc<AtomicBool>,
    quit_gamepad: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// How the window thread ended, once it has.
    result: Arc<Mutex<Option<Result<()>>>>,
}

impl Window {
    /// Opens the window. `closed` is told when it goes away for any reason.
    pub fn open(options: &PlayOptions, closed: Arc<tokio::sync::watch::Sender<bool>>) -> Self {
        let (frames, frames_rx) = std::sync::mpsc::channel();
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let stats = Arc::new(video::VideoStats::default());
        let quit = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let video_options = video::VideoOptions {
            fullscreen: options.fullscreen,
            screenshot: options.screenshot.clone().map(|path| (path, 300)),
            timings: Some(input_tx.clone()),
            enhance: options.enhance,
        };
        let thread = {
            let (stats, quit, result) = (stats.clone(), quit.clone(), result.clone());
            std::thread::spawn(move || {
                *result.lock().unwrap() = Some(video::run(frames_rx, video_options, stats, quit));
                let _ = closed.send(true);
            })
        };
        let quit_gamepad = Arc::new(AtomicBool::new(false));
        {
            let (events, quit) = (input_tx.clone(), quit_gamepad.clone());
            std::thread::spawn(move || gamepad::run(events, quit));
        }
        Self {
            frames,
            stats,
            input_tx,
            input_rx: Arc::new(tokio::sync::Mutex::new(input_rx)),
            quit,
            quit_gamepad,
            thread: Some(thread),
            result,
        }
    }

    /// Closes the window (if it is still open) and reports how its thread ended.
    pub fn close(&mut self) -> Result<()> {
        self.quit.store(true, Ordering::Relaxed);
        self.quit_gamepad.store(true, Ordering::Relaxed);
        // When Windows is ending the process, the window thread is itself
        // waiting for this clean-up to finish, so it cannot be waited for.
        let ending = lifecycle::SHUTDOWN_REQUESTED.load(Ordering::Relaxed);
        if let Some(thread) = self.thread.take().filter(|_| !ending) {
            let _ = thread.join();
        }
        self.result.lock().unwrap().take().unwrap_or(Ok(()))
    }
}
