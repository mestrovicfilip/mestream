//! `mestream decode-file`: plays a saved H.264 file through the real decoder
//! and renderer, and can put the overlay in any of its states. This is how
//! the picture path and the screens are checked without a console.

use crate::overlay::{self, Action, Button, Prompt, Role, Style, Tone};
use crate::text::text;
use crate::{app, auth, settings, video};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The screen to show instead of (or over) the video.
enum Screen {
    Video,
    Failure { offer_restart: bool },
    SignIn,
    ConsolePicker,
}

fn show(screen: &Screen) {
    let t = text();
    match screen {
        Screen::Video => {}
        Screen::Failure { offer_restart } => {
            let detail = if *offer_restart {
                "console refused the session: WNSError"
            } else {
                "timed out waiting for the console (last state: Provisioning)"
            };
            overlay::show_prompt(app::failure_prompt(detail, None));
        }
        Screen::SignIn => overlay::show_prompt(app::sign_in_prompt(&auth::DeviceCode {
            user_code: "ABCD1234".into(),
            device_code: String::new(),
            verification_uri: "https://www.microsoft.com/link".into(),
            expires_in: 900,
            interval: 5,
        })),
        Screen::ConsolePicker => {
            let mut prompt = Prompt::new(Tone::Neutral, t.pick_console_title, t.pick_console_body);
            prompt.list = vec![
                ("Living room".to_owned(), "XboxSeriesX".to_owned()),
                ("Bedroom".to_owned(), "XboxSeriesS".to_owned()),
            ];
            prompt.buttons = vec![
                Button::new(t.sign_out, Role::Do(Action::SignOut)).confirmed(),
                Button::new(t.quit, Role::Quit).styled(Style::Danger),
            ];
            overlay::show_prompt(prompt);
        }
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let mut file = None;
    let mut screenshot = None;
    let mut fullscreen = false;
    let mut enhance = true;
    let mut screenshot_at = 1u64;
    let mut hold_seconds = 0f32;
    let mut screen = Screen::Video;
    let mut menu_tab = None;
    let mut focus = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut number = || -> Result<f32> {
            it.next().ok_or_else(|| anyhow!("{arg} needs a value"))?.parse().context("not a number")
        };
        match arg.as_str() {
            "--plain" => enhance = false,
            "--menu" => menu_tab = Some(number()? as usize),
            "--focus" => focus = Some(number()? as usize),
            "--stats" => settings::override_with(|s| s.show_stats = true),
            "--hold" => hold_seconds = number()?,
            "--fail" => screen = Screen::Failure { offer_restart: false },
            "--fail-restart" => screen = Screen::Failure { offer_restart: true },
            "--sign-in" => screen = Screen::SignIn,
            "--picker" => screen = Screen::ConsolePicker,
            "--sharpen" | "--saturation" | "--contrast" => {
                let value = number()?;
                settings::override_with(|s| match arg.as_str() {
                    "--sharpen" => s.sharpen = value,
                    "--saturation" => s.saturation = value,
                    _ => s.contrast = value,
                });
            }
            "--frame" => screenshot_at = number()? as u64,
            "--screenshot" => screenshot = it.next().map(PathBuf::from),
            "--fullscreen" => fullscreen = true,
            other => file = Some(PathBuf::from(other)),
        }
    }
    let file = file.ok_or_else(|| anyhow!("decode-file needs a file"))?;
    let data = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let units = video::split_access_units(&data);
    println!("{} access units in {}", units.len(), file.display());
    if let Some(tab) = menu_tab {
        overlay::preview_menu(tab, focus);
        overlay::set_stats("60 fps   12.4 Mbit/s   2.3 ms".to_owned());
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let stats = Arc::new(video::VideoStats::default());
    let quit = Arc::new(AtomicBool::new(false));
    app::release_own_console();
    let feeder = std::thread::spawn(move || {
        // The start-up screen first: a few log lines, then the chosen screen.
        if hold_seconds > 0.0 {
            let lines = [
                "[auth] signing in",
                "[auth] signed in (1180 ms)",
                "[console] XBOX (XboxSeriesS), power: On",
                "[session] state: Provisioning",
                "[session] state: Provisioned",
                "[webrtc] negotiating",
                "[webrtc] connected (4210 ms since start)",
            ];
            let pause = Duration::from_secs_f32(hold_seconds / lines.len() as f32);
            for line in lines {
                crate::status!("{line}");
                std::thread::sleep(pause);
            }
        }
        if !matches!(screen, Screen::Video) {
            show(&screen);
            if focus.is_some() {
                overlay::navigate(overlay::Nav::Down);
            }
            std::thread::sleep(Duration::from_secs(3));
            return;
        }
        for (index, data) in units.into_iter().enumerate() {
            let rtp_timestamp = index as u32 * 1500;
            if tx.send(video::AccessUnit { data, arrival: Instant::now(), rtp_timestamp }).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_micros(16_667));
        }
    });
    let started = Instant::now();
    let options = video::VideoOptions {
        fullscreen,
        screenshot: screenshot.map(|path| (path, screenshot_at)),
        timings: None,
        enhance,
    };
    video::run(rx, options, stats.clone(), quit)?;
    let _ = feeder.join();

    let presented = stats.presented.load(Ordering::Relaxed);
    println!(
        "decoded {} / presented {} / skipped {} frames at {}x{} in {:.1} s, avg arrival-to-present {:.2} ms",
        stats.decoded.load(Ordering::Relaxed),
        presented,
        stats.skipped.load(Ordering::Relaxed),
        stats.width.load(Ordering::Relaxed),
        stats.height.load(Ordering::Relaxed),
        started.elapsed().as_secs_f64(),
        stats.latency_us.load(Ordering::Relaxed) as f64 / presented.max(1) as f64 / 1000.0,
    );
    Ok(())
}
