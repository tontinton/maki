use std::mem;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_STRS: [&str; 10] = ["⠋ ", "⠙ ", "⠹ ", "⠸ ", "⠼ ", "⠴ ", "⠦ ", "⠧ ", "⠇ ", "⠏ "];
const SPINNER_FRAME_MS: u128 = 80;

/// How long one glyph stays up. [`crate::repaint::Cadence::SPINNER`] paints at
/// exactly this rate, so no two frames show the same glyph.
pub const SPINNER_FRAME: Duration = Duration::from_millis(SPINNER_FRAME_MS as u64);

pub fn spinner_frame(elapsed_ms: u128) -> char {
    SPINNER_FRAMES[(elapsed_ms / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len()]
}

pub fn spinner_str(elapsed_ms: u128) -> &'static str {
    SPINNER_STRS[(elapsed_ms / SPINNER_FRAME_MS) as usize % SPINNER_STRS.len()]
}

/// Spinners need a consistent time reference. Using a static epoch avoids
/// passing Instant through every render call.
pub fn animation_elapsed_ms() -> u128 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

const DEFAULT_MS_PER_CHAR: u64 = 4;
/// When text arrives faster than `ms_per_char`, the reveal speeds up so the
/// whole backlog would be gone in this window. A big chunk then just types
/// faster, and the screen stays at most about this far behind the stream.
const BACKLOG_WINDOW_MS: u64 = 200;

pub struct Typewriter {
    buffer: String,
    visible_len: usize,
    visible_byte_offset: usize,
    anim_target: usize,
    last_tick: Instant,
    carry: f64,
    ms_per_char: u64,
}

impl Default for Typewriter {
    fn default() -> Self {
        Self::with_speed(DEFAULT_MS_PER_CHAR)
    }
}

impl Typewriter {
    pub fn new() -> Self {
        Self::with_speed(DEFAULT_MS_PER_CHAR)
    }

    pub fn with_speed(ms_per_char: u64) -> Self {
        Self {
            buffer: String::new(),
            visible_len: 0,
            visible_byte_offset: 0,
            anim_target: 0,
            last_tick: Instant::now(),
            carry: 0.0,
            ms_per_char,
        }
    }

