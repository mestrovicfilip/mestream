//! The settings menu as data: which settings exist, how each one is edited,
//! and where its controls sit on screen. Nothing here draws or keeps state,
//! which is what lets the tests check sizes and taps without a window.

use super::geometry::{Rect, unit};
use crate::settings::Settings;
use crate::text::{Text, fill, text};

/// One row of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Sharpen,
    Saturation,
    Contrast,
    Resolution,
    Bitrate,
    Sync,
    Smoothing,
    Volume,
    Rumble,
    Stats,
    SwitchConsole,
    RestartConsole,
    SignOut,
}

/// How a row is edited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A value moved in steps with minus and plus.
    Slider { min: f32, max: f32, step: f32 },
    /// One of two named options.
    Choice([&'static str; 2]),
    /// A button. `confirm` asks for a second press before it acts.
    Action { confirm: bool },
}

pub struct Tab {
    pub name: fn(&Text) -> &'static str,
    pub items: &'static [Item],
}

pub const TABS: [Tab; 4] = [
    Tab { name: |t| t.tab_picture, items: &[Item::Sharpen, Item::Saturation, Item::Contrast] },
    Tab { name: |t| t.tab_stream, items: &[Item::Resolution, Item::Bitrate, Item::Sync, Item::Smoothing] },
    Tab { name: |t| t.tab_device, items: &[Item::Volume, Item::Rumble, Item::Stats] },
    Tab { name: |t| t.tab_console, items: &[Item::SwitchConsole, Item::RestartConsole, Item::SignOut] },
];

/// The menu has room for this many rows; a tab must not hold more.
#[cfg(test)]
pub const MAX_ROWS: usize = 4;

impl Item {
    pub fn label(self) -> &'static str {
        let t = text();
        match self {
            Item::Sharpen => t.sharpness,
            Item::Saturation => t.saturation,
            Item::Contrast => t.contrast,
            Item::Resolution => t.resolution,
            Item::Bitrate => t.bitrate,
            Item::Sync => t.display_sync,
            Item::Smoothing => t.smoothing,
            Item::Volume => t.volume,
            Item::Rumble => t.rumble,
            Item::Stats => t.statistics,
            Item::SwitchConsole => t.switch_console_about,
            Item::RestartConsole => t.restart_console_about,
            Item::SignOut => t.sign_out_about,
        }
    }

    pub fn kind(self) -> Kind {
        let t = text();
        match self {
            Item::Sharpen => Kind::Slider { min: 0.0, max: 1.0, step: 0.1 },
            Item::Saturation => Kind::Slider { min: 0.5, max: 2.0, step: 0.05 },
            Item::Contrast => Kind::Slider { min: 0.5, max: 1.5, step: 0.05 },
            Item::Bitrate => Kind::Slider { min: 2.0, max: 40.0, step: 1.0 },
            Item::Smoothing => Kind::Slider { min: 0.0, max: 16.0, step: 2.0 },
            Item::Volume => Kind::Slider { min: 0.0, max: 1.0, step: 0.1 },
            Item::Resolution => Kind::Choice(["720p", "1080p"]),
            Item::Sync => Kind::Choice([t.sync_vsync, t.sync_low_latency]),
            Item::Rumble | Item::Stats => Kind::Choice([t.off, t.on]),
            Item::SwitchConsole => Kind::Action { confirm: false },
            Item::RestartConsole | Item::SignOut => Kind::Action { confirm: true },
        }
    }

    /// One short line explaining the setting, shown under its label.
    pub fn about(self, console_name: &str) -> String {
        let t = text();
        let line = match self {
            Item::Sharpen => t.about_sharpness,
            Item::Saturation => t.about_saturation,
            Item::Contrast => t.about_contrast,
            Item::Resolution => t.about_resolution,
            Item::Bitrate => t.about_bitrate,
            Item::Sync => t.about_display_sync,
            Item::Smoothing => t.about_smoothing,
            Item::Volume => t.about_volume,
            Item::Rumble => t.about_rumble,
            Item::Stats => t.about_statistics,
            Item::RestartConsole => t.about_restart_console,
            Item::SignOut => t.about_sign_out,
            Item::SwitchConsole if console_name.is_empty() => "",
            Item::SwitchConsole => return fill(t.about_switch_console, console_name),
        };
        line.to_owned()
    }

    /// The button text of an action row.
    pub fn action_label(self) -> &'static str {
        let t = text();
        match self {
            Item::SwitchConsole => t.switch_console,
            Item::RestartConsole => t.restart_console,
            Item::SignOut => t.sign_out,
            _ => "",
        }
    }

    /// The slider's value in the unit the user sees (bitrate in Mbit/s).
    pub fn value(self, s: &Settings) -> f32 {
        match self {
            Item::Sharpen => s.sharpen,
            Item::Saturation => s.saturation,
            Item::Contrast => s.contrast,
            Item::Bitrate => s.bitrate_kbps as f32 / 1000.0,
            Item::Smoothing => s.smoothing_ms,
            Item::Volume => s.volume,
            _ => 0.0,
        }
    }

    fn set_value(self, s: &mut Settings, value: f32) {
        match self {
            Item::Sharpen => s.sharpen = value,
            Item::Saturation => s.saturation = value,
            Item::Contrast => s.contrast = value,
            Item::Bitrate => s.bitrate_kbps = (value * 1000.0).round() as u32,
            Item::Smoothing => s.smoothing_ms = value,
            Item::Volume => s.volume = value,
            _ => {}
        }
    }

    /// Which of a choice row's two options is selected.
    pub fn choice(self, s: &Settings) -> usize {
        match self {
            Item::Resolution => usize::from(s.resolution == 1080),
            Item::Sync => usize::from(!s.vsync),
            Item::Rumble => usize::from(s.rumble),
            Item::Stats => usize::from(s.show_stats),
            _ => 0,
        }
    }

    pub fn choose(self, s: &mut Settings, option: usize) {
        let second = option == 1;
        match self {
            Item::Resolution => s.resolution = if second { 1080 } else { 720 },
            Item::Sync => s.vsync = !second,
            Item::Rumble => s.rumble = second,
            Item::Stats => s.show_stats = second,
            _ => {}
        }
    }

    /// Moves a slider one step, or a choice to its other option.
    pub fn nudge(self, s: &mut Settings, direction: i32) {
        match self.kind() {
            Kind::Slider { min, max, step } => {
                // Count in whole steps so repeated presses cannot drift off the grid.
                let steps = (self.value(s) / step).round() + direction as f32;
                self.set_value(s, (steps * step).clamp(min, max));
            }
            Kind::Choice(_) => self.choose(s, usize::from(direction > 0)),
            Kind::Action { .. } => {}
        }
    }

    pub fn reset(self, s: &mut Settings) {
        let defaults = Settings::default();
        match self.kind() {
            Kind::Slider { .. } => self.set_value(s, self.value(&defaults)),
            Kind::Choice(_) => self.choose(s, self.choice(&defaults)),
            Kind::Action { .. } => {}
        }
    }

    /// The value as shown beside the label.
    pub fn display(self, s: &Settings) -> String {
        let t = text();
        match self {
            Item::Sharpen | Item::Saturation | Item::Contrast => format!("{:.0} %", self.value(s) * 100.0),
            Item::Volume if s.volume <= 0.0 => t.muted.to_owned(),
            Item::Volume => format!("{:.0} %", s.volume * 100.0),
            Item::Bitrate => format!("{:.0} Mbit/s", self.value(s)),
            Item::Smoothing if s.smoothing_ms <= 0.0 => t.off.to_owned(),
            Item::Smoothing => format!("{:.0} ms", s.smoothing_ms),
            _ => String::new(),
        }
    }

    /// Where a slider value sits between its minimum and maximum, 0..1.
    pub fn fraction(self, value: f32) -> f32 {
        match self.kind() {
            Kind::Slider { min, max, .. } => ((value - min) / (max - min)).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

// ------------------------------------------------------------------ layout

pub struct Row {
    pub item: Item,
    pub label: Rect,
    pub value: Rect,
    /// The explanation under the label.
    pub about: Rect,
    /// The whole line of controls under the explanation.
    pub control: Rect,
    pub minus: Rect,
    pub track: Rect,
    pub plus: Rect,
    /// The two halves of a choice row.
    pub options: [Rect; 2],
}

pub struct Layout {
    pub sheet: Rect,
    pub title: Rect,
    pub close: Rect,
    pub tabs: [Rect; TABS.len()],
    pub rows: Vec<Row>,
    pub reset: Rect,
    pub quit: Rect,
    /// One design unit in pixels (1.0 on a 720-pixel-high screen).
    pub unit: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Adjust(Item, i32),
    Choose(Item, usize),
    Act(Item),
    Tab(usize),
    Reset,
    Quit,
    Close,
    /// Inside the menu but not on a control.
    Panel,
    Outside,
}

/// Geometry of the menu, docked to the right edge, for the given tab. All
/// sizes scale with the screen height so controls stay finger-sized.
pub fn layout(width: f32, height: f32, tab: usize) -> Layout {
    let u = unit(height);
    let sheet_width = (460.0 * u).min(width * 0.6);
    let sheet = Rect { left: width - sheet_width, top: 0.0, right: width, bottom: height };
    let pad = 28.0 * u;
    let left = sheet.left + pad;
    let right = sheet.right - pad;
    let button = 60.0 * u;

    let title = Rect { left, top: pad, right: right - button, bottom: pad + button };
    let close = Rect { left: right - button, top: pad, right, bottom: pad + button };

    let tab_gap = 6.0 * u;
    let tab_width = (right - left - tab_gap * (TABS.len() - 1) as f32) / TABS.len() as f32;
    let tabs_top = title.bottom + 4.0 * u;
    let tabs = std::array::from_fn(|index| {
        let tab_left = left + index as f32 * (tab_width + tab_gap);
        Rect { left: tab_left, top: tabs_top, right: tab_left + tab_width, bottom: tabs_top + 52.0 * u }
    });

    let rows_top = tabs_top + 52.0 * u + 10.0 * u;
    let row = |index: usize, item: Item| {
        let top = rows_top + index as f32 * 116.0 * u;
        let label = Rect { left, top, right: right - 120.0 * u, bottom: top + 26.0 * u };
        let value = Rect { left: right - 120.0 * u, top, right, bottom: top + 26.0 * u };
        let about = Rect { left, top: label.bottom, right, bottom: label.bottom + 18.0 * u };
        let control_top = about.bottom + 4.0 * u;
        let control = Rect { left, top: control_top, right, bottom: control_top + button };
        let minus = Rect { right: left + 64.0 * u, ..control };
        let plus = Rect { left: right - 64.0 * u, ..control };
        let track = Rect { left: minus.right + 16.0 * u, right: plus.left - 16.0 * u, ..control };
        let middle = (left + right) / 2.0;
        let options = [
            Rect { right: middle - 4.0 * u, ..control },
            Rect { left: middle + 4.0 * u, ..control },
        ];
        Row { item, label, value, about, control, minus, track, plus, options }
    };
    let rows = TABS[tab].items.iter().enumerate().map(|(index, item)| row(index, *item)).collect();

    let footer_top = height - pad - button;
    let middle = (left + right) / 2.0;
    let reset = Rect { left, top: footer_top, right: middle - 6.0 * u, bottom: footer_top + button };
    let quit = Rect { left: middle + 6.0 * u, top: footer_top, right, bottom: footer_top + button };

    Layout { sheet, title, close, tabs, rows, reset, quit, unit: u }
}

pub fn hit(layout: &Layout, x: f32, y: f32) -> Hit {
    for row in &layout.rows {
        match row.item.kind() {
            Kind::Slider { .. } if row.minus.contains(x, y) => return Hit::Adjust(row.item, -1),
            Kind::Slider { .. } if row.plus.contains(x, y) => return Hit::Adjust(row.item, 1),
            Kind::Choice(_) => {
                if let Some(option) = row.options.iter().position(|r| r.contains(x, y)) {
                    return Hit::Choose(row.item, option);
                }
            }
            Kind::Action { .. } if row.control.contains(x, y) => return Hit::Act(row.item),
            _ => {}
        }
    }
    if let Some(tab) = layout.tabs.iter().position(|r| r.contains(x, y)) {
        Hit::Tab(tab)
    } else if layout.reset.contains(x, y) {
        Hit::Reset
    } else if layout.quit.contains(x, y) {
        Hit::Quit
    } else if layout.close.contains(x, y) {
        Hit::Close
    } else if layout.sheet.contains(x, y) {
        Hit::Panel
    } else {
        Hit::Outside
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn center(rect: &Rect) -> (f32, f32) {
        ((rect.left + rect.right) / 2.0, (rect.top + rect.bottom) / 2.0)
    }

    #[test]
    fn every_tab_fits_and_its_controls_are_finger_sized() {
        for (w, h) in [(1280.0, 720.0), (1920.0, 1080.0), (1920.0, 1200.0), (1280.0, 800.0)] {
            for tab in 0..TABS.len() {
                assert!(TABS[tab].items.len() <= MAX_ROWS);
                let l = layout(w, h, tab);
                assert_eq!(l.sheet.right, w);
                assert!(l.sheet.left >= w * 0.4, "the picture stays visible beside the menu");
                let inside =
                    |r: &Rect| r.left >= l.sheet.left && r.right <= l.sheet.right && r.top >= 0.0 && r.bottom <= h;
                for row in &l.rows {
                    assert!(row.control.height() >= 60.0);
                    assert!(row.minus.width() >= 60.0 && row.plus.width() >= 60.0);
                    assert!(row.minus.right < row.track.left && row.track.right < row.plus.left);
                    assert!(row.track.width() >= 150.0, "room for the value bar");
                    assert!(row.options[0].right < row.options[1].left);
                    assert!(row.label.right <= row.value.left);
                    assert!(inside(&row.control));
                }
                for pair in l.rows.windows(2) {
                    assert!(pair[1].label.top >= pair[0].control.bottom, "rows do not overlap");
                }
                assert!(l.rows[0].label.top >= l.tabs[0].bottom);
                assert!(l.reset.top >= l.rows.last().unwrap().control.bottom, "the footer clears the last row");
                for row in &l.rows {
                    assert!(row.about.top >= row.label.bottom && row.about.bottom <= row.control.top);
                    assert!(row.about.height() >= 16.0);
                }
                assert!(inside(&l.reset) && inside(&l.quit) && inside(&l.close));
                assert!(l.reset.right <= l.quit.left && l.title.right <= l.close.left);
                for pair in l.tabs.windows(2) {
                    assert!(pair[0].right < pair[1].left);
                }
                assert!(l.tabs.iter().all(|t| inside(t) && t.height() >= 50.0 && t.width() >= 80.0));
            }
        }
    }

    #[test]
    fn a_full_tab_fits_on_the_smallest_screen() {
        let l = layout(1280.0, 720.0, 1);
        assert_eq!(l.rows.len(), MAX_ROWS);
        assert!(l.rows[MAX_ROWS - 1].control.bottom <= l.reset.top);
    }

    #[test]
    fn taps_map_to_the_right_controls() {
        let picture = layout(1280.0, 720.0, 0);
        let (x, y) = center(&picture.rows[0].plus);
        assert_eq!(hit(&picture, x, y), Hit::Adjust(Item::Sharpen, 1));
        let (x, y) = center(&picture.rows[1].minus);
        assert_eq!(hit(&picture, x, y), Hit::Adjust(Item::Saturation, -1));
        let (x, y) = center(&picture.rows[0].track);
        assert_eq!(hit(&picture, x, y), Hit::Panel, "the bar itself is not a button");
        let (x, y) = center(&picture.tabs[2]);
        assert_eq!(hit(&picture, x, y), Hit::Tab(2));
        let (x, y) = center(&picture.reset);
        assert_eq!(hit(&picture, x, y), Hit::Reset);
        let (x, y) = center(&picture.quit);
        assert_eq!(hit(&picture, x, y), Hit::Quit);
        let (x, y) = center(&picture.close);
        assert_eq!(hit(&picture, x, y), Hit::Close);
        assert_eq!(hit(&picture, 100.0, 360.0), Hit::Outside);

        let stream = layout(1280.0, 720.0, 1);
        let (x, y) = center(&stream.rows[0].options[1]);
        assert_eq!(hit(&stream, x, y), Hit::Choose(Item::Resolution, 1));
        let console = layout(1280.0, 720.0, 3);
        let (x, y) = center(&console.rows[2].control);
        assert_eq!(hit(&console, x, y), Hit::Act(Item::SignOut));
    }

    #[test]
    fn every_setting_has_an_explanation_that_fits_on_one_line() {
        for tab in &TABS {
            for item in tab.items {
                let about = item.about("XBOX");
                assert!(!about.is_empty(), "{item:?} has no explanation");
                assert!(about.chars().count() <= 60, "{item:?}: {about:?} is too long for one line");
            }
        }
        assert_eq!(Item::SwitchConsole.about(""), "");
    }

    #[test]
    fn nudging_steps_and_stops_at_the_limits() {
        let mut s = Settings::default();
        Item::Saturation.nudge(&mut s, 1);
        assert!((s.saturation - 1.05).abs() < 1e-4);
        Item::Sharpen.nudge(&mut s, -1);
        assert!((s.sharpen - 0.4).abs() < 1e-4);
        for _ in 0..60 {
            Item::Sharpen.nudge(&mut s, 1);
            Item::Contrast.nudge(&mut s, -1);
            Item::Bitrate.nudge(&mut s, 1);
            Item::Volume.nudge(&mut s, -1);
        }
        assert_eq!(s.sharpen, 1.0);
        assert_eq!(s.contrast, 0.5);
        assert_eq!(s.bitrate_kbps, 40_000);
        assert_eq!(Item::Saturation.display(&s), "105 %");
        assert_eq!(Item::Bitrate.display(&s), "40 Mbit/s");
        assert_eq!(Item::Volume.display(&s), text().muted);
    }

    #[test]
    fn choices_read_and_write_the_setting_behind_them() {
        let mut s = Settings::default();
        assert_eq!(Item::Resolution.choice(&s), 0);
        Item::Resolution.choose(&mut s, 1);
        assert_eq!(s.resolution, 1080);
        Item::Sync.nudge(&mut s, 1);
        assert!(!s.vsync, "the second option is low latency");
        Item::Rumble.choose(&mut s, 0);
        assert!(!s.rumble);
        for item in [Item::Resolution, Item::Sync, Item::Rumble, Item::Sharpen] {
            item.reset(&mut s);
        }
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn value_bar_fraction_spans_the_range() {
        assert_eq!(Item::Sharpen.fraction(0.0), 0.0);
        assert_eq!(Item::Sharpen.fraction(1.0), 1.0);
        assert!((Item::Saturation.fraction(1.0) - 1.0 / 3.0).abs() < 1e-4);
        assert_eq!(Item::Contrast.fraction(9.0), 1.0);
    }
}
