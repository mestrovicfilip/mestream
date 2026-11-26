/// Prints a status line to the terminal and adds it to the start-up screen's log.
#[macro_export]
macro_rules! status {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        eprintln!("{line}");
        $crate::overlay::log(&line);
    }};
}

mod app;
mod audio;
mod auth;
mod console;
mod gamepad;
mod input;
mod lifecycle;
mod options;
mod overlay;
mod pacing;
mod preview;
mod session;
mod settings;
mod store;
mod stream;
mod text;
mod video;
mod wasapi;
mod window;
mod xhome;

use anyhow::Result;
use options::PlayOptions;
use std::sync::atomic::Ordering;

const USAGE: &str = "\
mestream - Xbox remote play client

  mestream                       same as `mestream play`
  mestream play [options]        stream the console fullscreen
      --windowed                 use a window instead of fullscreen
      --seconds N                stop after N seconds
      --no-audio                 do not play sound
      --plain                    let the graphics driver convert and scale the picture
      --screenshot FILE.bmp      save one frame, 5 seconds in
    These replace a setting for this run only (the menu changes them for good):
      --1080p                    ask for 1080p instead of 720p
      --bitrate KBPS             video bitrate limit
      --smooth MS                most delay the picture smoothing may add (0 = off)
      --tearing                  show pictures at once instead of waiting for the
                                 display refresh (lowest latency, may tear without VRR)
      --sharpen N                sharpening, 0 (off) to 1
      --saturation N             colour intensity, 1 = unchanged
      --contrast N               contrast, 1 = unchanged
    For testing:
      --test-input               press d-pad right once, 3 seconds in
      --test-drop N              drop the stream once after N seconds

  mestream login                 sign in from the terminal with a Microsoft device code
  mestream logout                forget the stored sign-in
  mestream consoles              list consoles available for remote play
  mestream diagnose              ask the Xbox service what state the consoles are in
  mestream restart-console       restart the console and wait until it is back
  mestream probe [options]       connect without a window and measure the stream
      --seconds N                how long to measure (default 15)
      --dump FILE                save the raw H.264 video to FILE
  mestream decode-file FILE [--screenshot OUT.bmp] [--frame N] [--fullscreen]
                                 play a saved H.264 file through the decoder

While streaming: tap the picture, press Esc, or hold both sticks down to open
the settings menu. View + Menu together act as the Xbox button.
";

#[tokio::main]
async fn main() -> Result<()> {
    video::enable_dpi_awareness();
    lifecycle::install_console_handler();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = run_command(&args).await;
    // Anyone waiting to end the process may now do so.
    lifecycle::CLEANED_UP.store(true, Ordering::Relaxed);
    result
}

async fn run_command(args: &[String]) -> Result<()> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        None => app::run(PlayOptions::parse(&[], true)?).await,
        Some("play") => app::run(PlayOptions::parse(rest, true)?).await,
        Some("probe") => app::run(PlayOptions::parse(rest, false)?).await,
        Some("login") => login().await,
        Some("logout") => {
            store::clear()?;
            println!("Signed out.");
            Ok(())
        }
        Some("consoles") => consoles().await,
        Some("diagnose") => console::print_diagnosis().await,
        Some("restart-console") => {
            // Never closes: there is no window here.
            let (_keep, never_closed) = tokio::sync::watch::channel(false);
            let console_id = settings::get().console_id;
            console::restart_and_wait(&never_closed, console_id.as_deref()).await?;
            println!("The console is back.");
            Ok(())
        }
        Some("decode-file") => preview::run(rest),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

async fn login() -> Result<()> {
    let http = session::http_client()?;
    let code = auth::begin_login(&http).await?;
    println!("Open {} and enter the code: {}", code.verification_uri, code.user_code);
    println!("Waiting for approval (valid for {} minutes)...", code.expires_in / 60);
    auth::finish_login(&http, &code).await?;
    println!("Signed in. The sign-in is stored encrypted for this Windows user.");
    Ok(())
}

async fn consoles() -> Result<()> {
    let http = session::http_client()?;
    let account = auth::sign_in(&http).await?;
    let list = session::service(&http, &account).consoles().await?;
    if list.is_empty() {
        println!("No consoles found. Enable remote features on the Xbox (Settings > Devices & connections > Remote features).");
    }
    for console in list {
        println!(
            "{}  [{}]  power: {}  id: {}",
            console.device_name, console.console_type, console.power_state, console.server_id
        );
    }
    Ok(())
}
