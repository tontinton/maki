use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_lua::{CodeBlockOutcome, CodeBlockRender, EventHandle};
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_ENTRIES: usize = 128;
/// A drag resizes once per column, so a render waits for the width to rest.
pub(super) const RESIZE_SETTLE: Duration = Duration::from_millis(200);

/// A finished render, kept for the width it was asked at.
struct Entry {
    language: Arc<str>,
    source: String,
    width: u16,
    lines: Option<Vec<String>>,
    /// Every message that showed it, so it stays while any is on screen.
    messages: Vec<usize>,
    used: u64,
}

/// A claimed block the layout met without a result that fits its width.
struct Wanted {
    message: usize,
    language: Arc<str>,
    source: String,
    width: u16,
}

struct InFlight {
    language: Arc<str>,
    source: String,
    width: u16,
    request: CodeBlockRender,
}

pub(super) enum Refresh {
    Idle,
    /// A `ui.code_block.*` layer changed, so every block may draw differently.
    All,
    /// A render finished, and these messages show the block it drew.
    Messages(Vec<usize>),
}

/// Plugin renders of claimed code blocks. Layout only reads finished results
/// and notes the blocks it still needs. One block on screen renders at a
/// time, and each renders once per width until a `ui.code_block.*` layer
/// changes. A timeout skips its language until a resize, a load or a layer
/// change.
pub(super) struct CodeBlockRenders {
    handle: EventHandle,
    revision: u64,
    entries: Vec<Entry>,
    wanted: Vec<Wanted>,
    in_flight: Option<InFlight>,
    stalled: Vec<Arc<str>>,
    /// No render starts before this, set by the last resize.
    settle_until: Option<Instant>,
    /// The messages on screen at the last frame.
    visible: Vec<usize>,
    clock: u64,
}

impl CodeBlockRenders {
    pub fn new(handle: EventHandle) -> Self {
        Self {
            revision: handle.code_block_revision(),
            handle,
            entries: Vec::new(),
            wanted: Vec::new(),
            in_flight: None,
            stalled: Vec::new(),
            settle_until: None,
            visible: Vec::new(),
            clock: 0,
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.wanted.clear();
        self.in_flight = None;
        self.stalled.clear();
    }

    /// The first layout of a chat has nothing to settle.
    pub fn resized(&mut self, now: Instant) {
        self.stalled.clear();
        if !self.entries.is_empty() || !self.wanted.is_empty() || self.in_flight.is_some() {
            self.settle_until = Some(now + RESIZE_SETTLE);
        }
    }

    /// The lines a plugin drew for this block, or `None` to show the source.
    /// A claimed block without a result that fits `width` is noted for
    /// [`Self::schedule`].
    pub fn lookup(
        &mut self,
        message: usize,
        info: &str,
        source: &str,
        width: u16,
    ) -> Option<Vec<String>> {
        let language = self.handle.claimed_code_block(info, source, width)?;
        self.clock += 1;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.language == language && entry.source == source)
        {
            let fits = entry
                .lines
                .as_ref()
                .is_some_and(|lines| lines.iter().all(|line| line.width() <= usize::from(width)));
            if entry.width == width || fits {
                if !entry.messages.contains(&message) {
                    entry.messages.push(message);
                }
                entry.used = self.clock;
                self.wanted.retain(|wanted| {
                    wanted.message != message
                        || wanted.language != language
                        || wanted.source != source
                });
                return entry.lines.clone();
            }
        }
        if self.stalled.contains(&language) {
            return None;
        }
        match self.wanted.iter_mut().find(|wanted| {
            wanted.message == message && wanted.language == language && wanted.source == source
        }) {
            Some(wanted) => wanted.width = width,
            None => self.wanted.push(Wanted {
                message,
                language,
                source: source.to_owned(),
                width,
            }),
        }
        None
    }

    /// Keeps one render in flight for a block in a `visible` message: drops
    /// the one whose block left the screen or now wants another width, and
    /// starts the next once a resize has settled.
    pub fn schedule(&mut self, visible: &[usize], now: Instant) {
        visible.clone_into(&mut self.visible);
        self.trim();
        let on_screen = |wanted: &&Wanted| visible.contains(&wanted.message);
        if let Some(flight) = &self.in_flight
            && !self.wanted.iter().filter(on_screen).any(|wanted| {
                wanted.language == flight.language
                    && wanted.source == flight.source
                    && wanted.width == flight.width
            })
        {
            self.in_flight = None;
        }
        if self.in_flight.is_some() || self.settle_until.is_some_and(|until| now < until) {
            return;
        }
        if let Some(next) = self.wanted.iter().find(on_screen) {
            self.in_flight = Some(InFlight {
                language: Arc::clone(&next.language),
                source: next.source.clone(),
                width: next.width,
                request: self.handle.render_code_block(
                    Arc::clone(&next.language),
                    next.source.clone(),
                    next.width,
                ),
            });
        }
    }

