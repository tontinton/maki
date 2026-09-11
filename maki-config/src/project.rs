use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use maki_storage::StateDir;
use maki_storage::paths::{canonicalize_clean, home};
use maki_storage::trusted_folders::{CanonicalFolder, TrustDecision, TrustedFolders};
use strum::VariantArray;
use tracing::{info, warn};

use crate::PROJECT_DIR;

const SKIPPED: &str = "shared project config was skipped for this process";
const TRUST_NOT_SAVED: &str =
    "folder trust was not saved, but shared project config is enabled for this process";
pub const TRUST_QUESTION: &str = "Trust this folder?";
const TERMINAL_TRUST_QUESTION: &str = "Trust this folder? [y/N]";
const SHARED_FILE_POWERS: &str =
    "which can change the environment, start processes, and run Lua code";
const NO_SHARED_FILES_YET: &str =
    "This project ships no .maki files yet, but any added later would ask again.";
const ADDED_SINCE_TRUSTED: &str = "since you trusted it";
/// An answer given without knowing what the question covers is not consent, and
/// the card is too small to explain the whole gate.
pub const TRUST_DOCS: &str = "Learn more: https://maki.sh/docs/folder-trust/";
const DECLINED: &str = "Shared project config was skipped.";
/// Every skip says how to undo itself. `--trust` is here because the runs that
/// cannot answer a question are the ones that see these warnings.
const HOW_TO_TRUST: &str =
    "run `maki trust add --yes PATH` and restart Maki, or pass `--trust` to load it for one run";
const CLEAR_THE_REJECTION: &str = "or `maki trust remove PATH` to clear the decision";
const REJECTION_NOT_SAVED: &str = "folder rejection was not saved";

/// A file under `.maki/` that only a trusted folder may hand to Maki.
///
/// `config.toml` is deliberately absent. Maki stopped reading it, so it can do
/// nothing, and asking about a file that has no powers is a question with no
/// honest answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray)]
pub enum GatedFile {
    Env,
    Permissions,
    InitLua,
    Mcp,
}

impl GatedFile {
    /// Derived from the enum, because the trust store, the question and the
    /// docs all walk this, and a hand-written list is the one place a new
    /// variant gets forgotten.
    pub const ALL: &'static [GatedFile] = <GatedFile as VariantArray>::VARIANTS;

    /// The name recorded in the trust store and shown in the question. One
    /// spelling on purpose: a yes recorded under a different name than the one
    /// compared against would silently never match.
    pub const fn file_name(self) -> &'static str {
        match self {
            GatedFile::Env => ".env",
            GatedFile::Permissions => "permissions.toml",
            GatedFile::InitLua => "init.lua",
            GatedFile::Mcp => "mcp.toml",
        }
    }

    /// What the file can do, in the words the question and the docs use.
    pub const fn describes(self) -> &'static str {
        match self {
            GatedFile::Env => {
                "sets environment variables, including secrets, for maki and every process it starts"
            }
            GatedFile::Permissions => "decides which tools run without asking",
            GatedFile::InitLua => "runs Lua inside maki's own process at startup",
            GatedFile::Mcp => "starts MCP servers as child processes",
        }
    }
}

impl std::fmt::Display for GatedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PROJECT_DIR}/{}", self.file_name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    config_root: PathBuf,
    /// The home directory is the one folder that is never a project of its own,
    /// see [`ProjectConfig::rooted`].
    at_home: bool,
    trusted: bool,
    /// The store the trust answer came from, so a gated file Maki writes later
    /// can be added to that same answer. A config nobody resolved against a
    /// store has no answer to widen, so it records nothing, which also keeps
    /// tests off the real state directory.
    trust_store: Option<StateDir>,
}

impl ProjectConfig {
    pub fn discover(cwd: &Path) -> Self {
        let home = home().map(|path| canonicalize_clean(&path));
        Self::rooted(cwd, home.as_deref())
    }

    /// The walk stops below `home`, and a start in `home` itself is the same
    /// rule seen from the other side: `PROJECT_DIR` there is the user's own
    /// global config, not something a repository shipped, so there is no
    /// project to load and nothing to ask about.
    pub(crate) fn rooted(cwd: &Path, home: Option<&Path>) -> Self {
        let cwd = canonicalize_clean(cwd);
        let at_home = home == Some(cwd.as_path());
        Self {
            config_root: git_checkout_boundary(&cwd, home).unwrap_or(cwd),
            at_home,
            trusted: false,
            trust_store: None,
        }
    }

    /// Trust without consulting the store, for tests that need a trusted config
    /// without a state directory. `project::resolve` is the only production
    /// route to a trusted config.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_project(root: &Path) -> Self {
        Self::discover(root).with_trust(true)
    }

    pub fn config_root(&self) -> &Path {
        &self.config_root
    }

    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// The only route to a gated project file. `None` unless the folder is
    /// trusted, so a consumer cannot read one by forgetting a check.
    pub fn gated_path(&self, file: GatedFile) -> Option<PathBuf> {
        self.trusted.then(|| self.project_file(file))
    }

    /// Ungated, and private for that reason: `.maki/permissions.toml` is read
    /// at any trust level so a repository can narrow the agent inside it, and
    /// [`crate::load_permissions`] is the only place that exception belongs.
    pub(crate) fn project_file(&self, file: GatedFile) -> PathBuf {
        self.config_root.join(PROJECT_DIR).join(file.file_name())
    }

    pub(crate) fn with_trust(mut self, trusted: bool) -> Self {
        self.trusted = trusted;
        self
    }

    pub(crate) fn with_trust_store(mut self, storage: &StateDir) -> Self {
        self.trust_store = Some(storage.clone());
        self
    }
}

/// The question a folder poses, and the only thing an answer is recorded
/// against. Passing it whole rather than loose arguments is what keeps a
/// recorded yes covering exactly the file kinds the user was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustQuestion {
    pub folder: CanonicalFolder,
    /// The kinds present now; what a yes is recorded against.
    pub present: Vec<GatedFile>,
    /// Kinds gained since a previous yes; empty when there is no decision yet.
    pub added: Vec<GatedFile>,
}

