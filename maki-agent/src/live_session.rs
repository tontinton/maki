use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use arc_swap::ArcSwap;
use event_listener::Event;
use maki_config::ModelPolicy;
use maki_providers::{Message, RequestOptions, TaskEnd, TaskSettled, Timeouts};
use maki_storage::id::{MakiId, SessionRef};
use thiserror::Error;

use crate::agent::default_instruction_files;
use crate::cancel::CancelToken;
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::tools::registry::HeaderFuture;
use crate::tools::{BackgroundFor, FileAccess, ToolContext, ToolRegistry, interpreter_ctx};
use crate::{AgentConfig, AgentMode, EventSender, ModelSlot, ToolOutputLines};

pub const BACKGROUND_PLAN_WRITE_DENIED: &str =
    "the chat is in plan mode, so background sessions can't change files";
const MAILBOX_CAPACITY: usize = 100;
const NOTICE_DISPLAY_MAX_BYTES: usize = 256;
const NOTICE_HEAD: &str = "Background";
const NOTICE_BODY_SEP: &str = ":\n\n";
/// A fork can drop the call a receipt came from, and then nobody knows the tool.
const LOST_TOOL: &str = "task";
const LOST_NOTICE_BODY: &str =
    "Maki exited or reloaded, or this session was forked, before it reported back.";
/// Nobody listens on an unattended context's events, so its run id is never read.
const UNATTENDED_RUN_ID: u64 = 0;

static LIVE_SESSIONS: LazyLock<Mutex<HashMap<MakiId, Weak<LiveSession>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every frontend registers the mailbox, but only an interactive agent loop
/// publishes a seed. The seed is held weakly, so it dies with its loop. The
/// mailbox lives on through a respawn, because the new loop registers before
/// the old one lets go.
#[derive(Default)]
struct LiveSession {
    mailbox: Mutex<Mailbox>,
    seed: Mutex<Weak<AgentSeed>>,
    /// Fires on every change a [`SessionMailbox::next_wake`] waiter waits for.
    changed: Event,
}

/// Besides the notices, it tracks every background receipt the model holds
/// until the answer to it is in a run's history: [`Self::outstanding`] while
/// the work runs, then the `settles` mark of its queued notice. A queued
/// answer is not read yet, so it still counts. Whatever is unsettled when the
/// process ends is what the session meta keeps, so a resumed model learns the
/// result is lost instead of waiting for it.
#[derive(Default)]
struct Mailbox {
    pending: VecDeque<Message>,
    wake: bool,
    outstanding: Vec<String>,
    /// No run reads it again, so it takes no new receipts.
    closed: bool,
}

/// How a notice names a task: its tool, plus the call's summary when that
/// says more than the tool name. The summary is squeezed onto one line, so
/// the head of a notice never wraps. That is how [`notice_body`] finds where
/// the head stops.
pub struct TaskLabel {
    tool: String,
    summary: Option<String>,
}

impl TaskLabel {
    pub fn new(tool: &str, summary: Option<&str>) -> Self {
        Self {
            tool: tool.to_owned(),
            summary: summary
                .map(one_line)
                .filter(|summary| !summary.is_empty() && summary != tool),
        }
    }

    /// Names receipt `id` after its call in `history`. A plugin tool's header
    /// comes from the Lua thread, so it may still be on its way. Then it is
    /// handed back to wait for.
    fn of_receipt(history: &[Message], id: &str) -> (Self, Option<HeaderFuture>) {
        let call = history
            .iter()
            .flat_map(Message::tool_uses)
            .find(|(call_id, _, _)| *call_id == id);
        let Some((_, tool, input)) = call else {
            return (Self::new(LOST_TOOL, None), None);
        };
        match ToolRegistry::global().start_header(tool, input) {
            Some(HeaderFuture::Ready(header)) => (Self::new(tool, Some(&header.text())), None),
            late => (Self::new(tool, None), late),
        }
    }
}

/// The result of a background task, for the run that holds its receipt.
pub struct BackgroundNotice<'a> {
    pub text: String,
    pub display: Option<&'a str>,
    pub wake: bool,
    pub end: TaskEnd,
}

impl Mailbox {
    /// A full mailbox drops its oldest notice that settles nothing. One that
    /// settles a receipt is never dropped: the model waits for it, and nothing
    /// would report it lost. Receipts bound those anyway.
    fn push(&mut self, notice: Message, wake: bool) {
        if self.pending.len() >= MAILBOX_CAPACITY
            && let Some(evicted) = self
                .pending
                .iter()
                .position(|queued| queued.settles.is_none())
        {
            self.pending.remove(evicted);
        }
        self.pending.push_back(notice);
        self.wake |= wake;
    }

    fn take(&mut self) -> Vec<Message> {
        self.wake = false;
        self.pending.drain(..).collect()
    }

    fn unsettled(&self) -> impl Iterator<Item = &String> {
        let queued = self
            .pending
            .iter()
            .filter_map(|notice| notice.settles.as_ref());
        self.outstanding
            .iter()
            .chain(queued.map(|settled| &settled.task))
    }

