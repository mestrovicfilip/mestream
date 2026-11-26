//! One streaming session from start to finish: sign in, find the console,
//! have it provisioned, connect, and keep the stream running until it ends.

use crate::options::PlayOptions;
use crate::text::text;
use crate::window::Window;
use crate::xhome::{Console, Resolution, StreamSession, XHome};
use crate::{audio, auth, input, lifecycle, overlay, settings, store, stream, video};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

/// Becomes true when the window is closed or the process is being ended.
pub type Closed = tokio::sync::watch::Receiver<bool>;

/// How a stream ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The user left, or the requested time was up.
    Finished,
    /// It stopped by itself (or the machine went to sleep): reconnect.
    Lost,
    /// The user asked for something that needs the stream closed first.
    Requested(overlay::Action),
}

/// Something the user has to settle before a stream can start.
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// Several consoles and none chosen yet.
    ConsoleChoice,
    /// The account has no console set up for remote play.
    Console,
}

impl std::fmt::Display for Need {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Need::ConsoleChoice => "there are several consoles on this account and none has been chosen",
            Need::Console => "no console is registered for remote play",
        })
    }
}

impl std::error::Error for Need {}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?)
}

/// Runs a step of connecting, giving up at once if the window was closed.
pub async fn unless_closed<T>(closed: &Closed, step: impl Future<Output = Result<T>>) -> Result<T> {
    let mut closed = closed.clone();
    tokio::select! {
        result = step => result,
        _ = closed.wait_for(|c| *c) => Err(anyhow!("the window was closed")),
    }
}

fn resolution() -> Resolution {
    if settings::get().resolution == 1080 { Resolution::P1080 } else { Resolution::P720 }
}

/// The streaming service for a signed-in account.
pub fn service(http: &reqwest::Client, account: &auth::Session) -> XHome {
    XHome::new(http.clone(), account.base_uri.clone(), account.gs_token.clone(), resolution())
}

/// Signs in unless `account` already holds a sign-in from earlier.
pub async fn ensure_signed_in<'a>(
    http: &reqwest::Client,
    closed: &Closed,
    account: &'a mut Option<auth::Session>,
) -> Result<&'a auth::Session> {
    if account.is_none() {
        let started = Instant::now();
        crate::status!("[auth] signing in");
        *account = Some(unless_closed(closed, auth::sign_in(http)).await?);
        crate::status!("[auth] signed in ({} ms)", started.elapsed().as_millis());
    }
    Ok(account.as_ref().expect("signed in above"))
}

/// Connects and streams until the stream ends. `account` keeps the sign-in
/// between calls so a reconnect does not have to repeat it.
pub async fn connect_and_stream(
    options: &PlayOptions,
    window: Option<&mut Window>,
    closed: &Closed,
    account: &mut Option<auth::Session>,
) -> Result<Ended> {
    let http = http_client()?;
    let started = Instant::now();
    let signed_in = ensure_signed_in(&http, closed, account).await?;
    stream_on_console(options, window, closed, signed_in, &http, started).await
}

/// Closes a session that an earlier launch left open because it died without
/// the chance to. The console would otherwise stay "in use" until it timed out.
async fn close_leftover_session(http: &reqwest::Client, account: &auth::Session) {
    let Some((base_uri, path)) = store::leftover_session() else { return };
    // The record is only ever replayed against the service we signed in to.
    if base_uri == account.base_uri {
        match service(http, account).delete_session(&path).await {
            Ok(()) => crate::status!("[session] closed a session left over from last time"),
            Err(_) => crate::status!("[session] last time's session had already ended"),
        }
    }
    store::clear_session();
}

/// The console to play on: the remembered one, or the only one there is.
fn choose_console(consoles: &[Console], may_ask: bool) -> Result<&Console> {
    let remembered = settings::get().console_id;
    let found = remembered.and_then(|id| consoles.iter().find(|c| c.server_id == id));
    match (found, consoles) {
        (Some(console), _) => Ok(console),
        (None, []) => Err(Need::Console.into()),
        (None, [only]) => Ok(only),
        // Without a window there is nobody to ask.
        (None, [first, ..]) if !may_ask => Ok(first),
        (None, _) => Err(Need::ConsoleChoice.into()),
    }
}

