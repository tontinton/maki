//! The turns of one subagent prompt. A call the subagent moves to the
//! background reports back to the session's mailbox later, and the caller
//! wants the answer that comes after that report. So the prompt runs a turn,
//! waits for every moved call, and resumes on each wake. A sent message
//! wakes it too. An interrupt cuts the running turn, and the prompt resumes
//! with the message.
//!
//! Every run after the first is [`Agent::resume`], which reads the inbox
//! itself and answers only when something is left to answer. So what wakes
//! the prompt and what the run reads cannot drift apart: a message the tab
//! takes back in between leaves nothing to answer, not a request that ends
//! on the model's own reply.

use std::future::Future;
use std::mem;
use std::sync::Arc;

use maki_providers::{Message, RequestOptions};
use thiserror::Error;

use crate::cancel::CancelToken;
use crate::session_control::{Activity, EndReason, SessionControl};
use crate::{Agent, AgentError, AgentInput, DoneReason, InterruptSource, SessionMailbox};

/// What the session's owner does around the runs of [`run_turns`].
pub trait TurnHost {
    /// An agent over the session's history. [`run_turns`] sets its cancel
    /// token and interrupt source.
    fn agent(&mut self) -> Agent<'_>;

    /// Every run, before it is judged. `None` when the caller's cancel
    /// dropped it mid-flight.
    fn run_ended(&mut self, finished: Option<&Result<DoneReason, AgentError>>);
}

/// Why the caller got no answer.
#[derive(Debug, Error)]
pub enum CutShort {
    #[error(transparent)]
    Ended(#[from] EndReason),
    /// The caller's token fired, or the run came back cancelled on its own.
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Failed(#[from] AgentError),
    /// A layer dropped every message, so nothing reached the model.
    #[error("an `agent.user_message` layer dropped the prompt")]
    Dropped,
}

/// The caller can drop the prompt mid-run, and then nobody else would set the
/// session back to idle.
struct IdleOnDrop<'a>(&'a SessionControl);

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        self.0.set_activity(Activity::Idle);
    }
}

/// Runs {input} and every run after it until the session answers. The answer
/// is the last thing the model said in history. {caller} races every run and
/// every wait.
pub async fn run_turns(
    control: &Arc<SessionControl>,
    mailbox: &SessionMailbox,
    caller: Option<&CancelToken>,
    host: &mut impl TurnHost,
    input: AgentInput,
) -> Result<(), CutShort> {
    let _idle = IdleOnDrop(control);
    let opts = RequestOptions {
        thinking: input.thinking,
        fast: input.fast,
    };
    let mut prompt = Some(input);
    let mut notices = Vec::new();
    loop {
        let opening = prompt.is_some();
        let turn = control.enter();
        let mut agent = host
            .agent()
            .with_cancel(turn.clone())
            .with_interrupt_source(Arc::clone(control) as Arc<dyn InterruptSource>);
        let finished = race(caller, async {
            match prompt.take() {
                Some(input) => agent.run(input).await,
                None => agent.resume(mem::take(&mut notices), opts).await,
            }
        })
        .await;
        // Right away, so an interrupt landing while the run is judged finds no
        // turn to cut and leaves alone what its calls kept running.
        control.set_activity(Activity::Waiting);
        drop(agent);
        host.run_ended(finished.as_ref());
        let reason = finished.ok_or(CutShort::Cancelled)??;
        if let Some(end) = control.ended() {
            return Err(end.into());
        }
        match reason {
            // An interrupt cut it. The next run reads the message, or goes
            // on with the cut work if the tab took the message back.
            DoneReason::Cancelled if turn.is_cancelled() => continue,
            DoneReason::Cancelled => return Err(CutShort::Cancelled),
            DoneReason::Dropped if opening => return Err(CutShort::Dropped),
            // A resume with nothing new leaves the last answer standing.
            DoneReason::Dropped
            | DoneReason::EndTurn
            | DoneReason::MaxTokens
            | DoneReason::MaxTurns
            | DoneReason::Compact => {}
        }
        match next_wake(control, mailbox, caller).await? {
            Some(woken) => notices = woken,
            None => return Ok(()),
        }
    }
}

async fn race<T>(caller: Option<&CancelToken>, future: impl Future<Output = T>) -> Option<T> {
    match caller {
        Some(caller) => caller.race(future).await.ok(),
        None => Some(future.await),
    }
}

