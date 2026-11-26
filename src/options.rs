//! Command-line options of `play` and `probe`.

use crate::settings;
use anyhow::{Context, Result, anyhow, bail};
use std::path::PathBuf;

pub struct PlayOptions {
    /// How long to stream; `u64::MAX` means until the user leaves.
    pub seconds: u64,
    /// Show the stream in a window (`play`) or only measure it (`probe`).
    pub show: bool,
    pub fullscreen: bool,
    pub audio: bool,
    /// Draw with our own shader rather than the graphics driver's converter.
    pub enhance: bool,
    pub dump: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
    pub test_input: bool,
    /// Pretend the stream was lost after this many seconds (once).
    pub test_drop: Option<u64>,
}

impl PlayOptions {
    /// Reads the options. Those that mirror a setting (bitrate, resolution,
    /// picture) replace it for this run without being saved.
    pub fn parse(args: &[String], show: bool) -> Result<Self> {
        let mut options = Self {
            seconds: if show { u64::MAX } else { 15 },
            show,
            fullscreen: true,
            audio: true,
            enhance: true,
            dump: None,
            screenshot: None,
            test_input: false,
            test_drop: None,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || it.next().ok_or_else(|| anyhow!("{arg} needs a value"));
            match arg.as_str() {
                "--seconds" => options.seconds = value()?.parse().context("--seconds")?,
                "--dump" => options.dump = Some(PathBuf::from(value()?)),
                "--screenshot" => options.screenshot = Some(PathBuf::from(value()?)),
                "--windowed" => options.fullscreen = false,
                "--no-audio" => options.audio = false,
                "--plain" => options.enhance = false,
                "--test-input" => options.test_input = true,
                "--test-drop" => options.test_drop = Some(value()?.parse().context("--test-drop")?),
                "--1080p" => settings::override_with(|s| s.resolution = 1080),
                "--tearing" => settings::override_with(|s| s.vsync = false),
                "--bitrate" => {
                    let kbps = value()?.parse().context("--bitrate")?;
                    settings::override_with(|s| s.bitrate_kbps = kbps);
                }
                "--smooth" => {
                    let ms = value()?.parse().context("--smooth")?;
                    settings::override_with(|s| s.smoothing_ms = ms);
                }
                "--sharpen" | "--saturation" | "--contrast" => {
                    let number: f32 = value()?.parse().with_context(|| arg.clone())?;
                    settings::override_with(|s| match arg.as_str() {
                        "--sharpen" => s.sharpen = number,
                        "--saturation" => s.saturation = number,
                        _ => s.contrast = number,
                    });
                }
                other => bail!("unknown option {other} (run `mestream help` for the list)"),
            }
        }
        Ok(options)
    }
}
