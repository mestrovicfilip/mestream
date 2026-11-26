//! Controller input via XInput, polled on its own thread at ~500 Hz so a
//! button press leaves for the console within a couple of milliseconds
//! instead of waiting for the next video frame.
//!
//! The same thread drives the overlay: while a prompt or the menu is up the
//! controller moves the focus there and the game sees an untouched pad.

use crate::input::{GamepadState, InputEvent, button};
use crate::overlay::{self, Nav};
use crate::settings;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD_BUTTON_FLAGS, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState,
};

const NO_PAD: u32 = u32::MAX;
static ACTIVE_PAD: AtomicU32 = AtomicU32::new(NO_PAD);
static RUMBLE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Holding both sticks down this long opens the settings menu.
const MENU_HOLD: Duration = Duration::from_millis(600);
const MENU_COMBO: u16 = button::LEFT_THUMB | button::RIGHT_THUMB;
/// A held direction repeats after this pause, then at this pace.
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(120);
/// How far the stick must be pushed to count as a direction.
const STICK_THRESHOLD: i16 = 20_000;

/// XInput button bit -> wire button bit.
const BUTTON_MAP: [(u16, u16); 14] = [
    (0x0001, button::DPAD_UP),
    (0x0002, button::DPAD_DOWN),
    (0x0004, button::DPAD_LEFT),
    (0x0008, button::DPAD_RIGHT),
    (0x0010, button::MENU),
    (0x0020, button::VIEW),
    (0x0040, button::LEFT_THUMB),
    (0x0080, button::RIGHT_THUMB),
    (0x0100, button::LEFT_SHOULDER),
    (0x0200, button::RIGHT_SHOULDER),
    (0x1000, button::A),
    (0x2000, button::B),
    (0x4000, button::X),
    (0x8000, button::Y),
];

pub fn convert(state: &XINPUT_STATE) -> GamepadState {
    let pad = &state.Gamepad;
    let XINPUT_GAMEPAD_BUTTON_FLAGS(raw) = pad.wButtons;
    let mut buttons = 0u16;
    for (xinput, wire) in BUTTON_MAP {
        if raw & xinput != 0 {
            buttons |= wire;
        }
    }
    // Windows keeps the Xbox button for itself, so View + Menu together act as it.
    let combo = button::VIEW | button::MENU;
    if buttons & combo == combo {
        buttons = (buttons & !combo) | button::NEXUS;
    }
    let axis = |v: i16| v.max(-32767);
    GamepadState {
        index: 0,
        buttons,
        left_x: axis(pad.sThumbLX),
        left_y: axis(pad.sThumbLY),
        right_x: axis(pad.sThumbRX),
        right_y: axis(pad.sThumbRY),
        left_trigger: u16::from(pad.bLeftTrigger) * 257,
        right_trigger: u16::from(pad.bRightTrigger) * 257,
    }
}

/// The direction the d-pad or left stick is held in, if any.
fn direction(pad: &GamepadState) -> Option<Nav> {
    let pressed = |bit: u16| pad.buttons & bit != 0;
    if pressed(button::DPAD_UP) || pad.left_y > STICK_THRESHOLD {
        Some(Nav::Up)
    } else if pressed(button::DPAD_DOWN) || pad.left_y < -STICK_THRESHOLD {
        Some(Nav::Down)
    } else if pressed(button::DPAD_LEFT) || pad.left_x < -STICK_THRESHOLD {
        Some(Nav::Left)
    } else if pressed(button::DPAD_RIGHT) || pad.left_x > STICK_THRESHOLD {
        Some(Nav::Right)
    } else {
        None
    }
}

/// Turns controller samples into overlay navigation: one step per button
/// press, and a repeating step while a direction is held.
#[derive(Default)]
struct Navigator {
    previous_buttons: u16,
    held: Option<(Nav, Instant)>,
}

impl Navigator {
    /// The steps this sample produces. Call for every sample, also while the
    /// overlay is not listening, so a button already down is not mistaken
    /// for a new press when it starts to.
    fn steps(&mut self, pad: &GamepadState, now: Instant) -> Vec<Nav> {
        let mut steps = Vec::new();
        let newly_pressed = pad.buttons & !self.previous_buttons;
        self.previous_buttons = pad.buttons;
        for (bit, nav) in [
            (button::A, Nav::Accept),
            (button::B, Nav::Back),
            (button::LEFT_SHOULDER, Nav::PreviousTab),
            (button::RIGHT_SHOULDER, Nav::NextTab),
        ] {
            if newly_pressed & bit != 0 {
                steps.push(nav);
            }
        }
        match (direction(pad), self.held) {
            (Some(nav), Some((held, next))) if nav == held => {
                if now >= next {
                    steps.push(nav);
                    self.held = Some((nav, now + REPEAT_INTERVAL));
                }
            }
            (Some(nav), _) => {
                steps.push(nav);
                self.held = Some((nav, now + REPEAT_DELAY));
            }
            (None, _) => self.held = None,
        }
        steps
    }
}