impl TrustQuestion {
    /// What a folder would be asked about right now, with no stored answer
    /// consulted. `maki trust add` grants against this.
    pub fn for_folder(folder: &CanonicalFolder) -> Self {
        Self {
            present: gated_files(folder.path()),
            folder: folder.clone(),
            added: Vec::new(),
        }
    }

    /// The files the question is really about: what the project added since it
    /// was trusted, or everything it ships when there is no answer yet.
    fn named(&self) -> &[GatedFile] {
        if self.added.is_empty() {
            &self.present
        } else {
            &self.added
        }
    }
}

/// The whole answer about a folder. Every derived fact reads off this, so
/// "trusted" and "why" cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// Project config loads: a recorded yes, a grandfathered or v1 folder just
    /// written down, or `--trust`.
    Trusted,
    /// Nothing loads and nothing to ask: the home directory, a folder shipping
    /// no gated file, or a store this run could not read.
    Inert,
    /// Gated files present, no answer recorded. This is what the card asks.
    Unanswered(TrustQuestion),
    /// A recorded `Never`. Indicator and `/trust`, no card.
    Declined(TrustQuestion),
}

impl TrustState {
    /// `Some` for both restricted variants, which is what the status-bar
    /// indicator and `/trust` key off. Deliberately not `!is_trusted()`: that
    /// is true in every folder with no `.maki` at all.
    pub fn question(&self) -> Option<&TrustQuestion> {
        match self {
            TrustState::Unanswered(question) | TrustState::Declined(question) => Some(question),
            TrustState::Trusted | TrustState::Inert => None,
        }
    }

    /// What a run that cannot draw the card prints instead. Separate from
    /// [`ProjectDecision::warning`], which means "something broke": conflating
    /// the two is why a UI that shows the card would also print a redundant
    /// line about being restricted.
    pub fn restricted_warning(&self) -> Option<String> {
        let question = self.question()?;
        let folder = question.folder.path().display();
        let added = name_list(question.added.iter().map(|file| file.file_name()));
        let reason = match (self, added) {
            (TrustState::Declined(_), _) => {
                format!("folder trust was rejected; {HOW_TO_TRUST}, {CLEAR_THE_REJECTION}")
            }
            (_, Some(files)) => {
                format!("the project added {files} {ADDED_SINCE_TRUSTED}; {HOW_TO_TRUST}")
            }
            (_, None) => format!("the folder is not trusted; {HOW_TO_TRUST}"),
        };
        Some(format!(
            "skipped shared project config in {folder} because {reason}"
        ))
    }
}

/// The three answers the card offers. "Not now" is a no that records nothing,
/// not a session grant: `--trust` stays the only way to load project config
/// without writing a decision down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustAnswer {
    Trust,
    NotNow,
    Never,
}

#[derive(Debug)]
pub struct ProjectDecision {
    pub project_config: ProjectConfig,
    pub state: TrustState,
    /// Failures only: store unreadable, folder unresolvable, answer not saved.
    pub warning: Option<String>,
}

impl ProjectDecision {
    /// The one site that decides whether a config is trusted, derived from the
    /// state so the verdict and the reason cannot disagree.
    fn new(project_config: ProjectConfig, state: TrustState, warning: Option<String>) -> Self {
        let trusted = matches!(state, TrustState::Trusted);
        Self {
            project_config: project_config.with_trust(trusted),
            state,
            warning,
        }
    }

    fn inert(project_config: ProjectConfig, warning: String) -> Self {
        Self::new(project_config, TrustState::Inert, Some(warning))
    }

    /// Everything a run that cannot ask has to report: what broke, and the
    /// restriction it was left with. One route on purpose, because a caller
    /// that reports [`ProjectDecision::warning`] alone skips an untrusted
    /// folder in complete silence. The UI takes the two apart instead, since it
    /// showed the card and does not need to be told the answer it just got.
    pub fn notices(&self) -> Vec<String> {
        self.warning
            .iter()
            .cloned()
            .chain(self.state.restricted_warning())
            .collect()
    }
}

/// How a run reaches its trust answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    /// Read the store and report what it says. Whether the resulting question
    /// can be asked is the caller's business, not this module's.
    Consult,
    /// Trust for this process only. `--trust`.
    Session,
}

pub fn resolve(storage: &StateDir, cwd: &Path, mode: TrustMode) -> ProjectDecision {
    match mode {
        TrustMode::Session => session_grant(ProjectConfig::discover(cwd)),
        TrustMode::Consult => resolve_recorded(storage, ProjectConfig::discover(cwd)),
    }
}

/// For commands that never open the state directory for anything else: a state
/// directory they cannot resolve costs them the shared project config, not the
/// whole command.
pub fn resolve_noninteractive(cwd: &Path, mode: TrustMode) -> ProjectDecision {
    if mode == TrustMode::Session {
        return session_grant(ProjectConfig::discover(cwd));
    }
    match StateDir::resolve() {
        Ok(storage) => resolve(&storage, cwd, mode),
        Err(error) => ProjectDecision::inert(
            ProjectConfig::discover(cwd),
            format!("cannot resolve folder trust state: {error}; {SKIPPED}"),
        ),
    }
}

/// `--trust` is the same grant as `maki trust add --yes .` before the run, with
/// the state directory taken out of it. Nothing is read, so a stored no does not
/// override the flag the user just typed, and nothing is written, so a container
/// that mounts a state directory keeps no record of a folder it trusted once.
///
/// The config carries no store, so a gated file Maki writes during the run
/// records nothing either, and the next start without the flag asks about it.
fn session_grant(project_config: ProjectConfig) -> ProjectDecision {
    // `~/.maki` is the user's own global config, already loaded as global. No
    // flag turns that into a project, or it would load twice.
    let state = if project_config.at_home {
        TrustState::Inert
    } else {
        TrustState::Trusted
    };
    ProjectDecision::new(project_config, state, None)
}

