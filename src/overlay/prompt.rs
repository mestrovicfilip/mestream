//! Full-screen messages shown when there is no picture: signing in, choosing
//! a console, and failures. A prompt is plain data; the code that shows one
//! then waits for the `Action` the user picks.

use super::geometry::{Rect, unit};

/// What the user asked for. The connection code acts on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Retry,
    /// Close the stream and open it again with the current settings.
    Reconnect,
    RestartConsole,
    SwitchConsole,
    SignOut,
    /// The nth entry of the prompt's list.
    Pick(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Warning,
    Danger,
}

/// What pressing a button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Do(Action),
    /// Copy the prompt's `copy_text` to the clipboard.
    Copy,
    /// Open the prompt's `link` in the browser.
    Open,
    Quit,
}

#[derive(Debug, Clone)]
pub struct Button {
    pub label: &'static str,
    pub role: Role,
    pub style: Style,
    /// Needs a second press before it acts.
    pub confirm: bool,
}

impl Button {
    pub fn new(label: &'static str, role: Role) -> Self {
        Self { label, role, style: Style::Plain, confirm: false }
    }

    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn confirmed(mut self) -> Self {
        self.confirm = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub tone: Tone,
    pub title: String,
    pub body: String,
    /// A code to read off the screen, shown large.
    pub code: Option<String>,
    /// Shown as a QR code, and opened by a `Role::Open` button.
    pub link: Option<String>,
    /// A line with a spinner: something is being waited for.
    pub status: Option<String>,
    /// Entries to choose from: (name, detail).
    pub list: Vec<(String, String)>,
    pub buttons: Vec<Button>,
    /// What a `Role::Copy` button puts on the clipboard.
    pub copy_text: String,
}

impl Prompt {
    pub fn new(tone: Tone, title: &str, body: &str) -> Self {
        Self {
            tone,
            title: title.to_owned(),
            body: body.to_owned(),
            code: None,
            link: None,
            status: None,
            list: Vec::new(),
            buttons: Vec::new(),
            copy_text: String::new(),
        }
    }