/// Decides, sample by sample, what the game gets to see.
#[derive(Default)]
struct GameFilter {
    combo_since: Option<Instant>,
    /// After the overlay closes, buttons still down from operating it must
    /// not reach the game: wait until everything has been let go.
    waiting_for_release: bool,
}

impl GameFilter {
    /// The pad state to send to the console. Opens the menu when the combo
    /// has been held long enough.
    fn filter(&mut self, pad: GamepadState, overlay_active: bool, now: Instant) -> GamepadState {
        if overlay_active {
            self.waiting_for_release = true;
            self.combo_since = None;
            return GamepadState::default();
        }
        if self.waiting_for_release {
            if pad.buttons != 0 {
                return GamepadState::default();
            }
            self.waiting_for_release = false;
        }
        if pad.buttons & MENU_COMBO == MENU_COMBO {
            let since = *self.combo_since.get_or_insert(now);
            if now.duration_since(since) >= MENU_HOLD {
                overlay::open_menu();
                self.waiting_for_release = true;
                return GamepadState::default();
            }
        } else {
            self.combo_since = None;
        }
        pad
    }
}

fn find_controller() -> Option<u32> {
    (0..4).find(|&index| {
        let mut state = XINPUT_STATE::default();
        unsafe { XInputGetState(index, &mut state) == 0 }
    })
}

/// Polls the first connected controller until `quit` is set or the receiver
/// is gone. Runs for as long as the window is open; pad states are only
/// sent on while a stream is there to take them.
pub fn run(events: UnboundedSender<InputEvent>, quit: Arc<AtomicBool>) {
    unsafe { timeBeginPeriod(1) };
    let mut last_sent: Option<GamepadState> = None;
    let mut last_send_time = Instant::now();
    let mut last_scan: Option<Instant> = None;
    let mut navigator = Navigator::default();
    let mut filter = GameFilter::default();

    while !quit.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(2));
        let mut index = ACTIVE_PAD.load(Ordering::Relaxed);
        if index == NO_PAD && last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            last_scan = Some(Instant::now());
            if let Some(found) = find_controller() {
                crate::status!("[gamepad] using controller {found}");
                ACTIVE_PAD.store(found, Ordering::Relaxed);
                index = found;
            }
        }
        if index == NO_PAD {
            continue;
        }

        let mut state = XINPUT_STATE::default();
        if unsafe { XInputGetState(index, &mut state) } != 0 {
            crate::status!("[gamepad] controller {index} disconnected");
            ACTIVE_PAD.store(NO_PAD, Ordering::Relaxed);
            last_sent = None;
            continue;
        }
        let pad = convert(&state);
        let now = Instant::now();
        let overlay_active = overlay::wants_input();
        let steps = navigator.steps(&pad, now);
        if overlay_active {
            steps.into_iter().for_each(overlay::navigate);
        }

        if !overlay::streaming() {
            last_sent = None;
            continue;
        }
        let for_game = filter.filter(pad, overlay_active, now);
        // Send on every change, plus a slow heartbeat so a lost packet self-heals.
        if last_sent != Some(for_game) || last_send_time.elapsed() > Duration::from_millis(50) {
            if events.send(InputEvent::Pad(for_game)).is_err() {
                break;
            }
            last_sent = Some(for_game);
            last_send_time = now;
        }
    }
    set_motors(0, 0);
    unsafe { timeEndPeriod(1) };
}

fn set_motors(left: u16, right: u16) {
    let index = ACTIVE_PAD.load(Ordering::Relaxed);
    if index != NO_PAD {
        let vibration = XINPUT_VIBRATION { wLeftMotorSpeed: left, wRightMotorSpeed: right };
        unsafe { XInputSetState(index, &vibration) };
    }
}

