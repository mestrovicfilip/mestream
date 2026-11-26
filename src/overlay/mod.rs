//! Everything drawn on top of the stream, and what the user does with it:
//!  * the start-up screen: a loader with a dim, terminal-style log underneath;
//!  * prompts: signing in, choosing a console, failures;
//!  * the settings menu, which slides in from the right over the picture.
//!
//! Taps, keys and the controller all end up here as the same few calls
//! (`tap`, `navigate`). Whatever cannot be settled on the spot is queued as
//! an `Action` for the connection code to pick up with `take_action`.

mod geometry;
mod menu;
mod paint;
mod prompt;
mod system;

pub use paint::{Painter, Scene};
pub use prompt::{Action, Button, Prompt, Role, Style, Tone};

use crate::settings;
use crate::text::text;
use menu::{Hit, Item, Kind, TABS};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// A press that needs confirming stays armed this long.
const CONFIRM_SECONDS: f32 = 4.0;
const LOG_CAPACITY: usize = 32;

/// Something that was pressed once and acts on the second press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Armed {
    MenuItem(Item),
    PromptButton(usize),
}

/// A QR code as its side length and one flag per module, row by row.
#[derive(Clone)]
struct QrCode {
    side: usize,
    dark: Vec<bool>,
}

struct State {
    menu_open: bool,
    /// When the menu was last opened or closed, for the slide animation.
    toggled_at: Option<Instant>,
    tab: usize,
    /// Which control the controller or keyboard is on.
    focus: usize,
    /// The focus ring is shown only to someone actually using keys or a
    /// controller; a tap hides it again.
    focus_shown: bool,
    armed: Option<(Armed, Instant)>,
    /// Resolution and bitrate when the menu opened: changing either means
    /// the stream has to be opened again once the menu closes.
    stream_shape: Option<(u32, u32)>,
    prompt: Option<Prompt>,
    prompt_qr: Option<QrCode>,
    copied_at: Option<Instant>,
    log: VecDeque<(Instant, String)>,
    /// Connecting again: show the start-up screen instead of the last picture.
    reconnecting: bool,
    /// A stream is running, so the menu may be opened.
    streaming: bool,
    actions: VecDeque<Action>,
    quit: bool,
    /// The line shown in the corner when statistics are switched on.
    stats: String,
    console_name: String,
    /// Size of the surface we draw on, which a resized window need not match.
    surface: (f32, f32),
    /// Something a press asked of Windows, to be done once the lock is released.
    pending: Option<Effect>,
}

/// Work for Windows that a press sets off. It is never done while the state
/// is locked: opening the browser makes Windows deliver messages to our
/// window, whose handler needs the same lock and would wait for it forever.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    Copy(String),
    OpenLink(String),
}