    pub fn push(&mut self, text: &str) {
        // Once the reveal catches up we stop rendering, so nothing ticks. Without
        // this the next tick would count that whole quiet gap as reveal time and
        // show the new chunk in one go.
        if !self.is_animating() {
            self.last_tick = Instant::now();
            self.carry = 0.0;
        }
        self.buffer.push_str(text);
        self.anim_target = self.buffer.chars().count();
        if self.ms_per_char == 0 {
            self.advance_visible(self.anim_target);
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;
        self.advance(elapsed);
    }

    /// A frame can last longer than one char, so counting chars per frame would
    /// type too slowly. We go by elapsed time instead and keep the leftover
    /// fraction of a char for the next tick.
    fn advance(&mut self, elapsed: Duration) {
        let backlog = self.anim_target - self.visible_len;
        if backlog == 0 {
            return;
        }
        let base = 1.0 / self.ms_per_char as f64;
        let catch_up = backlog as f64 / BACKLOG_WINDOW_MS as f64;
        self.carry += base.max(catch_up) * elapsed.as_secs_f64() * 1_000.0;
        let step = self.carry.floor();
        self.carry -= step;
        self.advance_visible((self.visible_len + step as usize).min(self.anim_target));
    }

    pub fn visible(&self) -> &str {
        &self.buffer[..self.visible_byte_offset]
    }

    pub fn is_animating(&self) -> bool {
        self.visible_len < self.anim_target
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn buffer_line_count(&self) -> usize {
        if self.buffer.is_empty() {
            0
        } else {
            self.buffer.bytes().filter(|&b| b == b'\n').count() + 1
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.reset_anim();
    }

    pub fn take_all(&mut self) -> String {
        self.reset_anim();
        mem::take(&mut self.buffer)
    }

    #[cfg(test)]
    pub(crate) fn set_buffer(&mut self, text: &str) {
        self.buffer = text.into();
        let len = self.buffer.chars().count();
        self.visible_len = len;
        self.visible_byte_offset = self.buffer.len();
        self.anim_target = len;
    }

    fn reset_anim(&mut self) {
        self.visible_len = 0;
        self.visible_byte_offset = 0;
        self.anim_target = 0;
    }

    fn advance_visible(&mut self, new_len: usize) {
        let skip = new_len - self.visible_len;
        if skip > 0 {
            self.visible_byte_offset = self.buffer[self.visible_byte_offset..]
                .char_indices()
                .nth(skip)
                .map_or(self.buffer.len(), |(i, _)| self.visible_byte_offset + i);
        }
        self.visible_len = new_len;
    }
}

impl PartialEq<&str> for Typewriter {
    fn eq(&self, other: &&str) -> bool {
        self.buffer == *other
    }
}

impl std::fmt::Debug for Typewriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typewriter")
            .field("buffer", &self.buffer)
            .field("visible_len", &self.visible_len)
            .field("anim_target", &self.anim_target)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const MS_PER_CHAR: u64 = 4;
    const IDLE_GAP: Duration = Duration::from_secs(1);

    #[test]
    fn spinner_wraps_around() {
        let first = spinner_frame(0);
        let wrapped = spinner_frame(SPINNER_FRAME_MS * SPINNER_FRAMES.len() as u128);
        assert_eq!(first, wrapped);
        assert_ne!(first, spinner_frame(SPINNER_FRAME_MS));
    }

    #[test]
    fn push_animates_and_empty_push_is_noop() {
        let mut tw = Typewriter::new();
        tw.push("");
        assert!(!tw.is_animating());
        assert!(tw.is_empty());

        tw.push("hello world, this is a longer string");
        assert_eq!(tw.visible(), "");
        assert!(tw.is_animating());
    }

    #[test]
    fn set_buffer_makes_everything_visible() {
        let mut tw = Typewriter::new();
        tw.set_buffer("héllo 🌍");
        assert_eq!(tw.visible(), "héllo 🌍");
        assert!(!tw.is_animating());
    }

    #[test]
    fn push_leaves_an_in_flight_reveal_alone() {
        let mut tw = Typewriter::with_speed(MS_PER_CHAR);
        tw.push("aaaaaaaaaa");
        tw.advance(Duration::from_millis(9));
        tw.push("bbb");
        assert_eq!(tw.visible(), "aa");
        assert!(tw.is_animating());
    }

    #[test]
    fn push_restarts_the_clock_only_when_idle() {
        let mut tw = Typewriter::with_speed(MS_PER_CHAR);
        let stale = Instant::now() - IDLE_GAP;

        tw.last_tick = stale;
        tw.push("abc");
        assert!(
            tw.last_tick > stale,
            "the idle gap must not count as reveal time"
        );

        tw.last_tick = stale;
        tw.push("def");
        assert_eq!(
            tw.last_tick, stale,
            "a chunk landing mid reveal must not stall it"
        );
    }

    #[test_case(3,   &[9],     2 ; "base_rate_on_a_small_backlog")]
    #[test_case(100, &[9],     4 ; "catch_up_on_a_big_backlog")]
    #[test_case(3,   &[3, 3],  1 ; "fractions_carry_across_ticks")]
    #[test_case(3,   &[1_000], 3 ; "stops_at_the_end")]
    fn advance_reveals_by_rate(chunk_len: usize, ticks_ms: &[u64], expected_len: usize) {
        let mut tw = Typewriter::with_speed(MS_PER_CHAR);
        tw.push(&"a".repeat(chunk_len));
        for &ms in ticks_ms {
            tw.advance(Duration::from_millis(ms));
        }
        assert_eq!(tw.visible().len(), expected_len);
    }

    #[test]
    fn extend_preserves_visible_and_animates_new() {
        let mut tw = Typewriter::new();
        tw.set_buffer("ab");
        tw.push("cdefghijklmnop");
        assert_eq!(tw.visible(), "ab");
        assert!(tw.is_animating());
    }

    #[test]
    fn zero_speed_sequential_pushes_multibyte() {
        let mut tw = Typewriter::with_speed(0);
        tw.push("a");
        tw.push("é");
        tw.push("中");
        tw.push("🦀");
        assert_eq!(tw.visible(), "aé中🦀");
        assert!(!tw.is_animating());
    }

    #[test]
    fn clear_and_take_all_reset_byte_offset() {
        let mut tw = Typewriter::with_speed(0);

        tw.push("🔥🔥🔥");
        assert_eq!(tw.visible(), "🔥🔥🔥");
        tw.clear();
        assert!(tw.is_empty());
        assert_eq!(tw.visible(), "");

        tw.push("日本語");
        assert_eq!(tw.visible(), "日本語");
        let taken = tw.take_all();
        assert_eq!(taken, "日本語");
        assert!(tw.is_empty());
        assert_eq!(tw.visible(), "");

        tw.push("ok");
        assert_eq!(tw.visible(), "ok");
    }

    #[test]
    fn set_buffer_then_push_multibyte() {
        let mut tw = Typewriter::with_speed(0);
        tw.set_buffer("àá");
        tw.push("â🎉ã");
        assert_eq!(tw.visible(), "àáâ🎉ã");
    }

    #[test]
    fn repeated_clear_push_cycles() {
        let mut tw = Typewriter::with_speed(0);
        for _ in 0..3 {
            tw.push("🎵test🎵");
            assert_eq!(tw.visible(), "🎵test🎵");
            tw.clear();
            assert_eq!(tw.visible(), "");
        }
    }

    #[test]
    fn partial_eq_compares_full_buffer() {
        let mut tw = Typewriter::new();
        tw.push("hello world, this is enough text");
        assert_eq!(tw, "hello world, this is enough text");
        assert_eq!(tw.visible(), "");
    }
}