fn resolve_recorded(storage: &StateDir, project_config: ProjectConfig) -> ProjectDecision {
    // Every config that leaves here carries the store its answer came from,
    // trusted or not, so a later write can find the same answer again.
    let project_config = project_config.with_trust_store(storage);
    // Maki loads the home directory's own `.maki` as global config already, so
    // a start there has nothing a project shipped. Asking would be a question
    // about the user's own files, and a yes would load that config twice.
    if project_config.at_home {
        return ProjectDecision::new(project_config, TrustState::Inert, None);
    }
    let folder = match CanonicalFolder::resolve(project_config.config_root()) {
        Ok(folder) => folder,
        Err(error) => return ProjectDecision::inert(project_config, format!("{error}; {SKIPPED}")),
    };
    let trusted_folders = TrustedFolders::new(storage);
    let present = gated_files(project_config.config_root());
    let decision = match trusted_folders.decide(&folder, &store_names(&present), &project_root) {
        Ok(decision) => decision,
        Err(error) => return ProjectDecision::inert(project_config, format!("{error}; {SKIPPED}")),
    };

    // Nothing to load means nothing to ask about, so report whatever the store
    // already says and stay quiet. A folder that predates folder trust is the
    // exception: writing its record down even with nothing to load is what
    // bounds the grant to today's files.
    if present.is_empty() && decision != TrustDecision::Grandfathered {
        let state = match decision {
            TrustDecision::Trusted | TrustDecision::Unrecorded => TrustState::Trusted,
            _ => TrustState::Inert,
        };
        return ProjectDecision::new(project_config, state, None);
    }

    let question = |added: Vec<GatedFile>| TrustQuestion {
        folder: folder.clone(),
        present: present.clone(),
        added,
    };
    match decision {
        TrustDecision::Trusted => ProjectDecision::new(project_config, TrustState::Trusted, None),
        // An answer given before Maki recorded file sets stays good, and
        // writing down what the folder ships today bounds it from here on.
        TrustDecision::Unrecorded => {
            info!(folder = %folder.path().display(), "recording what an older trust decision covers");
            record_trust(storage, &question(Vec::new()), project_config)
        }
        // A folder the user was already working in before folder trust existed
        // loaded its shared config without a question, and re-asking there only
        // teaches people to answer without reading. The set behind this is
        // frozen at the first start of this version, so nothing a later run
        // does can put a folder into it, and that is what makes it safe on a
        // path that cannot ask anybody. What the folder shipped back then is
        // unknowable, so the record covers what it ships today and nothing
        // more, and a file added after this asks like any other.
        TrustDecision::Grandfathered => {
            info!(folder = %folder.path().display(), "trusting folder that was in use before folder trust existed");
            record_trust(storage, &question(Vec::new()), project_config)
        }
        TrustDecision::Rejected => ProjectDecision::new(
            project_config,
            TrustState::Declined(question(Vec::new())),
            None,
        ),
        TrustDecision::Unknown => ProjectDecision::new(
            project_config,
            TrustState::Unanswered(question(Vec::new())),
            None,
        ),
        TrustDecision::Widened { added } => ProjectDecision::new(
            project_config,
            TrustState::Unanswered(question(from_store_names(&added))),
            None,
        ),
    }
}

/// Records a yes. One of two writers of an interactive decision, shared by the
/// card, the `trust.paths` auto-grant, `/trust` and `maki trust add`, so what
/// gets recorded can never drift from what was shown.
///
/// A failure is reported, not fatal: the folder stays trusted for this process,
/// which is what the user just asked for, and is asked again next start.
pub fn grant(storage: &StateDir, question: &TrustQuestion) -> Result<(), String> {
    TrustedFolders::new(storage)
        .add(&question.folder, &store_names(&question.present))
        .map(drop)
        .map_err(|error| format!("{error}; {TRUST_NOT_SAVED}"))
}

/// Records a permanent no. The counterpart to [`grant`]; "not now" records
/// nothing and so goes through neither.
pub fn deny(storage: &StateDir, question: &TrustQuestion) -> Result<(), String> {
    TrustedFolders::new(storage)
        .reject(&question.folder)
        .map(drop)
        .map_err(|error| format!("{error}; {REJECTION_NOT_SAVED}"))
}

/// Records an answer and returns the decision the rest of the run uses. The
/// card and the policy auto-grant both land here, so what was written down and
/// what this process does cannot disagree.
///
/// A store that refused the write keeps the grant the user just gave for this
/// process and reports it, which is the one documented exception to "any
/// failure leaves the folder untrusted".
pub fn apply_answer(
    storage: &StateDir,
    decision: ProjectDecision,
    answer: TrustAnswer,
) -> ProjectDecision {
    let Some(question) = decision.state.question() else {
        return decision;
    };
    match answer {
        TrustAnswer::Trust => {
            let warning = grant(storage, question).err();
            ProjectDecision::new(decision.project_config, TrustState::Trusted, warning)
        }
        TrustAnswer::Never => {
            let question = question.clone();
            let warning = deny(storage, &question).err();
            ProjectDecision::new(
                decision.project_config,
                TrustState::Declined(question),
                warning,
            )
        }
        TrustAnswer::NotNow => decision,
    }
}

fn record_trust(
    storage: &StateDir,
    question: &TrustQuestion,
    project_config: ProjectConfig,
) -> ProjectDecision {
    let warning = grant(storage, question).err();
    ProjectDecision::new(project_config, TrustState::Trusted, warning)
}

/// Maki writing a gated file into a trusted project is not the project
/// shipping one, so the answer the user already gave has to learn about it at
/// the same moment. Without this the next start finds a file that answer never
/// named, and asks the user about a file Maki created for them.
pub fn record_written_file(project_config: &ProjectConfig, file: &str) {
    if let Some(storage) = project_config.trust_store.as_ref() {
        record_written_file_in(storage, project_config, file);
    }
}

fn record_written_file_in(storage: &StateDir, project_config: &ProjectConfig, file: &str) {
    let root = project_config.config_root();
    let recorded = CanonicalFolder::resolve(root)
        .and_then(|folder| TrustedFolders::new(storage).cover_written_file(&folder, file));
    if let Err(error) = recorded {
        warn!(%error, file, folder = %root.display(), "cannot record a file Maki wrote into a trusted project");
    }
}

