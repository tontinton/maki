use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use maki_storage::StateDir;
use maki_storage::paths::{canonicalize_clean, home};
use maki_storage::trusted_folders::{CanonicalFolder, TrustDecision, TrustedFolders};
use tracing::{info, warn};

use crate::PROJECT_DIR;

/// `config.toml` is deliberately absent. Maki stopped reading it, so it can do
/// nothing, and asking about a file that has no powers is a question with no
/// honest answer.
const SHARED_PROJECT_FILES: &[&str] = &[".env", "permissions.toml", "init.lua", "mcp.toml"];
const SKIPPED: &str = "shared project config was skipped for this process";
const TRUST_NOT_SAVED: &str =
    "folder trust was not saved, but shared project config is enabled for this process";
const TRUST_QUESTION: &str = "Trust this folder? [y/N]";
const SHARED_FILE_POWERS: &str =
    "which can change the environment, start processes, and run Lua code";
const NO_SHARED_FILES_YET: &str =
    "This project ships no .maki files yet, but any added later would ask again.";
const ADDED_SINCE_TRUSTED: &str = "since you trusted it";
const DECLINED: &str = "Shared project config was skipped.";
const REJECTION_NOT_SAVED: &str = "folder rejection was not saved";

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

    pub(crate) fn with_trust(mut self, trusted: bool) -> Self {
        self.trusted = trusted;
        self
    }

    pub(crate) fn with_trust_store(mut self, storage: &StateDir) -> Self {
        self.trust_store = Some(storage.clone());
        self
    }
}

#[derive(Debug)]
pub struct ProjectDecision {
    pub project_config: ProjectConfig,
    /// At most one thing can go wrong per decision, and the callers only log it.
    pub warning: Option<String>,
}

impl ProjectDecision {
    fn quiet(project_config: ProjectConfig) -> Self {
        Self {
            project_config,
            warning: None,
        }
    }

    fn skip(project_config: ProjectConfig, warning: String) -> Self {
        Self {
            project_config,
            warning: Some(warning),
        }
    }
}

pub fn resolve(storage: &StateDir, cwd: &Path, interactive: bool) -> ProjectDecision {
    resolve_with_prompt(
        storage,
        ProjectConfig::discover(cwd),
        interactive.then_some(ask_on_terminal),
    )
}

/// For commands that never open the state directory for anything else: a state
/// directory they cannot resolve costs them the shared project config, not the
/// whole command.
pub fn resolve_noninteractive(cwd: &Path) -> ProjectDecision {
    match StateDir::resolve() {
        Ok(storage) => resolve(&storage, cwd, false),
        Err(error) => ProjectDecision::skip(
            ProjectConfig::discover(cwd),
            format!("cannot resolve folder trust state: {error}; {SKIPPED}"),
        ),
    }
}

/// Takes stdin only here, where a question is really about to be asked. ACP
/// resolves trust from its dispatch loop while another thread owns stdin for
/// the whole of a read, so locking it on a path that never prompts parks the
/// loop until the client sends bytes it is waiting on the answer to send.
fn ask_on_terminal(folder: &CanonicalFolder, added: &[String]) -> io::Result<bool> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stderr = io::stderr();
    let mut output = stderr.lock();
    let trusted = confirm_trust(&mut input, &mut output, folder, added)?;
    if !trusted {
        writeln!(output, "{DECLINED}")?;
    }
    Ok(trusted)
}