    /// How many things can be pressed: list entries first, then buttons.
    pub fn targets(&self) -> usize {
        self.list.len() + self.buttons.len()
    }
}

pub struct PromptLayout {
    pub icon: Option<Rect>,
    pub title: Rect,
    pub body: Rect,
    pub code: Option<Rect>,
    pub qr: Option<Rect>,
    pub status: Option<Rect>,
    pub list: Vec<Rect>,
    pub buttons: Vec<Rect>,
    pub unit: f32,
}

/// Size of the body text, in design units.
pub const BODY_FONT: f32 = 19.0;

/// Roughly how many lines `text` wraps to. The layout runs where no text
/// engine is at hand (hit-testing, tests), so it estimates from the average
/// letter width; the box it reserves is a little generous rather than tight.
fn estimated_lines(text: &str, box_width: f32, font_size: f32) -> f32 {
    let letters_per_line = (box_width / (font_size * 0.5)).max(8.0);
    text.split('\n')
        .map(|paragraph| (paragraph.chars().count() as f32 / letters_per_line).ceil().max(1.0))
        .sum()
}

/// Stacks the prompt's parts in a column centred on the screen.
pub fn layout(width: f32, height: f32, prompt: &Prompt) -> PromptLayout {
    let u = unit(height);
    let column = (width * 0.72).min(760.0 * u);
    let (left, right) = ((width - column) / 2.0, (width + column) / 2.0);

    let body_height = if prompt.body.is_empty() {
        0.0
    } else {
        estimated_lines(&prompt.body, column, BODY_FONT * u) * 26.0 * u
    };
    // (height, gap below) of each part, top to bottom; absent parts take no room.
    let part = |present: bool, size: f32, gap: f32| if present { (size * u, gap * u) } else { (0.0, 0.0) };
    let list_height = prompt.list.len() as f32 * 78.0 - 10.0;
    let parts = [
        part(prompt.tone == Tone::Error, 52.0, 16.0),
        part(true, 38.0, 8.0),
        (body_height, if body_height > 0.0 { 18.0 * u } else { 0.0 }),
        part(prompt.code.is_some(), 76.0, 16.0),
        part(prompt.link.is_some(), 168.0, 14.0),
        part(prompt.status.is_some(), 26.0, 18.0),
        part(!prompt.list.is_empty(), list_height, 20.0),
        part(!prompt.buttons.is_empty(), 60.0, 0.0),
    ];
    let total: f32 = parts.iter().map(|(size, gap)| size + gap).sum();
    let mut top = ((height - total) / 2.0).max(12.0 * u);
    let mut next = |index: usize| {
        let (size, gap) = parts[index];
        let rect = Rect { left, top, right, bottom: top + size };
        top += size + gap;
        (size > 0.0).then_some(rect)
    };

    let centred = |rect: Rect, size: f32| {
        let middle = (rect.left + rect.right) / 2.0;
        Rect { left: middle - size / 2.0, right: middle + size / 2.0, ..rect }
    };
    let icon = next(0).map(|r| centred(r, 52.0 * u));
    let title = next(1).unwrap_or(Rect { left, top: 0.0, right, bottom: 0.0 });
    let body = next(2).unwrap_or(Rect { left, top: title.bottom, right, bottom: title.bottom });
    let code = next(3).map(|r| centred(r, (420.0 * u).min(column)));
    let qr = next(4).map(|r| centred(r, 168.0 * u));
    let status = next(5);
    let list = match next(6) {
        Some(area) => (0..prompt.list.len())
            .map(|index| {
                let entry_top = area.top + index as f32 * 78.0 * u;
                Rect { top: entry_top, bottom: entry_top + 68.0 * u, ..area }
            })
            .collect(),
        None => Vec::new(),
    };
    let buttons = match next(7) {
        Some(area) => {
            let count = prompt.buttons.len() as f32;
            let gap = 16.0 * u;
            let available = width * 0.94;
            let button_width = (220.0 * u).min((available - gap * (count - 1.0)) / count);
            let row_left = width / 2.0 - (count * button_width + (count - 1.0) * gap) / 2.0;
            (0..prompt.buttons.len())
                .map(|index| {
                    let button_left = row_left + index as f32 * (button_width + gap);
                    Rect { left: button_left, right: button_left + button_width, ..area }
                })
                .collect()
        }
        None => Vec::new(),
    };

    PromptLayout { icon, title, body, code, qr, status, list, buttons, unit: u }
}

/// Which target (list entry or button) is at the point, if any.
pub fn hit(layout: &PromptLayout, x: f32, y: f32) -> Option<usize> {
    layout.list.iter().chain(&layout.buttons).position(|rect| rect.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_in() -> Prompt {
        let mut prompt = Prompt::new(Tone::Neutral, "Sign in", "Open microsoft.com/link and enter this code.");
        prompt.code = Some("ABCD1234".into());
        prompt.link = Some("https://example.com".into());
        prompt.status = Some("Waiting".into());
        prompt.buttons = vec![
            Button::new("Open browser", Role::Open),
            Button::new("Copy code", Role::Copy),
            Button::new("Quit", Role::Quit),
        ];
        prompt
    }

    fn picker(entries: usize) -> Prompt {
        let mut prompt = Prompt::new(Tone::Neutral, "Choose", "More than one.");
        prompt.list = (0..entries).map(|i| (format!("Xbox {i}"), "Series S".to_owned())).collect();
        prompt.buttons = vec![Button::new("Sign out", Role::Do(Action::SignOut)), Button::new("Quit", Role::Quit)];
        prompt
    }

    fn failure() -> Prompt {
        let mut prompt = Prompt::new(Tone::Error, "Could not connect", &"long explanation ".repeat(20));
        prompt.buttons = (0..4).map(|_| Button::new("Button", Role::Quit)).collect();
        prompt
    }

    #[test]
    fn parts_stack_without_overlap_and_stay_on_screen() {
        for (w, h) in [(1280.0, 720.0), (1920.0, 1080.0), (1280.0, 800.0)] {
            for prompt in [sign_in(), picker(4), failure()] {
                let l = layout(w, h, &prompt);
                let mut column: Vec<Rect> = Vec::new();
                column.extend(l.icon);
                column.push(l.title);
                column.push(l.body);
                column.extend(l.code);
                column.extend(l.qr);
                column.extend(l.status);
                column.extend(l.list.iter().copied());
                column.extend(l.buttons.first().copied());
                for pair in column.windows(2) {
                    assert!(pair[1].top >= pair[0].bottom - 0.01, "{} overlaps", prompt.title);
                }
                assert!(column[0].top >= 0.0 && column.last().unwrap().bottom <= h, "{} fits", prompt.title);
                for button in &l.buttons {
                    assert!(button.height() >= 60.0 && button.width() >= 150.0);
                    assert!(button.left >= 0.0 && button.right <= w);
                }
                for pair in l.buttons.windows(2) {
                    assert!(pair[0].right < pair[1].left);
                }
                let (first, last) = (l.buttons[0], l.buttons[l.buttons.len() - 1]);
                assert!(((first.left + last.right) / 2.0 - w / 2.0).abs() < 0.5, "buttons are centred");
            }
        }
    }

    #[test]
    fn list_entries_come_before_buttons_as_targets() {
        let prompt = picker(2);
        let l = layout(1280.0, 720.0, &prompt);
        let middle = |r: &Rect| ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
        let (x, y) = middle(&l.list[1]);
        assert_eq!(hit(&l, x, y), Some(1));
        let (x, y) = middle(&l.buttons[0]);
        assert_eq!(hit(&l, x, y), Some(2));
        assert_eq!(hit(&l, 2.0, 2.0), None);
        assert_eq!(prompt.targets(), 4);
    }

    #[test]
    fn longer_text_reserves_more_lines() {
        assert_eq!(estimated_lines("short", 600.0, 19.0), 1.0);
        assert!(estimated_lines(&"word ".repeat(60), 600.0, 19.0) >= 4.0);
        assert_eq!(estimated_lines("one\ntwo", 600.0, 19.0), 2.0);
    }
}
