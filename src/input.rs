//! Binary packets for the `input` data channel (protocol "1.0").
//!
//! Layout, all little-endian unless noted:
//!   u16 report type bitmask | u32 sequence | f64 client time (ms)
//!   [metadata section] [gamepad section] ...   (in that order, if flagged)

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds on the client clock all input packets and frame timings share.
pub fn instant_ms(instant: Instant) -> f64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    instant.saturating_duration_since(epoch).as_secs_f64() * 1000.0
}

pub fn now_ms() -> f64 {
    instant_ms(Instant::now())
}

/// Something to put in the next input report.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    Pad(GamepadState),
    Frame(FrameTiming),
}

pub const REPORT_METADATA: u16 = 1;
pub const REPORT_GAMEPAD: u16 = 2;
pub const REPORT_CLIENT_METADATA: u16 = 8;
pub const REPORT_SERVER_METADATA: u16 = 16;
pub const REPORT_VIBRATION: u16 = 128;

pub mod button {
    pub const NEXUS: u16 = 0x0002;
    pub const MENU: u16 = 0x0004;
    pub const VIEW: u16 = 0x0008;
    pub const A: u16 = 0x0010;
    pub const B: u16 = 0x0020;
    pub const X: u16 = 0x0040;
    pub const Y: u16 = 0x0080;
    pub const DPAD_UP: u16 = 0x0100;
    pub const DPAD_DOWN: u16 = 0x0200;
    pub const DPAD_LEFT: u16 = 0x0400;
    pub const DPAD_RIGHT: u16 = 0x0800;
    pub const LEFT_SHOULDER: u16 = 0x1000;
    pub const RIGHT_SHOULDER: u16 = 0x2000;
    pub const LEFT_THUMB: u16 = 0x4000;
    pub const RIGHT_THUMB: u16 = 0x8000;
}

/// One controller sample. Stick axes use the XInput convention (up/right positive).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
    pub index: u8,
    pub buttons: u16,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u16,
    pub right_trigger: u16,
}

/// Per-video-frame timing report the console uses to adapt the stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTiming {
    pub server_data_key: u32,
    pub first_packet_arrival_ms: u32,
    pub submitted_ms: u32,
    pub decoded_ms: u32,
    pub rendered_ms: u32,
}

fn header(report_type: u16, sequence: u32, now_ms: f64, capacity: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(capacity);
    p.extend_from_slice(&report_type.to_le_bytes());
    p.extend_from_slice(&sequence.to_le_bytes());
    p.extend_from_slice(&now_ms.to_le_bytes());
    p
}

/// First packet on the input channel after the handshake.
pub fn client_metadata(now_ms: f64, max_touch_points: u8) -> Vec<u8> {
    let mut p = header(REPORT_CLIENT_METADATA, 0, now_ms, 15);
    p.push(max_touch_points);
    p
}

/// Regular input report. Either slice may be empty, but not both.
pub fn report(sequence: u32, now_ms: f64, timings: &[FrameTiming], pads: &[GamepadState]) -> Vec<u8> {
    let mut kind = 0;
    if !timings.is_empty() {
        kind |= REPORT_METADATA;
    }
    if !pads.is_empty() {
        kind |= REPORT_GAMEPAD;
    }
    // Each section starts with a one-byte count; if more has queued up than
    // that can say, the newest entries are the ones worth sending.
    let timings = &timings[timings.len().saturating_sub(255)..];
    let pads = &pads[pads.len().saturating_sub(255)..];
    let mut p = header(kind, sequence, now_ms, 16 + timings.len() * 28 + pads.len() * 23);

    if !timings.is_empty() {
        p.push(timings.len() as u8);
        let now = now_ms as u32;
        for t in timings {
            for v in [
                t.server_data_key,
                t.first_packet_arrival_ms,
                t.submitted_ms,
                t.decoded_ms,
                t.rendered_ms,
                now,
                now,
            ] {
                p.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    if !pads.is_empty() {
        p.push(pads.len() as u8);
        for g in pads {
            p.push(g.index);
            p.extend_from_slice(&g.buttons.to_le_bytes());
            p.extend_from_slice(&g.left_x.to_le_bytes());
            p.extend_from_slice(&g.left_y.to_le_bytes());
            p.extend_from_slice(&g.right_x.to_le_bytes());
            p.extend_from_slice(&g.right_y.to_le_bytes());
            p.extend_from_slice(&g.left_trigger.to_le_bytes());
            p.extend_from_slice(&g.right_trigger.to_le_bytes());
            p.extend_from_slice(&1u32.to_le_bytes()); // physical physicality
            p.extend_from_slice(&1u32.to_be_bytes()); // virtual physicality (big-endian on the wire)
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_metadata_layout() {
        let p = client_metadata(1.5, 1);
        assert_eq!(p.len(), 15);
        assert_eq!(&p[0..2], &[8, 0]);
        assert_eq!(&p[2..6], &[0, 0, 0, 0]);
        assert_eq!(f64::from_le_bytes(p[6..14].try_into().unwrap()), 1.5);
        assert_eq!(p[14], 1);
    }

    #[test]
    fn gamepad_report_layout() {
        let pad = GamepadState {
            index: 0,
            buttons: button::A | button::DPAD_UP,
            left_x: -32767,
            left_y: 32767,
            right_x: 1,
            right_y: -1,
            left_trigger: 65535,
            right_trigger: 0,
        };
        let p = report(7, 0.0, &[], &[pad]);
        assert_eq!(p.len(), 14 + 1 + 23);
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), REPORT_GAMEPAD);
        assert_eq!(u32::from_le_bytes(p[2..6].try_into().unwrap()), 7);
        assert_eq!(p[14], 1); // frame count
        assert_eq!(p[15], 0); // gamepad index
        assert_eq!(u16::from_le_bytes([p[16], p[17]]), 0x0110);
        assert_eq!(i16::from_le_bytes([p[18], p[19]]), -32767);
        assert_eq!(i16::from_le_bytes([p[20], p[21]]), 32767);
        assert_eq!(u16::from_le_bytes([p[26], p[27]]), 65535);
        assert_eq!(&p[30..34], &[1, 0, 0, 0]);
        assert_eq!(&p[34..38], &[0, 0, 0, 1]);
    }

    #[test]
    fn combined_report_puts_timing_first() {
        let p = report(1, 0.0, &[FrameTiming::default()], &[GamepadState::default()]);
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), REPORT_METADATA | REPORT_GAMEPAD);
        assert_eq!(p.len(), 14 + 1 + 28 + 1 + 23);
        assert_eq!(p[14], 1);
        assert_eq!(p[14 + 1 + 28], 1);
    }
}