fn resolve_with_prompt<P>(
    storage: &StateDir,
    project_config: ProjectConfig,
    prompt: Option<P>,
) -> ProjectDecision
where
    P: FnOnce(&CanonicalFolder, &[String]) -> io::Result<bool>,
{
    // Every config that leaves here carries the store its answer came from,
    // trusted or not, so a later write can find the same answer again.
    let project_config = project_config.with_trust_store(storage);
    // Maki loads the home directory's own `.maki` as global config already, so
    // a start there has nothing a project shipped. Asking would be a question
    // about the user's own files, and a yes would load that config twice.
    if project_config.at_home {
        return ProjectDecision::quiet(project_config);
    }
    let folder = match CanonicalFolder::resolve(project_config.config_root()) {
        Ok(folder) => folder,
        Err(error) => {
            return ProjectDecision::skip(project_config, format!("{error}; {SKIPPED}"));
        }
    };
    let trusted_folders = TrustedFolders::new(storage);
    let present = gated_files(project_config.config_root());
    let decision = match trusted_folders.decide(&folder, &present, &project_root) {
        Ok(decision) => decision,
        Err(error) => {
            return ProjectDecision::skip(project_config, format!("{error}; {SKIPPED}"));
        }
    };

    // Nothing to load means nothing to ask about, so report whatever the store
    // already says and stay quiet. A folder that predates folder trust is the
    // exception: writing its record down even with nothing to load is what
    // bounds the grant to today's files.
    if present.is_empty() && decision != TrustDecision::Grandfathered {
        let trusted = matches!(decision, TrustDecision::Trusted | TrustDecision::Unrecorded);
        return ProjectDecision::quiet(project_config.with_trust(trusted));
    }

    let added = match decision {
        TrustDecision::Trusted => return ProjectDecision::quiet(project_config.with_trust(true)),
        // An answer given before Maki recorded file sets stays good, and
        // writing down what the folder ships today bounds it from here on.
        TrustDecision::Unrecorded => {
            info!(folder = %folder.path().display(), "recording what an older trust decision covers");
            return record_trust(&trusted_folders, &folder, &present, project_config);
        }
        TrustDecision::Rejected => {
            return ProjectDecision::skip(
                project_config,
                format!(
                    "skipped shared project config in {} because folder trust was rejected; run `maki trust add --yes PATH` to trust it or `maki trust remove PATH` to clear the decision",
                    folder.path().display()
                ),
            );
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
            return record_trust(&trusted_folders, &folder, &present, project_config);
        }
        TrustDecision::Unknown => Vec::new(),
        TrustDecision::Widened { added } => added,
    };

    let Some(prompt) = prompt else {
        return ProjectDecision::skip(project_config, not_trusted_warning(&folder, &added));
    };
    match prompt(&folder, &added) {
        Ok(true) => record_trust(&trusted_folders, &folder, &present, project_config),
        Ok(false) => ProjectDecision {
            project_config,
            warning: trusted_folders
                .reject(&folder)
                .err()
                .map(|error| format!("{error}; {REJECTION_NOT_SAVED}")),
        },
        Err(error) => ProjectDecision::skip(
            project_config,
            format!("could not read the folder trust answer: {error}; {SKIPPED}"),
        ),
    }
}

fn not_trusted_warning(folder: &CanonicalFolder, added: &[String]) -> String {
    let folder = folder.path().display();
    match name_list(added) {
        Some(files) => format!(
            "skipped shared project config in {folder} because the project added {files} {ADDED_SINCE_TRUSTED}; run `maki trust add --yes PATH` and restart Maki"
        ),
        None => format!(
            "skipped shared project config in {folder} because the folder is not trusted; run `maki trust add --yes PATH` and restart Maki"
        ),
    }
}

