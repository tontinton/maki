//! When the UI paints.
//!
//! Three ideas, and keeping them apart is what keeps the loop cheap:
//!
//! - [`Dirty`]: something changed what is on screen, so a frame is owed. Every
//!   poller in `App::tick` reports it, and every event the loop handles implies
//!   it.
//! - [`Watch`]: how a poller notices a background thread writing to a shared
//!   slot, the one kind of change nothing else announces.
//! - [`Cadence`]: how soon the loop has to come back when no event will wake
//!   it. It is a wake request, not a paint request: a timeout only buys another
//!   `tick`, and that tick's [`Dirty`] decides whether a frame is owed.
//!
//! Two rules keep them honest, and breaking either one looks the same from the
//! outside: a screen that will not update, or a laptop fan that will not stop.
//!
//! **`view` stays a pure render.** A poller hidden inside a widget's `view`
//! only runs when a frame happens, so the only way to keep it fed is to claim
//! the screen is animating, which pins the loop at full frame rate. Same story
//! for a shared slot read mid render, like the model list or the provider
//! quota: it puts data on screen that no poller ever saw. Put a [`Watch`] on
//! the slot, poll it in `App::tick`, render from the [`Watch`].
//!
//! **Whatever draws the motion reports the [`Cadence`] and advances the clock
//! in `tick`**, instead of some condition in `app` that slowly stops meaning
//! what it used to. A timeout only asks `tick` to look again; the frame follows
//! only if that tick returned [`Dirty`]. It describes pixels, not work: a
//! running tool that draws one spinner glyph is [`Cadence::Spinner`], and a
//! reveal collapsed out of view moves nothing at all.

use std::ops::{BitOr, BitOrAssign};
use std::sync::Arc;
use std::time::Duration;

use crate::animation::SPINNER_FRAME;

/// How long the loop may sleep when nothing is animating. Real events wake it
/// immediately, so this only bounds how stale a polled result can get.
pub const IDLE_POLL: Duration = Duration::from_millis(100);

/// Fast enough that per-character reveals and colour fades look continuous.
pub(crate) const SMOOTH_FRAME: Duration = Duration::from_millis(16);

/// What every poller in the crate is tested against, so the two invariants
/// read the same wherever they are checked.
#[cfg(test)]
pub(crate) mod expect {
    pub(crate) const OWED: &str = "the change must owe a frame or the screen stays stale";
    pub(crate) const QUIET: &str = "nothing changed, so no frame may be owed";
}

/// Something changed what is on screen, so a frame is owed.
///
/// `#[must_use]` is the whole point of the newtype: a poller added to
/// `App::tick` a year from now cannot quietly skip its repaint, because
/// dropping this warns and the workspace builds with `-D warnings`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct Dirty(bool);

impl Dirty {
    pub const NO: Self = Self(false);
    pub const YES: Self = Self(true);

    /// Runs every element. Pollers have side effects, so short-circuiting
    /// would stop draining channels as soon as one reported a change.
    pub fn any(flags: impl IntoIterator<Item = Self>) -> Self {
        flags.into_iter().fold(Self::NO, Self::bitor)
    }

    /// Clears the flag as it reports, so the debt is paid exactly once.
    pub fn take(&mut self) -> bool {
        std::mem::take(self).0
    }
}

impl From<bool> for Dirty {
    fn from(changed: bool) -> Self {
        Self(changed)
    }
}

impl BitOr for Dirty {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Dirty {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

/// The last value seen in a shared slot a background thread publishes to, so
/// `view` renders from here and never loads the slot itself.
///
/// Every publish stores a fresh `Arc`, and holding the last one alive is what
/// stops its address being reused, so comparing pointers is exact. That asks
/// nothing of the slot: no `PartialEq` (`ProviderUsage` has none), no
/// generation to remember to bump, and no stand-in like a length, which misses
/// a same-sized republish.
pub struct Watch<T>(Option<Arc<T>>);

impl<T> Watch<T> {
    /// Starts on what the slot already holds, so the first poll reports an
    /// arrival and not the watcher's own birth. Only watches that poll from
    /// startup need it; one gated on an open modal is repainted by the opening.
    pub fn seeded(latest: impl Into<Option<Arc<T>>>) -> Self {
        Self(latest.into())
    }

    pub fn poll(&mut self, latest: impl Into<Option<Arc<T>>>) -> Dirty {
        let latest = latest.into();
        if self.0.as_ref().map(Arc::as_ptr) == latest.as_ref().map(Arc::as_ptr) {
            return Dirty::NO;
        }
        self.0 = latest;
        Dirty::YES
    }

