//! The one vocabulary every driver uses to talk about a persisted session.

use maki_providers::{ContextGauge, Message, TokenUsage};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::Session;
use tracing::warn;

use crate::agent::History;
use crate::tools::RequestTools;
use crate::types::EventSender;
use crate::{AgentRunParams, ToolOutput};

/// The one spelling of a persisted maki session. It cannot live in
/// maki-storage: [`ToolOutput`] is ours, and maki-storage must not depend on us.
pub type StoredSession = Session<Message, TokenUsage, ToolOutput>;

/// A transcript a driver starts from. The id comes from the caller, because
/// every entry point resolves one and reports it before the run starts, and a
/// second answer minted here would not be the one the user was told.
pub struct Resumed {
    pub id: SessionRef,
    /// The transcript to run on. It is moved out of [`Self::session`] when
    /// there is one, so the two never hold two copies of it.
    pub history: Vec<Message>,
    /// The provider's last prompt count for `history`, so a resumed run budgets
    /// from a measurement instead of an estimate.
    pub context_size: u32,
    /// The stored session `history` came out of, if the driver read one. The
    /// run writes back into it, so the title, spending and meta survive without
    /// a second parse of the file. `None` when nothing is stored under the id
    /// yet, or when the driver owns the session and persists it itself.
    pub session: Option<StoredSession>,
}

impl Resumed {
    /// A session nothing has been stored for yet. Not [`Default`], because
    /// minting an id is a decision, and reaching for it by accident writes a
    /// transcript under an id nobody reported.
    pub fn fresh() -> Self {
        Self::empty(SessionRef::generate())
    }

    /// A run under `id` with nothing read from disk for it.
    pub fn empty(id: SessionRef) -> Self {
        Self {
            id,
            history: Vec::new(),
            context_size: 0,
            session: None,
        }
    }

    /// A run continuing a session the driver already loaded.
    pub fn stored(id: SessionRef, mut session: StoredSession) -> Self {
        Self {
            id,
            history: session.drain_messages(),
            context_size: session.meta.context_size,
            session: Some(session),
        }
    }
}

/// Everything a session run owns: what it said, how big that is, and where it
/// is written. [`AgentRunParams`] already wants `history` and `gauge` under one
/// owner, and the store joins them so a driver cannot hold a transcript it
/// never persists.
///
/// The transcript is only reachable through [`Self::turn`], whose guard writes
/// it back when it drops, so "built an agent and forgot to persist it" is not a
/// mistake a driver can make.
pub struct SessionTrack {
    history: History,
    gauge: ContextGauge,
    store: SessionStore,
}

impl SessionTrack {
    /// Built once the provider resolves, so a run that never started leaves no
    /// file behind. The state dir comes from the caller rather than being
    /// resolved here, so the run writes where its history was read from.
    pub fn open(resumed: Resumed, storage: StateDir, cwd: &str) -> Self {
        Self {
            store: SessionStore::open(storage, resumed.session, resumed.id.id(), cwd),
            history: History::restored(resumed.history),
            gauge: ContextGauge::restored(resumed.context_size),
        }
    }

    /// One turn against this transcript. The returned guard is the only way to
    /// reach [`AgentRunParams`] and it persists on drop. An
    /// [`Agent`](crate::Agent) built from it borrows the guard, so borrowck
    /// puts the write after the run without the driver arranging it.
    ///
    /// The spec lands here rather than at the end because a driver picks its
    /// model before it builds the agent. Since this is the only path to a
    /// write, no stored session can carry a spec no turn ran on.
    pub fn turn(&mut self, model_spec: String) -> SessionTurn<'_> {
        self.store.session.set_model(model_spec);
        SessionTurn(self)
    }
}

/// A turn in progress. See [`SessionTrack::turn`].
pub struct SessionTurn<'a>(&'a mut SessionTrack);

impl SessionTurn<'_> {
    pub fn run_params(
        &mut self,
        system: String,
        event_tx: EventSender,
        tools: RequestTools,
    ) -> AgentRunParams<'_> {
        AgentRunParams {
            history: &mut self.0.history,
            gauge: &mut self.0.gauge,
            system,
            event_tx,
            tools,
        }
    }
}

impl Drop for SessionTurn<'_> {
    fn drop(&mut self) {
        let SessionTrack {
            history,
            gauge,
            store,
        } = &mut *self.0;
        store.record_turn(history.as_slice(), gauge.size());
    }
}

struct SessionStore {
    dir: StateDir,
    session: StoredSession,
}

impl SessionStore {
    /// Opening touches no file: the driver either read the session already or
    /// there is nothing under the id to read. A session nothing was written for
    /// yet is held in memory until [`Self::record_turn`] has something to
    /// store, so every file on disk has a transcript in it. The blank model
    /// spec is [`SessionTrack::turn`]'s to fill, and it runs before any write.
    fn open(dir: StateDir, stored: Option<StoredSession>, id: MakiId, cwd: &str) -> Self {
        let session = stored.unwrap_or_else(|| {
            let mut session = StoredSession::new("", cwd);
            session.id = id;
            session
        });
        Self { dir, session }
    }

