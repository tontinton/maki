//! What `-c`, `-s`, `--fork-session` and `--session-id` mean. One definition,
//! so the TUI, the SDK and `--print` cannot disagree about a flag again.

use std::fmt::Display;
use std::str::FromStr;

use color_eyre::Result;
use color_eyre::eyre::eyre;
use maki_agent::session::{Resumed, StoredSession};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::{SessionClaim, SessionError};
use maki_ui::OpenSession;

use crate::cli::Cli;

const NO_PREVIOUS_SESSION: &str = "no previous session found for this directory, starting new";
const ID_IN_USE: &str = "--session-id names a session that already exists";
const ID_IN_USE_HINT: &str = "pass -s/--session to continue it, or --fork-session to copy it";
/// For a run that already passed `--fork-session`. Suggesting the flag they
/// just used reads like we did not listen.
const ID_IN_USE_FORK_HINT: &str = "pass -s/--session to continue it, or drop --session-id";
/// Both ways out, since neither is obviously right: a copy keeps the history
/// but splits off from it, and dropping the flag starts over here.
const BUSY_HINT: &str = "  --fork-session  work on a copy of it
  or drop -c/-s to start a new session here";

pub struct Resolved {
    /// The id this run writes under: the resumed one, `--session-id`, a fresh
    /// fork id, or a generated one. Always concrete, so every caller can report
    /// it before the run starts, and always spelled the way whoever named it
    /// spelled it, since every `session_id` the run reports is this reference.
    pub id: SessionRef,
    pub start_type: &'static str,
    /// The right to write [`Self::id`], taken before anything else is decided.
    /// A second run on the same session is refused right here, before any
    /// request goes out.
    claim: SessionClaim,
    /// The transcript to restore, under [`Self::id`] either way: the stored
    /// session itself when the run continues it in place, or the copy a fork or
    /// a redirecting `--session-id` already wrote under the new id.
    session: Option<StoredSession>,
}

impl Resolved {
    /// A session no flag asked for: the replacement tab `/reload` opens when a
    /// reload closed the last one. Minting the id here keeps every session id
    /// in the process coming from the same place, claim included.
    pub fn fresh(storage: &StateDir) -> Self {
        let claim = SessionClaim::fresh(storage);
        Self {
            id: SessionRef::from(claim.id()),
            start_type: maki_otel::emit::START_FRESH,
            claim,
            session: None,
        }
    }

    /// What the agent needs: the transcript, what it measured and the session
    /// it came out of, under the id the run writes to.
    pub fn into_resumed(self) -> (Resumed, SessionClaim) {
        let resumed = match self.session {
            Some(session) => Resumed::stored(self.id, session),
            None => Resumed::empty(self.id),
        };
        (resumed, self.claim)
    }

    /// What the TUI needs: the stored session itself, or a fresh one. Either
    /// way it carries [`Self::id`], so a tab cannot open under one id while the
    /// run writes another.
    pub fn into_session(self, model_spec: &str, cwd: &str) -> OpenSession {
        let session = self.session.unwrap_or_else(|| {
            let mut fresh = StoredSession::new(model_spec, cwd);
            fresh.id = self.id.id();
            fresh
        });
        OpenSession {
            session,
            claim: self.claim,
        }
    }
}

/// A transcript a flag pointed at, under the reference that named it, and the
/// claim it was read under.
struct Loaded {
    session: StoredSession,
    claim: SessionClaim,
    /// The caller's own spelling of the id. [`MakiId`](maki_storage::id::MakiId)
    /// renders canonical base58, so rebuilding this from `session.id` would
    /// answer a client that resumed by hex uuid with a different string for the
    /// session it just named, and correlating by that string is the whole
    /// reason [`SessionRef`] keeps a raw form at all.
    reference: SessionRef,
    start_type: &'static str,
}

