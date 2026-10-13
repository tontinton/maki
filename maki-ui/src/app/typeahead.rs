//! Key handlers run on the Lua thread while the user keeps typing. While a
//! handler runs, the keys after it wait here, and once it is done they take
//! the normal key path as if just typed. So a fast `dd` after a vim plugin's
//! `<Esc>` lands in normal mode, and the letter typed after a completion
//! `<Tab>` cannot change the text under it before the completion lands.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyEvent, MouseEventKind};
use maki_lua::{Key, KeybindTicket, PASTE, PluginReach, SLICE_WARN};

use super::Msg;

/// Where the keys go. A handler that opens a picker may not return until the
/// user closes it, so when the keyboard moves away from where the held keys
/// were typed, they stop waiting and follow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KeyboardFocus {
    pub window: Option<u32>,
    pub input: bool,
    /// While the agent waits on the user, on a permission prompt, a question
    /// or a plan form, how many such prompts have opened so far. These answer
    /// on a single key, so held keys typed before one opened must not answer
    /// it. The count tells a prompt that opened while they waited from one
    /// that was already there.
    pub agent_prompt: Option<u64>,
}

/// A key or a paste on its way through the bindings, kept in the hold so
/// `ev.fallback()` can still hand it to the host.
#[derive(Clone)]
pub(super) enum Pressed {
    Key(KeyEvent),
    Paste(String),
}

impl Pressed {
    /// What a binding for this is set on. A press no notation names has none.
    pub fn key(&self) -> Option<Key> {
        match self {
            Self::Key(event) => Key::from_event(*event),
            Self::Paste(_) => Some(PASTE),
        }
    }
}

pub(super) struct Hold {
    ticket: u64,
    since: Instant,
    focus: KeyboardFocus,
    plugin: Arc<str>,
    key: Key,
    /// Taken by the first `ev.fallback()`, so a second one does nothing.
    pressed: Option<Pressed>,
    /// The input's version at the press, to tell whether the handler changed
    /// it.
    version: u64,
    reach: PluginReach,
}

impl Hold {
    pub fn new(
        ticket: &KeybindTicket,
        focus: KeyboardFocus,
        pressed: Pressed,
        version: u64,
    ) -> Self {
        Self {
            ticket: ticket.id(),
            since: Instant::now(),
            focus,
            plugin: Arc::clone(ticket.plugin()),
            key: ticket.key(),
            pressed: Some(pressed),
            version,
            reach: ticket.reach().clone(),
        }
    }
}

#[derive(Default)]
pub(super) struct Typeahead {
    hold: Option<Hold>,
    queue: VecDeque<Msg>,
    /// The plugins whose handlers changed the input while keys waited. Those
    /// keys were typed before the change was on screen, so what they send is
    /// partly the plugin's text, and the send is held to its grants. Cleared
    /// once the last of them has run.
    blind_to: Vec<PluginReach>,
}

impl Typeahead {
    pub fn hold(&mut self, hold: Hold) {
        self.hold = Some(hold);
    }

    /// Keeps {msg} while a handler runs. Scrolls and agent events do not
    /// depend on the keys before them, so they never wait.
    pub fn defer(&mut self, msg: Msg) -> Option<Msg> {
        if self.hold.is_none() || !waits(&msg) {
            return Some(msg);
        }
        self.queue.push_back(msg);
        None
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
        let Some(hold) = &self.hold else {
            return false;
        };
        let slow = hold.since.elapsed() >= SLICE_WARN;
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

    /// The press behind {ticket}, while it still holds the keys after it and
    /// the keyboard is still at {focus}, where the user pressed it. The press
    /// is one of the keys a handler that changed the input since leaves blind.
    pub fn take_fallback(
        &mut self,
        ticket: u64,
        focus: KeyboardFocus,
        version: u64,
    ) -> Option<Pressed> {
        let hold = self
            .hold
            .as_mut()
            .filter(|hold| hold.ticket == ticket && hold.focus == focus)?;
        let pressed = hold.pressed.take()?;
        if hold.version != version {
            self.blind_to.push(hold.reach.clone());
        }
        Some(pressed)
    }

    /// {version} is the input's now, to tell whether the handler changed it
    /// under the keys still waiting.
    ///
    /// The keys follow the keyboard to {focus}, a window the handler opened
    /// included, except while an agent prompt is up that was not where they
    /// were typed: the user never saw it, so a held `y` would approve a call
    /// nobody read. Those keys are dropped, and the count of them is
    /// returned.
    pub fn release(&mut self, version: u64, focus: KeyboardFocus) -> usize {
        let Some(hold) = self.hold.take() else {
            return 0;
        };
        if focus.agent_prompt.is_some() && focus.agent_prompt != hold.focus.agent_prompt {
            let dropped = self.queue.len();
            self.queue.clear();
            tracing::warn!(
                plugin = %hold.plugin,
                key = %hold.key.notation(),
                ticket = hold.ticket,
                dropped,
                "held keys dropped: an agent prompt opened while they waited"
            );
            return dropped;
        }
        if hold.version != version && !self.queue.is_empty() {
            self.blind_to.push(hold.reach);
        }
        0
    }

    pub fn blind_to(&self) -> &[PluginReach] {
        &self.blind_to
    }

    /// Takes over another tab's hold and queue, so its keys still wait for
    /// the same handler, now at {focus} on an input at {version}. The press
    /// stays behind: a fallback would land in a tab the user never pressed it
    /// in.
    pub fn adopt(&mut self, from: Self, focus: KeyboardFocus, version: u64) {
        debug_assert!(
            self.hold.is_none() && self.queue.is_empty(),
            "only the focused tab holds keys"
        );
        if let Some(hold) = from.hold {
            self.hold = Some(Hold {
                focus,
                pressed: None,
                version,
                ..hold
            });
        }
        self.queue.extend(from.queue);
        self.blind_to.extend(from.blind_to);
    }

    /// The next message to replay. Stops as soon as a replayed key starts a
    /// handler of its own, and the rest wait for that one.
    pub fn next(&mut self) -> Option<Msg> {
        if self.hold.is_some() {
            return None;
        }
        let next = self.queue.pop_front();
        if next.is_none() {
            self.blind_to.clear();
        }
        next
    }
}

/// Clicks place the cursor and drags select, so they are ordered with the
/// keys. A button release goes with its press.
fn waits(msg: &Msg) -> bool {
    match msg {
        Msg::Key(_) | Msg::Paste(_) => true,
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
    use maki_lua::PluginPermissions;
    use std::time::Duration;
    use test_case::test_case;

    const TYPING: KeyboardFocus = KeyboardFocus {
        window: None,
        input: true,
        agent_prompt: None,
    };

    fn held_for(age: Duration) -> Typeahead {
        Typeahead {
            hold: Some(Hold {
                ticket: 1,
                since: Instant::now() - age,
                focus: TYPING,
                plugin: Arc::from("plug"),
                key: Key::parse("<Tab>").unwrap(),
                pressed: None,
                version: 0,
                reach: PluginReach::new(PluginPermissions::trusted()),
            }),
            queue: VecDeque::new(),
            blind_to: Vec::new(),
        }
    }

    #[test_case(Duration::ZERO => false ; "a_handler_at_work_keeps_holding")]
    #[test_case(SLICE_WARN     => true  ; "a_slow_handler_lets_go")]
    fn a_hold_goes_stale(age: Duration) -> bool {
        held_for(age).stale(|| TYPING)
    }
}
