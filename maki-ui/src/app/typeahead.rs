//! Key handlers run on the Lua thread while the user keeps typing. While a
//! handler runs, the keys after it wait here, and once it is done they take
//! the normal key path as if just typed. So a fast `dd` after a vim plugin's
//! `<Esc>` lands in normal mode, and the letter typed after a snippet key
//! cannot change the text under it before the snippet lands.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyEvent, MouseEventKind};
use maki_lua::{InputMode, Key, KeybindTicket, PASTE, SLICE_WARN, is_reserved};

use super::Msg;

/// Where the keys go. A handler that opens a picker may not return until the
/// user closes it, so when the keyboard moves away from where the held keys
/// were typed, they stop waiting and follow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KeyboardFocus {
    pub window: Option<u32>,
    pub input: bool,
    pub prompts: Option<Prompts>,
}

/// The modals and prompts up while any is, like an alert, a permission
/// prompt or a question. They answer on a single key, so held keys typed
/// before one opened must not answer it. Which ones are up tells one that
/// opened while the keys waited from one that was already there, and the
/// count of agent prompts opened so far tells a new prompt from an older one
/// of the same kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Prompts {
    pub up: u32,
    pub opened: u64,
}

/// Whether the user saw what a key would act on when they pressed it.
#[derive(Clone, Copy, Default)]
pub(super) struct Sight {
    /// The input changed or the keyboard moved while the key waited behind a
    /// handler, so the user never saw what it would send. Such a send is
    /// dropped.
    pub blind: bool,
}

/// A key or a paste on its way through the bindings, kept in the hold so
/// `ev.fallback()` can still hand it to the host.
#[derive(Clone)]
pub(super) enum Pressed {
    Key(KeyEvent, Sight),
    Paste(String),
}

impl Pressed {
    /// What a binding for this is set on. A press no notation names has none.
    pub fn key(&self) -> Option<Key> {
        match self {
            Self::Key(event, _) => Key::from_event(*event),
            Self::Paste(_) => Some(PASTE),
        }
    }
}

/// The chat input at one moment: the version of its text, and where the
/// cursor and the selection were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct InputAt {
    pub version: u64,
    pub cursor: usize,
    pub anchor: Option<usize>,
}

impl InputAt {
    /// Whether a key pressed at this lands somewhere else at {now}.
    fn caret_moved(&self, now: &Self) -> bool {
        self.cursor != now.cursor || self.anchor != now.anchor
    }
}

/// The turn of a message that left the wait. Only [`Typeahead`] hands one
/// out, and whatever can start a key handler or put text in needs one. So
/// input that arrives later, like a middle-click paste read in the
/// background, cannot get ahead of the keys still waiting.
pub(super) struct Turn(());

pub(super) struct Hold {
    ticket: u64,
    since: Instant,
    focus: KeyboardFocus,
    /// The mode the key was bound in. A normal mode key does not type, and
    /// its fallback must not either, or a plugin could switch to insert
    /// mode first and read every key the user types.
    mode: InputMode,
    plugin: Arc<str>,
    key: Key,
    /// Taken by the first `ev.fallback()`, so a second one does nothing.
    pressed: Option<Pressed>,
    /// The input at the press, to tell whether it changed while the keys
    /// waited.
    input: InputAt,
}

impl Hold {
    pub fn new(
        ticket: &KeybindTicket,
        focus: KeyboardFocus,
        mode: InputMode,
        pressed: Pressed,
        input: InputAt,
    ) -> Self {
        Self {
            ticket: ticket.id(),
            since: Instant::now(),
            focus,
            mode,
            plugin: Arc::clone(ticket.plugin()),
            key: ticket.key(),
            pressed: Some(pressed),
            input,
        }
    }
}

#[derive(Default)]
pub(super) struct Typeahead {
    hold: Option<Hold>,
    queue: VecDeque<(Msg, Sight)>,
}

