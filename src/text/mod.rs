//! Every sentence the user can read on screen, in one place per language.
//!
//! Lines written to the terminal and the connection log stay in English in
//! the code: they are for diagnosing problems, and error reports are easier
//! to compare when they all read the same.

mod en;

pub struct Text {
    pub settings: &'static str,
    pub tab_picture: &'static str,
    pub tab_stream: &'static str,
    pub tab_device: &'static str,
    pub tab_console: &'static str,
    pub sharpness: &'static str,
    pub saturation: &'static str,
    pub contrast: &'static str,
    pub resolution: &'static str,
    pub bitrate: &'static str,
    pub display_sync: &'static str,
    pub sync_vsync: &'static str,
    pub sync_low_latency: &'static str,
    pub smoothing: &'static str,
    pub volume: &'static str,
    pub rumble: &'static str,
    pub statistics: &'static str,
    pub on: &'static str,
    pub off: &'static str,
    pub muted: &'static str,
    pub switch_console: &'static str,
    pub switch_console_about: &'static str,
    pub restart_console: &'static str,
    pub restart_console_about: &'static str,
    pub sign_out: &'static str,
    pub sign_out_about: &'static str,
    pub confirm: &'static str,
    pub defaults: &'static str,
    pub quit: &'static str,
    pub about_sharpness: &'static str,
    pub about_saturation: &'static str,
    pub about_contrast: &'static str,
    pub about_resolution: &'static str,
    pub about_bitrate: &'static str,
    pub about_display_sync: &'static str,
    pub about_smoothing: &'static str,
    pub about_volume: &'static str,
    pub about_rumble: &'static str,
    pub about_statistics: &'static str,
    pub about_switch_console: &'static str,
    pub about_restart_console: &'static str,
    pub about_sign_out: &'static str,

    pub sign_in_title: &'static str,
    pub sign_in_body: &'static str,
    pub sign_in_waiting: &'static str,
    pub open_browser: &'static str,
    pub copy_code: &'static str,
    pub copied: &'static str,

    pub pick_console_title: &'static str,
    pub pick_console_body: &'static str,
    pub no_console_title: &'static str,
    pub no_console_body: &'static str,

    pub failure_title: &'static str,
    pub try_again: &'static str,
    pub copy_error: &'static str,
    pub remote_features_off: &'static str,
    pub console_off: &'static str,
    pub console_unreachable: &'static str,
    pub console_deaf: &'static str,
    pub restart_failed: &'static str,

    pub report_error: &'static str,
    pub report_log: &'static str,
}

/// The text in the language in use. Only English exists so far; a language
/// setting would choose between the tables here.
pub fn text() -> &'static Text {
    &en::TEXT
}

/// Puts `value` where the template says `{0}`.
pub fn fill(template: &str, value: &str) -> String {
    template.replace("{0}", value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_the_placeholder() {
        assert_eq!(fill("Connected to {0}", "XBOX"), "Connected to XBOX");
        assert_eq!(fill(text().about_switch_console, "Den"), "Currently connected to Den.");
    }
}