    pub fn has_pending(&self) -> bool {
        self.in_flight.is_some() || (self.settle_until.is_some() && !self.wanted.is_empty())
    }

    pub fn poll(&mut self, now: Instant) -> Refresh {
        let revision = self.handle.code_block_revision();
        if self.revision != revision {
            self.revision = revision;
            self.clear();
            return Refresh::All;
        }
        if self.settle_until.is_some_and(|until| now >= until) {
            self.settle_until = None;
            if !self.wanted.is_empty() {
                return Refresh::Messages(Vec::new());
            }
        }
        let Some(flight) = self.in_flight.take() else {
            return Refresh::Idle;
        };
        let outcome = match flight.request.try_recv() {
            Ok(outcome) => outcome,
            Err(flume::TryRecvError::Empty) => {
                self.in_flight = Some(flight);
                return Refresh::Idle;
            }
            Err(flume::TryRecvError::Disconnected) => CodeBlockOutcome::Declined,
        };
        let mut messages = Vec::new();
        self.wanted.retain(|wanted| {
            let done = wanted.language == flight.language
                && wanted.source == flight.source
                && wanted.width == flight.width;
            if done {
                messages.push(wanted.message);
            }
            !done
        });
        let lines = match outcome {
            CodeBlockOutcome::Rendered(lines) => Some(lines),
            CodeBlockOutcome::Declined => None,
            CodeBlockOutcome::TimedOut => {
                tracing::warn!(
                    language = %flight.language,
                    "code block renderer timed out, its language shows as source until a resize, a load or a layer change"
                );
                self.wanted
                    .retain(|wanted| wanted.language != flight.language);
                self.stalled.push(Arc::clone(&flight.language));
                None
            }
        };
        let refresh = if lines.is_some() {
            messages.clone()
        } else {
            Vec::new()
        };
        self.store(flight, lines, messages);
        Refresh::Messages(refresh)
    }

    fn store(&mut self, flight: InFlight, lines: Option<Vec<String>>, messages: Vec<usize>) {
        self.entries
            .retain(|entry| entry.language != flight.language || entry.source != flight.source);
        self.clock += 1;
        self.entries.push(Entry {
            language: flight.language,
            source: flight.source,
            width: flight.width,
            lines,
            messages,
            used: self.clock,
        });
        self.trim();
    }