/// The body of the trust question: every entry point shows these words, so what
/// the user agreed to never depends on which one they came through.
pub fn trust_question_lines(question: &TrustQuestion) -> Vec<String> {
    let mut lines = vec![format!(
        "Maki can load shared project configuration from {}.",
        question.folder.path().display()
    )];
    lines.push(
        match name_list(question.named().iter().map(|file| file.file_name())) {
            Some(files) if question.added.is_empty() => {
                format!("This project ships {files}, {SHARED_FILE_POWERS}.")
            }
            Some(files) => {
                format!("This project added {files} {ADDED_SINCE_TRUSTED}, {SHARED_FILE_POWERS}.")
            }
            None => NO_SHARED_FILES_YET.to_owned(),
        },
    );
    lines.push(String::new());
    lines.push(TRUST_DOCS.to_owned());
    lines
}

/// The terminal form of the question, for `maki trust add` outside the UI.
pub fn confirm_trust(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &TrustQuestion,
) -> io::Result<bool> {
    for line in trust_question_lines(question) {
        writeln!(output, "{line}")?;
    }
    write!(output, "{TERMINAL_TRUST_QUESTION} ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !trusted {
        writeln!(output, "{DECLINED}")?;
    }
    Ok(trusted)
}

/// Which gated files a project ships right now. This is the set a trust answer
/// is recorded against, so `maki trust add` and `resolve` agree on what an
/// answer covers.
pub fn gated_files(config_root: &Path) -> Vec<GatedFile> {
    let project_dir = config_root.join(PROJECT_DIR);
    GatedFile::ALL
        .iter()
        .copied()
        .filter(|file| project_dir.join(file.file_name()).exists())
        .collect()
}

/// The trust store keeps plain names on disk: it is a wire format, and a name
/// written by a newer Maki must load and simply never match. Conversion happens
/// here and nowhere else.
fn store_names(files: &[GatedFile]) -> Vec<&'static str> {
    files.iter().copied().map(GatedFile::file_name).collect()
}

/// The other direction. A name a newer Maki wrote has no variant here and is
/// dropped, which is the same "never matches" the store already relies on.
fn from_store_names(names: &[String]) -> Vec<GatedFile> {
    GatedFile::ALL
        .iter()
        .copied()
        .filter(|file| names.iter().any(|name| name == file.file_name()))
        .collect()
}

fn name_list<'a>(files: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let names: Vec<String> = files
        .into_iter()
        .map(|file| format!("{PROJECT_DIR}/{file}"))
        .collect();
    match names.split_last()? {
        (last, []) => Some(last.clone()),
        (last, rest) => Some(format!("{} and {last}", rest.join(", "))),
    }
}

/// The folder a start in `cwd` would decide trust about. The grandfather
/// snapshot resolves every working directory it remembers through this, so a
/// grant covers the checkout that held the session and nothing above it.
///
/// A directory that is gone by then has no `.git` anywhere it could still be
/// found, so it resolves to itself. That grants nothing until somebody recreates
/// exactly the directory that had the history.
pub fn project_root(cwd: &Path) -> PathBuf {
    ProjectConfig::discover(cwd).config_root
}

/// The walk stops below `home`: a dotfiles repository at `$HOME` would otherwise
/// make every folder under home share one project root, and `PROJECT_DIR` there
/// is the user's own global config, not something a repository shipped.
fn git_checkout_boundary(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    cwd.ancestors()
        .take_while(|path| Some(*path) != home)
        .find(|path| fs::symlink_metadata(path.join(".git")).is_ok())
        .map(canonicalize_clean)
}

#[cfg(test)]
mod tests {
    use maki_storage::sessions::{SESSIONS_DIR, Session, TitleSource};
    use maki_storage::trusted_folders::TrustStatus;
    use serde::{Deserialize, Serialize};
    use test_case::test_case;

    use super::*;

    const INIT_SOURCE: &str = "return {}";
    const MCP_SOURCE: &str = "[servers]\n";
    const PERMISSIONS_SOURCE: &str = "[bash]\n";
    const INIT_FILE: &str = ".maki/init.lua";
    const MCP_FILE: &str = ".maki/mcp.toml";
    const CONFIG_FILE: &str = ".maki/config.toml";
    const PERMISSIONS_FILE: &str = ".maki/permissions.toml";
    const INIT_NAME: &str = "init.lua";
    const PERMISSIONS_NAME: &str = "permissions.toml";
    const DECLINE: &[u8] = b"n\n";
    /// A run with no question to answer; whatever is passed is never reached.
    const ANSWER_UNUSED: Option<TrustAnswer> = None;
    /// A run that cannot ask at all: headless, ACP, a utility subcommand.
    const CANNOT_ASK: Option<TrustAnswer> = None;
    const MODEL: &str = "test-model";
    const PROJECTS_DIR: &str = "projects";
    const PROJECT_STATE_DIR: &str = "project-cbf29ce484222325";
    const TRUST_FILE: &str = "trusted-folders.json";
    const PRE_TRUST_FILE: &str = "pre-trust-roots.json";
    const EMPTY_SNAPSHOT: &str = "[]";
    const TRUST_LOCK: &str = "trusted-folders.lock";
    const UNREADABLE_STORE: &str = "broken";
    const GIT_DIR: &str = ".git";
    const GIT_FILE_POINTER: &str = "gitdir: ../main/.git/worktrees/linked\n";
    const HOME_ITSELF: &str = "";
    const NESTED_REPO: &str = "work";
    const NESTED_CWD: &str = "work/src";
    const SOURCE_DIR: &str = "src";
    const NO_FILES: &[&str] = &[];

    /// Invariant 1, structurally: every gated kind is reachable only through a
    /// trusted config. Table-driven over the enum, so a new variant is covered
    /// the day it is added.
    #[test]
    fn gated_paths_exist_only_for_a_trusted_folder() {
        let dir = tempfile::tempdir().unwrap();
        let trusted = ProjectConfig::for_project(dir.path());
        let untrusted = ProjectConfig::discover(dir.path());

        for file in GatedFile::ALL.iter().copied() {
            assert_eq!(untrusted.gated_path(file), None, "{file}");
            assert_eq!(
                trusted.gated_path(file),
                Some(trusted.project_file(file)),
                "{file}"
            );
        }
    }

    /// The store keys on these names, so a rename would silently orphan every
    /// recorded answer.
    #[test_case(GatedFile::Env, ".env" ; "env")]
    #[test_case(GatedFile::Permissions, "permissions.toml" ; "permissions")]
    #[test_case(GatedFile::InitLua, "init.lua" ; "init_lua")]
    #[test_case(GatedFile::Mcp, "mcp.toml" ; "mcp")]
    fn gated_file_names_are_the_wire_format(file: GatedFile, expected: &str) {
        assert_eq!(file.file_name(), expected);
    }