/// Everything here follows from one question: is the id this run writes under
/// the one it loaded its transcript from? Only then is the run continuing that
/// transcript *in place*, which is what decides both the start it reports and
/// the one stored transcript it may write over. A fork answers no by
/// construction, and so does a `--session-id` naming anything but the session
/// being resumed, which is why both land on a transcript of their own, report
/// [`START_FRESH`](maki_otel::emit::START_FRESH), and find the original as off
/// limits as any other session's.
pub fn resolve(cli: &Cli, cwd: &str, storage: &StateDir) -> Result<Resolved> {
    let loaded = load(cli, cwd, storage)?;
    // `--fork-session` promises to leave the original alone, so its transcript
    // is never a candidate to write over, however the id lands.
    let in_place = loaded
        .as_ref()
        .filter(|_| !cli.fork_session)
        .map(|loaded| loaded.reference.clone());

    let id = match &cli.session_id {
        Some(raw) => parse_id(raw)?,
        None => in_place.clone().unwrap_or_else(SessionRef::generate),
    };
    let continuing = continues_in_place(in_place.as_ref(), &id);

    let (start_type, session, claim) = match loaded {
        // Keeps the claim the transcript was read under. A second one on the
        // same id would conflict with it, since the lock belongs to the open
        // file and not to the process.
        Some(loaded) if continuing => (loaded.start_type, Some(loaded.session), loaded.claim),
        // A fork or a redirect writes somewhere else, so it lets go of the
        // original first. That way `--fork-session` onto its own source id
        // fails as an id already in use, not as a clash with our own read.
        Some(Loaded { session, claim, .. }) => {
            drop(claim);
            let claim = claim_unused(&id, cli.fork_session, storage)?;
            let copy = copy(session, &claim, cwd, storage)?;
            (maki_otel::emit::START_FRESH, Some(copy), claim)
        }
        None => (
            maki_otel::emit::START_FRESH,
            None,
            claim_unused(&id, cli.fork_session, storage)?,
        ),
    };

    Ok(Resolved {
        id,
        start_type,
        claim,
        session,
    })
}

/// A copy is a session of its own from the moment it resolves, written before
/// the run starts rather than at the end of its first turn. Every consumer then
/// reads the fork from one place: the TUI opens the file it was handed, a
/// headless turn writes back to the same one instead of creating a blank
/// session under the new id, and a fork whose run dies early is still there.
fn copy(
    session: StoredSession,
    claim: &SessionClaim,
    cwd: &str,
    storage: &StateDir,
) -> Result<StoredSession> {
    let id = claim.id();
    let mut copy = session.fork(id, cwd);
    copy.save(claim, storage)
        .map_err(|e| eyre!("failed to write session {id}: {e}"))?;
    Ok(copy)
}

/// The one question [`Resolved`] is built on, so the start a run reports and
/// the transcript it is allowed to replace cannot disagree about whether it is
/// the same session.
fn continues_in_place(in_place: Option<&SessionRef>, id: &SessionRef) -> bool {
    in_place.is_some_and(|original| original.id() == id.id())
}

/// An id this run is about to write from scratch: a generated one, or the
/// `--session-id` a fork or a redirect lands on. A run replaces the transcript
/// it writes to, so finding one there is refused rather than silently emptied.
/// The lock comes before the `exists` check, so two runs naming the same new
/// `--session-id` cannot both pass it.
fn claim_unused(id: &SessionRef, forking: bool, storage: &StateDir) -> Result<SessionClaim> {
    let claim = SessionClaim::acquire(id.id(), storage).map_err(with_busy_hint)?;
    if StoredSession::exists(id.id(), storage) {
        let hint = match forking {
            true => ID_IN_USE_FORK_HINT,
            false => ID_IN_USE_HINT,
        };
        return Err(eyre!("{ID_IN_USE}: {id}\n{hint}"));
    }
    Ok(claim)
}

/// A busy session is the one failure with more than one sensible way out, so
/// its message lists them.
fn with_busy_hint(e: SessionError) -> color_eyre::Report {
    match e.is_busy() {
        true => eyre!("{e}\n{BUSY_HINT}"),
        false => eyre!("{e}"),
    }
}

fn parse_id<T: FromStr<Err: Display>>(raw: &str) -> Result<T> {
    raw.parse()
        .map_err(|e| eyre!("invalid session id {raw:?}: {e}"))
}