    /// Answers `id` with a quiet notice that its result is lost. It stays
    /// unsettled until a run drains it, so quitting before that keeps it.
    fn push_lost(&mut self, id: &str, label: &TaskLabel) {
        self.push(lost_notice(id, label), false);
    }

    /// A notice a run already read stays the way the model saw it.
    fn relabel_lost(&mut self, id: &str, label: &TaskLabel) {
        let queued = self.pending.iter_mut().find(|notice| {
            notice
                .settles
                .as_ref()
                .is_some_and(|settled| settled.task == id && settled.end == TaskEnd::Lost)
        });
        if let Some(notice) = queued {
            *notice = lost_notice(id, label);
        }
    }

    /// Stops tracking `id` as running work. False when nothing tracked it.
    fn untrack(&mut self, id: &str) -> bool {
        let Some(index) = self.outstanding.iter().position(|tracked| tracked == id) else {
            return false;
        };
        self.outstanding.swap_remove(index);
        true
    }
}

fn lost_notice(id: &str, label: &TaskLabel) -> Message {
    let (text, display) = notice(label, id, TaskEnd::Lost, TaskEnd::Lost, LOST_NOTICE_BODY);
    settling(id, TaskEnd::Lost, text, Some(&display))
}

fn end_word(end: TaskEnd) -> &'static str {
    match end {
        TaskEnd::Finished => "finished",
        TaskEnd::Failed => "failed",
        TaskEnd::Cancelled => "cancelled",
        TaskEnd::Lost => "lost",
    }
}

/// Returns the text the model reads and the line the transcript shows. They
/// can disagree on the ending: a stopped command often exits with an error,
/// so the model reads `failed` while the user, who stopped it, sees
/// `cancelled`. [`notice_body`] parses the text back, so its shape lives
/// here alone.
fn notice(
    label: &TaskLabel,
    id: &str,
    read: TaskEnd,
    shown: TaskEnd,
    body: &str,
) -> (String, String) {
    let TaskLabel { tool, summary } = label;
    let (named, summarized) = summary
        .as_ref()
        .map(|summary| (format!(" ({summary})"), format!(": {summary}")))
        .unwrap_or_default();
    (
        format!(
            "{NOTICE_HEAD} {tool} {id}{named} {}{NOTICE_BODY_SEP}{body}",
            end_word(read)
        ),
        format!("{NOTICE_HEAD} {tool} {}{summarized}", end_word(shown)),
    )
}

/// The notice a moved call's final result becomes: the text the model reads,
/// the line the transcript shows, and how the task ended.
pub fn result_notice(
    label: &TaskLabel,
    id: &str,
    failed: bool,
    stopped: bool,
    output: &str,
) -> (String, String, TaskEnd) {
    let read = if failed {
        TaskEnd::Failed
    } else {
        TaskEnd::Finished
    };
    let shown = if stopped { TaskEnd::Cancelled } else { read };
    let (text, display) = notice(label, id, read, shown, output);
    (text, display, shown)
}

/// The part of a [`notice`] text after its head line, for a transcript line
/// that already names the task. Older builds did not squeeze the summary
/// onto one line, so a head that wraps is not trusted and the whole text
/// comes back.
pub fn notice_body(text: &str) -> &str {
    text.strip_prefix(NOTICE_HEAD)
        .and_then(|rest| rest.split_once(NOTICE_BODY_SEP))
        .filter(|(head, _)| !head.contains('\n'))
        .map_or(text, |(_, body)| body)
}

/// How a receipt names the task its call moved to.
pub fn receipt_tag(id: &str) -> String {
    format!("(task {id})")
}

fn observation(text: String, display: Option<&str>) -> Message {
    let mut message = Message::observation(text);
    message.display_text = display.and_then(notice_label);
    message
}

fn settling(task: &str, end: TaskEnd, text: String, display: Option<&str>) -> Message {
    Message {
        settles: Some(TaskSettled {
            task: task.to_owned(),
            end,
        }),
        ..observation(text, display)
    }
}

#[derive(Debug, Error)]
#[error("session not live: {0}")]
pub struct MailboxError(MakiId);

#[derive(Clone)]
pub struct SessionMailbox {
    session_id: MakiId,
    live: Arc<LiveSession>,
}

/// A plugin that starts a subagent outside of any tool call has no context
/// to clone, so it builds one from this.
pub struct AgentSeed {
    pub session_id: SessionRef,
    pub model_slot: Arc<ArcSwap<ModelSlot>>,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    /// The instruction file list background sessions resolve against. Starts
    /// at the built-in list and is swapped in by the owning loop's
    /// instruction reload.
    pub instruction_files: ArcSwap<Vec<String>>,
    pub mcp: Option<McpSession>,
    pub timeouts: Timeouts,
    pub file_access: Arc<FileAccess>,
    pub model_policy: Arc<ModelPolicy>,
    /// The chat's thinking and whether it plans, as the user has them right
    /// now. The UI keeps them fresh even before the first message, so
    /// background work never thinks harder than the user asked. We keep a bool
    /// and not the mode on purpose. Handed to a background run, the mode would
    /// bring the plan prompt along, plus a free pass to write the plan file.
    chat_settings: Mutex<(RequestOptions, bool)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One line, cut on a character boundary, or `None` when nothing is left.
fn notice_label(text: &str) -> Option<String> {
    let line = one_line(text);
    let label = line[..line.floor_char_boundary(NOTICE_DISPLAY_MAX_BYTES)].trim_end();
    (!label.is_empty()).then(|| label.to_owned())
}

impl SessionMailbox {
    pub fn register(session_id: MakiId) -> Self {
        let mut sessions = lock(&LIVE_SESSIONS);
        if let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) {
            return Self { session_id, live };
        }