    /// `context_size` travels with the messages, since a resumed session seeds
    /// its gauge from it. Stored without one, the next process is back to
    /// estimating a transcript this one had measured.
    ///
    /// An empty transcript is not a session, and this is the one place that
    /// decides so. An empty log is one the picker offers and `--continue`
    /// resolves to, so a run killed before its first turn would leave a dead
    /// entry behind for good. The same guard stops a history that sanitized
    /// down to nothing from replacing the copy it was restored from.
    fn record_turn(&mut self, messages: &[Message], context_size: u32) {
        if messages.is_empty() {
            return;
        }
        self.session.replace_messages(messages.to_vec());
        self.session.meta.context_size = context_size;
        self.session.update_title_if_default();
        if let Err(e) = self.session.save(&self.dir) {
            warn!(error = %e, session_id = %self.session.id, "failed to persist session");
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_storage::sessions::generate_title;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const OTHER_SPEC: &str = "other/model";
    const CONTEXT_SIZE: u32 = 42_000;
    const PROMPT: &str = "fix the login bug";
    const OBSERVATION: &str = "build failed";
    const TITLE: &str = "a title the user set";
    const PLAN_PATH: &str = "/plans/plan.md";
    const NO_EMPTY_FILE: &str = "a session with no transcript must not be on disk";
    const NO_EMPTY_LATEST: &str = "an empty session must not be what --continue resolves to";
    const NOT_WIPED: &str = "an empty history must not replace the transcript it came from";

    fn session_id() -> MakiId {
        SESSION_ID.parse().unwrap()
    }

    fn state_dir(tmp: &TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &state_dir(tmp)).unwrap()
    }

    fn resumed() -> Resumed {
        Resumed::empty(SessionRef::from(session_id()))
    }

    fn track_on(tmp: &TempDir) -> SessionTrack {
        SessionTrack::open(resumed(), state_dir(tmp), CWD)
    }

    fn push_turn(track: &mut SessionTrack, spec: &str, edit: impl FnOnce(AgentRunParams<'_>)) {
        let mut turn = track.turn(spec.to_owned());
        edit(turn.run_params(
            String::new(),
            EventSender::new(flume::unbounded().0, 0),
            RequestTools::default(),
        ));
    }

    fn push_prompt(track: &mut SessionTrack, spec: &str, text: &str) {
        push_turn(track, spec, |params| {
            params.history.push(Message::user(text.into()))
        });
    }

    /// A run killed before its first turn has to leave nothing behind, neither
    /// a file under its id nor something for `--continue` to land on. `maki -p`
    /// is the reason: it is short, scripted and interrupted often, and the
    /// previous session in the directory has to stay the one `-c` finds.
    #[test_case(false ; "a run that never took a turn")]
    #[test_case(true ; "a turn that produced no messages")]
    fn a_run_with_no_transcript_leaves_nothing_behind(took_turn: bool) {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        if took_turn {
            push_turn(&mut track, MODEL_SPEC, |_| {});
        }
        drop(track);

        assert!(
            StoredSession::load(session_id(), &state_dir(&tmp)).is_err(),
            "{NO_EMPTY_FILE}"
        );
        assert!(
            StoredSession::latest(CWD, &state_dir(&tmp))
                .expect("an empty store is readable")
                .is_none(),
            "{NO_EMPTY_LATEST}"
        );
    }

    /// The same guard from the other side: once a transcript is stored, a turn
    /// that ends with an empty history leaves it alone instead of emptying the
    /// only copy of it.
    #[test]
    fn an_empty_history_does_not_wipe_a_stored_transcript() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        push_turn(&mut track, MODEL_SPEC, |params| {
            *params.history = History::new(Vec::new())
        });

        assert_eq!(load(&tmp).messages().len(), 1, "{NOT_WIPED}");
    }

    /// What a driver pushes through `run_params` is on disk once the turn ends,
    /// under the id, cwd, spec, title and measured size a resumed run reads
    /// back. Nothing here calls a save, ending the turn is the save.
    ///
    /// The observation is in there because it is the message kind a transcript
    /// can lose silently: it is not part of the model's reply, so nothing
    /// downstream complains when it fails to round trip.
    #[test]
    fn a_finished_turn_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let messages = vec![
            Message::user(PROMPT.into()),
            Message::observation(OBSERVATION.into()),
        ];
        push_turn(&mut track, MODEL_SPEC, |params| {
            for message in &messages {
                params.history.push(message.clone());
            }
            *params.gauge = ContextGauge::restored(CONTEXT_SIZE);
        });

        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_id());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert_eq!(loaded.title, generate_title(&messages));
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.messages()[1].is_observation());
        assert_eq!(
            loaded.meta.context_size, CONTEXT_SIZE,
            "a resumed session seeds its gauge from this, so it has to be stored"
        );
    }

    /// The next process continues the transcript instead of starting one
    /// beside it, which is the whole point of `-c` and `-s`. The title and plan
    /// are set only in memory, so they reach disk only if the run writes back
    /// into the session it was handed rather than reading the file again.
    #[test]
    fn reopening_resumes_the_stored_session() {
        let tmp = TempDir::new().unwrap();
        push_prompt(&mut track_on(&tmp), MODEL_SPEC, PROMPT);
        let mut stored = load(&tmp);
        stored.set_title(TITLE.to_owned());
        stored.meta.plan_path = Some(PLAN_PATH.to_owned());

        let mut track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), stored),
            state_dir(&tmp),
            CWD,
        );
        push_prompt(&mut track, OTHER_SPEC, PROMPT);

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, OTHER_SPEC);
        assert_eq!(loaded.title, TITLE);
        assert_eq!(loaded.meta.plan_path.as_deref(), Some(PLAN_PATH));
    }
}