    #[derive(Clone, Copy, Debug)]
    enum Marker {
        None,
        Directory,
        WorktreeFile,
    }

    /// What the state directory already knows about the folder.
    #[derive(Clone, Copy, Debug)]
    enum Prior {
        Nothing,
        Trusted,
        Rejected,
        RejectedAfterUse,
        Session,
        ProjectState,
    }

    #[derive(Clone, Serialize, Deserialize)]
    struct StoredMessage;

    impl TitleSource for StoredMessage {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    fn record_session(storage: &StateDir, cwd: &Path) {
        let mut session: Session<StoredMessage, u32, ()> =
            Session::new(MODEL, cwd.to_str().unwrap());
        session.save(storage).unwrap();
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, StateDir) {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(GIT_DIR)).unwrap();
        fs::create_dir(project.path().join(PROJECT_DIR)).unwrap();
        fs::write(project.path().join(INIT_FILE), INIT_SOURCE).unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        (state, project, storage)
    }

    fn record_prior(prior: Prior, storage: &StateDir, state: &Path, folder: &CanonicalFolder) {
        let store = TrustedFolders::new(storage);
        match prior {
            Prior::Nothing => {}
            Prior::Trusted => {
                store
                    .add(folder, &store_names(&gated_files(folder.path())))
                    .unwrap();
            }
            Prior::Rejected => {
                store.reject(folder).unwrap();
            }
            Prior::RejectedAfterUse => {
                store.reject(folder).unwrap();
                record_session(storage, folder.path());
            }
            Prior::Session => record_session(storage, &folder.path().join("src")),
            Prior::ProjectState => {
                fs::create_dir_all(state.join(PROJECTS_DIR).join(PROJECT_STATE_DIR)).unwrap();
            }
        }
    }

    /// Every test project is a temporary directory, so its parent stands in
    /// for the home directory: the walk stops there and can never reach a
    /// `.git` that happens to sit above the temp directory.
    fn project_at(path: &Path) -> ProjectConfig {
        let path = path.canonicalize().unwrap();
        ProjectConfig::rooted(&path, path.parent())
    }

    struct Answered {
        config: ProjectConfig,
        /// The question this run put to the user, `None` when nothing was
        /// asked.
        asked: Option<TrustQuestion>,
        warning: String,
    }

    impl Answered {
        fn asked_about(&self, file: GatedFile) -> bool {
            self.asked
                .as_ref()
                .is_some_and(|question| question.named().contains(&file))
        }
    }

    /// Stands in for the card. `resolve` only reports the question now, so the
    /// test plays the caller that can ask, and records through the same
    /// `grant`/`deny` every other entry point uses. `answer: None` is a run
    /// that cannot ask at all, which is where `restricted_warning` is the whole
    /// output.
    fn answer_question(
        storage: &StateDir,
        project: ProjectConfig,
        answer: Option<TrustAnswer>,
    ) -> Answered {
        let decision = resolve_recorded(storage, project);
        let pending = match &decision.state {
            TrustState::Unanswered(question) => answer.map(|answer| (question.clone(), answer)),
            _ => None,
        };
        let Some((question, answer)) = pending else {
            return Answered {
                config: decision.project_config,
                asked: None,
                warning: decision
                    .warning
                    .or_else(|| decision.state.restricted_warning())
                    .unwrap_or_default(),
            };
        };
        let failure = match answer {
            TrustAnswer::Trust => grant(storage, &question).err(),
            TrustAnswer::Never => deny(storage, &question).err(),
            TrustAnswer::NotNow => None,
        };
        Answered {
            config: decision
                .project_config
                .with_trust(answer == TrustAnswer::Trust),
            asked: Some(question),
            warning: failure.unwrap_or_default(),
        }
    }

    #[test_case("y", true ; "y")]
    #[test_case("Y", true ; "uppercase_y")]
    #[test_case(" YES \n", true ; "padded_yes")]
    #[test_case("", false ; "empty")]
    #[test_case("n", false ; "n")]
    #[test_case("sure", false ; "anything_else")]
    fn confirmation_accepts_only_y_and_yes(answer: &str, expected: bool) {
        let (_state, project, _storage) = setup();
        let question = question_at(project.path());

        let mut input = io::Cursor::new(answer.as_bytes());

        assert_eq!(
            confirm_trust(&mut input, &mut Vec::new(), &question).unwrap(),
            expected
        );
    }

    fn question_at(path: &Path) -> TrustQuestion {
        TrustQuestion::for_folder(&CanonicalFolder::resolve(path).unwrap())
    }

    #[test]
    fn the_question_names_the_files_it_would_load() {
        let (_state, project, _storage) = setup();
        fs::write(project.path().join(MCP_FILE), MCP_SOURCE).unwrap();
        fs::write(project.path().join(CONFIG_FILE), "").unwrap();

        let mut output = Vec::new();
        confirm_trust(
            &mut io::Cursor::new(DECLINE),
            &mut output,
            &question_at(project.path()),
        )
        .unwrap();
        let prompt = String::from_utf8(output).unwrap();

        assert!(prompt.contains(INIT_FILE));
        assert!(prompt.contains(MCP_FILE));
        assert!(
            !prompt.contains(CONFIG_FILE),
            "config.toml is inert, so the question must not claim powers for it: {prompt:?}"
        );
        assert!(prompt.contains(TRUST_QUESTION));
        assert!(
            prompt.contains(TRUST_DOCS),
            "the question has to say where the full rules live: {prompt:?}"
        );

        fs::remove_file(project.path().join(INIT_FILE)).unwrap();
        fs::remove_file(project.path().join(MCP_FILE)).unwrap();

        assert!(
            trust_question_lines(&question_at(project.path()))
                .join("\n")
                .contains(NO_SHARED_FILES_YET)
        );
    }