impl Effect {
    fn run(self) {
        match self {
            Effect::Copy(text) => match system::copy_to_clipboard(&text) {
                Ok(()) => STATE.lock().unwrap().copied_at = Some(Instant::now()),
                Err(e) => eprintln!("[overlay] could not copy to the clipboard: {e:#}"),
            },
            // On a thread of its own, so a slow browser start cannot hold up the picture.
            Effect::OpenLink(link) => {
                std::thread::spawn(move || {
                    if let Err(e) = system::open_in_browser(&link) {
                        eprintln!("[overlay] {e:#}");
                    }
                });
            }
        }
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

impl State {
    const fn new() -> Self {
        Self {
            menu_open: false,
            toggled_at: None,
            tab: 0,
            focus: 0,
            focus_shown: false,
            armed: None,
            stream_shape: None,
            prompt: None,
            prompt_qr: None,
            copied_at: None,
            log: VecDeque::new(),
            reconnecting: false,
            streaming: false,
            actions: VecDeque::new(),
            quit: false,
            stats: String::new(),
            console_name: String::new(),
            surface: (1280.0, 720.0),
            pending: None,
        }
    }

    fn is_armed(&self, what: Armed) -> bool {
        self.armed
            .is_some_and(|(armed, at)| armed == what && at.elapsed().as_secs_f32() < CONFIRM_SECONDS)
    }

    /// True when the press should act now; otherwise it has just been armed.
    fn confirm(&mut self, what: Armed) -> bool {
        if self.is_armed(what) {
            self.armed = None;
            true
        } else {
            self.armed = Some((what, Instant::now()));
            false
        }
    }

    fn open_menu(&mut self) {
        if self.menu_open || !self.streaming || self.prompt.is_some() {
            return;
        }
        let now = settings::get();
        self.stream_shape = Some((now.resolution, now.bitrate_kbps));
        self.menu_open = true;
        self.toggled_at = Some(Instant::now());
        self.focus = 0;
        self.armed = None;
    }

    fn close_menu(&mut self) {
        if !self.menu_open {
            return;
        }
        self.menu_open = false;
        self.toggled_at = Some(Instant::now());
        self.armed = None;
        let now = settings::get();
        if self.stream_shape.take().is_some_and(|before| before != (now.resolution, now.bitrate_kbps)) {
            self.actions.push_back(Action::Reconnect);
        }
    }

    /// Number of things the focus can be on in the menu: the rows, then the
    /// two buttons at the bottom.
    fn menu_targets(&self) -> usize {
        TABS[self.tab].items.len() + 2
    }

    fn press_menu(&mut self, hit: Hit) {
        match hit {
            Hit::Adjust(item, direction) => settings::update(|s| item.nudge(s, direction)),
            Hit::Choose(item, option) => settings::update(|s| item.choose(s, option)),
            Hit::Act(item) => {
                let needs_confirming = matches!(item.kind(), Kind::Action { confirm: true });
                if needs_confirming && !self.confirm(Armed::MenuItem(item)) {
                    return;
                }
                let action = match item {
                    Item::SwitchConsole => Action::SwitchConsole,
                    Item::RestartConsole => Action::RestartConsole,
                    Item::SignOut => Action::SignOut,
                    _ => return,
                };
                // These replace the stream, so a pending "open it again" is moot.
                self.stream_shape = None;
                self.close_menu();
                self.actions.push_back(action);
            }
            Hit::Tab(tab) => {
                self.tab = tab;
                self.focus = 0;
                self.armed = None;
            }
            Hit::Reset => {
                let items = TABS[self.tab].items;
                settings::update(|s| items.iter().for_each(|item| item.reset(s)));
            }
            Hit::Quit => self.quit = true,
            Hit::Close | Hit::Outside => self.close_menu(),
            Hit::Panel => {}
        }
    }

    fn press_prompt(&mut self, target: usize) {
        // A copy, because acting on the press changes the rest of the state.
        let Some(prompt) = self.prompt.clone() else { return };
        if target < prompt.list.len() {
            self.actions.push_back(Action::Pick(target));
            return;
        }
        let index = target - prompt.list.len();
        let Some(button) = prompt.buttons.get(index).cloned() else { return };
        if button.confirm && !self.confirm(Armed::PromptButton(index)) {
            return;
        }
        match button.role {
            Role::Do(action) => self.actions.push_back(action),
            Role::Quit => self.quit = true,
            Role::Copy => self.pending = Some(Effect::Copy(prompt.copy_text)),
            Role::Open => self.pending = prompt.link.map(Effect::OpenLink),
        }
    }
}

// ---------------------------------------------------------------- the log

/// Adds a line to the start-up screen's log.
pub fn log(line: &str) {
    let mut state = STATE.lock().unwrap();
    if state.log.len() == LOG_CAPACITY {
        state.log.pop_front();
    }
    state.log.push_back((Instant::now(), line.to_owned()));
}

/// True for text shaped like an IPv4 or IPv6 address.
fn looks_like_address(token: &str) -> bool {
    let v4: Vec<&str> = token.split('.').collect();
    let is_v4 = v4.len() == 4 && v4.iter().all(|part| part.parse::<u8>().is_ok());
    let is_v6 = token.matches(':').count() >= 2 && token.chars().any(|c| c.is_ascii_hexdigit());
    is_v4 || is_v6
}

/// Replaces network addresses with a placeholder. Reports get pasted into
/// public issue trackers, and an address says where someone's console is.
fn without_addresses(text: &str) -> String {
    let in_address = |c: char| c.is_ascii_hexdigit() || c == '.' || c == ':';
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let run = rest.find(|c| !in_address(c)).unwrap_or(rest.len());
        if run == 0 {
            let c = rest.chars().next().expect("rest is not empty");
            clean.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        // Punctuation after an address ("at 10.0.0.2:") is not part of it.
        let token = &rest[..run];
        let core = token.trim_end_matches(['.', ':']);
        if looks_like_address(core) {
            clean.push_str("<address>");
            clean.push_str(&token[core.len()..]);
        } else {
            clean.push_str(token);
        }
        rest = &rest[run..];
    }
    clean
}

/// A failure and the steps that led to it, as text to paste elsewhere.
pub fn report(error: &str) -> String {
    let t = text();
    let state = STATE.lock().unwrap();
    let mut report = format!(
        "Mestream {} - {}:\n{error}\n\n{}:\n",
        env!("CARGO_PKG_VERSION"),
        t.report_error,
        t.report_log
    );
    for (_, line) in &state.log {
        report.push_str(line);
        report.push('\n');
    }
    without_addresses(&report)
}

// ---------------------------------------------------------------- prompts

fn qr_code(link: &str) -> Option<QrCode> {
    let code = qrcode::QrCode::new(link.as_bytes()).ok()?;
    let dark = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Some(QrCode { side: code.width(), dark })
}

/// Shows a prompt in place of the start-up screen until it is cleared.
pub fn show_prompt(prompt: Prompt) {
    let qr = prompt.link.as_deref().and_then(qr_code);
    let mut state = STATE.lock().unwrap();
    state.close_menu();
    state.prompt_qr = qr;
    state.focus = 0;
    state.armed = None;
    state.copied_at = None;
    state.reconnecting = false;
    state.actions.clear();
    state.prompt = Some(prompt);
}

pub fn prompt_shown() -> bool {
    STATE.lock().unwrap().prompt.is_some()
}

/// Leaves whatever prompt is up and shows the start-up screen again.
pub fn begin_reconnect() {
    let mut state = STATE.lock().unwrap();
    state.prompt = None;
    state.prompt_qr = None;
    state.close_menu();
    state.actions.clear();
    state.reconnecting = true;
}

pub fn reconnecting() -> bool {
    STATE.lock().unwrap().reconnecting
}

/// Called when video is on screen again.
pub fn end_reconnect() {
    STATE.lock().unwrap().reconnecting = false;
}

// ------------------------------------------------- talking to the app code

/// The next thing the user asked for that the connection code must do.
pub fn take_action() -> Option<Action> {
    STATE.lock().unwrap().actions.pop_front()
}

/// True once the user has asked to leave Mestream.
pub fn quit_requested() -> bool {
    STATE.lock().unwrap().quit
}

/// Tells the overlay whether a stream is running (the menu needs one).
pub fn set_streaming(streaming: bool) {
    let mut state = STATE.lock().unwrap();
    state.streaming = streaming;
    if !streaming {
        state.stats.clear();
        // Nothing left to apply a pending change to.
        state.stream_shape = None;
        state.close_menu();
    }
}

pub fn streaming() -> bool {
    STATE.lock().unwrap().streaming
}

pub fn set_stats(line: String) {
    STATE.lock().unwrap().stats = line;
}

pub fn set_console_name(name: &str) {
    STATE.lock().unwrap().console_name = name.to_owned();
}

pub fn set_surface(width: f32, height: f32) {
    STATE.lock().unwrap().surface = (width, height);
}

// ------------------------------------------------------------------- input

/// A step of keyboard or controller navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    PreviousTab,
    NextTab,
}

/// True while a prompt or the menu is up, so input belongs to the overlay
/// and must not reach the game.
pub fn wants_input() -> bool {
    let state = STATE.lock().unwrap();
    state.prompt.is_some() || state.menu_open
}

/// Opens the settings menu (ignored when there is no stream to adjust).
pub fn open_menu() {
    let mut state = STATE.lock().unwrap();
    state.open_menu();
    state.focus_shown = true;
}

/// Handles a tap or click at a point of a window whose client area has the
/// given size.
pub fn tap(x: f32, y: f32, client_width: f32, client_height: f32) {
    let mut state = STATE.lock().unwrap();
    let (width, height) = state.surface;
    let (x, y) = (x * width / client_width.max(1.0), y * height / client_height.max(1.0));
    state.focus_shown = false;
    if let Some(prompt) = state.prompt.as_ref() {
        let target = prompt::hit(&prompt::layout(width, height, prompt), x, y);
        if let Some(target) = target {
            state.press_prompt(target);
        }
    } else if state.menu_open {
        let hit = menu::hit(&menu::layout(width, height, state.tab), x, y);
        state.press_menu(hit);
    } else {
        state.open_menu();
    }
    finish_press(state);
}

/// Releases the state, then does whatever the press asked of Windows.
fn finish_press(mut state: std::sync::MutexGuard<'_, State>) {
    let effect = state.pending.take();
    drop(state);
    if let Some(effect) = effect {
        effect.run();
    }
}

/// Moves the focus, or acts on the control it is on.
pub fn navigate(nav: Nav) {
    let mut state = STATE.lock().unwrap();
    // The first press only reveals where the focus is.
    let revealing = !state.focus_shown;
    state.focus_shown = true;
    if state.prompt.is_some() {
        navigate_prompt(&mut state, nav, revealing);
    } else if state.menu_open {
        navigate_menu(&mut state, nav, revealing);
    }
    finish_press(state);
}

fn step(index: usize, count: usize, forward: bool) -> usize {
    if count == 0 {
        0
    } else if forward {
        (index + 1) % count
    } else {
        (index + count - 1) % count
    }
}

fn navigate_prompt(state: &mut State, nav: Nav, revealing: bool) {
    let targets = state.prompt.as_ref().map_or(0, Prompt::targets);
    match nav {
        Nav::Up | Nav::Left if !revealing => state.focus = step(state.focus, targets, false),
        Nav::Down | Nav::Right if !revealing => state.focus = step(state.focus, targets, true),
        Nav::Accept if !revealing && state.focus < targets => {
            let focus = state.focus;
            state.press_prompt(focus);
        }
        _ => {}
    }
}

fn navigate_menu(state: &mut State, nav: Nav, revealing: bool) {
    let items = TABS[state.tab].items;
    let targets = state.menu_targets();
    let focused_item = items.get(state.focus).copied();
    let footer = [Hit::Reset, Hit::Quit];
    match nav {
        Nav::Back => state.close_menu(),
        Nav::PreviousTab | Nav::NextTab => {
            let tab = step(state.tab, TABS.len(), nav == Nav::NextTab);
            state.press_menu(Hit::Tab(tab));
        }
        _ if revealing => {}
        Nav::Up => state.focus = step(state.focus, targets, false),
        Nav::Down => state.focus = step(state.focus, targets, true),
        Nav::Left | Nav::Right => {
            let direction = if nav == Nav::Right { 1 } else { -1 };
            match focused_item {
                Some(item) if !matches!(item.kind(), Kind::Action { .. }) => {
                    state.press_menu(Hit::Adjust(item, direction));
                }
                Some(_) => {}
                // Between the two buttons at the bottom.
                None => state.focus = items.len() + usize::from(nav == Nav::Right),
            }
        }
        Nav::Accept => match focused_item {
            Some(item) => match item.kind() {
                Kind::Action { .. } => state.press_menu(Hit::Act(item)),
                Kind::Choice(_) => {
                    let other = 1 - item.choice(&settings::get());
                    state.press_menu(Hit::Choose(item, other));
                }
                Kind::Slider { .. } => {}
            },
            None => state.press_menu(footer[(state.focus - items.len()).min(1)]),
        },
    }
}

// ----------------------------------------------------------------- preview

/// Puts the overlay in a given state without a stream, for looking at it
/// with `mestream decode-file`.
pub fn preview_menu(tab: usize, focus: Option<usize>) {
    let mut state = STATE.lock().unwrap();
    state.streaming = true;
    state.open_menu();
    state.tab = tab.min(TABS.len() - 1);
    state.toggled_at = None;
    state.console_name = "XBOX".to_owned();
    if let Some(focus) = focus {
        state.focus = focus;
        state.focus_shown = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressing_a_button_leaves_work_for_windows_until_the_lock_is_released() {
        let mut prompt = Prompt::new(Tone::Neutral, "Sign in", "");
        prompt.link = Some("https://www.microsoft.com/link?otc=ABCD1234".to_owned());
        prompt.copy_text = "ABCD1234".to_owned();
        prompt.buttons = vec![Button::new("Open browser", Role::Open), Button::new("Copy code", Role::Copy)];
        let mut state = State::new();
        state.prompt = Some(prompt);

        state.press_prompt(0);
        assert_eq!(state.pending, Some(Effect::OpenLink("https://www.microsoft.com/link?otc=ABCD1234".to_owned())));
        state.press_prompt(1);
        assert_eq!(state.pending, Some(Effect::Copy("ABCD1234".to_owned())));
        assert!(state.copied_at.is_none(), "nothing has been copied yet");
    }

    #[test]
    fn stepping_wraps_around() {
        assert_eq!(step(0, 3, false), 2);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(1, 3, true), 2);
        assert_eq!(step(0, 0, true), 0);
    }

    #[test]
    fn reports_carry_no_network_addresses() {
        let line = "ice: HTTP 500: candidate 192.168.1.20 3074 and 2001:0:abcd:1234:5678:9abc:def0:1 at 10.0.0.2:";
        assert_eq!(
            without_addresses(line),
            "ice: HTTP 500: candidate <address> 3074 and <address> at <address>:"
        );
        let plain = "[session] state: Provisioned (6929 ms), 4.5 Mbit/s, version 0.2.0 - ok";
        assert_eq!(without_addresses(plain), plain);
    }

    #[test]
    fn a_qr_code_is_square() {
        let code = qr_code("https://www.microsoft.com/link?otc=ABCD1234").unwrap();
        assert_eq!(code.dark.len(), code.side * code.side);
        assert!(code.side >= 21);
    }
}