        let live = Arc::new(LiveSession::default());
        sessions.insert(session_id, Arc::downgrade(&live));
        Self { session_id, live }
    }

    /// `display` becomes the observation's transcript line, shown and saved
    /// once an agent run accepts it. The model only ever reads `text`.
    pub fn notify(
        session_id: MakiId,
        text: String,
        display: Option<&str>,
        wake: bool,
    ) -> Result<(), MailboxError> {
        let mailbox = Self::open(session_id)?;
        mailbox.change(|state| state.push(observation(text, display), wake));
        Ok(())
    }

    /// The mailbox of a live session, never a new one.
    pub fn open(session_id: MakiId) -> Result<Self, MailboxError> {
        let mut sessions = lock(&LIVE_SESSIONS);
        let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) else {
            sessions.remove(&session_id);
            return Err(MailboxError(session_id));
        };
        Ok(Self { session_id, live })
    }

    pub fn session_id(&self) -> MakiId {
        self.session_id
    }

    pub fn drain(&self) -> Vec<Message> {
        lock(&self.live.mailbox).take()
    }

    pub fn claim_wake(&self) -> Vec<Message> {
        let mut mailbox = lock(&self.live.mailbox);
        if !mailbox.wake {
            return Vec::new();
        }
        mailbox.take()
    }

    /// The model holds a receipt for `id` and waits for its result. `false`
    /// once [`Self::close`]d: nobody would read the result.
    pub fn track(&self, id: &str) -> bool {
        self.change(|state| {
            if !state.closed && !state.outstanding.iter().any(|known| known == id) {
                state.outstanding.push(id.to_owned());
            }
            !state.closed
        })
    }

    pub fn close(&self) {
        lock(&self.live.mailbox).closed = true;
    }

    /// The work behind `id` ended. `None` when the model already has the
    /// answer (its own stop waited for it). A notice for a receipt nobody
    /// tracks is dropped, so a result never answers a call the model no
    /// longer made.
    pub fn settle(&self, id: &str, notice: Option<BackgroundNotice<'_>>) {
        self.change(|state| {
            if !state.untrack(id) {
                tracing::debug!(id, "result of a background task nobody waits for");
                return;
            }
            if let Some(notice) = notice {
                let message = settling(id, notice.end, notice.text, notice.display);
                state.push(message, notice.wake);
            }
        });
    }

    /// The receipt for `id` left the history (a rewind): nothing may answer it.
    pub fn forget(&self, id: &str) {
        self.change(|state| {
            state.untrack(id);
            state.pending.retain(|notice| {
                notice
                    .settles
                    .as_ref()
                    .is_none_or(|settled| settled.task != id)
            });
            state.wake &= !state.pending.is_empty();
        });
    }

    /// Every receipt whose answer is not in a run's history yet.
    pub fn unsettled(&self) -> Vec<String> {
        lock(&self.live.mailbox).unsettled().cloned().collect()
    }

    /// How the answer still queued for `id` says its task ended. `None` while
    /// the work runs or once a run took the answer.
    pub fn queued_end(&self, id: &str) -> Option<TaskEnd> {
        lock(&self.live.mailbox)
            .pending
            .iter()
            .filter_map(|notice| notice.settles.as_ref())
            .find(|settled| settled.task == id)
            .map(|settled| settled.end)
    }

    /// Background work still running for this mailbox's receipts.
    pub fn outstanding(&self) -> usize {
        lock(&self.live.mailbox).outstanding.len()
    }

    /// The receipts a stored session left unsettled, each answered with a
    /// quiet notice that its result is lost. One this process still tracks is
    /// skipped: its work runs here, or [`Self::lose`] already answered it.
    ///
    /// On a resume the plugins are still loading when this runs, so a plugin
    /// tool cannot name its call yet. The notice goes out with just the tool
    /// name and picks up the summary when the plugin answers. A host that
    /// goes away first answers with the tool name, so no task waits on a dead
    /// host.
    pub fn restore_lost(&self, ids: &[String], history: &[Message]) {
        let late = self.change(|state| {
            let mut late = Vec::new();
            for id in ids {
                if state.unsettled().any(|unsettled| unsettled == id) {
                    continue;
                }
                let (label, header) = TaskLabel::of_receipt(history, id);
                state.push_lost(id, &label);
                if let Some(header) = header {
                    late.push((id.clone(), label.tool, header));
                }
            }
            late
        });
        for (id, tool, header) in late {
            let mailbox = self.clone();
            tokio::spawn(async move {
                let label = TaskLabel::new(&tool, Some(&header.await.text()));
                mailbox.change(|state| state.relabel_lost(&id, &label));
            });
        }
    }

    /// The work behind `id` died without a result, with the plugin host that
    /// ran it (`/reload`, exit). Nothing else would ever settle it.
    pub fn lose(&self, id: &str, label: &TaskLabel) {
        self.change(|state| {
            if state.untrack(id) {
                state.push_lost(id, label);
            }
        });
    }

    /// The notices of the next wake, waiting for one while background work
    /// is outstanding. `None` once nothing is out and nothing woke: quiet
    /// notices stay queued for whatever run comes next.
    pub async fn next_wake(&self) -> Option<Vec<Message>> {
        self.wait_for_wake(false).await
    }

    /// [`Self::next_wake`] for an agent whose caller waits on its answer.
    /// Once nothing is out, no later run of it would read a quiet notice,
    /// like the cut output of a call the user stopped, so that is its last
    /// wake.
    pub async fn next_wake_before_answer(&self) -> Option<Vec<Message>> {
        self.wait_for_wake(true).await
    }

    async fn wait_for_wake(&self, take_quiet: bool) -> Option<Vec<Message>> {
        loop {
            let listener = self.live.changed.listen();
            {
                let mut mailbox = lock(&self.live.mailbox);
                if mailbox.wake {
                    return Some(mailbox.take());
                }
                if mailbox.outstanding.is_empty() {
                    return (take_quiet && !mailbox.pending.is_empty()).then(|| mailbox.take());
                }
            }
            listener.await;
        }
    }

    fn change<T>(&self, apply: impl FnOnce(&mut Mailbox) -> T) -> T {
        let changed = apply(&mut lock(&self.live.mailbox));
        self.live.changed.notify(usize::MAX);
        changed
    }

    /// The registry only keeps a weak link, so the caller has to hold on to
    /// the seed. A rewind publishes a new seed, and it starts with the old chat
    /// settings, so background work never sees the defaults in the short gap
    /// before the UI sends them again. The file locks carry over too, because
    /// background sessions opened before the rewind still edit through the old
    /// ones, and their edits have to keep taking turns with the new loop's.
    pub fn publish_seed(&self, mut seed: AgentSeed) -> Arc<AgentSeed> {
        let mut published = lock(&self.live.seed);
        if let Some(previous) = published.upgrade() {
            *lock(&seed.chat_settings) = *lock(&previous.chat_settings);
            seed.file_access = previous.file_access.sharing_locks();
        }
        let seed = Arc::new(seed);
        *published = Arc::downgrade(&seed);
        seed
    }

    pub fn seed(&self) -> Option<Arc<AgentSeed>> {
        lock(&self.live.seed).upgrade()
    }
}

