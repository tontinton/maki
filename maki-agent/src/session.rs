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
    pub history: Vec<Message>,
    /// The provider's last prompt count for `history`, so a resumed run budgets
    /// from a measurement instead of an estimate.
    pub context_size: u32,
}

impl Resumed {
    /// A session nothing has been stored for yet. Not [`Default`], because
    /// minting an id is a decision, and reaching for it by accident writes a
    /// transcript under an id nobody reported.
    pub fn fresh() -> Self {
        Self {
            id: SessionRef::generate(),
            history: Vec::new(),
            context_size: 0,
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
    /// `None` only when the id names a session this process cannot read, see
    /// [`SessionStore::open`]. There is no "not opened yet" state to forget.
    store: Option<SessionStore>,
}

impl SessionTrack {
    /// Built once the provider resolves, so a run that never started leaves no
    /// file behind. The state dir comes from the caller rather than being
    /// resolved here, so the run writes where its history was read from.
    pub fn open(resumed: Resumed, storage: StateDir, cwd: &str) -> Self {
        Self {
            store: SessionStore::open(storage, resumed.id.id(), cwd),
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
        if let Some(store) = &mut self.store {
            store.session.set_model(model_spec);
        }
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
        if let Some(store) = store {
            store.record_turn(history.as_slice(), gauge.size());
        }
    }
}

struct SessionStore {
    dir: StateDir,
    session: StoredSession,
}

impl SessionStore {
    /// Opening touches no file. A session nothing was written for yet is held
    /// in memory until [`Self::record_turn`] has something to store, so every
    /// file on disk has a transcript in it. The blank model spec is
    /// [`SessionTrack::turn`]'s to fill, and it runs before any write.
    ///
    /// `None` when the id names a session this process cannot read. Creating a
    /// blank one in its place is worse than not persisting at all, because the
    /// first write would replace a transcript whose only copy is that file.
    fn open(dir: StateDir, session_id: MakiId, cwd: &str) -> Option<Self> {
        match StoredSession::load(session_id, &dir) {
            Ok(session) => Some(Self { dir, session }),
            Err(e) if e.is_not_found() => {
                let mut session = StoredSession::new("", cwd);
                session.id = session_id;
                Some(Self { dir, session })
            }
            Err(e) => {
                warn!(error = %e, %session_id, "session unreadable; this run will not be persisted");
                None
            }
        }
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
    use maki_storage::sessions::{SESSIONS_DIR, generate_title};
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
    const CORRUPT_LOG: &str = "{not json\n";
    const KEPT: &str = "the file the run could not read has to survive it";
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
        Resumed {
            id: SessionRef::from(session_id()),
            history: Vec::new(),
            context_size: 0,
        }
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
    /// beside it, which is the whole point of `-c` and `-s`.
    #[test]
    fn reopening_resumes_the_stored_transcript() {
        let tmp = TempDir::new().unwrap();
        push_prompt(&mut track_on(&tmp), MODEL_SPEC, PROMPT);

        let mut track = SessionTrack::open(
            Resumed {
                history: load(&tmp).take_messages(),
                ..resumed()
            },
            state_dir(&tmp),
            CWD,
        );
        push_prompt(&mut track, OTHER_SPEC, "second");

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, OTHER_SPEC);
    }

    /// A session file this process cannot parse is still the user's only copy.
    /// Opening it gives up, and the turn that follows writes nothing, so the
    /// transcript is there to recover instead of replaced by an empty one.
    #[test]
    fn an_unreadable_session_is_never_overwritten() {
        let tmp = TempDir::new().unwrap();
        let path = tmp
            .path()
            .join(SESSIONS_DIR)
            .join(format!("{}.jsonl", session_id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, CORRUPT_LOG).unwrap();

        push_prompt(&mut track_on(&tmp), MODEL_SPEC, PROMPT);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            CORRUPT_LOG,
            "{KEPT}"
        );
    }
}
