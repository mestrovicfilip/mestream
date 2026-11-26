//! What the user has chosen, kept in `settings.json` next to the sign-in.
//!
//! Everything that reads a setting asks here, so a change made in the menu
//! reaches the picture, the sound and the controller without anyone passing
//! values around. `version()` lets hot paths notice a change cheaply.

use crate::store;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 0 = no sharpening, 1 = strongest.
    pub sharpen: f32,
    /// 1 = unchanged.
    pub saturation: f32,
    /// 1 = unchanged.
    pub contrast: f32,
    /// Picture height the console is asked to encode: 720 or 1080.
    pub resolution: u32,
    /// Upper limit for the video bitrate.
    pub bitrate_kbps: u32,
    /// Wait for the display refresh before showing a picture (no tearing).
    pub vsync: bool,
    /// Most delay the picture smoothing may add; 0 shows pictures on arrival.
    pub smoothing_ms: f32,
    /// 0 = silent, 1 = as loud as the console sends it.
    pub volume: f32,
    pub rumble: bool,
    /// Show frame rate, bitrate and delay in a corner of the picture.
    pub show_stats: bool,
    /// The console to play on; `None` until one has been chosen.
    pub console_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sharpen: 0.5,
            saturation: 1.0,
            contrast: 1.0,
            resolution: 720,
            bitrate_kbps: 15_000,
            vsync: true,
            smoothing_ms: 8.0,
            volume: 1.0,
            rumble: true,
            show_stats: false,
            console_id: None,
        }
    }
}

impl Settings {
    /// A hand-edited or damaged file must not be able to break the stream.
    fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        let within = |value: f32, min: f32, max: f32, fallback: f32| {
            if value.is_finite() { value.clamp(min, max) } else { fallback }
        };
        self.sharpen = within(self.sharpen, 0.0, 1.0, defaults.sharpen);
        self.saturation = within(self.saturation, 0.5, 2.0, defaults.saturation);
        self.contrast = within(self.contrast, 0.5, 1.5, defaults.contrast);
        self.smoothing_ms = within(self.smoothing_ms, 0.0, 16.0, defaults.smoothing_ms);
        self.volume = within(self.volume, 0.0, 1.0, defaults.volume);
        self.bitrate_kbps = self.bitrate_kbps.clamp(2_000, 40_000);
        if self.resolution != 1080 {
            self.resolution = 720;
        }
        self
    }
}

static CURRENT: LazyLock<Mutex<Settings>> = LazyLock::new(|| Mutex::new(load()));
static VERSION: AtomicU64 = AtomicU64::new(1);

pub fn get() -> Settings {
    CURRENT.lock().unwrap().clone()
}

/// Goes up every time a setting changes.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// Changes settings and remembers them for next time.
pub fn update(change: impl FnOnce(&mut Settings)) {
    let saved = apply(change);
    if let Err(e) = save(&saved) {
        eprintln!("[settings] could not save: {e:#}");
    }
}

/// Changes settings for this run only (command-line options).
pub fn override_with(change: impl FnOnce(&mut Settings)) {
    apply(change);
}

fn apply(change: impl FnOnce(&mut Settings)) -> Settings {
    let mut current = CURRENT.lock().unwrap();
    let mut changed = current.clone();
    change(&mut changed);
    *current = changed.sanitized();
    VERSION.fetch_add(1, Ordering::Relaxed);
    current.clone()
}

fn save(settings: &Settings) -> anyhow::Result<()> {
    let dir = store::dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("settings.json"), serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

fn load() -> Settings {
    let read = |name: &str| {
        let text = std::fs::read_to_string(store::dir().ok()?.join(name)).ok()?;
        serde_json::from_str::<Settings>(&text).ok()
    };
    // Version 0.1 kept only the picture settings, in a file of their own with
    // the same field names.
    read("settings.json").or_else(|| read("picture.json")).unwrap_or_default().sanitized()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_values_fall_back_to_something_usable() {
        let broken = Settings {
            sharpen: f32::NAN,
            saturation: 9.0,
            bitrate_kbps: 5,
            resolution: 1234,
            volume: -3.0,
            ..Settings::default()
        };
        let fixed = broken.sanitized();
        assert_eq!(fixed.sharpen, 0.5);
        assert_eq!(fixed.saturation, 2.0);
        assert_eq!(fixed.bitrate_kbps, 2_000);
        assert_eq!(fixed.resolution, 720);
        assert_eq!(fixed.volume, 0.0);
    }

    #[test]
    fn a_file_with_only_some_fields_keeps_defaults_for_the_rest() {
        let old: Settings = serde_json::from_str(r#"{"sharpen":0.3,"saturation":1.1,"contrast":1.0}"#).unwrap();
        assert_eq!(old.sharpen, 0.3);
        assert_eq!(old.bitrate_kbps, 15_000);
        assert!(old.vsync && old.rumble);
    }
}