impl Drop for SessionMailbox {
    fn drop(&mut self) {
        if Arc::strong_count(&self.live) != 1 {
            return;
        }
        let weak = Arc::downgrade(&self.live);
        let mut sessions = lock(&LIVE_SESSIONS);
        if Arc::strong_count(&self.live) == 1
            && sessions
                .get(&self.session_id)
                .is_some_and(|registered| registered.ptr_eq(&weak))
        {
            sessions.remove(&self.session_id);
        }
    }
}

impl AgentSeed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionRef,
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: Arc<PermissionManager>,
        mcp: Option<McpSession>,
        timeouts: Timeouts,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        Self {
            session_id,
            model_slot,
            config,
            tool_output_lines,
            permissions,
            mcp,
            timeouts,
            file_access: FileAccess::fresh(),
            model_policy,
            chat_settings: Mutex::default(),
            instruction_files: ArcSwap::from_pointee(default_instruction_files()),
        }
    }

    /// `None` when the session is closed or has no interactive loop, like a
    /// headless or ACP run.
    pub fn live(session_id: MakiId) -> Option<Arc<Self>> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        lock(&live.seed).upgrade()
    }

    pub fn set_chat_settings(&self, opts: RequestOptions, mode: &AgentMode) {
        *lock(&self.chat_settings) = (opts, mode.plan_path().is_some());
    }

    pub fn chat_opts(&self) -> RequestOptions {
        lock(&self.chat_settings).0
    }

    /// Why background work for `chat` may not change a file right now.
    ///
    /// That work always runs in build mode, but plan mode still means "leave
    /// my files alone". So every write asks here, and a switch to plan mode
    /// also covers the next write of a prompt that is already running. A chat
    /// that is gone can't say it isn't planning, so that is a no too.
    pub fn background_write_block(chat: MakiId) -> Option<String> {
        let Some(seed) = Self::live(chat) else {
            return Some(MailboxError(chat).to_string());
        };
        let (_, planning) = *lock(&seed.chat_settings);
        planning.then(|| BACKGROUND_PLAN_WRITE_DENIED.to_owned())
    }

    /// No chat shows this work, so its events go to a channel nobody reads.
    /// Nobody can answer a permission prompt either, so there is no answer
    /// channel and `enforce` denies the call instead of waiting.
    pub fn tool_context(&self, cancel: CancelToken) -> ToolContext {
        let (sink, _) = flume::unbounded();
        let slot = self.model_slot.load();
        ToolContext {
            provider: Arc::clone(&slot.provider),
            model: Arc::new(slot.model.clone()),
            session_id: Some(self.session_id.clone()),
            background_for: BackgroundFor::Chat(self.session_id.id()),
            mcp: self.mcp.clone(),
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            timeouts: self.timeouts,
            opts: self.chat_opts(),
            model_policy: Arc::clone(&self.model_policy),
            instruction_files: self.instruction_files.load_full(),
            ..interpreter_ctx(
                &AgentMode::Build,
                &EventSender::new(sink, UNATTENDED_RUN_ID),
                cancel,
                Arc::clone(&self.permissions),
                self.file_access.sharing_locks(),
                None,
                Arc::clone(ToolRegistry::global_arc()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use futures::FutureExt;
    use maki_providers::{ContentBlock, Role, ThinkingConfig};
    use test_case::test_case;

    use super::*;
    use crate::tools::FileKey;
    use crate::tools::test_support::{future_mtime, stub_seed};

    const CHAT_OPTS: RequestOptions = RequestOptions {
        thinking: ThinkingConfig::Adaptive,
        fast: true,
    };
    const PLAN_FILE: &str = "/tmp/plan.md";
    const EDITED_FILE: &str = "foo.rs";
    const NOTICE_BODY: &str = "child finished";
    const LABEL_HEAD: &str = "Background agent finished:";
    const WIDE_CHAR: &str = "é";

    fn text(message: &Message) -> &str {
        message.first_text_content().unwrap()
    }

    #[test]
    fn notify_cuts_the_label_to_one_line_on_a_char_boundary() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let wide = WIDE_CHAR.repeat(NOTICE_DISPLAY_MAX_BYTES);
        let kept = (NOTICE_DISPLAY_MAX_BYTES - LABEL_HEAD.len() - 1) / WIDE_CHAR.len();
        let expected = format!("{LABEL_HEAD} {}", WIDE_CHAR.repeat(kept));
        assert_eq!(
            expected.len(),
            NOTICE_DISPLAY_MAX_BYTES - 1,
            "the byte limit must fall inside a character"
        );
        let label = format!("{LABEL_HEAD}\n {wide}");

        SessionMailbox::notify(id, NOTICE_BODY.into(), Some(&label), false).unwrap();

        let message = mailbox.drain().remove(0);
        assert_eq!(message.display_text, Some(expected));
        assert_eq!(text(&message), NOTICE_BODY);
    }

    #[test_case(None ; "absent")]
    #[test_case(Some(" \n ") ; "blank")]
    fn notify_leaves_a_notice_without_a_label_unlabelled(label: Option<&str>) {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);

        SessionMailbox::notify(id, NOTICE_BODY.into(), label, false).unwrap();

        let message = mailbox.drain().remove(0);
        assert_eq!(message.display_text, None);
        assert_eq!(text(&message), NOTICE_BODY);
    }

    #[test]
    fn notifications_drain_in_order_and_clear_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "first".into(), None, true).unwrap();
        SessionMailbox::notify(id, "second".into(), None, true).unwrap();

        let messages = mailbox.drain();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(messages.iter().all(Message::is_observation));
        assert!(mailbox.claim_wake().is_empty());
    }

    #[test]
    fn quiet_notifications_do_not_claim_a_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), None, false).unwrap();

        assert!(mailbox.claim_wake().is_empty());
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn waking_notification_claims_all_pending_messages() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "quiet".into(), None, false).unwrap();
        SessionMailbox::notify(id, "wake".into(), None, true).unwrap();

        let messages = mailbox.claim_wake();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["quiet", "wake"]
        );
        assert!(mailbox.drain().is_empty());
    }

    #[test]
    fn notifications_drop_the_oldest_message_at_capacity() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        for index in 0..=MAILBOX_CAPACITY {
            SessionMailbox::notify(id, index.to_string(), None, false).unwrap();
        }

        let messages = mailbox.drain();
        assert_eq!(messages.len(), MAILBOX_CAPACITY);
        assert_eq!(text(&messages[0]), "1");
        assert_eq!(text(messages.last().unwrap()), MAILBOX_CAPACITY.to_string());
    }

    #[test]
    fn registrations_for_the_same_id_share_state() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), None, false).unwrap();

        assert_eq!(second.drain().len(), 1);
        assert!(first.drain().is_empty());
    }

    #[test]
    fn dropping_the_last_registration_closes_the_mailbox() {
        let id = MakiId::generate();
        drop(SessionMailbox::register(id));

        assert!(!lock(&LIVE_SESSIONS).contains_key(&id));
        assert!(SessionMailbox::notify(id, "late".into(), None, false).is_err());
    }

    #[test]
    fn dropping_one_registration_keeps_the_shared_mailbox() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);

        drop(first);

        assert!(lock(&LIVE_SESSIONS).contains_key(&id));
        SessionMailbox::notify(id, "built".into(), None, false).unwrap();
        assert_eq!(second.drain().len(), 1);
    }

    #[test]
    fn stale_drop_does_not_remove_a_replacement() {
        let id = MakiId::generate();
        let stale = SessionMailbox::register(id);
        let replacement = SessionMailbox {
            session_id: id,
            live: Arc::default(),
        };
        lock(&LIVE_SESSIONS).insert(id, Arc::downgrade(&replacement.live));

        drop(stale);
        SessionMailbox::notify(id, "built".into(), None, false).unwrap();

        assert_eq!(replacement.drain().len(), 1);
    }

    #[test]
    fn legacy_and_canonical_ids_address_the_same_mailbox() {
        let legacy: MakiId = "01965087-4c71-7f00-8000-000000000001".parse().unwrap();
        let canonical: MakiId = legacy.to_string().parse().unwrap();
        let mailbox = SessionMailbox::register(legacy);
        SessionMailbox::notify(canonical, "built".into(), None, false).unwrap();

        assert_eq!(mailbox.drain().len(), 1);
    }

    const TASK_ID: &str = "toolu_bg";
    const OTHER_TASK_ID: &str = "toolu_other";
    const TOOL: &str = "bash";
    const SUMMARY: &str = "Run the (slow) tests: all of them";
    const LOST_IS_QUIET: &str = "a lost notice must not wake";
    const QUIET_NOTICE: &str = "quiet";

    fn result(wake: bool) -> Option<BackgroundNotice<'static>> {
        Some(BackgroundNotice {
            text: NOTICE_BODY.into(),
            display: None,
            wake,
            end: TaskEnd::Finished,
        })
    }

    #[test]
    fn settled_notice_stays_unsettled_until_a_run_drains_it() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        assert_eq!(mailbox.unsettled(), [TASK_ID]);
        assert_eq!(mailbox.outstanding(), 1);

        mailbox.settle(TASK_ID, result(true));
        assert_eq!(mailbox.outstanding(), 0);
        assert_eq!(mailbox.unsettled(), [TASK_ID]);

        assert_eq!(text(&mailbox.claim_wake()[0]), NOTICE_BODY);
        assert!(mailbox.unsettled().is_empty());
    }

    #[test_case(true ; "forgotten")]
    #[test_case(false ; "never_tracked")]
    fn settle_drops_the_result_of_an_untracked_receipt(tracked_then_forgotten: bool) {
        let mailbox = SessionMailbox::register(MakiId::generate());
        if tracked_then_forgotten {
            mailbox.track(TASK_ID);
            mailbox.forget(TASK_ID);
        }

        mailbox.settle(TASK_ID, result(true));

        assert!(mailbox.drain().is_empty());
        assert!(mailbox.unsettled().is_empty());
    }

    #[test]
    fn settle_without_a_notice_only_untracks() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);

        mailbox.settle(TASK_ID, None);

        assert!(mailbox.unsettled().is_empty());
        assert!(mailbox.drain().is_empty());
    }

    #[tokio::test]
    async fn forgetting_the_only_waking_result_leaves_nothing_to_wake_for() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        mailbox.settle(TASK_ID, result(true));

        mailbox.forget(TASK_ID);

        assert!(mailbox.claim_wake().is_empty());
        assert!(mailbox.next_wake().await.is_none());
    }

    #[test]
    fn a_full_mailbox_drops_plain_notices_before_results() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        mailbox.track(TASK_ID);
        mailbox.settle(TASK_ID, result(false));
        for index in 0..MAILBOX_CAPACITY {
            SessionMailbox::notify(id, index.to_string(), None, false).unwrap();
        }

        assert_eq!(mailbox.unsettled(), [TASK_ID]);
        assert_eq!(text(&mailbox.drain()[0]), NOTICE_BODY);
    }

    #[test]
    fn a_mailbox_full_of_results_keeps_every_one() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let ids: Vec<String> = (0..=MAILBOX_CAPACITY).map(|i| i.to_string()).collect();
        for id in &ids {
            mailbox.track(id);
            mailbox.settle(id, result(false));
        }

        assert_eq!(mailbox.unsettled(), ids);
        assert_eq!(mailbox.drain().len(), ids.len());
    }

    #[test]
    fn forget_drops_a_queued_result() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        mailbox.track(OTHER_TASK_ID);
        mailbox.settle(TASK_ID, result(false));
        mailbox.settle(OTHER_TASK_ID, result(false));

        mailbox.forget(TASK_ID);

        assert_eq!(mailbox.unsettled(), [OTHER_TASK_ID]);
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn restore_lost_answers_each_stored_receipt_once_and_skips_live_ones() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let stored = [TASK_ID.to_owned(), OTHER_TASK_ID.to_owned()];

        mailbox.restore_lost(&stored, &[]);
        mailbox.restore_lost(&stored, &[]);

        assert_eq!(mailbox.unsettled(), [TASK_ID, OTHER_TASK_ID]);
        assert!(mailbox.claim_wake().is_empty(), "{LOST_IS_QUIET}");
        let lost = mailbox.drain();
        assert_eq!(lost.len(), 1);
        assert!(text(&lost[0]).contains(OTHER_TASK_ID));
        assert!(text(&lost[0]).ends_with(LOST_NOTICE_BODY));
        assert_eq!(mailbox.unsettled(), [TASK_ID]);
    }

    #[test]
    fn lose_answers_outstanding_work_once_and_quietly() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);

        mailbox.lose(TASK_ID, &label(Some(SUMMARY)));
        mailbox.lose(TASK_ID, &label(Some(SUMMARY)));
        mailbox.lose(OTHER_TASK_ID, &label(Some(SUMMARY)));
        mailbox.restore_lost(&[TASK_ID.to_owned()], &[]);

        assert_eq!(mailbox.outstanding(), 0);
        assert_eq!(mailbox.unsettled(), [TASK_ID]);
        assert!(mailbox.claim_wake().is_empty(), "{LOST_IS_QUIET}");
        let lost = mailbox.drain();
        assert_eq!(lost.len(), 1);
        assert!(text(&lost[0]).ends_with(LOST_NOTICE_BODY));
        assert_eq!(lost[0].settles, settled(TaskEnd::Lost));
    }

    fn label(summary: Option<&str>) -> TaskLabel {
        TaskLabel::new(TOOL, summary)
    }

    fn lost_line(mailbox: &SessionMailbox) -> Option<String> {
        mailbox.drain().pop()?.display_text
    }

    #[test]
    fn a_late_header_names_the_lost_notice_still_queued() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        mailbox.lose(TASK_ID, &label(None));

        mailbox.change(|state| state.relabel_lost(TASK_ID, &label(Some(SUMMARY))));

        let lost = mailbox.drain();
        assert_eq!(
            lost[0].display_text,
            Some(format!("{NOTICE_HEAD} {TOOL} lost: {SUMMARY}"))
        );
        assert!(text(&lost[0]).contains(SUMMARY));
        mailbox.change(|state| state.relabel_lost(TASK_ID, &label(Some(SUMMARY))));
        assert!(mailbox.drain().is_empty());
    }

    #[test_case(true,  TOOL      ; "its_call_in_history")]
    #[test_case(false, LOST_TOOL ; "no_call")]
    fn a_restored_lost_notice_names_the_call_in_history(in_history: bool, tool: &str) {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let call = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(TASK_ID, TOOL, serde_json::json!({}))],
            ..Default::default()
        };
        let history = if in_history { vec![call] } else { Vec::new() };

        mailbox.restore_lost(&[TASK_ID.to_owned()], &history);

        assert_eq!(
            lost_line(&mailbox),
            Some(format!("{NOTICE_HEAD} {tool} lost"))
        );
    }

    fn settled(end: TaskEnd) -> Option<TaskSettled> {
        Some(TaskSettled {
            task: TASK_ID.into(),
            end,
        })
    }

    #[test_case(false, false, TaskEnd::Finished ; "finished")]
    #[test_case(true,  false, TaskEnd::Failed   ; "failed")]
    #[test_case(true,  true,  TaskEnd::Cancelled ; "stopped")]
    fn a_result_notice_marks_the_receipt_it_settles(failed: bool, stopped: bool, end: TaskEnd) {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let (text, display, shown) =
            result_notice(&label(Some(SUMMARY)), TASK_ID, failed, stopped, NOTICE_BODY);

        mailbox.settle(
            TASK_ID,
            Some(BackgroundNotice {
                text,
                display: Some(&display),
                wake: false,
                end: shown,
            }),
        );

        assert_eq!(mailbox.drain()[0].settles, settled(end));
    }

    #[test]
    fn a_plugin_notice_in_the_same_words_settles_nothing() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let (text, display, _) =
            result_notice(&label(Some(SUMMARY)), TASK_ID, false, false, NOTICE_BODY);

        SessionMailbox::notify(mailbox.session_id(), text, Some(&display), false).unwrap();

        assert_eq!(mailbox.drain()[0].settles, None);
    }

    #[test_case(Some(SUMMARY), NOTICE_BODY ; "plain")]
    #[test_case(None, NOTICE_BODY ; "unnamed")]
    #[test_case(Some(SUMMARY), "tests:\n\nall green" ; "output_with_the_separator")]
    #[test_case(Some(SUMMARY), "" ; "empty_output")]
    #[test_case(Some("Wait:\n\nthen print"), NOTICE_BODY ; "summary_with_the_separator")]
    #[test_case(Some("Wait\nthen print"), NOTICE_BODY ; "summary_with_a_line_break")]
    fn notice_body_is_what_follows_the_head(summary: Option<&str>, output: &str) {
        let (text, display, _) = result_notice(&label(summary), TASK_ID, false, false, output);

        assert_eq!(notice_body(&text), output);
        assert!(!display.contains('\n'), "{display}");
    }

    #[test_case(Some(TOOL), None ; "the_tool_name_says_nothing_more")]
    #[test_case(Some("  "), None ; "blank")]
    #[test_case(Some("Run\n  the tests"), Some("Run the tests") ; "kept_to_one_line")]
    fn a_label_summary_names_more_than_the_tool(summary: Option<&str>, kept: Option<&str>) {
        assert_eq!(label(summary).summary.as_deref(), kept);
    }

    #[test]
    fn notice_body_keeps_other_text_whole() {
        assert_eq!(notice_body(NOTICE_BODY), NOTICE_BODY);
        let spanning = format!("{NOTICE_HEAD} note\nsaid{NOTICE_BODY_SEP}{NOTICE_BODY}");
        assert_eq!(notice_body(&spanning), spanning);
    }

    #[tokio::test]
    async fn next_wake_waits_while_work_is_outstanding() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        SessionMailbox::notify(mailbox.session_id(), QUIET_NOTICE.into(), None, false).unwrap();
        assert!(mailbox.next_wake().await.is_none());

        mailbox.track(TASK_ID);
        let mut wake = Box::pin(mailbox.next_wake());
        assert!(wake.as_mut().now_or_never().is_none());

        mailbox.settle(TASK_ID, result(true));
        let woken = wake.await.unwrap();
        assert_eq!(
            woken.iter().map(text).collect::<Vec<_>>(),
            [QUIET_NOTICE, NOTICE_BODY]
        );
        assert!(mailbox.next_wake().await.is_none());
    }

    #[tokio::test]
    async fn next_wake_before_answer_takes_a_quiet_last_result() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let mut wake = Box::pin(mailbox.next_wake_before_answer());
        assert!(wake.as_mut().now_or_never().is_none());

        mailbox.settle(TASK_ID, result(false));
        let woken = wake.await.unwrap();
        assert_eq!(woken.iter().map(text).collect::<Vec<_>>(), [NOTICE_BODY]);
        assert!(mailbox.next_wake_before_answer().await.is_none());
    }

    #[test]
    fn open_never_creates_a_mailbox() {
        let id = MakiId::generate();
        assert!(SessionMailbox::open(id).is_err());
        let registered = SessionMailbox::register(id);
        assert_eq!(SessionMailbox::open(id).unwrap().session_id(), id);
        drop(registered);
        assert!(SessionMailbox::open(id).is_err());
    }

    #[test]
    fn seed_is_live_only_while_its_owner_holds_it() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        assert!(AgentSeed::live(id).is_none());

        let owned = mailbox.publish_seed(stub_seed(id));
        assert!(Arc::ptr_eq(&AgentSeed::live(id).unwrap(), &owned));

        drop(owned);
        assert!(AgentSeed::live(id).is_none());
        SessionMailbox::notify(id, "built".into(), None, true).unwrap();
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn respawned_seed_wins_and_keeps_the_chat_settings() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let old = mailbox.publish_seed(stub_seed(id));
        old.set_chat_settings(CHAT_OPTS, &AgentMode::Plan(PLAN_FILE.into()));

        let new = mailbox.publish_seed(stub_seed(id));
        drop(old);

        let live = AgentSeed::live(id).unwrap();
        assert!(Arc::ptr_eq(&live, &new));
        let ctx = live.tool_context(CancelToken::none());
        assert_eq!(ctx.opts, CHAT_OPTS);
        assert_eq!(ctx.mode, AgentMode::Build);
        assert_eq!(ctx.background_for.chat(), Some(id));
        assert_eq!(
            AgentSeed::background_write_block(id).as_deref(),
            Some(BACKGROUND_PLAN_WRITE_DENIED)
        );
    }

    /// A background session reads the seed's list: the built-in default until
    /// the owning loop's instruction reload swaps in the resolved one.
    #[test]
    fn tool_context_reads_the_seeds_current_instruction_files() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let seed = mailbox.publish_seed(stub_seed(id));

        assert_eq!(
            seed.tool_context(CancelToken::none()).instruction_files,
            Arc::new(default_instruction_files())
        );

        let resolved = Arc::new(vec!["RULES.md".to_owned()]);
        seed.instruction_files.store(Arc::clone(&resolved));
        assert_eq!(
            seed.tool_context(CancelToken::none()).instruction_files,
            resolved
        );
    }

    /// A chat that is gone can't tell us it stopped planning.
    #[test]
    fn background_writes_stop_once_the_chat_is_gone() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let seed = mailbox.publish_seed(stub_seed(id));
        assert_eq!(AgentSeed::background_write_block(id), None);

        drop(seed);
        assert_eq!(
            AgentSeed::background_write_block(id),
            Some(MailboxError(id).to_string())
        );
    }

    /// The loop reads a file, the user edits it, and a monitor reads it too.
    /// The loop must still be told its view is stale. After a rewind the
    /// monitor's edits must still wait for the new loop's.
    #[tokio::test]
    async fn background_reads_stay_apart_but_locks_survive_a_respawn() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(EDITED_FILE);
        fs::write(&path, "").unwrap();
        let key = FileKey::new(&path);
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let old = mailbox.publish_seed(stub_seed(id));
        let background = old.tool_context(CancelToken::none());

        old.file_access.record_read(&key);
        future_mtime(&path);
        background.file_access.record_read(&key);
        assert!(old.file_access.check_before_edit(&key).is_err());

        let respawned = mailbox.publish_seed(stub_seed(id));
        drop(old);
        let _held = respawned.file_access.acquire(&key).await;
        assert!(
            background
                .file_access
                .acquire(&key)
                .now_or_never()
                .is_none()
        );
    }
}
