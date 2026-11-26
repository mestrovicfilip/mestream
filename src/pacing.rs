//! Adaptive smoothing for network jitter. Both pieces add as little delay as
//! the network allows: they grow only when it is actually uneven, and shrink
//! back when it calms down.

use std::collections::VecDeque;

/// Decides when each video picture should be shown.
///
/// Pictures are stamped by the console at a steady cadence but arrive
/// unevenly. The pacer learns the fastest network path (`offset`), measures
/// how late recent pictures were against it, and delays presentation by just
/// enough (never more than `max_delay_ms`) for most of them to be on time.
pub struct VideoPacer {
    max_delay_ms: f64,
    last_rtp: Option<u32>,
    /// Console media clock in milliseconds, unwrapped from RTP timestamps.
    media_ms: f64,
    /// Smallest recent (arrival - media) difference: the no-jitter baseline.
    offset: Option<f64>,
    lateness: VecDeque<f64>,
    delay_ms: f64,
}

const LATENESS_WINDOW: usize = 120; // two seconds at 60 fps
/// Let the baseline creep upward so clock drift cannot pin it forever.
const OFFSET_DRIFT_MS_PER_FRAME: f64 = 0.01;

impl VideoPacer {
    pub fn new(max_delay_ms: f64) -> Self {
        Self {
            max_delay_ms: max_delay_ms.max(0.0),
            last_rtp: None,
            media_ms: 0.0,
            offset: None,
            lateness: VecDeque::with_capacity(LATENESS_WINDOW),
            delay_ms: 0.0,
        }
    }

    /// Current added delay in milliseconds.
    pub fn delay_ms(&self) -> f64 {
        self.delay_ms
    }

    /// Registers a picture and returns the client time (ms) to present it at.
    pub fn schedule(&mut self, arrival_ms: f64, rtp_timestamp: u32) -> f64 {
        if let Some(last) = self.last_rtp {
            self.media_ms += f64::from(rtp_timestamp.wrapping_sub(last) as i32) / 90.0;
        }
        self.last_rtp = Some(rtp_timestamp);

        let sample = arrival_ms - self.media_ms;
        let offset = match self.offset {
            None => sample,
            Some(offset) => (offset + OFFSET_DRIFT_MS_PER_FRAME).min(sample),
        };
        self.offset = Some(offset);

        if self.lateness.len() == LATENESS_WINDOW {
            self.lateness.pop_front();
        }
        self.lateness.push_back(sample - offset);

        // Aim to have 90% of recent pictures on time; long stalls are not
        // worth waiting for and are handled by skipping to the newest picture.
        let mut sorted: Vec<f64> = self.lateness.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let target = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)].min(self.max_delay_ms);
        // Grow at once, shrink gently, so the cadence does not wobble.
        self.delay_ms = if target > self.delay_ms {
            target
        } else {
            self.delay_ms + (target - self.delay_ms) * 0.02
        };

        self.media_ms + offset + self.delay_ms
    }
}

/// Audio samples waiting for the sound card.
///
/// Whatever sits in here is delay you can hear, so the buffer keeps only a
/// small reserve: the level it should never drop below between two packets.
/// It watches the lowest level it actually reaches. Anything above the
/// reserve was never needed, and is played off slightly fast until it is
/// gone. Running empty means the reserve was too small, so it grows a step,
/// and shrinks back slowly once playback has been clean for a while.
pub struct AudioBuffer {
    samples: VecDeque<f32>,
    channels: usize,
    samples_per_ms: usize,
    reserve_ms: usize,
    min_ms: usize,
    max_ms: usize,
    /// Size of the latest packet: the level swings by this much between arrivals.
    packet_samples: usize,
    primed: bool,
    fade_in_left: usize,
    /// Lowest level seen in the current window.
    low_water: usize,
    /// Output still to be played before the window is judged.
    window_left: usize,
    /// Samples more than needed, being played off.
    surplus: usize,
    /// Position between two input frames while playing fast.
    phase: f32,
    clean_samples: usize,
    /// Everything played so far, to know when the start is behind us.
    played_samples: usize,
    /// Silence put out since the buffer ran empty, until the next packet
    /// shows whether that was a late packet or a pause.
    dry_samples: Option<usize>,
    /// Silence put out while sound was waiting and no new packet came.
    waited_samples: usize,
    /// The reserve that was last too small, and how much has been played since.
    too_small_ms: usize,
    since_too_small: usize,
    pub underruns: u64,
    /// Samples removed to catch up, by fast playback or by cutting a backlog.
    pub skipped_samples: u64,
}

