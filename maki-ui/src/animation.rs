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
/// Draining the whole backlog takes this long, so the typewriter never falls
/// further behind than roughly this and a seal has almost nothing left to
/// dump. Without it a burst types at the base rate long after the model
/// stopped and the whole remainder lands at once when the turn seals.
const BACKLOG_WINDOW_MS: u64 = 200;

pub struct Typewriter {
    buffer: String,
    visible_len: usize,
    visible_byte_offset: usize,
    anim_target: usize,
    #[cfg(not(test))]
    last_tick: Instant,
    #[cfg(test)]
    forced_elapsed_ms: Option<f64>,
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
            #[cfg(not(test))]
            last_tick: Instant::now(),
            #[cfg(test)]
            forced_elapsed_ms: None,
            carry: 0.0,
            ms_per_char,
        }
    }

    pub fn push(&mut self, text: &str) {
        self.buffer.push_str(text);
        self.anim_target = self.buffer.chars().count();
        if self.ms_per_char == 0 {
            self.advance_visible(self.anim_target);
        }
    }

    pub fn tick(&mut self) {
        let elapsed_ms = self.elapsed_ms();
        let backlog = self.anim_target - self.visible_len;
        if backlog == 0 {
            return;
        }
        if self.ms_per_char == 0 {
            self.advance_visible(self.anim_target);
            return;
        }
        let base = 1.0 / self.ms_per_char as f64;
        let catch_up = backlog as f64 / BACKLOG_WINDOW_MS as f64;
        self.carry += base.max(catch_up) * elapsed_ms;
        let step = self.carry.floor();
        self.carry -= step;
        if step > 0.0 {
            let new_len = (self.visible_len + step as usize).min(self.anim_target);
            self.advance_visible(new_len);
        }
    }

    #[cfg(not(test))]
    fn elapsed_ms(&mut self) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;
        elapsed.as_secs_f64() * 1_000.0
    }

    /// Tests drive the rate math off an exact injected elapsed time, so the
    /// microsecond gap between two `Instant::now` calls never shifts a reveal.
    #[cfg(test)]
    fn elapsed_ms(&mut self) -> f64 {
        std::mem::take(&mut self.forced_elapsed_ms).unwrap_or(0.0)
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
        self.carry = 0.0;
    }

    /// Hands the next `tick` an exact elapsed time, so the rate math is
    /// deterministic instead of chasing the wall clock.
    #[cfg(test)]
    pub(crate) fn set_elapsed(&mut self, elapsed: Duration) {
        self.forced_elapsed_ms = Some(elapsed.as_secs_f64() * 1_000.0);
    }

    fn reset_anim(&mut self) {
        self.visible_len = 0;
        self.visible_byte_offset = 0;
        self.anim_target = 0;
        self.carry = 0.0;
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
    fn push_does_not_snap_an_in_flight_reveal() {
        let mut tw = Typewriter::with_speed(1_000);
        tw.push("aaaaaaaaaa");
        assert_eq!(tw.visible(), "");

        tw.push("bbb");
        assert_eq!(tw.visible(), "", "a new chunk must not jump the pending reveal");
        assert!(tw.is_animating());
    }

    #[test]
    fn the_reveal_tracks_elapsed_time_at_the_base_rate() {
        let mut tw = Typewriter::with_speed(DEFAULT_MS_PER_CHAR);
        tw.push("abcdefghij");
        tw.set_elapsed(Duration::from_millis(DEFAULT_MS_PER_CHAR * 3));
        tw.tick();
        assert_eq!(tw.visible().chars().count(), 3, "three chars in three steps");
    }

    #[test]
    fn a_backlog_speeds_the_reveal_up() {
        let mut tw = Typewriter::with_speed(DEFAULT_MS_PER_CHAR);
        tw.push(&"a".repeat(BACKLOG_WINDOW_MS as usize));
        tw.set_elapsed(Duration::from_millis(DEFAULT_MS_PER_CHAR));
        tw.tick();
        assert!(
            tw.visible().chars().count() > 1,
            "a backlog past the base rate catches up faster than one char per step"
        );
    }

    #[test]
    fn carry_accumulates_across_ticks() {
        let mut tw = Typewriter::with_speed(DEFAULT_MS_PER_CHAR);
        tw.push("abc");
        tw.set_elapsed(Duration::from_millis(1));
        tw.tick();
        assert_eq!(tw.visible().chars().count(), 0, "a sub-char tick reveals nothing");
        tw.set_elapsed(Duration::from_millis(3));
        tw.tick();
        assert_eq!(tw.visible().chars().count(), 1, "the carry adds up across ticks");
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