/// The notices to resume with, none when only the inbox woke it, or `None`
/// once no moved call is out, nothing they reported is left unread and
/// nothing waits in the inbox.
async fn next_wake(
    control: &SessionControl,
    mailbox: &SessionMailbox,
    caller: Option<&CancelToken>,
) -> Result<Option<Vec<Message>>, CutShort> {
    let session = control.token();
    let wake = async {
        tokio::select! {
            biased;
            () = session.cancelled() => None,
            () = control.wait_nonempty() => Some(Vec::new()),
            wake = mailbox.next_wake_before_answer() => wake,
        }
    };
    let woken = race(caller, wake).await.ok_or(CutShort::Cancelled)?;
    match control.ended() {
        Some(end) => Err(end.into()),
        None => Ok(woken),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, ContextGauge, Model, ModelInfo, ProviderEvent, Role, StopReason,
        StreamResponse, TaskEnd,
    };
    use maki_storage::id::{MakiId, SessionRef};
    use serde_json::Value;
    use test_case::test_case;
    use tokio::sync::Notify;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::agent::testing::{agent_params, fixed_context};
    use crate::cancel::CancelTrigger;
    use crate::session_control::Delivery;
    use crate::session_control::testing::input;
    use crate::{
        AgentRunParams, BackgroundNotice, Cutter, Envelope, EventSender, History, InputSource,
    };

    const MODEL_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const GO: &str = "go";
    const FIRST: &str = "first answer";
    const SECOND: &str = "second answer";
    const THIRD: &str = "third answer";
    const PARTIAL: &str = "partial answer";
    const NUDGE: &str = "Focus on the lexer.";
    const MOVED_CALL: &str = "toolu_moved";
    const NOTICE: &str = "Background bash toolu_moved finished:\n\nok";
    const NO_REPLY: &str = "the provider ran out of replies";
    const EDITED: &str = "earlier messages were edited";
    const NOTHING_TO_CUT: &str = "a run that came back has no turn left to cut";

    enum Reply {
        Text(&'static str),
        /// Streams {PARTIAL}, then hangs until the run is cancelled.
        Hang,
    }

    struct Scripted {
        replies: Mutex<VecDeque<Reply>>,
        requests: Mutex<Vec<Vec<Message>>>,
        started: Arc<Notify>,
    }

    impl Provider for Scripted {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            events: &'a UnboundedSender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(messages.to_vec());
                let reply = self.replies.lock().unwrap().pop_front().expect(NO_REPLY);
                match reply {
                    Reply::Text(text) => Ok(StreamResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::Text { text: text.into() }],
                            ..Default::default()
                        },
                        stop_reason: Some(StopReason::EndTurn),
                        ..Default::default()
                    }),
                    Reply::Hang => {
                        events
                            .send(ProviderEvent::TextDelta {
                                text: PARTIAL.into(),
                            })
                            .unwrap();
                        self.started.notify_one();
                        std::future::pending().await
                    }
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    type AfterRun = Box<dyn FnOnce()>;

    struct Host {
        provider: Arc<Scripted>,
        history: History,
        gauge: ContextGauge,
        event_tx: EventSender,
        _events: UnboundedReceiver<Envelope>,
        /// One per run, in order, done as the run ends.
        after_runs: VecDeque<AfterRun>,
    }

    impl TurnHost for Host {
        fn agent(&mut self) -> Agent<'_> {
            Agent::new(
                agent_params(
                    Arc::clone(&self.provider) as Arc<dyn Provider>,
                    Model::from_spec(MODEL_SPEC).unwrap(),
                ),
                AgentRunParams {
                    history: &mut self.history,
                    gauge: &mut self.gauge,
                    event_tx: self.event_tx.clone(),
                    context: Arc::new(|_, _| fixed_context()),
                },
            )
        }

        fn run_ended(&mut self, _: Option<&Result<DoneReason, AgentError>>) {
            if let Some(after) = self.after_runs.pop_front() {
                after();
            }
        }
    }

    struct Harness {
        control: Arc<SessionControl>,
        parent: CancelTrigger,
        mailbox: SessionMailbox,
        host: Host,
        started: Arc<Notify>,
    }

    impl Harness {
        fn new(replies: Vec<Reply>) -> Self {
            let (parent, parent_token) = CancelToken::new();
            let control = Arc::new(SessionControl::new(&parent_token));
            let started = Arc::new(Notify::new());
            let (raw_tx, events) = unbounded_channel();
            let host = Host {
                provider: Arc::new(Scripted {
                    replies: Mutex::new(replies.into()),
                    requests: Mutex::default(),
                    started: Arc::clone(&started),
                }),
                history: History::new(Vec::new()),
                gauge: ContextGauge::default(),
                event_tx: EventSender::new(raw_tx, 0),
                _events: events,
                after_runs: VecDeque::new(),
            };
            Self {
                control,
                parent,
                mailbox: SessionMailbox::register(MakiId::generate()),
                host,
                started,
            }
        }

        fn after_run(&mut self, after: impl FnOnce() + 'static) {
            self.host.after_runs.push_back(Box::new(after));
        }

        fn requests(&self) -> Vec<Vec<Message>> {
            self.host.provider.requests.lock().unwrap().clone()
        }

        async fn run(&mut self, caller: Option<&CancelToken>) -> Result<(), CutShort> {
            run_turns(
                &self.control,
                &self.mailbox,
                caller,
                &mut self.host,
                input(GO, InputSource::Plugin),
            )
            .await
        }
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn last(history: &History) -> &[Message] {
        let messages = history.as_slice();
        &messages[messages.len() - 1..]
    }

    #[track_caller]
    fn assert_append_only(before: &[Message], after: &[Message]) {
        assert!(after.starts_with(before), "{EDITED}");
    }

    fn notice() -> BackgroundNotice<'static> {
        BackgroundNotice {
            markdown: false,
            text: NOTICE.into(),
            display: None,
            wake: true,
            end: TaskEnd::Finished,
        }
    }

    async fn reach(control: &SessionControl, activity: Activity) {
        while control.activity() != activity {
            tokio::task::yield_now().await;
        }
    }

    /// Waiting on a moved call there is no turn to cut, so an interrupt
    /// only wakes the prompt, and so does the call's report after it.
    #[tokio::test]
    async fn a_message_and_then_the_moved_call_each_wake_the_waiting_prompt() {
        let mut h = Harness::new(vec![
            Reply::Text(FIRST),
            Reply::Text(SECOND),
            Reply::Text(THIRD),
        ]);
        h.mailbox.track(MOVED_CALL);
        h.after_run(|| {});
        let mailbox = h.mailbox.clone();
        h.after_run(move || mailbox.settle(MOVED_CALL, Some(notice())));
        let control = Arc::clone(&h.control);
        let interrupt = async move {
            reach(&control, Activity::Waiting).await;
            control
                .send(input(NUDGE, InputSource::Plugin), Delivery::Interrupt)
                .unwrap();
        };

        let (result, ()) = tokio::join!(h.run(None), interrupt);

        result.unwrap();
        let requests = h.requests();
        let [first, second, third] = &requests[..] else {
            panic!("expected one run per wake, got {}", requests.len());
        };
        assert_append_only(first, second);
        assert_append_only(second, third);
        let appended = texts(&second[first.len()..]);
        let [answer, nudge] = appended[..] else {
            panic!("expected the answer and the message, got {appended:?}");
        };
        assert_eq!(answer, FIRST);
        assert!(nudge.contains(NUDGE) && nudge != NUDGE, "{nudge}");
        assert_eq!(texts(&third[second.len()..]), [SECOND, NOTICE]);
        assert_eq!(texts(last(&h.host.history)), [THIRD]);
        assert_eq!(h.control.activity(), Activity::Idle);
    }

    /// The cut reply and the marker name whoever sent the interrupt, and the
    /// message follows them. If the tab takes the message back first, the
    /// marker alone is enough for the subagent to go on with the cut work.
    #[test_case(InputSource::Plugin, Cutter::Parent, false ; "from_the_parent")]
    #[test_case(InputSource::Tui,    Cutter::User,   false ; "from_the_user")]
    #[test_case(InputSource::Tui,    Cutter::User,   true  ; "taken_back_by_the_tab")]
    #[tokio::test]
    async fn an_interrupt_cuts_the_running_turn_and_the_prompt_goes_on(
        source: InputSource,
        cutter: Cutter,
        taken_back: bool,
    ) {
        let mut h = Harness::new(vec![Reply::Hang, Reply::Text(SECOND)]);
        let (control, started) = (Arc::clone(&h.control), Arc::clone(&h.started));
        let interrupt = async move {
            started.notified().await;
            control
                .send(input(NUDGE, source), Delivery::Interrupt)
                .unwrap();
            if taken_back {
                assert!(control.remove(0).is_some());
            }
        };

        let (result, ()) = tokio::join!(h.run(None), interrupt);

        result.unwrap();
        let requests = h.requests();
        let [first, second] = &requests[..] else {
            panic!("expected the cut request and the one after it");
        };
        assert_append_only(first, second);
        let appended = texts(&second[first.len()..]);
        let cut_reply = format!("{PARTIAL}\n\n{}", cutter.cut_reply_note());
        let (cut_turn, message) = appended.split_at(2);
        assert_eq!(cut_turn, [cut_reply.as_str(), cutter.cancel_marker()]);
        match message {
            [] => assert!(taken_back),
            [interrupt] => assert!(!taken_back && interrupt.contains(NUDGE)),
            _ => panic!("expected at most the interrupt, got {message:?}"),
        }
        assert_eq!(texts(last(&h.host.history)), [SECOND]);
    }

    enum Ender {
        Control(EndReason),
        Parent,
    }

    #[test_case(Ender::Control(EndReason::Stopped(Cutter::User)), EndReason::Stopped(Cutter::User), Cutter::User ; "stopped_by_user")]
    #[test_case(Ender::Control(EndReason::Stopped(Cutter::Parent)), EndReason::Stopped(Cutter::Parent), Cutter::Parent ; "stopped_by_parent")]
    #[test_case(Ender::Control(EndReason::Closed), EndReason::Closed, Cutter::Parent ; "closed")]
    #[test_case(Ender::Parent, EndReason::Cancelled, Cutter::User ; "cancelled_from_above")]
    #[tokio::test]
    async fn an_end_cuts_the_prompt_with_its_reason(
        ender: Ender,
        reason: EndReason,
        cutter: Cutter,
    ) {
        let mut h = Harness::new(vec![Reply::Hang]);
        let started = Arc::clone(&h.started);
        let control = Arc::clone(&h.control);
        let parent = h.parent.handle();
        let end = async move {
            started.notified().await;
            match ender {
                Ender::Control(reason) => control.end(reason),
                Ender::Parent => parent.fire(),
            }
        };

        let (result, ()) = tokio::join!(h.run(None), end);

        assert!(matches!(result, Err(CutShort::Ended(ended)) if ended == reason));
        assert_eq!(texts(last(&h.host.history)), [cutter.cancel_marker()]);
        assert_eq!(h.control.ended(), Some(reason));
        assert_eq!(h.control.activity(), Activity::Idle);
    }

    /// Firing the turn once its run is back would kill what its calls kept
    /// running past their replies. A cut turn holds its messages back from
    /// `poll`, so reading one proves nothing was cut.
    #[tokio::test]
    async fn an_interrupt_while_the_run_is_judged_cuts_nothing() {
        let mut h = Harness::new(vec![Reply::Text(FIRST)]);
        let control = Arc::clone(&h.control);
        h.after_run(move || {
            control
                .send(input(NUDGE, InputSource::Plugin), Delivery::Interrupt)
                .unwrap();
            assert!(control.poll().is_some(), "{NOTHING_TO_CUT}");
        });

        h.run(None).await.unwrap();

        assert_eq!(texts(last(&h.host.history)), [FIRST]);
    }

    #[tokio::test]
    async fn a_caller_cancel_ends_the_wait_for_moved_calls() {
        let mut h = Harness::new(vec![Reply::Text(FIRST)]);
        h.mailbox.track(MOVED_CALL);
        let (caller_trigger, caller) = CancelToken::new();
        let control = Arc::clone(&h.control);
        let cancel = async move {
            reach(&control, Activity::Waiting).await;
            caller_trigger.cancel();
        };

        let (result, ()) = tokio::join!(h.run(Some(&caller)), cancel);

        assert!(matches!(result, Err(CutShort::Cancelled)));
        assert_eq!(h.control.activity(), Activity::Idle);
    }
}