    /// A linked worktree spells `.git` as a file pointing at the main checkout,
    /// so the marker has to count whether it is a file or a directory.
    #[test_case(Marker::None ; "a_plain_directory_is_its_own_root")]
    #[test_case(Marker::Directory ; "a_git_directory_marks_the_root")]
    #[test_case(Marker::WorktreeFile ; "a_worktree_git_file_marks_the_root")]
    fn discovery_walks_up_to_the_checkout_root(marker: Marker) {
        let project = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        let child = project.join("src/child");
        fs::create_dir_all(&child).unwrap();
        let git = project.join(GIT_DIR);
        let expected = match marker {
            Marker::None => child.clone(),
            Marker::Directory => {
                fs::create_dir(&git).unwrap();
                project.clone()
            }
            Marker::WorktreeFile => {
                fs::write(&git, GIT_FILE_POINTER).unwrap();
                project.clone()
            }
        };

        assert_eq!(
            ProjectConfig::rooted(&child, project.parent()).config_root(),
            expected
        );
    }

    #[test_case(Some(HOME_ITSELF), None ; "a_repository_at_home_is_not_a_root")]
    #[test_case(Some(NESTED_REPO), Some(NESTED_REPO) ; "a_repository_below_home_is_a_root")]
    #[test_case(None, None ; "a_plain_directory_below_home_is_its_own_root")]
    fn project_discovery_stops_below_home(repo: Option<&str>, expected_root: Option<&str>) {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let cwd = home.join(NESTED_CWD);
        fs::create_dir_all(&cwd).unwrap();
        if let Some(repo) = repo {
            fs::create_dir(home.join(repo).join(GIT_DIR)).unwrap();
        }

        let root = git_checkout_boundary(&cwd, Some(&home));

        assert_eq!(
            root.unwrap_or_else(|| cwd.clone()),
            expected_root.map_or(cwd, |relative| home.join(relative))
        );
    }

    /// The other end of the same boundary: starting Maki in the home directory
    /// itself. On the legacy layout `~/.maki` holds init.lua and friends, so
    /// without this the gated file scan would ask the user to trust their own
    /// home, a no would store a rejection for it forever, and a yes would run
    /// the global config a second time as project config.
    #[test]
    fn a_start_in_the_home_directory_asks_nothing() {
        let state = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        fs::create_dir(home.join(PROJECT_DIR)).unwrap();
        fs::write(home.join(INIT_FILE), INIT_SOURCE).unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());

        let answered = answer_question(
            &storage,
            ProjectConfig::rooted(&home, Some(&home)),
            Some(TrustAnswer::Trust),
        );