    pub fn get(&self) -> Option<&T> {
        self.0.as_deref()
    }
}

impl<T> Default for Watch<T> {
    fn default() -> Self {
        Self(None)
    }
}

/// How soon the loop has to look again when no event wakes it.
///
/// This is a wake request, not a paint request: a timeout buys another `tick`,
/// and only that tick's [`Dirty`] decides whether a frame is owed. A spinner
/// and a smooth source differ only in how soon they need the look; they
/// combine by urgency, so `Smooth` always outranks `Spinner`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cadence {
    /// Nothing to come back for. The loop sleeps until something happens.
    #[default]
    Idle,
    /// Spinner glyphs, which advance once per [`SPINNER_FRAME`]. Painting any
    /// faster redraws the same glyph.
    Spinner,
    /// Typewriter reveals and colour fades, which change every frame.
    Smooth,
}

impl Cadence {
    /// `cadence` while `applies`, else [`Cadence::Idle`].
    pub fn when(applies: bool, cadence: Self) -> Self {
        if applies { cadence } else { Self::Idle }
    }

    /// The most urgent source. `Smooth` is always at least as urgent as
    /// `Spinner` because the spinner interval is clamped to the smooth frame.
    pub fn any(cadences: impl IntoIterator<Item = Self>) -> Self {
        cadences.into_iter().max().unwrap_or(Self::Idle)
    }

    /// How long the loop may sleep, or `None` to sleep until something wakes
    /// it. `smooth` is the loop's fastest frame; a spinner never asks for one
    /// faster than that.
    pub fn interval(self, smooth: Duration) -> Option<Duration> {
        match self {
            Self::Idle => None,
            Self::Spinner => Some(SPINNER_FRAME.max(smooth)),
            Self::Smooth => Some(smooth),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::expect::{OWED, QUIET};
    use super::{Cadence, Dirty, SMOOTH_FRAME, Watch};
    use crate::animation::SPINNER_FRAME;
    use std::sync::Arc;
    use std::time::Duration;

    const EXPECT_SETTLED: &str = "nothing to combine must leave the loop asleep";
    const EXPECT_URGENT: &str = "the most urgent source must win";
    const EXPECT_UNSEEN: &str = "an unpolled watch has nothing to render";
    const SLOW_FRAME: Duration = Duration::from_millis(100);

    #[test]
    fn any_runs_every_poller() {
        let mut ran = 0;
        let dirty = Dirty::any(
            [Dirty::YES, Dirty::NO, Dirty::NO]
                .into_iter()
                .inspect(|_| ran += 1),
        );
        assert_eq!(dirty, Dirty::YES);
        assert_eq!(ran, 3, "a reported change must not stop later pollers");
    }

    #[test]
    fn take_pays_the_debt_once() {
        let mut dirty = Dirty::YES;
        assert!(dirty.take());
        assert!(!dirty.take());
    }

    /// The empty fold is the common case: a settled session takes it on every
    /// loop turn.
    #[test]
    fn any_takes_the_most_urgent_source() {
        assert_eq!(Dirty::any([]), Dirty::NO, "{EXPECT_SETTLED}");
        assert_eq!(Cadence::any([]), Cadence::Idle, "{EXPECT_SETTLED}");

        let picked = Cadence::any([Cadence::Idle, Cadence::Spinner, Cadence::Smooth]);
        assert_eq!(picked, Cadence::Smooth, "{EXPECT_URGENT}");

        let spinning = Cadence::any([Cadence::Idle, Cadence::Spinner]);
        assert_eq!(spinning, Cadence::Spinner, "{EXPECT_URGENT}");
    }

    #[test]
    fn interval_maps_the_three_rates() {
        assert_eq!(Cadence::Idle.interval(SMOOTH_FRAME), None);
        assert_eq!(Cadence::Smooth.interval(SMOOTH_FRAME), Some(SMOOTH_FRAME));
        assert_eq!(Cadence::Spinner.interval(SMOOTH_FRAME), Some(SPINNER_FRAME));
        assert_eq!(
            Cadence::Spinner.interval(SLOW_FRAME),
            Some(SLOW_FRAME),
            "a slow frame never makes the spinner ask faster than it can paint"
        );
    }

    /// A generation counter answers this too, as long as every publisher
    /// remembers to bump one. Identity asks nothing of the publisher, and an
    /// equal value republished is still a change worth a frame.
    #[test]
    fn watch_reports_each_arrival_once() {
        let mut watch = Watch::default();
        assert_eq!(watch.get(), None, "{EXPECT_UNSEEN}");
        assert_eq!(watch.poll(None), Dirty::NO, "{QUIET}");

        let first = Arc::new(7);
        assert_eq!(watch.poll(Arc::clone(&first)), Dirty::YES, "{OWED}");
        assert_eq!(watch.get(), Some(&7));
        assert_eq!(watch.poll(first), Dirty::NO, "{QUIET}");

        assert_eq!(watch.poll(Arc::new(7)), Dirty::YES, "{OWED}");
        assert_eq!(watch.poll(None), Dirty::YES, "{OWED}");
        assert_eq!(watch.get(), None, "{EXPECT_UNSEEN}");
    }
}