/// The session the flags point at, if there is one, claimed before it is read.
/// A missing `--continue` history is not an error, the caller just starts
/// fresh.
fn load(cli: &Cli, cwd: &str, storage: &StateDir) -> Result<Option<Loaded>> {
    if let Some(raw) = &cli.session {
        let reference: SessionRef = parse_id(raw)?;
        let (session, claim) =
            StoredSession::claim_and_load(reference.id(), storage).map_err(with_busy_hint)?;
        return Ok(Some(Loaded {
            session,
            claim,
            reference,
            start_type: maki_otel::emit::START_RESUME,
        }));
    }
    if cli.continue_session {
        match StoredSession::claim_latest(cwd, storage) {
            // Nobody spelled this id, so canonical is the only spelling of it.
            Ok(Some((session, claim))) => {
                return Ok(Some(Loaded {
                    reference: SessionRef::from(session.id),
                    session,
                    claim,
                    start_type: maki_otel::emit::START_CONTINUE,
                }));
            }
            Ok(None) => tracing::info!(NO_PREVIOUS_SESSION),
            // Starting over suits a session that is missing or will not parse.
            // A busy one is intact and just belongs to someone else, and
            // quietly opening another session would hide that.
            Err(e) if e.is_busy() => return Err(with_busy_hint(e)),
            Err(e) => tracing::warn!(error = %e, "failed to load latest session, starting new"),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use maki_providers::{Message, TokenUsage};
    use maki_storage::id::MakiId;
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::*;

    const STORED_SPEC: &str = "zai/glm-4.6";
    const DECOY_SPEC: &str = "openai/gpt-5";
    const STORED_MESSAGE: &str = "the turn the stored session already had";
    const DECOY_MESSAGE: &str = "an older session in the same directory";
    const MALFORMED_SESSION_ID: &str = "not-a-session-id";
    const UNUSED_SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const INVALID_ID_ERROR: &str = "invalid session id";
    const NOT_FOUND_ERROR: &str = "not found";
    const FRESH_MODEL: &str = "anthropic/claude-fresh";
    const CONTEXT_SIZE: u32 = 4_242;
    const ID_PLACEHOLDER: &str = "{id}";
    const SURVIVED: &str = "a refused run must leave the session it pointed at alone";
    const COPY_ON_DISK: &str = "a copy has to be a session on disk before the run starts";
    const FORK_CWD: &str = "/project/fork";
    const STORED_INPUT_TOKENS: u32 = 1_234;
    /// A session id spelled as the hex uuid a Claude Code compatible client
    /// sends, rather than the base58 a `MakiId` renders.
    const HEX_SESSION_ID: &str = "01965087-4c71-7f00-8000-0000000000aa";
    const SPELLING: &str = "the reported id must keep the spelling the caller sent";
    const ORIGINAL_FREE: &str = "a fork holds its copy, not the session it copied";

    fn empty_storage() -> (TempDir, StateDir, String) {
        let dir = tempdir().expect("tempdir");
        let storage = StateDir::from_path(dir.path().join("state"));
        let cwd = dir.path().to_string_lossy().into_owned();
        (dir, storage, cwd)
    }

    fn save_session(storage: &StateDir, cwd: &str, spec: &str, text: &str) -> MakiId {
        save_session_as(storage, MakiId::generate(), cwd, spec, text)
    }

    fn save_session_as(
        storage: &StateDir,
        id: MakiId,
        cwd: &str,
        spec: &str,
        text: &str,
    ) -> MakiId {
        let mut session = StoredSession::new(spec, cwd);
        session.id = id;
        session.push_message(Message::user(text.to_owned()));
        session.meta.context_size = CONTEXT_SIZE;
        session.token_usage = TokenUsage {
            input: STORED_INPUT_TOKENS,
            ..Default::default()
        };
        let claim = SessionClaim::acquire(id, storage).expect("claim the session");
        session
            .save(&claim, storage)
            .expect("write session to disk");
        session.id
    }

    /// Two sessions in one cwd, so `--continue` has to actually pick instead of
    /// taking the only one there is. Saving claims the cwd index `latest` reads
    /// first, so saving the target last pins the order rather than leaning on a
    /// one second `updated_at`.
    fn storage_with_stored_session() -> (TempDir, StateDir, String, MakiId) {
        let (dir, storage, cwd) = empty_storage();
        save_session(&storage, &cwd, DECOY_SPEC, DECOY_MESSAGE);
        let id = save_session(&storage, &cwd, STORED_SPEC, STORED_MESSAGE);
        (dir, storage, cwd, id)
    }

    /// Built from the real parser, so what is under test is the flag a user
    /// types rather than a hand-set field. `{id}` stands in for the stored id,
    /// which the parameterized cases cannot spell in an attribute.
    fn cli(args: &[&str], stored: MakiId) -> Cli {
        let owned: Vec<String> = std::iter::once("maki".to_owned())
            .chain(
                args.iter()
                    .map(|a| a.replace(ID_PLACEHOLDER, &stored.to_string())),
            )
            .collect();
        Cli::parse_from(owned)
    }

    fn user_texts(resolved: &Resolved) -> Vec<&str> {
        resolved
            .session
            .as_ref()
            .map(|s| s.messages().iter().filter_map(|m| m.user_text()).collect())
            .unwrap_or_default()
    }

    fn refusal(cli: &Cli, cwd: &str, storage: &StateDir) -> String {
        match resolve(cli, cwd, storage) {
            Err(e) => e.to_string(),
            Ok(r) => panic!("expected an error, got session {}", r.id),
        }
    }

    fn stored_messages(storage: &StateDir, id: MakiId) -> usize {
        StoredSession::load(id, storage)
            .expect(SURVIVED)
            .messages()
            .len()
    }

    #[test_case(&[], maki_otel::emit::START_FRESH, false ; "no flags is a fresh session")]
    #[test_case(&["-s", ID_PLACEHOLDER], maki_otel::emit::START_RESUME, true ; "an explicit id resumes")]
    #[test_case(&["-c"], maki_otel::emit::START_CONTINUE, true ; "continue takes the latest session")]
    fn flags_pick_a_session_and_a_start_type(
        args: &[&str],
        expected_start: &str,
        expect_history: bool,
    ) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let resolved = resolve(&cli(args, stored), &cwd, &storage).expect("flags resolve");

        assert_eq!(resolved.start_type, expected_start);
        if expect_history {
            assert_eq!(resolved.id.id(), stored);
            assert_eq!(user_texts(&resolved), vec![STORED_MESSAGE]);
        } else {
            assert_ne!(resolved.id.id(), stored);
            assert!(resolved.session.is_none());
        }
    }

    /// Nothing about copying a session is a reason to keep it locked.
    #[test]
    fn a_fork_leaves_the_session_it_copied_free() {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();

        let resolved = resolve(&cli(&["--fork-session", "-c"], stored), &cwd, &storage)
            .expect("a fork resolves");

        assert_ne!(resolved.id.id(), stored);
        SessionClaim::acquire(stored, &storage).expect(ORIGINAL_FREE);
    }

    /// `--continue` in a directory nobody has worked in yet is a fresh start,
    /// not a reason to refuse to launch.
    #[test]
    fn continue_without_history_is_fresh() {
        let (_dir, storage, cwd) = empty_storage();

        let resolved = resolve(&cli(&["-c"], MakiId::generate()), &cwd, &storage)
            .expect("missing history must not be an error");

        assert_eq!(resolved.start_type, maki_otel::emit::START_FRESH);
        assert!(resolved.session.is_none());
    }

    /// A fork is the transcript under a new id, so the original is still there
    /// to resume afterwards.
    #[test]
    fn fork_copies_the_history_under_a_new_id() {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let resolved = resolve(
            &cli(&["-s", ID_PLACEHOLDER, "--fork-session"], stored),
            &cwd,
            &storage,
        )
        .expect("fork resolves");

        assert_eq!(resolved.start_type, maki_otel::emit::START_FRESH);
        assert_ne!(resolved.id.id(), stored);
        assert_eq!(user_texts(&resolved), vec![STORED_MESSAGE]);
        assert_eq!(
            resolved.session.as_ref().map(|s| s.id),
            Some(resolved.id.id()),
            "the copy has to carry the id the run writes under"
        );
        assert_eq!(stored_messages(&storage, stored), 1, "{SURVIVED}");
    }

    /// Both ways of copying write the new session before the run starts, so
    /// every consumer reads one file: the tab the TUI opens, the transcript a
    /// headless turn writes back to, and the entry `maki session list` shows
    /// even when the run dies in its first turn.
    #[test_case(&["-s", ID_PLACEHOLDER, "--fork-session"] ; "a fork")]
    #[test_case(&["-s", ID_PLACEHOLDER, "--session-id", UNUSED_SESSION_ID] ; "a redirected write target")]
    fn a_copy_is_a_session_of_its_own(args: &[&str]) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();

        let resolved = resolve(&cli(args, stored), FORK_CWD, &storage).expect("a copy resolves");

        let copy = StoredSession::load(resolved.id.id(), &storage).expect(COPY_ON_DISK);
        assert_eq!(copy.messages().len(), 1);
        assert_eq!(
            copy.cwd, FORK_CWD,
            "a copy belongs to the directory that forked it"
        );
        assert_eq!(
            copy.token_usage.input, 0,
            "a copy pays for the turns it runs, not the ones it inherited"
        );
        let original = StoredSession::load(stored, &storage).expect(SURVIVED);
        assert_eq!(original.cwd, cwd, "{SURVIVED}");
        assert_eq!(
            original.token_usage.input, STORED_INPUT_TOKENS,
            "{SURVIVED}"
        );
    }

    /// `--session-id` names where the run is written, which is a separate
    /// question from which transcript it continues. Naming the session being
    /// resumed is a no-op rather than a collision, the run was going to write
    /// there anyway.
    #[test_case(&["-c", "--session-id", ID_PLACEHOLDER] ; "an id the run already resolved to")]
    #[test_case(&["-s", ID_PLACEHOLDER, "--session-id", ID_PLACEHOLDER] ; "the resumed id spelled twice")]
    fn session_id_names_the_id_the_run_writes_under(args: &[&str]) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();

        let resolved = resolve(&cli(args, stored), &cwd, &storage).expect("session id resolves");

        assert_eq!(resolved.id.id(), stored);
        assert_eq!(user_texts(&resolved), vec![STORED_MESSAGE]);
    }

    /// `--session-id` naming anything but the session being resumed is a copy
    /// under a new id, not a continuation of the old one, so it reports the
    /// start a fork reports and leaves the original's transcript alone.
    #[test]
    fn a_redirected_session_id_starts_fresh_and_keeps_the_original() {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let target: MakiId = UNUSED_SESSION_ID.parse().expect("a spare id");

        let resolved = resolve(
            &cli(
                &["-s", ID_PLACEHOLDER, "--session-id", UNUSED_SESSION_ID],
                stored,
            ),
            &cwd,
            &storage,
        )
        .expect("a redirected write target resolves");

        assert_eq!(resolved.start_type, maki_otel::emit::START_FRESH);
        assert_eq!(resolved.id.id(), target);
        assert_eq!(user_texts(&resolved), vec![STORED_MESSAGE]);
        assert_eq!(
            resolved.session.as_ref().map(|s| s.id),
            Some(target),
            "the copy has to carry the id the run writes under"
        );
        assert_eq!(stored_messages(&storage, stored), 1, "{SURVIVED}");
    }

    /// Every `session_id` a run reports is [`Resolved::id`], so a caller that
    /// named the session by hex uuid has to be answered with the string it
    /// sent: an SDK client correlating by that string cannot be handed a second
    /// spelling of the session it just asked for.
    #[test_case(&["-s", HEX_SESSION_ID], true ; "resuming an id spelled as hex")]
    #[test_case(&["--session-id", HEX_SESSION_ID], false ; "writing under an id spelled as hex")]
    fn the_reported_id_keeps_the_callers_spelling(args: &[&str], stored: bool) {
        let (_dir, storage, cwd) = empty_storage();
        let id: MakiId = HEX_SESSION_ID.parse().expect("a hex uuid is a session id");
        if stored {
            save_session_as(&storage, id, &cwd, STORED_SPEC, STORED_MESSAGE);
        }

        let resolved = resolve(&cli(args, id), &cwd, &storage).expect("resolves");

        assert_eq!(resolved.id.as_str(), HEX_SESSION_ID, "{SPELLING}");
        assert_eq!(resolved.id.id(), id);
    }

    /// A run replaces the transcript it writes to, so `--session-id` pointing
    /// at somebody else's session has to stop: the answer to a typo cannot be
    /// deleted history. A fork gets no exemption, it continues nothing in
    /// place, and `--fork-session` promises to leave the original alone.
    #[test_case(&["--session-id", ID_PLACEHOLDER], ID_IN_USE_HINT ; "an id already in use")]
    #[test_case(&["-s", ID_PLACEHOLDER, "--session-id", ID_PLACEHOLDER, "--fork-session"], ID_IN_USE_FORK_HINT ; "a fork claiming the id it forked from")]
    fn a_claimed_session_id_is_refused(args: &[&str], hint: &str) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();

        let error = refusal(&cli(args, stored), &cwd, &storage);

        assert!(error.contains(ID_IN_USE), "{error}");
        assert!(error.contains(hint), "{error}");
        assert_eq!(stored_messages(&storage, stored), 1, "{SURVIVED}");
    }

    /// A session flag that cannot be opened has to fail loudly. Starting fresh
    /// on the same terminal looks exactly like the history was lost.
    #[test_case(&["-s", MALFORMED_SESSION_ID], INVALID_ID_ERROR ; "a malformed id to resume")]
    #[test_case(&["--session-id", MALFORMED_SESSION_ID], INVALID_ID_ERROR ; "a malformed id to write under")]
    #[test_case(&["-s", UNUSED_SESSION_ID], NOT_FOUND_ERROR ; "an id nothing was written for")]
    fn an_unopenable_session_flag_errors(args: &[&str], expected: &str) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();

        let error = refusal(&cli(args, stored), &cwd, &storage);

        assert!(error.contains(expected), "{error}");
    }

    /// Two runs on one session used to both load it and both write it back, and
    /// whichever finished last wiped the other's turns. The second one is now
    /// refused before any request goes out. `-c` must refuse too, not quietly
    /// start a new session and hide the conflict.
    #[test_case(&["-c"] ; "two continues of the same session")]
    #[test_case(&["-s", ID_PLACEHOLDER] ; "two resumes of the same session")]
    #[test_case(&["-c", "--session-id", ID_PLACEHOLDER] ; "a redirect onto a session already running")]
    #[test_case(&["--session-id", UNUSED_SESSION_ID] ; "two runs naming one new session id")]
    fn a_session_another_run_holds_is_refused(args: &[&str]) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let _first = resolve(&cli(args, stored), &cwd, &storage).expect("the first run resolves");

        let error = refusal(&cli(args, stored), &cwd, &storage);

        assert!(error.contains(BUSY_HINT), "{error}");
        assert_eq!(stored_messages(&storage, stored), 1, "{SURVIVED}");
    }

    /// The measured prompt size travels with the messages, or a resumed run
    /// budgets its first request from an estimate.
    #[test]
    fn into_resumed_carries_the_measured_context_size() {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let (resumed, claim) = resolve(&cli(&["-c"], stored), &cwd, &storage)
            .expect("continue resolves")
            .into_resumed();
        assert_eq!(claim.id(), stored, "the run writes what it claimed");

        assert_eq!(resumed.context_size, CONTEXT_SIZE);
        assert_eq!(resumed.history.len(), 1);
        assert_eq!(resumed.id.id(), stored);
        let session = resumed.session.expect("the loaded session travels along");
        assert_eq!(session.id, stored);
    }

    /// The tab the TUI opens and the transcript a headless run writes have to
    /// be the same session, whichever way `Resolved` is consumed.
    #[test_case(&[] ; "a fresh session")]
    #[test_case(&["-c"] ; "a continued session")]
    #[test_case(&["-c", "--fork-session"] ; "a fork")]
    fn both_consumers_agree_on_the_id(args: &[&str]) {
        let (_dir, storage, cwd, stored) = storage_with_stored_session();
        let cli = cli(args, stored);

        let as_tab = resolve(&cli, &cwd, &storage).expect("resolves");
        let reported = as_tab.id.id();
        assert_eq!(as_tab.into_session(FRESH_MODEL, &cwd).session.id, reported);

        let as_run = resolve(&cli, &cwd, &storage).expect("resolves");
        let reported = as_run.id.id();
        assert_eq!(as_run.into_resumed().0.id.id(), reported);
    }
}