        assert!(!answered.config.is_trusted());
        assert!(answered.asked.is_none());
        assert!(answered.warning.is_empty(), "{:?}", answered.warning);
        assert_eq!(
            TrustedFolders::new(&storage)
                .status(&CanonicalFolder::resolve(&home).unwrap())
                .unwrap(),
            TrustStatus::Unknown,
            "the home directory must not collect a stored decision either"
        );
    }

    /// `--trust` is the answer for a container that throws its state directory
    /// away, so it cannot depend on one. It also has to beat a stored no, or a
    /// mounted state directory would keep overriding the flag the user typed.
    #[test_case(Prior::Nothing ; "with_no_stored_decision")]
    #[test_case(Prior::Rejected ; "over_a_stored_rejection")]
    fn a_session_grant_trusts_without_touching_the_store(prior: Prior) {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(GIT_DIR)).unwrap();
        fs::create_dir(project.path().join(PROJECT_DIR)).unwrap();
        fs::write(project.path().join(INIT_FILE), INIT_SOURCE).unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        record_prior(prior, &storage, state.path(), &folder);
        let before = TrustedFolders::new(&storage).status(&folder).unwrap();

        let decision = resolve(&storage, project.path(), TrustMode::Session);

        assert!(decision.project_config.is_trusted());
        assert_eq!(decision.warning, None);
        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            before,
            "a grant for one process must record nothing"
        );
        // Without a store behind it there is no answer to widen, so a gated
        // file written during the run cannot enrol the folder by the back door.
        record_written_file(&decision.project_config, PERMISSIONS_NAME);
        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            before
        );
    }

    #[test]
    fn a_session_grant_still_loads_no_project_in_the_home_directory() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();

        let decision = session_grant(ProjectConfig::rooted(&home, Some(&home)));

        assert!(!decision.project_config.is_trusted());
    }

    #[test_case(Prior::Trusted, ANSWER_UNUSED, true, false, false ; "stored_trust_needs_no_question")]
    #[test_case(Prior::Session, ANSWER_UNUSED, true, false, false ; "a_session_in_the_folder_grandfathers_it")]
    #[test_case(Prior::ProjectState, Some(TrustAnswer::Trust), true, true, false ; "project_state_alone_still_asks")]
    #[test_case(Prior::ProjectState, CANNOT_ASK, false, false, true ; "project_state_alone_grants_nothing_headless")]
    #[test_case(Prior::Nothing, Some(TrustAnswer::Trust), true, true, false ; "trust_records_the_folder")]
    #[test_case(Prior::Nothing, Some(TrustAnswer::Never), false, true, false ; "never_rejects_the_folder")]
    #[test_case(Prior::Nothing, Some(TrustAnswer::NotNow), false, true, false ; "not_now_records_nothing")]
    #[test_case(Prior::Nothing, CANNOT_ASK, false, false, true ; "headless_skips_and_warns")]
    #[test_case(Prior::Rejected, Some(TrustAnswer::Trust), false, false, true ; "a_rejection_is_not_asked_again")]
    #[test_case(Prior::RejectedAfterUse, Some(TrustAnswer::Trust), false, false, true ; "a_rejection_beats_prior_use")]
    fn resolve_answers_from_what_the_state_dir_knows(
        prior: Prior,
        answer: Option<TrustAnswer>,
        trusted: bool,
        asked: bool,
        warned: bool,
    ) {
        let (state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        record_prior(prior, &storage, state.path(), &folder);

        let answered = answer_question(&storage, project_at(project.path()), answer);

        assert_eq!(answered.config.is_trusted(), trusted);
        assert_eq!(answered.asked.is_some(), asked);
        assert_eq!(
            !answered.warning.is_empty(),
            warned,
            "{:?}",
            answered.warning
        );
        assert_eq!(
            TrustedFolders::new(&storage).contains(&folder).unwrap(),
            trusted,
            "the stored decision must agree with the one this run used"
        );
    }

    /// "Not now" is the bug fix: one stray Enter must leave the store exactly
    /// as it was, so the next start asks again.
    #[test]
    fn not_now_leaves_no_trace_and_asks_again() {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();

        answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::NotNow),
        );

        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            TrustStatus::Unknown
        );
        let again = answer_question(&storage, project_at(project.path()), CANNOT_ASK);
        assert!(matches!(
            resolve_recorded(&storage, project_at(project.path())).state,
            TrustState::Unanswered(_)
        ));
        assert!(!again.config.is_trusted());
    }

    /// A run that cannot ask reports through `notices`, and the reason it is
    /// restricted has to be in there: `warning` alone is `None` for a folder
    /// nobody answered for, which is silence about skipped project config.
    #[test]
    fn notices_carry_the_restriction_a_bare_warning_omits() {
        let (_state, project, storage) = setup();

        let decision = resolve_recorded(&storage, project_at(project.path()));

        assert_eq!(decision.warning, None);
        assert_eq!(
            decision.notices(),
            vec![format!(
                "skipped shared project config in {} because the folder is not trusted; {HOW_TO_TRUST}",
                CanonicalFolder::resolve(project.path())
                    .unwrap()
                    .path()
                    .display()
            )]
        );
    }

    /// Invariant 2: a yes covers exactly what was shown, so a kind that appears
    /// after the question is not covered by it.
    #[test]
    fn a_grant_covers_exactly_the_kinds_the_question_named() {
        let (_state, project, storage) = setup();
        let question = question_at(project.path());
        assert_eq!(question.present, vec![GatedFile::InitLua]);

        fs::write(project.path().join(MCP_FILE), MCP_SOURCE).unwrap();
        grant(&storage, &question).unwrap();

        match resolve_recorded(&storage, project_at(project.path())).state {
            TrustState::Unanswered(later) => assert_eq!(later.added, vec![GatedFile::Mcp]),
            other => panic!("a kind added after the question must re-ask, got {other:?}"),
        }
    }

    /// The bypass the frozen snapshot closes. An ACP or headless run skips the
    /// shared config of an untrusted folder and still records a session in it,
    /// so a live session index would read that back as prior use and grant the
    /// folder trust on the next start with nobody ever asked.
    #[test_case(Some(TrustAnswer::Never), true ; "the_next_interactive_run_still_asks")]
    #[test_case(CANNOT_ASK, false ; "the_next_headless_run_gains_nothing")]
    fn a_session_recorded_after_the_snapshot_never_grandfathers(
        answer: Option<TrustAnswer>,
        asked: bool,
    ) {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        let first = answer_question(&storage, project_at(project.path()), CANNOT_ASK);
        assert!(!first.config.is_trusted());

        record_session(&storage, project.path());
        let answered = answer_question(&storage, project_at(project.path()), answer);

        assert!(!answered.config.is_trusted());
        assert_eq!(answered.asked.is_some(), asked);
        assert!(
            !TrustedFolders::new(&storage).contains(&folder).unwrap(),
            "a folder that entered the session index after the snapshot must not be trusted"
        );
    }

    /// Somebody installing Maki for the first time has no history to
    /// grandfather, so the snapshot their first start takes is empty and stays
    /// empty.
    #[test]
    fn a_fresh_install_grandfathers_nothing() {
        let (state, project, storage) = setup();

        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Never),
        );

        assert!(!answered.config.is_trusted());
        assert!(answered.asked.is_some());
        let snapshot =
            fs::read_to_string(state.path().join(SESSIONS_DIR).join(PRE_TRUST_FILE)).unwrap();
        assert_eq!(snapshot, EMPTY_SNAPSHOT);
    }

    /// The escalation an ancestor match would open. Running Maki once in a
    /// checkout must not hand the directory that holds it a grant, or a
    /// `.maki/init.lua` dropped in there runs the first time somebody starts
    /// Maki one level up, with no question asked.
    #[test]
    fn a_session_in_a_nested_checkout_never_grandfathers_the_directory_above_it() {
        let state = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join(NESTED_REPO);
        fs::create_dir_all(nested.join(GIT_DIR)).unwrap();
        for root in [parent.path(), nested.as_path()] {
            fs::create_dir(root.join(PROJECT_DIR)).unwrap();
            fs::write(root.join(INIT_FILE), INIT_SOURCE).unwrap();
        }
        let storage = StateDir::from_path(state.path().to_path_buf());
        record_session(&storage, &nested.join(SOURCE_DIR));

        let checkout = answer_question(&storage, project_at(&nested), CANNOT_ASK);
        assert!(
            checkout.config.is_trusted(),
            "the checkout the session ran in keeps what it always loaded"
        );

        let above = answer_question(&storage, project_at(parent.path()), CANNOT_ASK);
        assert!(
            !above.config.is_trusted(),
            "a directory that only holds a checkout was never in use itself"
        );
        assert!(!above.warning.is_empty(), "{:?}", above.warning);
    }

    /// The home directory is nobody's project root, so a session in a project
    /// below it must not grandfather everything the user owns.
    #[test]
    fn home_is_never_grandfathered_from_a_project_below_it() {
        let state = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let repo = home.join(NESTED_REPO);
        fs::create_dir_all(repo.join(GIT_DIR)).unwrap();
        fs::create_dir(repo.join(SOURCE_DIR)).unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        record_session(&storage, &repo.join(SOURCE_DIR));

        let root_of = |cwd: &Path| ProjectConfig::rooted(cwd, Some(&home)).config_root;
        let store = TrustedFolders::new(&storage);

        assert_eq!(
            store
                .decide(
                    &CanonicalFolder::resolve(&repo).unwrap(),
                    NO_FILES,
                    &root_of
                )
                .unwrap(),
            TrustDecision::Grandfathered
        );
        assert_eq!(
            store
                .decide(
                    &CanonicalFolder::resolve(&home).unwrap(),
                    NO_FILES,
                    &root_of
                )
                .unwrap(),
            TrustDecision::Unknown
        );
    }

    /// The escalation this closes: `<state>/projects/<id>/` is a directory the
    /// agent can create through the write tool, and the memory plugin writes
    /// there with no prompt. A repository must not be able to manufacture its
    /// own consent that way.
    #[test]
    fn project_state_left_by_the_agent_is_not_evidence_of_trust() {
        let (state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        record_prior(Prior::ProjectState, &storage, state.path(), &folder);

        let answered = answer_question(&storage, project_at(project.path()), CANNOT_ASK);

        assert!(!answered.config.is_trusted());
        assert!(!answered.warning.is_empty());
        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            TrustStatus::Unknown
        );
    }

    /// The other half of the same defect: an answer about one file must not
    /// cover a kind of file the project added later.
    #[test]
    fn a_gated_file_added_after_the_answer_asks_again() {
        let (_state, project, storage) = setup();
        fs::remove_file(project.path().join(INIT_FILE)).unwrap();
        fs::write(project.path().join(PERMISSIONS_FILE), PERMISSIONS_SOURCE).unwrap();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        TrustedFolders::new(&storage)
            .add(&folder, &[PERMISSIONS_NAME])
            .unwrap();

        fs::write(project.path().join(INIT_FILE), INIT_SOURCE).unwrap();
        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Trust),
        );

        assert!(answered.config.is_trusted());
        assert!(answered.asked_about(GatedFile::InitLua));
        assert!(
            !answered.asked_about(GatedFile::Permissions),
            "the file already covered is not new"
        );

        let again = answer_question(&storage, project_at(project.path()), ANSWER_UNUSED);
        assert!(again.config.is_trusted());
        assert!(again.asked.is_none(), "the widened answer must stick");
    }

    /// Maki writes `.maki/permissions.toml` itself the first time somebody
    /// answers "allow always for this project", so the next start must not
    /// turn that write into a question about a file the project never added.
    #[test]
    fn a_gated_file_maki_wrote_itself_asks_nothing() {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        TrustedFolders::new(&storage)
            .add(&folder, &[INIT_NAME])
            .unwrap();

        fs::write(project.path().join(PERMISSIONS_FILE), PERMISSIONS_SOURCE).unwrap();
        let written = project_at(project.path()).with_trust(true);
        record_written_file_in(&storage, &written, PERMISSIONS_NAME);

        let answered = answer_question(&storage, project_at(project.path()), ANSWER_UNUSED);

        assert!(answered.config.is_trusted());
        assert!(answered.asked.is_none());
        assert!(answered.warning.is_empty(), "{:?}", answered.warning);
    }

    /// The other side of the same rule: a file Maki wrote into a folder that
    /// was never trusted must not hand that folder an answer nobody gave.
    #[test]
    fn recording_a_written_file_never_creates_trust() {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();

        fs::write(project.path().join(PERMISSIONS_FILE), PERMISSIONS_SOURCE).unwrap();
        let written = project_at(project.path()).with_trust(true);
        record_written_file_in(&storage, &written, PERMISSIONS_NAME);

        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            TrustStatus::Unknown
        );
    }

    /// Contents are not part of the record, so editing a trusted file asks
    /// nothing. Only a new kind of file does.
    #[test]
    fn changing_the_contents_of_a_trusted_file_asks_nothing() {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        TrustedFolders::new(&storage)
            .add(&folder, &[INIT_NAME])
            .unwrap();

        fs::write(project.path().join(INIT_FILE), "return { changed = true }").unwrap();
        let answered = answer_question(&storage, project_at(project.path()), ANSWER_UNUSED);

        assert!(answered.config.is_trusted());
        assert!(answered.asked.is_none());
        assert!(answered.warning.is_empty(), "{:?}", answered.warning);
    }

    /// A store written before file sets existed keeps its answers, and the
    /// first start after the upgrade writes down what the folder ships that
    /// day, which bounds it from then on.
    #[test]
    fn a_decision_from_before_file_sets_survives_and_is_bounded() {
        let (state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        fs::write(
            state.path().join(TRUST_FILE),
            serde_json::json!({"version": 1, "folders": [folder.path()]}).to_string(),
        )
        .unwrap();

        let answered = answer_question(&storage, project_at(project.path()), ANSWER_UNUSED);
        assert!(answered.config.is_trusted());
        assert!(answered.asked.is_none());
        assert!(answered.warning.is_empty(), "{:?}", answered.warning);

        fs::write(project.path().join(MCP_FILE), MCP_SOURCE).unwrap();
        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Never),
        );
        assert!(!answered.config.is_trusted());
        assert!(answered.asked_about(GatedFile::Mcp));
    }

    #[test_case(false, false ; "an_unreadable_store_warns_and_stays_untrusted")]
    #[test_case(true, true ; "stored_trust_still_applies")]
    fn a_project_without_shared_files_is_never_asked(store_trust: bool, expected: bool) {
        let (state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        if store_trust {
            TrustedFolders::new(&storage).add(&folder, &[]).unwrap();
        } else {
            fs::write(state.path().join(TRUST_FILE), UNREADABLE_STORE).unwrap();
        }
        fs::remove_dir_all(project.path().join(PROJECT_DIR)).unwrap();

        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Trust),
        );

        assert_eq!(answered.config.is_trusted(), expected);
        assert!(answered.asked.is_none());
        assert_eq!(
            answered.warning.is_empty(),
            store_trust,
            "a store nobody can read must be reported here too: {:?}",
            answered.warning
        );
    }

    #[test]
    fn corrupt_state_skips_config_but_a_failed_save_keeps_session_trust() {
        let (state, project, storage) = setup();
        fs::write(state.path().join(TRUST_FILE), UNREADABLE_STORE).unwrap();

        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Trust),
        );
        assert!(!answered.config.is_trusted());
        assert!(answered.warning.contains(SKIPPED), "{:?}", answered.warning);

        fs::remove_file(state.path().join(TRUST_FILE)).unwrap();
        fs::create_dir(state.path().join(TRUST_LOCK)).unwrap();

        // Invariant 5's one documented exception: the grant the user just gave
        // holds for this process, and the failure to save it is reported.
        let answered = answer_question(
            &storage,
            project_at(project.path()),
            Some(TrustAnswer::Trust),
        );
        assert!(answered.config.is_trusted());
        assert!(
            answered.warning.contains(TRUST_NOT_SAVED),
            "{:?}",
            answered.warning
        );
    }
}