impl Typeahead {
    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }

    /// Keys only start a handler once the keys before them have gone, and the
    /// keys that skip the wait can never be bound, so only a free
    /// [`Typeahead`] takes a hold.
    pub fn hold(&mut self, hold: Hold) {
        debug_assert!(self.hold.is_none(), "only a free typeahead takes a hold");
        self.hold = Some(hold);
    }

    /// Keeps {msg} while a handler runs or keys before it still wait.
    pub fn defer(&mut self, msg: Msg) -> Option<(Msg, Sight, Turn)> {
        if (self.hold.is_none() && self.queue.is_empty()) || !waits(&msg) {
            return Some((msg, Sight::default(), Turn(())));
        }
        self.queue.push_back((msg, Sight::default()));
        None
    }

    /// Drops the waiting keys and the fallback of the key they wait for.
    /// Returns whether any typed key was dropped. A fallback alone is not
    /// something the user typed, so it goes without a word.
    pub fn abandon(&mut self) -> bool {
        let pressed = self.hold.as_mut().and_then(|hold| hold.pressed.take());
        if pressed.is_none() && self.queue.is_empty() {
            return false;
        }
        tracing::debug!(
            dropped = self.queue.len(),
            fallback = pressed.is_some(),
            "held keys dropped: the user pressed Ctrl+C"
        );
        let dropped = !self.queue.is_empty();
        self.queue.clear();
        dropped
    }

    /// A handler we stopped waiting for can still finish later, while another
    /// key's handler holds. Its report must not let those keys go early.
    pub fn held_by(&self, ticket: u64) -> bool {
        self.hold.as_ref().is_some_and(|hold| hold.ticket == ticket)
    }

    /// Whether to stop waiting without the handler: it ran past the slow
    /// handler warning, so a hung plugin cannot freeze typing, or the
    /// keyboard moved.
    pub fn stale(&self, focus: impl FnOnce() -> KeyboardFocus) -> bool {
        self.stale_at(Instant::now(), focus)
    }

    fn stale_at(&self, now: Instant, focus: impl FnOnce() -> KeyboardFocus) -> bool {
        let Some(hold) = &self.hold else {
            return false;
        };
        let slow = now.saturating_duration_since(hold.since) >= SLICE_WARN;
        if slow {
            tracing::warn!(
                plugin = %hold.plugin,
                key = %hold.key.notation(),
                queued = self.queue.len(),
                "key handler still running, letting the keys typed after it through"
            );
        }
        slow || focus() != hold.focus
    }

    /// The press behind {ticket}, while it still holds the keys after it,
    /// the keyboard is still at {focus} and in {mode}, where the user
    /// pressed it, and the cursor and the selection of {input} are still
    /// where they were. A key
    /// would otherwise land somewhere the user never aimed it. When the text
    /// is no longer at the press's version, whoever wrote it, the key was
    /// pressed on text the user no longer sees, so it sends nothing.
    pub fn take_fallback(
        &mut self,
        ticket: u64,
        focus: KeyboardFocus,
        mode: InputMode,
        input: &InputAt,
    ) -> Option<Pressed> {
        let hold = self.hold.as_mut().filter(|hold| {
            hold.ticket == ticket
                && hold.focus == focus
                && hold.mode == mode
                && !hold.input.caret_moved(input)
        })?;
        let mut pressed = hold.pressed.take()?;
        if hold.input.version != input.version
            && let Pressed::Key(_, sight) = &mut pressed
        {
            sight.blind = true;
        }
        Some(pressed)
    }

    /// The fallback of {ticket} took the input from {before} to {after}. The
    /// user typed the keys behind it expecting that change, so it leaves them
    /// sighted, unless something else had changed the input first.
    pub fn fell_back(&mut self, ticket: u64, before: &InputAt, after: InputAt) {
        if let Some(hold) = self
            .hold
            .as_mut()
            .filter(|hold| hold.ticket == ticket && hold.input == *before)
        {
            hold.input = after;
        }
    }

    /// {input} is the chat input now. When its text, its cursor or its
    /// selection changed while the keys waited, whoever changed them, or the
    /// keyboard moved, they were typed blind to the change, see
    /// [`Sight::blind`].
    ///
    /// The keys follow the keyboard to {focus}, a window or a picker the
    /// handler opened included, except while a modal or a prompt is up that
    /// was not where they were typed: the user never saw it, so a held `y`
    /// would approve a call nobody read. Those keys are dropped, and this
    /// says whether there were any.
    pub fn release(&mut self, input: &InputAt, focus: KeyboardFocus) -> bool {
        let Some(hold) = self.hold.take() else {
            return false;
        };
        if focus.prompts.is_some() && focus.prompts != hold.focus.prompts {
            let dropped = self.queue.len();
            self.queue.clear();
            tracing::warn!(
                plugin = %hold.plugin,
                key = %hold.key.notation(),
                ticket = hold.ticket,
                dropped,
                "held keys dropped: a modal or a prompt opened while they waited"
            );
            return dropped > 0;
        }
        if hold.input != *input || hold.focus != focus {
            for (_, sight) in &mut self.queue {
                sight.blind = true;
            }
        }
        false
    }

    /// Takes over another tab's hold and queue, so its keys still wait for
    /// the same handler, now at {focus} on the input there, {input}. The press
    /// stays behind: a fallback would land in a tab the user never pressed it
    /// in. Keys a hold carried over were typed blind to the new tab.
    pub fn adopt(&mut self, from: Self, focus: KeyboardFocus, input: InputAt) {
        debug_assert!(
            self.hold.is_none() && self.queue.is_empty(),
            "only the focused tab holds keys"
        );
        let blind = from.hold.is_some();
        self.queue
            .extend(from.queue.into_iter().map(|(msg, mut sight)| {
                sight.blind |= blind;
                (msg, sight)
            }));
        if let Some(hold) = from.hold {
            self.hold = Some(Hold {
                focus,
                pressed: None,
                input,
                ..hold
            });
        }
    }

    /// The next message to replay. Stops as soon as a replayed key starts a
    /// handler of its own, and the rest wait for that one.
    pub fn next(&mut self) -> Option<(Msg, Sight, Turn)> {
        if self.hold.is_some() {
            return None;
        }
        let (msg, sight) = self.queue.pop_front()?;
        Some((msg, sight, Turn(())))
    }
}