const FADE_MS: usize = 5;
/// The lowest level is judged over stretches this long.
const WINDOW_MS: usize = 2000;
/// A lowest level this close to the reserve is left alone.
const TOLERANCE_MS: usize = 3;
/// How much faster than real time a surplus is played off: too little to
/// hear as a change of pitch, enough to shed 30 ms every second.
const CATCH_UP: f32 = 0.03;
/// A backlog this far above the reserve (a burst after a network stall) is
/// cut out at once instead of played off.
const BACKLOG_MS: usize = 80;
/// Running empty raises the reserve by this much...
const RESERVE_STEP_MS: usize = 10;
/// ...and this much clean playback takes one millisecond off again.
const RESERVE_DECAY_MS: usize = 10_000;
/// A reserve that just proved too small is not tried again for this long:
/// shrinking straight back to it would only produce the same gap again,
/// every couple of minutes.
const CAUTION_MS: usize = 600_000;
/// The console stops sending while there is nothing to hear. Running empty
/// for longer than this is such a pause, not a late packet, and no reason to
/// hold more in reserve.
const PAUSE_MS: usize = 150;
/// A sound shorter than the starting level (a menu blip after silence) would
/// wait for more sound that is not coming. Once nothing new has arrived for
/// this long, what is there gets played.
const LONE_SOUND_MS: usize = 40;
/// The first moments of a stream are always bumpy (the console is still
/// starting it), so running empty this early says nothing about the network.
const SETTLING_MS: usize = 5000;