async fn stream_on_console(
    options: &PlayOptions,
    window: Option<&mut Window>,
    closed: &Closed,
    account: &auth::Session,
    http: &reqwest::Client,
    started: Instant,
) -> Result<Ended> {
    let api = service(http, account);
    close_leftover_session(http, account).await;

    let consoles = unless_closed(closed, api.consoles()).await?;
    let console = choose_console(&consoles, window.is_some())?;
    crate::status!("[console] {} ({}), power: {}", console.device_name, console.console_type, console.power_state);
    overlay::set_console_name(&console.device_name);
    if window.is_some() && settings::get().console_id.as_deref() != Some(&console.server_id) {
        let id = console.server_id.clone();
        settings::update(|s| s.console_id = Some(id));
    }

    let session = provision(&api, console, account, http, closed).await?;
    crate::status!("[session] console ready ({} ms since start)", started.elapsed().as_millis());
    let result = run_stream(options, &api, &session, started, window, closed).await;
    match session.stop().await {
        Ok(()) => store::clear_session(),
        // Leave the record: the next launch will try again.
        Err(e) => crate::status!("[session] could not stop the session cleanly: {e}"),
    }
    lifecycle::SESSION_CLOSED.store(true, Ordering::Relaxed);
    result
}

/// Asks the console for a session and waits until it is ready to connect.
/// The Xbox service sometimes cannot reach the console on the first try (it
/// is still waking up or re-registering); that clears up within seconds.
async fn provision<'a>(
    api: &'a XHome,
    console: &Console,
    account: &auth::Session,
    http: &reqwest::Client,
    closed: &Closed,
) -> Result<StreamSession<'a>> {
    const ATTEMPTS: u32 = 4;
    for attempt in 1..=ATTEMPTS {
        let session = unless_closed(closed, api.start(&console.server_id)).await?;
        store::save_session(&account.base_uri, session.path());
        let ready = unless_closed(closed, session.wait_provisioned(|| account.transfer_token(http))).await;
        let Err(error) = ready else { return Ok(session) };
        if session.stop().await.is_ok() {
            store::clear_session();
        }
        let unreachable = format!("{error:#}").contains("WNSError");
        if !unreachable || *closed.borrow() {
            return Err(error);
        }
        if attempt == ATTEMPTS {
            return Err(error.context(text().console_unreachable));
        }
        crate::status!("[session] console not reachable yet, retrying ({}/{ATTEMPTS})", attempt + 1);
        unless_closed(closed, async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            Ok(())
        })
        .await?;
    }
    unreachable!("the last attempt returns")
}

/// Raises the given flags when dropped, so helper threads stop however the
/// stream function is left.
struct StopOnDrop(Vec<Arc<AtomicBool>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        for flag in &self.0 {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Tells the overlay a stream is running for as long as this lives.
struct Streaming;

impl Streaming {
    fn begin() -> Self {
        overlay::set_streaming(true);
        Self
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        overlay::set_streaming(false);
    }
}

async fn wait_until_connected(stream: &stream::Stream) -> Result<()> {
    let mut state = stream.state();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match *state.borrow_and_update() {
                RTCPeerConnectionState::Connected => return Ok(()),
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    return Err(anyhow!("WebRTC connection to the console failed"));
                }
                _ => {}
            }
            if state.changed().await.is_err() {
                return Err(anyhow!("WebRTC connection closed unexpectedly"));
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out connecting to the console over the network"))?
}

/// Watchdog: how late does a 5 ms timer fire? Large values mean the async
/// runtime itself stalled, which would explain a stutter the network cannot.
fn watch_runtime_lag() -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let lag_us = Arc::new(AtomicU64::new(0));
    let lag = lag_us.clone();
    let task = tokio::spawn(async move {
        loop {
            let before = Instant::now();
            tokio::time::sleep(Duration::from_millis(5)).await;
            let late = before.elapsed().saturating_sub(Duration::from_millis(5));
            lag.fetch_max(late.as_micros() as u64, Ordering::Relaxed);
        }
    });
    (lag_us, task)
}

/// Presses d-pad right once, three seconds in (`--test-input`).
fn send_test_input(pad_events: tokio::sync::mpsc::UnboundedSender<input::InputEvent>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut pad = input::GamepadState { buttons: input::button::DPAD_RIGHT, ..Default::default() };
        let _ = pad_events.send(input::InputEvent::Pad(pad));
        tokio::time::sleep(Duration::from_millis(100)).await;
        pad.buttons = 0;
        let _ = pad_events.send(input::InputEvent::Pad(pad));
        crate::status!("[test] sent d-pad right");
    });
}