/// Clicks place the cursor and drags select, so they are ordered with the
/// keys. A button release goes with its press. Scrolls and agent events do
/// not depend on the keys before them. Leaving and suspending cannot wait on
/// a plugin, and no plugin can bind those keys. Every other key waits, `Esc`
/// included, since a popup or a picker ahead of it may take it.
fn waits(msg: &Msg) -> bool {
    match msg {
        Msg::Key(key) => !is_reserved(*key),
        Msg::Paste(_) => true,
        Msg::Mouse(mouse) => matches!(
            mouse.kind,
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_)
        ),
        Msg::Scroll { .. } | Msg::Agent(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use test_case::test_case;

    const TYPING: KeyboardFocus = KeyboardFocus {
        window: None,
        input: true,
        prompts: None,
    };

    fn held_since(since: Instant) -> Typeahead {
        Typeahead {
            hold: Some(Hold {
                ticket: 1,
                since,
                focus: TYPING,
                mode: InputMode::Insert,
                plugin: Arc::from("plug"),
                key: Key::parse("<Tab>").unwrap(),
                pressed: None,
                input: InputAt {
                    version: 0,
                    cursor: 0,
                    anchor: None,
                },
            }),
            queue: VecDeque::new(),
        }
    }

    #[test_case(Duration::ZERO => false ; "a_handler_at_work_keeps_holding")]
    #[test_case(SLICE_WARN     => true  ; "a_slow_handler_lets_go")]
    fn a_hold_goes_stale(age: Duration) -> bool {
        let since = Instant::now();
        held_since(since).stale_at(since + age, || TYPING)
    }
}