impl AudioBuffer {
    pub fn new(sample_rate: usize, channels: usize, min_ms: usize, max_ms: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            channels,
            samples_per_ms: sample_rate * channels / 1000,
            reserve_ms: min_ms,
            min_ms,
            max_ms,
            packet_samples: 0,
            primed: false,
            fade_in_left: 0,
            low_water: usize::MAX,
            window_left: 0,
            surplus: 0,
            phase: 0.0,
            clean_samples: 0,
            played_samples: 0,
            dry_samples: None,
            waited_samples: 0,
            too_small_ms: 0,
            since_too_small: 0,
            underruns: 0,
            skipped_samples: 0,
        }
    }

    pub fn reserve_ms(&self) -> usize {
        self.reserve_ms
    }

    pub fn buffered_ms(&self) -> usize {
        self.samples.len() / self.samples_per_ms
    }

    fn ms(&self, milliseconds: usize) -> usize {
        milliseconds * self.samples_per_ms
    }

    /// The level playback starts from: one packet above the reserve, so the
    /// level is back at the reserve just as the next packet is due.
    fn starting_level(&self) -> usize {
        self.ms(self.reserve_ms) + self.packet_samples
    }

    fn start_window(&mut self) {
        self.low_water = usize::MAX;
        self.window_left = self.ms(WINDOW_MS);
    }

    /// Adds decoded audio. A backlog far beyond anything the reserve calls
    /// for is cut down with a crossfade.
    pub fn push(&mut self, decoded: &[f32]) {
        self.packet_samples = decoded.len();
        self.samples.extend(decoded);
        self.waited_samples = 0;
        if let Some(dry) = self.dry_samples.take() {
            let late_packet = dry <= self.ms(PAUSE_MS);
            if late_packet && self.played_samples > self.ms(SETTLING_MS) {
                self.too_small_ms = self.reserve_ms;
                self.since_too_small = 0;
                self.reserve_ms = (self.reserve_ms + RESERVE_STEP_MS).min(self.max_ms);
            }
        }
        let keep = self.starting_level();
        let fade = self.ms(FADE_MS);
        if self.samples.len() > keep + self.ms(BACKLOG_MS) && keep >= fade {
            let excess = (self.samples.len() - keep) / self.channels * self.channels;
            let buffer = self.samples.make_contiguous();
            for i in 0..fade {
                let t = (i / self.channels) as f32 / (fade / self.channels) as f32;
                buffer[i] = buffer[i] * (1.0 - t) + buffer[i + excess] * t;
            }
            self.samples.drain(fade..fade + excess);
            self.skipped_samples += excess as u64;
            self.surplus = 0;
            self.start_window();
        }
    }

    /// Fills the sound card's buffer. Outputs silence until there is enough
    /// to start from, and fades in and out around gaps so they do not click.
    pub fn pull(&mut self, out: &mut [f32]) {
        if !self.primed {
            let enough = self.packet_samples > 0 && self.samples.len() >= self.starting_level();
            let lone_sound = !self.samples.is_empty() && self.waited_samples >= self.ms(LONE_SOUND_MS);
            if !enough && !lone_sound {
                out.fill(0.0);
                if let Some(dry) = self.dry_samples.as_mut() {
                    *dry += out.len();
                }
                if !self.samples.is_empty() {
                    self.waited_samples += out.len();
                }
                return;
            }
            self.primed = true;
            self.fade_in_left = self.ms(FADE_MS);
            self.surplus = 0;
            self.start_window();
        }

        let fast = self.surplus > 0;
        let needed = if fast { self.fast_input_len(out.len()) } else { out.len() };
        if self.samples.len() < needed {
            self.play_until_empty(out);
            return;
        }
        if fast {
            self.play_fast(out);
        } else {
            for slot in out.iter_mut() {
                *slot = self.samples.pop_front().unwrap_or(0.0);
            }
        }
        self.fade_in(out);
        self.judge_level(out.len());
    }

    /// How much input playing `out_len` samples fast can touch.
    fn fast_input_len(&self, out_len: usize) -> usize {
        let frames = out_len / self.channels;
        ((frames as f32 * (1.0 + CATCH_UP)) as usize + 2) * self.channels
    }

    /// Plays a little more input than output, reading between input frames.
    fn play_fast(&mut self, out: &mut [f32]) {
        let channels = self.channels;
        let frames = out.len() / channels;
        let input = self.samples.make_contiguous();
        let mut position = self.phase;
        for frame in 0..frames {
            let index = position as usize;
            let t = position - index as f32;
            for channel in 0..channels {
                let here = input[index * channels + channel];
                let next = input[(index + 1) * channels + channel];
                out[frame * channels + channel] = here + (next - here) * t;
            }
            position += 1.0 + CATCH_UP;
        }
        let consumed = position as usize;
        self.phase = position - consumed as f32;
        self.samples.drain(..consumed * channels);
        let removed = consumed.saturating_sub(frames) * channels;
        self.skipped_samples += removed as u64;
        self.surplus = self.surplus.saturating_sub(removed);
        if self.surplus == 0 {
            self.phase = 0.0;
        }
    }

    fn fade_in(&mut self, out: &mut [f32]) {
        let fade = self.ms(FADE_MS);
        for value in out.iter_mut() {
            if self.fade_in_left == 0 {
                break;
            }
            *value *= 1.0 - self.fade_in_left as f32 / fade as f32;
            self.fade_in_left -= 1;
        }
    }

    /// Too little is left: play what there is, fading out. Whether to keep
    /// more in reserve from now on is decided when the next packet arrives.
    fn play_until_empty(&mut self, out: &mut [f32]) {
        let available = self.samples.len().min(out.len()) / self.channels * self.channels;
        let frames = (available / self.channels).max(1);
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = if i < available {
                let fade_out = 1.0 - (i / self.channels) as f32 / frames as f32;
                self.samples.pop_front().unwrap_or(0.0) * fade_out
            } else {
                0.0
            };
        }
        self.samples.clear();
        self.primed = false;
        self.surplus = 0;
        self.phase = 0.0;
        self.clean_samples = 0;
        self.underruns += 1;
        self.dry_samples = Some(out.len() - available);
    }

    /// After a clean pull: note the level, and at the end of each window
    /// decide whether more is being held than the reserve asks for.
    fn judge_level(&mut self, played: usize) {
        self.low_water = self.low_water.min(self.samples.len());
        self.played_samples += played;
        self.clean_samples += played;
        self.since_too_small = self.since_too_small.saturating_add(played);
        if self.clean_samples >= self.ms(RESERVE_DECAY_MS) {
            self.clean_samples = 0;
            let cautious = self.too_small_ms > 0 && self.since_too_small < self.ms(CAUTION_MS);
            let floor = if cautious { self.too_small_ms + RESERVE_STEP_MS / 2 } else { self.min_ms };
            self.reserve_ms = self.reserve_ms.saturating_sub(1).max(floor.min(self.reserve_ms));
        }
        self.window_left = self.window_left.saturating_sub(played);
        if self.window_left > 0 {
            return;
        }
        // A window during which a surplus was still being played off says
        // nothing about where the level settles; wait for the next one.
        if self.surplus == 0 {
            let excess = self.low_water.saturating_sub(self.ms(self.reserve_ms));
            if excess > self.ms(TOLERANCE_MS) {
                self.surplus = excess / self.channels * self.channels;
            }
        }
        self.start_window();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: f64 = 1000.0 / 60.0;

    fn rtp(frame: u32) -> u32 {
        frame * 1500
    }

    #[test]
    fn steady_stream_adds_no_delay() {
        let mut pacer = VideoPacer::new(8.0);
        for frame in 0..300u32 {
            let arrival = 1000.0 + f64::from(frame) * FRAME_MS;
            let present = pacer.schedule(arrival, rtp(frame));
            assert!((present - arrival).abs() < 0.01, "frame {frame}: {present} vs {arrival}");
        }
        assert!(pacer.delay_ms() < 0.01);
    }

    #[test]
    fn jittery_stream_is_evened_out() {
        let mut pacer = VideoPacer::new(8.0);
        let mut presents = Vec::new();
        for frame in 0..600u32 {
            // Every third picture is 4 ms late.
            let jitter = if frame % 3 == 2 { 4.0 } else { 0.0 };
            let arrival = 1000.0 + f64::from(frame) * FRAME_MS + jitter;
            let present = pacer.schedule(arrival, rtp(frame));
            assert!(present >= arrival - 0.01, "never present before arrival once settled");
            presents.push(present);
        }
        assert!((pacer.delay_ms() - 4.0).abs() < 0.2, "delay {}", pacer.delay_ms());
        // Once settled, pictures are shown at the console's even cadence.
        for pair in presents[400..].windows(2) {
            assert!((pair[1] - pair[0] - FRAME_MS).abs() < 0.05);
        }
    }

    #[test]
    fn long_stalls_do_not_inflate_the_delay() {
        let mut pacer = VideoPacer::new(8.0);
        for frame in 0..600u32 {
            // A 150 ms stall once per second delivers nine pictures in a burst.
            let in_burst = frame % 60 < 9;
            let stall_end = f64::from(frame - frame % 60 + 9) * FRAME_MS;
            let ideal = f64::from(frame) * FRAME_MS;
            let arrival = 1000.0 + if in_burst { stall_end } else { ideal };
            pacer.schedule(arrival, rtp(frame));
            assert!(pacer.delay_ms() <= 8.0);
        }
    }

    #[test]
    fn delay_shrinks_when_the_network_calms_down() {
        let mut pacer = VideoPacer::new(8.0);
        for frame in 0..300u32 {
            let jitter = if frame % 2 == 1 { 6.0 } else { 0.0 };
            pacer.schedule(1000.0 + f64::from(frame) * FRAME_MS + jitter, rtp(frame));
        }
        assert!(pacer.delay_ms() > 5.0);
        for frame in 300..1200u32 {
            pacer.schedule(1000.0 + f64::from(frame) * FRAME_MS, rtp(frame));
        }
        assert!(pacer.delay_ms() < 0.5, "delay {}", pacer.delay_ms());
    }

    #[test]
    fn zero_budget_means_present_on_arrival() {
        let mut pacer = VideoPacer::new(0.0);
        for frame in 0..200u32 {
            let jitter = if frame % 2 == 1 { 5.0 } else { 0.0 };
            let arrival = 1000.0 + f64::from(frame) * FRAME_MS + jitter;
            assert!(pacer.schedule(arrival, rtp(frame)) <= arrival + 0.01);
        }
    }

    /// 48 kHz stereo, reserve between 10 and 80 ms.
    fn buffer() -> AudioBuffer {
        AudioBuffer::new(48000, 2, 10, 80)
    }

    /// Samples in `ms` milliseconds of 48 kHz stereo.
    const fn samples(ms: usize) -> usize {
        ms * 96
    }

    /// Feeds 20 ms packets and pulls 10 ms blocks, as the stream does, for
    /// the given time. Returns the lowest level seen after a pull, in ms.
    fn play_steadily(b: &mut AudioBuffer, seconds: usize) -> usize {
        let mut lowest = usize::MAX;
        let mut out = vec![0f32; samples(10)];
        for _ in 0..seconds * 50 {
            b.push(&vec![0.25; samples(20)]);
            for _ in 0..2 {
                b.pull(&mut out);
                lowest = lowest.min(b.buffered_ms());
            }
        }
        lowest
    }

    #[test]
    fn audio_waits_for_a_packet_above_the_reserve_then_plays_in_order() {
        let mut b = buffer();
        let mut out = vec![1f32; samples(10)];
        b.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0), "silence before anything has arrived");
        b.push(&vec![0.5; samples(20)]);
        b.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0), "one packet is not yet a packet above the reserve");
        b.push(&vec![0.5; samples(20)]);
        b.pull(&mut out);
        assert_eq!(out[0], 0.0, "starts with a fade-in");
        assert_eq!(out[samples(10) - 1], 0.5, "fade-in is over within 5 ms");
        assert_eq!(b.underruns, 0);
    }

    /// Asks for more than is waiting, once the stream has settled; the next
    /// packet then arrives 30 ms late.
    fn run_dry_after_settling(b: &mut AudioBuffer) -> (Vec<f32>, usize) {
        play_steadily(b, 6);
        let waiting = b.samples.len();
        let mut out = vec![0f32; waiting + samples(30)];
        b.pull(&mut out);
        b.push(&vec![0.25; samples(20)]);
        (out, waiting)
    }

    #[test]
    fn audio_underrun_fades_out_and_raises_the_reserve() {
        let mut b = buffer();
        let (out, waiting) = run_dry_after_settling(&mut b);
        assert_eq!(b.underruns, 1);
        assert!(out[waiting - 2] < 0.01, "ends near silence instead of cutting off");
        assert!(out[waiting..].iter().all(|s| *s == 0.0));
        assert_eq!(b.reserve_ms(), 20);
    }

    #[test]
    fn audio_pausing_during_silence_is_not_mistaken_for_a_bad_network() {
        let mut b = buffer();
        play_steadily(&mut b, 6);
        // The console stops sending for a second, then carries on.
        let mut out = vec![0f32; samples(10)];
        for _ in 0..100 {
            b.pull(&mut out);
        }
        assert_eq!(b.underruns, 1);
        play_steadily(&mut b, 2);
        assert_eq!(b.reserve_ms(), 10, "a pause says nothing about the network");
        assert_eq!(b.underruns, 1);
    }

    #[test]
    fn audio_a_short_sound_after_silence_is_still_played() {
        let mut b = buffer();
        play_steadily(&mut b, 6);
        let mut out = vec![0f32; samples(10)];
        for _ in 0..50 {
            b.pull(&mut out); // the console has gone quiet
        }
        // One packet on its own: less than playback normally starts from.
        b.push(&vec![0.5; samples(20)]);
        let mut heard = 0;
        for _ in 0..12 {
            b.pull(&mut out);
            heard += out.iter().filter(|s| **s != 0.0).count();
        }
        assert!(heard > samples(18), "the whole blip comes out: {} of {} samples", heard, samples(20));
        assert_eq!(b.buffered_ms(), 0, "nothing is left behind to turn up later");
        assert_eq!(b.reserve_ms(), 10);
    }

    #[test]
    fn audio_running_dry_while_the_stream_starts_does_not_count() {
        let mut b = buffer();
        b.push(&vec![0.5; samples(20)]);
        b.push(&vec![0.5; samples(20)]);
        let mut out = vec![0f32; samples(45)];
        b.pull(&mut out);
        b.push(&vec![0.5; samples(20)]);
        assert_eq!((b.underruns, b.reserve_ms()), (1, 10));
    }

    #[test]
    fn audio_reserve_shrinks_back_after_clean_playback() {
        let mut b = buffer();
        run_dry_after_settling(&mut b);
        assert_eq!((b.underruns, b.reserve_ms()), (1, 20));
        play_steadily(&mut b, 9);
        assert_eq!(b.reserve_ms(), 20, "not yet");
        play_steadily(&mut b, 2);
        assert_eq!(b.reserve_ms(), 19);
        assert_eq!(b.underruns, 1);
    }

    #[test]
    fn audio_reserve_does_not_return_to_a_level_that_just_failed() {
        let mut b = buffer();
        run_dry_after_settling(&mut b);
        assert_eq!(b.reserve_ms(), 20);
        play_steadily(&mut b, 200);
        assert_eq!(b.reserve_ms(), 15, "stays clear of the 10 ms that was too little");
        play_steadily(&mut b, 500);
        assert_eq!(b.reserve_ms(), 10, "after ten clean minutes it may try again");
        assert_eq!(b.underruns, 1);
    }

    #[test]
    fn audio_held_beyond_the_reserve_is_played_off() {
        let mut b = buffer();
        // Start 60 ms deeper than needed, as after a wobbly start.
        b.push(&vec![0.25; samples(20)]);
        b.push(&vec![0.25; samples(60)]);
        let before = play_steadily(&mut b, 1);
        assert!(before >= 60, "nothing is removed before the level has been watched: {before} ms");
        play_steadily(&mut b, 8);
        let settled = play_steadily(&mut b, 2);
        assert!((9..=14).contains(&settled), "settles at the reserve: {settled} ms");
        assert_eq!(b.underruns, 0);
    }

    #[test]
    fn audio_at_the_reserve_is_left_alone() {
        let mut b = buffer();
        play_steadily(&mut b, 10);
        let skipped_while_settling = b.skipped_samples;
        let lowest = play_steadily(&mut b, 20);
        assert_eq!(b.skipped_samples, skipped_while_settling, "nothing more is removed once settled");
        assert_eq!(b.underruns, 0);
        assert!((9..=13).contains(&lowest), "holds the reserve: {lowest} ms");
    }

    #[test]
    fn audio_playing_fast_keeps_the_signal_intact() {
        let mut b = buffer();
        b.push(&vec![0.25; samples(20)]);
        b.push(&vec![0.25; samples(60)]);
        let mut out = vec![0f32; samples(10)];
        for _ in 0..250 {
            b.push(&vec![0.25; samples(20)]);
            b.pull(&mut out);
            b.pull(&mut out);
            assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6 || *s < 0.25));
        }
        assert!(b.skipped_samples > 0, "some of it was played fast");
        b.push(&vec![0.25; samples(20)]);
        b.pull(&mut out);
        assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6), "a steady level stays steady");
    }

    #[test]
    fn audio_backlog_after_a_stall_is_cut_at_once() {
        let mut b = buffer();
        // Nothing is being played, so six packets pile up to 120 ms.
        for _ in 0..6 {
            b.push(&vec![0.25; samples(20)]);
        }
        assert_eq!(b.buffered_ms(), 30, "cut back to the reserve plus one packet");
        assert_eq!(b.skipped_samples, samples(90) as u64);
        let mut out = vec![0f32; samples(10)];
        b.pull(&mut out);
        assert!(out[samples(6)..].iter().all(|s| (*s - 0.25).abs() < 1e-6), "the crossfade keeps the level");
    }
}