/// Running totals from the last report, to turn counters into per-second rates.
#[derive(Default)]
struct Totals {
    bytes: u64,
    frames: u64,
    packets: u64,
    presented: u64,
    latency_us: u64,
    cadence_us: u64,
}

/// Prints the once-a-second measurement line and feeds the on-screen statistics.
struct Reporter {
    last: Totals,
    video: Arc<video::VideoStats>,
    audio: Arc<audio::AudioStats>,
    runtime_lag_us: Arc<AtomicU64>,
    with_window: bool,
}

impl Reporter {
    fn report(&mut self, second: u64, s: &stream::Stats) {
        let now = Totals {
            bytes: s.video_bytes.load(Ordering::Relaxed),
            frames: s.video_frames.load(Ordering::Relaxed),
            packets: s.video_packets.load(Ordering::Relaxed),
            presented: self.video.presented.load(Ordering::Relaxed),
            latency_us: self.video.latency_us.load(Ordering::Relaxed),
            cadence_us: self.video.cadence_error_us.load(Ordering::Relaxed),
        };
        let megabits = (now.bytes - self.last.bytes) as f64 * 8.0 / 1e6;
        let mut line = format!(
            "{second:>3}s  video {megabits:>6.2} Mbit/s  {:>3} fps  max gap {:>5.1} ms  {:>4} pkt/s  keyframes {}  lost {}  audio pkts {}",
            now.frames - self.last.frames,
            s.video_max_gap_us.swap(0, Ordering::Relaxed) as f64 / 1000.0,
            now.packets - self.last.packets,
            s.video_keyframes.load(Ordering::Relaxed),
            s.video_lost.load(Ordering::Relaxed),
            s.audio_packets.load(Ordering::Relaxed),
        );
        line += &format!("  runtime lag {:.1} ms", self.runtime_lag_us.swap(0, Ordering::Relaxed) as f64 / 1000.0);
        if self.with_window {
            let shown = now.presented - self.last.presented;
            let per_frame_ms = |total_us: u64| total_us as f64 / shown.max(1) as f64 / 1000.0;
            let delay_ms = per_frame_ms(now.latency_us - self.last.latency_us);
            line += &format!(
                "  | shown {shown:>3} fps  skipped {}  receive-to-screen {delay_ms:.2} ms (smoothing {:.1})  cadence error {:.2} ms",
                self.video.skipped.load(Ordering::Relaxed),
                self.video.smoothing_us.load(Ordering::Relaxed) as f64 / 1000.0,
                per_frame_ms(now.cadence_us - self.last.cadence_us),
            );
            line += &format!(
                "  | audio played {:.1} s  peak {:.3}  waiting {} ms (reserve {})  underruns {}  caught up {} ms",
                self.audio.played_frames.load(Ordering::Relaxed) as f64 / 48000.0,
                f64::from(self.audio.peak_milli.swap(0, Ordering::Relaxed)) / 1000.0,
                self.audio.buffered_ms.load(Ordering::Relaxed),
                self.audio.reserve_ms.load(Ordering::Relaxed),
                self.audio.underruns.load(Ordering::Relaxed),
                self.audio.skipped_frames.load(Ordering::Relaxed) / 48,
            );
            overlay::set_stats(format!("{shown} fps   {megabits:.1} Mbit/s   {delay_ms:.1} ms"));
        }
        println!("{line}");
        self.last = now;
    }

    /// Packets counted since the previous report.
    fn packets_since_last(&self, s: &stream::Stats) -> u64 {
        s.video_packets.load(Ordering::Relaxed) - self.last.packets
    }
}