    /// Evicts the least recently used entries past [`MAX_ENTRIES`], skipping
    /// any a message on screen shows, so a drawn block cannot fall back to
    /// source in its own reflow.
    fn trim(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = (0..self.entries.len())
                .filter(|&i| {
                    !self.entries[i]
                        .messages
                        .iter()
                        .any(|message| self.visible.contains(message))
                })
                .min_by_key(|&i| self.entries[i].used)
            else {
                return;
            };
            self.entries.swap_remove(oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_lua::test_support::{RequestProbe, probed_event_handle_layering};
    use test_case::test_case;

    use super::*;

    const SLOT: &str = "ui.code_block.mermaid";
    const LANGUAGE: &str = "mermaid";
    const WIDTH: u16 = 80;
    const NARROW: u16 = 40;
    const ON_SCREEN: usize = 0;
    const OFF_SCREEN: usize = usize::MAX;
    const DRAWN: &str = "drawn";

    fn renders() -> (CodeBlockRenders, RequestProbe) {
        let (handle, probe) = probed_event_handle_layering(&[SLOT]);
        (CodeBlockRenders::new(handle), probe)
    }

    fn answer(renders: &mut CodeBlockRenders, probe: &RequestProbe, outcome: CodeBlockOutcome) {
        let (_, _, _, reply, _) = probe.try_recv_code_block().unwrap();
        reply.send(outcome).unwrap();
        assert!(matches!(renders.poll(Instant::now()), Refresh::Messages(_)));
    }

    fn finish(renders: &mut CodeBlockRenders, probe: &RequestProbe, lines: &str) {
        answer(
            renders,
            probe,
            CodeBlockOutcome::Rendered(vec![lines.into()]),
        );
    }

    #[test]
    fn a_block_on_screen_outlives_later_renders() {
        let now = Instant::now();
        let (mut renders, probe) = renders();
        let shared = ON_SCREEN.to_string();
        renders.lookup(ON_SCREEN, LANGUAGE, &shared, WIDTH);
        renders.schedule(&[ON_SCREEN], now);
        finish(&mut renders, &probe, DRAWN);
        renders.lookup(OFF_SCREEN, LANGUAGE, &shared, WIDTH);
        for message in 1..=MAX_ENTRIES {
            renders.lookup(message, LANGUAGE, &message.to_string(), WIDTH);
            renders.schedule(&[ON_SCREEN, message], now);
            finish(&mut renders, &probe, DRAWN);
        }
        assert_eq!(
            renders.lookup(ON_SCREEN, LANGUAGE, &shared, WIDTH),
            Some(vec![DRAWN.to_owned()])
        );
    }

    #[test]
    fn returning_to_a_cached_width_cancels_the_narrow_render() {
        let now = Instant::now();
        let (mut renders, probe) = renders();
        let source = ON_SCREEN.to_string();
        renders.lookup(ON_SCREEN, LANGUAGE, &source, WIDTH);
        renders.schedule(&[ON_SCREEN], now);
        finish(&mut renders, &probe, &"x".repeat(WIDTH.into()));
        renders.lookup(ON_SCREEN, LANGUAGE, &source, NARROW);
        renders.schedule(&[ON_SCREEN], now);
        let (_, _, width, _, cancel) = probe.try_recv_code_block().unwrap();
        assert_eq!(width, NARROW);
        assert!(
            renders
                .lookup(ON_SCREEN, LANGUAGE, &source, WIDTH)
                .is_some()
        );
        renders.schedule(&[ON_SCREEN], now);
        assert!(cancel.is_cancelled());
        assert!(!renders.has_pending());
    }

    #[test]
    fn the_cache_shrinks_once_a_large_message_leaves_the_screen() {
        let now = Instant::now();
        const BLOCKS: usize = MAX_ENTRIES + 12;
        let (mut renders, probe) = renders();
        for block in 0..BLOCKS {
            renders.lookup(ON_SCREEN, LANGUAGE, &block.to_string(), WIDTH);
            renders.schedule(&[ON_SCREEN], now);
            finish(&mut renders, &probe, DRAWN);
        }
        assert_eq!(renders.entries.len(), BLOCKS);
        renders.schedule(&[], now);
        assert_eq!(renders.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn a_resize_renders_once_the_width_settles() {
        const STEPS: u16 = 3;
        let (mut renders, probe) = renders();
        let source = ON_SCREEN.to_string();
        let mut now = Instant::now();
        renders.lookup(ON_SCREEN, LANGUAGE, &source, WIDTH);
        renders.schedule(&[ON_SCREEN], now);
        finish(&mut renders, &probe, &"x".repeat(WIDTH.into()));
        for step in 1..=STEPS {
            renders.resized(now);
            renders.lookup(ON_SCREEN, LANGUAGE, &source, WIDTH - step);
            renders.schedule(&[ON_SCREEN], now);
            assert!(probe.try_recv_code_block().is_none());
            now += RESIZE_SETTLE / 2;
        }
        let settled = now + RESIZE_SETTLE;
        assert!(matches!(renders.poll(settled), Refresh::Messages(_)));
        renders.schedule(&[ON_SCREEN], settled);
        let (_, _, width, _, _) = probe.try_recv_code_block().unwrap();
        assert_eq!(width, WIDTH - STEPS);
        assert!(probe.try_recv_code_block().is_none());
    }

    #[test_case(true ; "resize")]
    #[test_case(false ; "load")]
    fn a_timed_out_language_comes_back_after(resize: bool) {
        let (mut renders, probe) = renders();
        let now = Instant::now();
        let source = ON_SCREEN.to_string();
        renders.lookup(ON_SCREEN, LANGUAGE, &source, WIDTH);
        renders.schedule(&[ON_SCREEN], now);
        answer(&mut renders, &probe, CodeBlockOutcome::TimedOut);
        renders.lookup(ON_SCREEN, LANGUAGE, &OFF_SCREEN.to_string(), WIDTH);
        renders.schedule(&[ON_SCREEN], now);
        assert!(probe.try_recv_code_block().is_none());

        if resize {
            renders.resized(now);
        } else {
            renders.clear();
        }
        let settled = now + RESIZE_SETTLE;
        renders.poll(settled);
        renders.lookup(ON_SCREEN, LANGUAGE, &source, NARROW);
        renders.schedule(&[ON_SCREEN], settled);
        assert!(probe.try_recv_code_block().is_some());
    }
}