fn record_trust(
    trusted_folders: &TrustedFolders,
    folder: &CanonicalFolder,
    files: &[&str],
    project_config: ProjectConfig,
) -> ProjectDecision {
    ProjectDecision {
        project_config: project_config.with_trust(true),
        warning: trusted_folders
            .add(folder, files)
            .err()
            .map(|error| format!("{error}; {TRUST_NOT_SAVED}")),
    }
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

/// The one trust prompt: every entry point asks with these words, so what the
/// user agreed to never depends on which one they came through. `added` names
/// the gated files a folder gained since it was trusted, and is empty when the
/// folder has no decision yet.
pub fn confirm_trust(
    input: &mut impl BufRead,
    output: &mut impl Write,
    folder: &CanonicalFolder,
    added: &[String],
) -> io::Result<bool> {
    writeln!(
        output,
        "Maki can load shared project configuration from {}.",
        folder.path().display()
    )?;
    match name_list(added) {
        Some(files) => writeln!(
            output,
            "This project added {files} {ADDED_SINCE_TRUSTED}, {SHARED_FILE_POWERS}."
        )?,
        None => match name_list(&gated_files(folder.path())) {
            Some(files) => writeln!(output, "This project ships {files}, {SHARED_FILE_POWERS}.")?,
            None => writeln!(output, "{NO_SHARED_FILES_YET}")?,
        },
    }
    write!(output, "{TRUST_QUESTION} ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Which gated files a project ships right now. This is the set a trust answer
/// is recorded against, so `maki trust add` and `resolve` agree on what an
/// answer covers.
pub fn gated_files(config_root: &Path) -> Vec<&'static str> {
    let project_dir = config_root.join(PROJECT_DIR);
    SHARED_PROJECT_FILES
        .iter()
        .copied()
        .filter(|file| project_dir.join(file).exists())
        .collect()
}

fn name_list<S: AsRef<str>>(files: &[S]) -> Option<String> {
    let names: Vec<String> = files
        .iter()
        .map(|file| format!("{PROJECT_DIR}/{}", file.as_ref()))
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
    use std::cell::Cell;

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
    const NO_ADDED_FILES: &[String] = &[];
    const DECLINE: &[u8] = b"n\n";
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
                store.add(folder, &gated_files(folder.path())).unwrap();
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

    /// Stands in for the terminal prompt. A non-interactive run passes `None`,
    /// which is the whole point: nothing on that path can reach for stdin.
    fn resolve_io(
        storage: &StateDir,
        project: ProjectConfig,
        interactive: bool,
        answer: &str,
    ) -> (ProjectConfig, String, String) {
        let output = Cell::new(Vec::new());
        let decision = resolve_with_prompt(
            storage,
            project,
            interactive.then_some(|folder: &CanonicalFolder, added: &[String]| {
                let mut written = Vec::new();
                let trusted = confirm_trust(
                    &mut io::Cursor::new(answer.as_bytes()),
                    &mut written,
                    folder,
                    added,
                )?;
                output.set(written);
                Ok(trusted)
            }),
        );
        (
            decision.project_config,
            String::from_utf8(output.into_inner()).unwrap(),
            decision.warning.unwrap_or_default(),
        )
    }

    #[test_case("y", true ; "y")]
    #[test_case("Y", true ; "uppercase_y")]
    #[test_case(" YES \n", true ; "padded_yes")]
    #[test_case("", false ; "empty")]
    #[test_case("n", false ; "n")]
    #[test_case("sure", false ; "anything_else")]
    fn confirmation_accepts_only_y_and_yes(answer: &str, expected: bool) {
        let (_state, project, _storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();

        let mut input = io::Cursor::new(answer.as_bytes());

        assert_eq!(
            confirm_trust(&mut input, &mut Vec::new(), &folder, NO_ADDED_FILES).unwrap(),
            expected
        );
    }

    #[test]
    fn the_prompt_names_the_files_it_would_load() {
        let (_state, project, _storage) = setup();
        fs::write(project.path().join(MCP_FILE), MCP_SOURCE).unwrap();
        fs::write(project.path().join(CONFIG_FILE), "").unwrap();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();

        let mut output = Vec::new();
        confirm_trust(
            &mut io::Cursor::new(DECLINE),
            &mut output,
            &folder,
            NO_ADDED_FILES,
        )
        .unwrap();
        let prompt = String::from_utf8(output).unwrap();

        assert!(prompt.contains(INIT_FILE));
        assert!(prompt.contains(MCP_FILE));
        assert!(
            !prompt.contains(CONFIG_FILE),
            "config.toml is inert, so the prompt must not claim powers for it: {prompt:?}"
        );
        assert!(prompt.contains(TRUST_QUESTION));

        fs::remove_file(project.path().join(INIT_FILE)).unwrap();
        fs::remove_file(project.path().join(MCP_FILE)).unwrap();
        let mut empty = Vec::new();
        confirm_trust(
            &mut io::Cursor::new(DECLINE),
            &mut empty,
            &folder,
            NO_ADDED_FILES,
        )
        .unwrap();

        assert!(
            String::from_utf8(empty)
                .unwrap()
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

        let (config, output, warning) = resolve_io(
            &storage,
            ProjectConfig::rooted(&home, Some(&home)),
            true,
            "yes\n",
        );

        assert!(!config.is_trusted());
        assert!(output.is_empty(), "prompt: {output:?}");
        assert!(warning.is_empty(), "warning: {warning:?}");
        assert_eq!(
            TrustedFolders::new(&storage)
                .status(&CanonicalFolder::resolve(&home).unwrap())
                .unwrap(),
            TrustStatus::Unknown,
            "the home directory must not collect a stored decision either"
        );
    }

    #[test_case(Prior::Trusted, true, "", true, false, false ; "stored_trust_needs_no_question")]
    #[test_case(Prior::Session, true, "", true, false, false ; "a_session_in_the_folder_grandfathers_it")]
    #[test_case(Prior::ProjectState, true, "yes\n", true, true, false ; "project_state_alone_still_asks")]
    #[test_case(Prior::ProjectState, false, "", false, false, true ; "project_state_alone_grants_nothing_headless")]
    #[test_case(Prior::Nothing, true, "yes\n", true, true, false ; "yes_trusts_the_folder")]
    #[test_case(Prior::Nothing, true, "no\n", false, true, false ; "no_rejects_the_folder")]
    #[test_case(Prior::Nothing, false, "", false, false, true ; "headless_skips_and_warns")]
    #[test_case(Prior::Rejected, true, "yes\n", false, false, true ; "a_rejection_is_not_asked_again")]
    #[test_case(Prior::RejectedAfterUse, true, "yes\n", false, false, true ; "a_rejection_beats_prior_use")]
    fn resolve_answers_from_what_the_state_dir_knows(
        prior: Prior,
        interactive: bool,
        answer: &str,
        trusted: bool,
        asked: bool,
        warned: bool,
    ) {
        let (state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        record_prior(prior, &storage, state.path(), &folder);

        let (config, output, warning) =
            resolve_io(&storage, project_at(project.path()), interactive, answer);

        assert_eq!(config.is_trusted(), trusted);
        assert_eq!(output.contains(TRUST_QUESTION), asked, "prompt: {output:?}");
        assert_eq!(!warning.is_empty(), warned, "warning: {warning:?}");
        assert_eq!(
            TrustedFolders::new(&storage).contains(&folder).unwrap(),
            trusted,
            "the stored decision must agree with the one this run used"
        );
    }

    /// The bypass the frozen snapshot closes. An ACP or headless run skips the
    /// shared config of an untrusted folder and still records a session in it,
    /// so a live session index would read that back as prior use and grant the
    /// folder trust on the next start with nobody ever asked.
    #[test_case(true, "no\n", true ; "the_next_interactive_run_still_asks")]
    #[test_case(false, "", false ; "the_next_headless_run_gains_nothing")]
    fn a_session_recorded_after_the_snapshot_never_grandfathers(
        interactive: bool,
        answer: &str,
        asked: bool,
    ) {
        let (_state, project, storage) = setup();
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        let (first, _, _) = resolve_io(&storage, project_at(project.path()), false, "");
        assert!(!first.is_trusted());

        record_session(&storage, project.path());
        let (config, output, _) =
            resolve_io(&storage, project_at(project.path()), interactive, answer);

        assert!(!config.is_trusted());
        assert_eq!(output.contains(TRUST_QUESTION), asked, "prompt: {output:?}");
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

        let (config, output, _) = resolve_io(&storage, project_at(project.path()), true, "no\n");

        assert!(!config.is_trusted());
        assert!(output.contains(TRUST_QUESTION), "prompt: {output:?}");
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

        let (checkout, _, _) = resolve_io(&storage, project_at(&nested), false, "");
        assert!(
            checkout.is_trusted(),
            "the checkout the session ran in keeps what it always loaded"
        );

        let (above, _, warning) = resolve_io(&storage, project_at(parent.path()), false, "");
        assert!(
            !above.is_trusted(),
            "a directory that only holds a checkout was never in use itself"
        );
        assert!(!warning.is_empty(), "warning: {warning:?}");
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

        let (config, _, warning) = resolve_io(&storage, project_at(project.path()), false, "");

        assert!(!config.is_trusted());
        assert!(!warning.is_empty());
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
        let (config, output, _) = resolve_io(&storage, project_at(project.path()), true, "yes\n");

        assert!(config.is_trusted());
        assert!(output.contains(INIT_FILE), "prompt: {output:?}");
        assert!(output.contains(ADDED_SINCE_TRUSTED), "prompt: {output:?}");
        assert!(
            !output.contains(PERMISSIONS_FILE),
            "the file already covered is not new: {output:?}"
        );

        let (config, output, _) = resolve_io(&storage, project_at(project.path()), true, "");
        assert!(config.is_trusted());
        assert!(
            output.is_empty(),
            "the widened answer must stick: {output:?}"
        );
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

        let (config, output, warning) = resolve_io(&storage, project_at(project.path()), true, "");

        assert!(config.is_trusted());
        assert!(output.is_empty(), "prompt: {output:?}");
        assert!(warning.is_empty(), "warning: {warning:?}");
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
        let (config, output, warning) = resolve_io(&storage, project_at(project.path()), true, "");

        assert!(config.is_trusted());
        assert!(output.is_empty(), "prompt: {output:?}");
        assert!(warning.is_empty(), "warning: {warning:?}");
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

        let (config, output, warning) = resolve_io(&storage, project_at(project.path()), true, "");
        assert!(config.is_trusted());
        assert!(output.is_empty(), "prompt: {output:?}");
        assert!(warning.is_empty(), "warning: {warning:?}");

        fs::write(project.path().join(MCP_FILE), MCP_SOURCE).unwrap();
        let (config, output, _) = resolve_io(&storage, project_at(project.path()), true, "no\n");
        assert!(!config.is_trusted());
        assert!(output.contains(MCP_FILE), "prompt: {output:?}");
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

        let (config, output, warning) =
            resolve_io(&storage, project_at(project.path()), true, "y\n");

        assert_eq!(config.is_trusted(), expected);
        assert!(output.is_empty());
        assert_eq!(
            warning.is_empty(),
            store_trust,
            "a store nobody can read must be reported here too: {warning:?}"
        );
    }

    #[test]
    fn corrupt_state_skips_config_but_a_failed_save_keeps_session_trust() {
        let (state, project, storage) = setup();
        fs::write(state.path().join(TRUST_FILE), UNREADABLE_STORE).unwrap();

        let (config, _, warning) = resolve_io(&storage, project_at(project.path()), true, "yes\n");
        assert!(!config.is_trusted());
        assert!(warning.contains(SKIPPED), "warning: {warning:?}");

        fs::remove_file(state.path().join(TRUST_FILE)).unwrap();
        fs::create_dir(state.path().join(TRUST_LOCK)).unwrap();

        let (config, _, warning) = resolve_io(&storage, project_at(project.path()), true, "yes\n");
        assert!(config.is_trusted());
        assert!(warning.contains(TRUST_NOT_SAVED), "warning: {warning:?}");
    }
}