async fn run_stream(
    options: &PlayOptions,
    api: &XHome,
    session: &StreamSession<'_>,
    started: Instant,
    window: Option<&mut Window>,
    closed: &Closed,
) -> Result<Ended> {
    let mut sink = stream::VideoSink { dump: options.dump.clone(), frames: None, audio: None };
    let (video_stats, pad_events, input_queue) = match window.as_deref() {
        Some(window) => {
            sink.frames = Some(window.frames.clone());
            (window.stats.clone(), window.input_tx.clone(), window.input_rx.clone())
        }
        None => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (Arc::new(video::VideoStats::default()), tx, Arc::new(tokio::sync::Mutex::new(rx)))
        }
    };
    // Input meant for an earlier stream must not be replayed into this one.
    if let Ok(mut queue) = input_queue.try_lock() {
        while queue.try_recv().is_ok() {}
    }

    let audio_stats = Arc::new(audio::AudioStats::default());
    let quit_audio = Arc::new(AtomicBool::new(false));
    let _stop_audio = StopOnDrop(vec![quit_audio.clone()]);
    let mut audio_thread = None;
    if window.is_some() && options.audio {
        let (tx, rx) = std::sync::mpsc::channel();
        sink.audio = Some(tx);
        let (stats, quit) = (audio_stats.clone(), quit_audio.clone());
        audio_thread = Some(std::thread::spawn(move || {
            if let Err(e) = audio::run(rx, stats, quit) {
                crate::status!("[audio] disabled: {e:#}");
            }
        }));
    }
    if options.test_input {
        send_test_input(pad_events);
    }

    crate::status!("[webrtc] negotiating");
    let stream = stream::Stream::new(api.resolution(), sink, input_queue).await?;
    let offer = stream.create_offer(settings::get().bitrate_kbps).await?;
    let answer = unless_closed(closed, session.exchange_sdp(&offer)).await?;
    stream.set_answer(answer).await?;
    let local = unless_closed(closed, stream.local_candidates()).await?;
    let remote = unless_closed(closed, session.exchange_ice(local)).await?;
    stream.add_remote_candidates(remote).await?;
    wait_until_connected(&stream).await?;
    crate::status!("[webrtc] connected ({} ms since start)", started.elapsed().as_millis());

    let _streaming = Streaming::begin();
    let (runtime_lag_us, lag_watchdog) = watch_runtime_lag();
    let mut reporter = Reporter {
        // The window's counters keep running across reconnects: start from where they are.
        last: Totals {
            presented: video_stats.presented.load(Ordering::Relaxed),
            latency_us: video_stats.latency_us.load(Ordering::Relaxed),
            cadence_us: video_stats.cadence_error_us.load(Ordering::Relaxed),
            ..Totals::default()
        },
        video: video_stats,
        audio: audio_stats,
        runtime_lag_us,
        with_window: window.is_some(),
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.tick().await;
    let mut last_keepalive = Instant::now();
    let mut window_closed = closed.clone();
    let mut ended = Ended::Finished;
    let mut silent_seconds = 0;
    lifecycle::SUSPEND_REQUESTED.store(false, Ordering::Relaxed);
    'stream: for second in 1..=options.seconds {
        loop {
            tokio::select! {
                _ = ticker.tick() => break,
                _ = tokio::signal::ctrl_c() => break 'stream,
                _ = window_closed.wait_for(|c| *c) => break 'stream,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if lifecycle::SUSPEND_REQUESTED.swap(false, Ordering::Relaxed) {
                        crate::status!("[power] going to sleep, closing the session");
                        ended = Ended::Lost;
                        break 'stream;
                    }
                    if let Some(action) = overlay::take_action() {
                        ended = Ended::Requested(action);
                        break 'stream;
                    }
                }
            }
        }
        static DROPPED_ONCE: AtomicBool = AtomicBool::new(false);
        if options.test_drop == Some(second) && !DROPPED_ONCE.swap(true, Ordering::Relaxed) {
            crate::status!("[test] simulating a lost stream");
            ended = Ended::Lost;
            break;
        }
        silent_seconds = if reporter.packets_since_last(&stream.stats) == 0 { silent_seconds + 1 } else { 0 };
        if silent_seconds >= 4 {
            crate::status!("[webrtc] no video for 4 seconds");
            ended = Ended::Lost;
            break;
        }
        reporter.report(second, &stream.stats);
        if last_keepalive.elapsed() > Duration::from_secs(30) {
            last_keepalive = Instant::now();
            if let Err(e) = session.keepalive().await {
                crate::status!("[session] keepalive failed: {e}");
            }
        }
    }

    lag_watchdog.abort();
    stream.close().await;
    quit_audio.store(true, Ordering::Relaxed);
    if let Some(thread) = audio_thread {
        let _ = thread.join();
    }
    // The window stays open: the caller either closes it or connects again into it.
    Ok(ended)
}