/// Applies a rumble report from the console. Must be called from the tokio runtime.
pub fn rumble(left_percent: u8, right_percent: u8, duration_ms: u16) {
    let generation = RUMBLE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if !settings::get().rumble {
        set_motors(0, 0);
        return;
    }
    let scale = |p: u8| (u32::from(p.min(100)) * 65535 / 100) as u16;
    set_motors(scale(left_percent), scale(right_percent));
    if (left_percent > 0 || right_percent > 0) && duration_ms > 0 {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(u64::from(duration_ms))).await;
            if RUMBLE_GENERATION.load(Ordering::Relaxed) == generation {
                set_motors(0, 0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::XboxController::XINPUT_GAMEPAD;

    fn state(buttons: u16) -> XINPUT_STATE {
        XINPUT_STATE {
            dwPacketNumber: 0,
            Gamepad: XINPUT_GAMEPAD {
                wButtons: XINPUT_GAMEPAD_BUTTON_FLAGS(buttons),
                bLeftTrigger: 255,
                bRightTrigger: 0,
                sThumbLX: i16::MIN,
                sThumbLY: 100,
                sThumbRX: 0,
                sThumbRY: 0,
            },
        }
    }

    fn pressing(buttons: u16) -> GamepadState {
        GamepadState { buttons, ..Default::default() }
    }

    #[test]
    fn maps_buttons_axes_and_triggers() {
        let pad = convert(&state(0x1000 | 0x0001)); // A + d-pad up
        assert_eq!(pad.buttons, button::A | button::DPAD_UP);
        assert_eq!(pad.left_x, -32767);
        assert_eq!(pad.left_y, 100);
        assert_eq!(pad.left_trigger, 65535);
        assert_eq!(pad.right_trigger, 0);
    }

    #[test]
    fn view_plus_menu_becomes_xbox_button() {
        let pad = convert(&state(0x0010 | 0x0020 | 0x2000)); // Menu + View + B
        assert_eq!(pad.buttons, button::NEXUS | button::B);
    }

    #[test]
    fn a_button_steps_once_and_a_held_direction_repeats() {
        let mut navigator = Navigator::default();
        let start = Instant::now();
        let held = pressing(button::A | button::DPAD_DOWN);
        assert_eq!(navigator.steps(&held, start), vec![Nav::Accept, Nav::Down]);
        assert!(navigator.steps(&held, start + Duration::from_millis(100)).is_empty());
        assert_eq!(navigator.steps(&held, start + REPEAT_DELAY), vec![Nav::Down]);
        assert!(navigator.steps(&held, start + REPEAT_DELAY + Duration::from_millis(50)).is_empty());
        assert_eq!(navigator.steps(&held, start + REPEAT_DELAY + REPEAT_INTERVAL), vec![Nav::Down]);
        assert!(navigator.steps(&pressing(0), start + Duration::from_secs(2)).is_empty());
        assert_eq!(navigator.steps(&pressing(button::B), start + Duration::from_secs(3)), vec![Nav::Back]);
    }

    #[test]
    fn the_stick_counts_as_a_direction_only_when_pushed_far() {
        let nudge = GamepadState { left_x: 5_000, ..Default::default() };
        assert_eq!(direction(&nudge), None);
        let push = GamepadState { left_x: -30_000, ..Default::default() };
        assert_eq!(direction(&push), Some(Nav::Left));
    }

    #[test]
    fn the_game_sees_nothing_while_the_overlay_is_up_or_buttons_linger() {
        let mut filter = GameFilter::default();
        let now = Instant::now();
        let pad = pressing(button::A);
        assert_eq!(filter.filter(pad, false, now), pad);
        assert_eq!(filter.filter(pad, true, now), GamepadState::default());
        // The overlay closed with A still down: that A is not for the game.
        assert_eq!(filter.filter(pad, false, now), GamepadState::default());
        assert_eq!(filter.filter(pressing(0), false, now), GamepadState::default());
        assert_eq!(filter.filter(pad, false, now), pad);
    }

    #[test]
    fn a_short_press_of_both_sticks_still_reaches_the_game() {
        let mut filter = GameFilter::default();
        let now = Instant::now();
        let pad = pressing(MENU_COMBO);
        assert_eq!(filter.filter(pad, false, now), pad);
        assert_eq!(filter.filter(pad, false, now + MENU_HOLD / 2), pad);
        assert_eq!(filter.filter(pressing(0), false, now + MENU_HOLD), pressing(0));
        // Pressed again later, the hold is timed from the new press.
        assert_eq!(filter.filter(pad, false, now + MENU_HOLD * 3), pad);
    }
}
