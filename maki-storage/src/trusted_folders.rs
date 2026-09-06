use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lock::{Lock, LockError};
use crate::paths::{canonicalize_clean, normalize_path};
use crate::sessions::{SESSIONS_DIR, recorded_cwds};
use crate::{StateDir, StorageError, atomic_write};

const DOCUMENT_FILE: &str = "trusted-folders.json";
const DOCUMENT_VERSION: u32 = 2;
/// Version 1 kept a bare path per trusted folder. Those answers are still good,
/// they just do not say which files they covered, so they load as `Unrecorded`.
const OLDEST_SUPPORTED_VERSION: u32 = 1;
const LOCK_FILE: &str = "trusted-folders.lock";
/// Lives next to the session index on purpose, see [`TrustedFolders::grandfathered_roots`].
const PRE_TRUST_FILE: &str = "pre-trust-roots.json";
/// How many recorded working directories the snapshot is willing to resolve.
/// Resolving one costs a realpath per path component and a stat per ancestor,
/// and this runs on the first start after an upgrade, on the same thread ACP
/// answers its client on. A thousand distinct working directories is already a
/// long history, and the ones kept are the ones used most recently.
const MAX_FROZEN_CWDS: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalFolder {
    path: PathBuf,
    entry: String,
}

impl CanonicalFolder {
    pub fn resolve(path: &Path) -> Result<Self, TrustedFoldersError> {
        let metadata = fs::metadata(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                TrustedFoldersError::Missing {
                    path: path.to_path_buf(),
                }
            } else {
                TrustedFoldersError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        if !metadata.is_dir() {
            return Err(TrustedFoldersError::NotDirectory {
                path: path.to_path_buf(),
            });
        }

        let canonical = fs::canonicalize(path).map_err(|source| TrustedFoldersError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let canonical = canonicalize_clean(&canonical);
        let entry = path_entry(&canonical)?;
        Ok(Self {
            path: canonical,
            entry,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn as_str(&self) -> &str {
        &self.entry
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Changed,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustStatus {
    Trusted,
    Rejected,
    Unknown,
}

/// How a recorded working directory maps to the folder a trust answer is about.
/// The project root rule (the git checkout that owns a directory, stopping below
/// the home directory) lives in `maki-config`, which sits above this crate, so
/// the caller hands it in.
pub type ProjectRootOf<'a> = &'a dyn Fn(&Path) -> PathBuf;

/// What the store says about a folder, read against the gated files that folder
/// ships right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrustDecision {
    /// Trusted, and every kind of gated file present now was named in the
    /// question the user answered.
    Trusted,
    /// Trusted before, and now shipping kinds of gated file that answer never
    /// covered. `added` names them so the question can say what changed.
    Widened {
        added: Vec<String>,
    },
    /// A yes from a store written before Maki wrote down what a yes covered.
    Unrecorded,
    /// No answer, but this is the project root of a folder that was already in
    /// use before folder trust existed, so its shared config used to load
    /// without a question.
    Grandfathered,
    Rejected,
    Unknown,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FolderTrustDecision {
    pub path: PathBuf,
    pub status: TrustStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustedFoldersError {
    #[error("trusted folder does not exist: {path:?}")]
    Missing { path: PathBuf },
    #[error("trusted folder is not a directory: {path:?}")]
    NotDirectory { path: PathBuf },
    #[error("trusted folder path is not valid UTF-8: {path:?}")]
    NonUtf8 { path: PathBuf },
    #[error("trusted folder path contains a control character: {path:?}")]
    ControlCharacter { path: PathBuf },
    #[error("a missing folder must be removed by absolute path: {path:?}")]
    RelativeMissing { path: PathBuf },
    #[error("unsupported trusted folder store version {version} in {path:?}")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("trusted folders are being changed by another Maki process: {path:?}")]
    Locked { path: PathBuf },
    #[error("cannot access trusted folder state at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse trusted folder state at {path:?}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot write trusted folder state at {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: StorageError,
    },
}

/// A recorded yes, together with the kinds of gated file the user could see
/// named in the question. Keeping the set means a yes about
/// `.maki/permissions.toml` never quietly covers an `.maki/init.lua` that
/// arrives in a later pull. It is a set of file names, never a hash of their
/// contents, so editing a file the user already trusted asks nothing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum FolderEntry {
    Recorded {
        path: String,
        files: Vec<String>,
    },
    /// How version 1 spelled a trusted folder. Nothing recorded what such an
    /// answer covered, so it keeps covering everything until the next start
    /// writes down what the folder ships that day.
    Unrecorded(String),
}

impl FolderEntry {
    fn path(&self) -> &str {
        match self {
            Self::Recorded { path, .. } | Self::Unrecorded(path) => path,
        }
    }

    fn files(&self) -> Option<&[String]> {
        match self {
            Self::Recorded { files, .. } => Some(files),
            Self::Unrecorded(_) => None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Document {
    version: u32,
    folders: Vec<FolderEntry>,
    #[serde(default)]
    rejected: Vec<String>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            folders: Vec::new(),
            rejected: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct TrustedFolders {
    document_path: PathBuf,
    lock_path: PathBuf,
    sessions_dir: PathBuf,
}

impl TrustedFolders {
    pub fn new(state: &StateDir) -> Self {
        Self {
            document_path: state.path().join(DOCUMENT_FILE),
            lock_path: state.path().join(LOCK_FILE),
            sessions_dir: state.path().join(SESSIONS_DIR),
        }
    }

    pub fn contains(&self, folder: &CanonicalFolder) -> Result<bool, TrustedFoldersError> {
        Ok(self.status(folder)? == TrustStatus::Trusted)
    }

    pub fn status(&self, folder: &CanonicalFolder) -> Result<TrustStatus, TrustedFoldersError> {
        let document = self.load()?;
        Ok(document.status(folder))
    }

    /// `present` is the gated file kinds the folder ships now. Passing them in
    /// keeps this crate out of the business of knowing which files are gated.
    ///
    /// This is also where the grandfather snapshot gets frozen, because it is
    /// the one call every startup makes before it opens a session.
    pub fn decide(
        &self,
        folder: &CanonicalFolder,
        present: &[&str],
        project_root: ProjectRootOf,
    ) -> Result<TrustDecision, TrustedFoldersError> {
        let grandfathered = self.grandfathered_roots(project_root);
        Ok(self.load()?.decide(folder, present, &grandfathered))
    }

    /// Grandfathering answers a one-time migration question: which project
    /// roots already loaded their `.maki` without anybody being asked, back
    /// before folder trust existed. So the set is taken once and then frozen.
    /// Reading the live session index on every start would instead let a run
    /// that was just denied write its own evidence, because every entry point
    /// records a session for the folder it was refused in, and come back
    /// trusted next time. A fresh install has no sessions yet, so its snapshot
    /// is empty and nothing is grandfathered.
    ///
    /// The snapshot lives beside the session index rather than in the trust
    /// store, and it is claimed with a create-new open rather than the store
    /// lock. That is what keeps the two in step: a run can only add a working
    /// directory to the index if it can write that directory, and if it can
    /// write that directory it could also have frozen the snapshot first. So
    /// there is no longer a run that is refused, records its own working
    /// directory anyway, and gets it grandfathered by the next start. A
    /// concurrent `maki trust add` holding the store lock cannot delay the
    /// freeze either, because the freeze does not want that lock.
    ///
    /// Whatever ends up on disk is what this run uses, never the set it just
    /// computed, so a run only ever trusts a snapshot that is durable and that
    /// every later run will read the same way.
    fn grandfathered_roots(&self, project_root: ProjectRootOf) -> Vec<String> {
        let path = self.sessions_dir.join(PRE_TRUST_FILE);
        if !path.exists()
            && let Err(error) = self.freeze_pre_trust_roots(&path, project_root)
        {
            tracing::warn!(%error, path = %path.display(), "cannot record which folders predate folder trust");
        }
        let frozen = match fs::read(&path) {
            Ok(frozen) => frozen,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, path = %path.display(), "cannot read which folders predate folder trust, nothing is grandfathered this run");
                }
                return Vec::new();
            }
        };
        match serde_json::from_slice(&frozen) {
            Ok(roots) => roots,
            // An empty snapshot is a real answer, a broken one is not, so a
            // file nobody can parse must not sit there answering "nothing" for
            // the rest of this install's life. Grandfathering stays off for
            // this run and the next start takes the snapshot again. Taking it
            // again right here would race a start that has claimed the path
            // and is about to fill it.
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "the record of folders that predate folder trust is damaged, taking it again on the next start");
                if let Err(error) = fs::remove_file(&path) {
                    tracing::warn!(%error, path = %path.display(), "cannot clear the damaged record of folders that predate folder trust");
                }
                Vec::new()
            }
        }
    }

    /// The snapshot is about project roots, so every recorded working directory
    /// goes through the same walk a start in that directory would do. A prefix
    /// test instead of this would hand a grant to every directory above a
    /// session, including one that only happens to contain somebody's checkout.
    /// Every one of those walks touches the disk, so how many a start pays for
    /// is capped, see [`MAX_FROZEN_CWDS`].
    ///
    /// The path is claimed with a create-new open before anything is written,
    /// so two starts racing here cannot freeze two different sets: the loser
    /// stops and reads what the winner wrote. The content then arrives through
    /// `atomic_write`, because a half-written snapshot would be read as an
    /// answer by every later start.
    fn freeze_pre_trust_roots(
        &self,
        path: &Path,
        project_root: ProjectRootOf,
    ) -> Result<(), StorageError> {
        fs::create_dir_all(&self.sessions_dir)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut roots: Vec<String> = recorded_cwds(&self.sessions_dir, MAX_FROZEN_CWDS)
            .iter()
            .filter_map(|cwd| path_entry(&project_root(Path::new(cwd))).ok())
            .collect();
        roots.sort();
        roots.dedup();
        atomic_write(path, &serde_json::to_vec(&roots)?)
    }

    /// Records a yes for `folder` covering `files`. An existing entry keeps
    /// what it already covered, so a file that is temporarily absent, say
    /// because another branch is checked out, does not cost the folder its
    /// answer for that file.
    pub fn add(
        &self,
        folder: &CanonicalFolder,
        files: &[&str],
    ) -> Result<Change, TrustedFoldersError> {
        let _lock = self.mutation_lock()?;
        let mut document = self.load()?;
        let index = document
            .folders
            .iter()
            .position(|entry| entry.path() == folder.as_str());

        let mut covered: Vec<String> = files.iter().map(|file| (*file).to_owned()).collect();
        if let Some(recorded) = index.and_then(|index| document.folders[index].files()) {
            covered.extend_from_slice(recorded);
        }
        covered.sort();
        covered.dedup();

        let was_rejected = document
            .rejected
            .iter()
            .any(|entry| entry == folder.as_str());
        let already_covered =
            index.is_some_and(|index| document.folders[index].files() == Some(covered.as_slice()));
        if already_covered && !was_rejected {
            return Ok(Change::Unchanged);
        }

        document.rejected.retain(|entry| entry != folder.as_str());
        let entry = FolderEntry::Recorded {
            path: folder.as_str().to_owned(),
            files: covered,
        };
        match index {
            Some(index) => document.folders[index] = entry,
            None => document.folders.push(entry),
        }
        self.save(&mut document)?;
        Ok(Change::Changed)
    }

    /// Adds a gated file Maki wrote itself to a folder that is already
    /// trusted, so the next start does not read Maki's own write as something
    /// the project added and ask about it.
    ///
    /// A folder with no answer keeps none: writing a file into a folder is not
    /// a reason to trust it. An answer from before file sets already covers
    /// everything, so it is left alone rather than narrowed to this one file.
    pub fn cover_written_file(
        &self,
        folder: &CanonicalFolder,
        file: &str,
    ) -> Result<Change, TrustedFoldersError> {
        let _lock = self.mutation_lock()?;
        let mut document = self.load()?;
        let Some(FolderEntry::Recorded { files, .. }) = document
            .folders
            .iter_mut()
            .find(|entry| entry.path() == folder.as_str())
        else {
            return Ok(Change::Unchanged);
        };
        if files.iter().any(|covered| covered == file) {
            return Ok(Change::Unchanged);
        }
        files.push(file.to_owned());
        files.sort();
        self.save(&mut document)?;
        Ok(Change::Changed)
    }

    pub fn reject(&self, folder: &CanonicalFolder) -> Result<Change, TrustedFoldersError> {
        let _lock = self.mutation_lock()?;
        let mut document = self.load()?;
        if document
            .rejected
            .iter()
            .any(|entry| entry == folder.as_str())
        {
            return Ok(Change::Unchanged);
        }
        document
            .folders
            .retain(|entry| entry.path() != folder.as_str());
        document.rejected.push(folder.as_str().to_owned());
        self.save(&mut document)?;
        Ok(Change::Changed)
    }

    /// Matching on the normalized path rather than the exact stored spelling is
    /// what makes this the way out of a store somebody hand-edited. A stray
    /// trailing slash can never grant trust, because `status` compares exactly,
    /// but it must never be able to trap a decision in the file either.
    pub fn remove(&self, path: &Path) -> Result<Change, TrustedFoldersError> {
        let candidates = removal_entries(path)?;
        let _lock = self.mutation_lock()?;
        let mut document = self.load()?;
        let Some(candidate) = candidates.iter().find(|candidate| {
            document
                .folders
                .iter()
                .any(|entry| names_same_folder(entry.path(), candidate))
                || document
                    .rejected
                    .iter()
                    .any(|entry| names_same_folder(entry, candidate))
        }) else {
            return Ok(Change::Unchanged);
        };
        document
            .folders
            .retain(|entry| !names_same_folder(entry.path(), candidate));
        document
            .rejected
            .retain(|entry| !names_same_folder(entry, candidate));
        self.save(&mut document)?;
        Ok(Change::Changed)
    }

    pub fn list(&self) -> Result<Vec<FolderTrustDecision>, TrustedFoldersError> {
        let document = self.load()?;
        let mut decisions = document
            .folders
            .iter()
            .map(|entry| FolderTrustDecision {
                path: PathBuf::from(entry.path()),
                status: TrustStatus::Trusted,
            })
            .chain(document.rejected.iter().map(|path| FolderTrustDecision {
                path: PathBuf::from(path),
                status: TrustStatus::Rejected,
            }))
            .collect::<Vec<_>>();
        decisions.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(decisions)
    }

    fn mutation_lock(&self) -> Result<Lock, TrustedFoldersError> {
        match Lock::acquire(&self.lock_path) {
            Ok(lock) => Ok(lock),
            Err(LockError::Held { path }) => Err(TrustedFoldersError::Locked { path }),
            Err(LockError::Io { path, source }) => Err(TrustedFoldersError::Io { path, source }),
        }
    }

    fn load(&self) -> Result<Document, TrustedFoldersError> {
        let contents = match fs::read_to_string(&self.document_path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document::default());
            }
            Err(source) => {
                return Err(TrustedFoldersError::Io {
                    path: self.document_path.clone(),
                    source,
                });
            }
        };
        let mut document: Document =
            serde_json::from_str(&contents).map_err(|source| TrustedFoldersError::Json {
                path: self.document_path.clone(),
                source,
            })?;
        if !(OLDEST_SUPPORTED_VERSION..=DOCUMENT_VERSION).contains(&document.version) {
            return Err(TrustedFoldersError::UnsupportedVersion {
                path: self.document_path.clone(),
                version: document.version,
            });
        }
        document.version = DOCUMENT_VERSION;

        // A malformed entry cannot grant anything, because `status` compares
        // against a canonical absolute path and nothing else. Refusing to load
        // over one would only turn a cosmetic typo into a store nobody can fix
        // without a text editor, `maki trust remove` included.
        Ok(document)
    }

    fn save(&self, document: &mut Document) -> Result<(), TrustedFoldersError> {
        document
            .folders
            .sort_by(|left, right| left.path().cmp(right.path()));
        document
            .folders
            .dedup_by(|left, right| left.path() == right.path());
        document.rejected.sort();
        document.rejected.dedup();
        let mut bytes =
            serde_json::to_vec_pretty(document).map_err(|source| TrustedFoldersError::Json {
                path: self.document_path.clone(),
                source,
            })?;
        bytes.push(b'\n');
        atomic_write(&self.document_path, &bytes).map_err(|source| TrustedFoldersError::Write {
            path: self.document_path.clone(),
            source,
        })
    }
}

impl Document {
    fn entry(&self, folder: &CanonicalFolder) -> Option<&FolderEntry> {
        self.folders
            .iter()
            .find(|entry| entry.path() == folder.as_str())
    }

    fn status(&self, folder: &CanonicalFolder) -> TrustStatus {
        if self.entry(folder).is_some() {
            TrustStatus::Trusted
        } else if self.rejected.iter().any(|entry| entry == folder.as_str()) {
            TrustStatus::Rejected
        } else {
            TrustStatus::Unknown
        }
    }

    /// `grandfathered` already holds project roots and `folder` is the project
    /// root the caller is deciding about, so this is an exact compare. Anything
    /// looser would grant a folder that never had a session of its own.
    fn decide(
        &self,
        folder: &CanonicalFolder,
        present: &[&str],
        grandfathered: &[String],
    ) -> TrustDecision {
        let Some(entry) = self.entry(folder) else {
            return match self.status(folder) {
                TrustStatus::Rejected => TrustDecision::Rejected,
                _ if grandfathered.iter().any(|root| root == folder.as_str()) => {
                    TrustDecision::Grandfathered
                }
                _ => TrustDecision::Unknown,
            };
        };
        let Some(covered) = entry.files() else {
            return TrustDecision::Unrecorded;
        };
        let added: Vec<String> = present
            .iter()
            .filter(|file| !covered.iter().any(|recorded| recorded == *file))
            .map(|file| (*file).to_owned())
            .collect();
        if added.is_empty() {
            TrustDecision::Trusted
        } else {
            TrustDecision::Widened { added }
        }
    }
}

fn path_entry(path: &Path) -> Result<String, TrustedFoldersError> {
    let Some(value) = path.to_str() else {
        return Err(TrustedFoldersError::NonUtf8 {
            path: path.to_path_buf(),
        });
    };
    if value.chars().any(char::is_control) {
        return Err(TrustedFoldersError::ControlCharacter {
            path: path.to_path_buf(),
        });
    }
    Ok(value.to_owned())
}

/// `candidate` is already normalized, so normalizing the stored side is what
/// lets `remove` reach an entry somebody typed a trailing slash or a `..` into.
fn names_same_folder(entry: &str, candidate: &str) -> bool {
    normalize_path(Path::new(entry)) == Path::new(candidate)
}

fn removal_entries(path: &Path) -> Result<Vec<String>, TrustedFoldersError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            let normalized = normalize_path(path);
            let mut entries = vec![path_entry(&normalized)?];
            if metadata.is_dir() {
                let canonical = CanonicalFolder::resolve(path)?;
                if !entries.iter().any(|entry| entry == canonical.as_str()) {
                    entries.push(canonical.as_str().to_owned());
                }
            }
            Ok(entries)
        }
        Err(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            if !path.is_absolute() {
                return Err(TrustedFoldersError::RelativeMissing {
                    path: path.to_path_buf(),
                });
            }
            let normalized = normalize_path(path);
            Ok(vec![path_entry(&normalized)?])
        }
        Err(source) => Err(TrustedFoldersError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::sessions::{Session, TitleSource};

    const SESSION_MODEL: &str = "test-model";
    const INIT_LUA: &str = "init.lua";
    const MCP_TOML: &str = "mcp.toml";
    const ENV_FILE: &str = ".env";
    const NOTHING: &[&str] = &[];
    const GIT_DIR: &str = ".git";
    const SOURCE_DIR: &str = "src";

    fn store(dir: &tempfile::TempDir) -> TrustedFolders {
        TrustedFolders::new(&StateDir::from_path(dir.path().to_path_buf()))
    }

    fn folder(dir: &tempfile::TempDir, name: &str) -> CanonicalFolder {
        let path = dir.path().join(name);
        fs::create_dir(&path).unwrap();
        CanonicalFolder::resolve(&path).unwrap()
    }

    fn checkout(dir: &tempfile::TempDir, name: &str) -> CanonicalFolder {
        let folder = folder(dir, name);
        fs::create_dir(folder.path().join(GIT_DIR)).unwrap();
        folder
    }

    /// Stands in for the rule `maki-config` owns: the checkout a directory
    /// belongs to, or the directory itself when it belongs to none.
    fn checkout_root(cwd: &Path) -> PathBuf {
        cwd.ancestors()
            .find(|path| path.join(GIT_DIR).is_dir())
            .unwrap_or(cwd)
            .to_path_buf()
    }

    #[derive(Clone, Deserialize, Serialize)]
    struct StoredMessage;

    impl TitleSource for StoredMessage {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    fn record_session(dir: &tempfile::TempDir, cwd: &Path) {
        let state = StateDir::from_path(dir.path().to_path_buf());
        let mut session: Session<StoredMessage, u32, ()> =
            Session::new(SESSION_MODEL, cwd.to_str().unwrap());
        session.save(&state).unwrap();
    }

    #[test]
    fn missing_document_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store(&dir).list().unwrap().is_empty());
    }

    #[test]
    fn add_remove_and_duplicate_changes_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");

        assert_eq!(store.add(&folder, &[INIT_LUA]).unwrap(), Change::Changed);
        assert_eq!(store.add(&folder, &[INIT_LUA]).unwrap(), Change::Unchanged);
        assert!(store.contains(&folder).unwrap());
        assert_eq!(store.remove(folder.path()).unwrap(), Change::Changed);
        assert!(!store.contains(&folder).unwrap());
        assert_eq!(store.remove(folder.path()).unwrap(), Change::Unchanged);
    }

    #[test]
    fn rejection_is_stored_and_later_trust_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");

        assert_eq!(store.reject(&folder).unwrap(), Change::Changed);
        assert_eq!(store.reject(&folder).unwrap(), Change::Unchanged);
        assert_eq!(store.status(&folder).unwrap(), TrustStatus::Rejected);
        assert_eq!(
            store.list().unwrap(),
            vec![FolderTrustDecision {
                path: folder.path().to_path_buf(),
                status: TrustStatus::Rejected,
            }]
        );

        assert_eq!(store.add(&folder, NOTHING).unwrap(), Change::Changed);
        assert_eq!(store.status(&folder).unwrap(), TrustStatus::Trusted);
    }

    #[test]
    fn rejection_replaces_existing_trust() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");
        store.add(&folder, &[INIT_LUA]).unwrap();

        assert_eq!(store.reject(&folder).unwrap(), Change::Changed);
        assert_eq!(store.status(&folder).unwrap(), TrustStatus::Rejected);
        assert_eq!(store.remove(folder.path()).unwrap(), Change::Changed);
        assert_eq!(store.status(&folder).unwrap(), TrustStatus::Unknown);
    }

    #[test]
    fn old_documents_default_to_no_rejections() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        fs::write(&store.document_path, r#"{"version":1,"folders":[]}"#).unwrap();

        assert!(store.list().unwrap().is_empty());
    }

    /// The recorded set is what a later start compares against, so an answer
    /// only ever covers the kinds of file the user was shown.
    #[test_case(&[INIT_LUA], &[INIT_LUA], TrustDecision::Trusted ; "same_file_is_covered")]
    #[test_case(&[INIT_LUA], NOTHING, TrustDecision::Trusted ; "a_removed_file_is_covered")]
    #[test_case(&[INIT_LUA, MCP_TOML], &[INIT_LUA], TrustDecision::Trusted ; "a_subset_is_covered")]
    fn a_recorded_answer_covers_what_it_named(
        recorded: &[&str],
        present: &[&str],
        expected: TrustDecision,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");
        store.add(&folder, recorded).unwrap();

        assert_eq!(
            store.decide(&folder, present, &checkout_root).unwrap(),
            expected
        );
    }

    /// The defect this field exists to prevent: a yes about one file must not
    /// carry over to a kind of file the project added afterwards.
    #[test]
    fn a_newly_shipped_kind_of_file_is_not_covered() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");
        store.add(&folder, &[MCP_TOML]).unwrap();

        assert_eq!(
            store
                .decide(&folder, &[INIT_LUA, MCP_TOML], &checkout_root)
                .unwrap(),
            TrustDecision::Widened {
                added: vec![INIT_LUA.to_owned()],
            }
        );

        store.add(&folder, &[INIT_LUA]).unwrap();
        assert_eq!(
            store
                .decide(&folder, &[INIT_LUA, MCP_TOML], &checkout_root)
                .unwrap(),
            TrustDecision::Trusted
        );
    }

    /// A file that comes and goes with the checked out branch must not cost the
    /// folder the answer it already has for that file.
    #[test]
    fn a_file_that_disappears_keeps_its_place_in_the_recorded_set() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");
        store.add(&folder, &[INIT_LUA, ENV_FILE]).unwrap();

        assert_eq!(store.add(&folder, &[ENV_FILE]).unwrap(), Change::Unchanged);
        assert_eq!(
            store
                .decide(&folder, &[INIT_LUA, ENV_FILE], &checkout_root)
                .unwrap(),
            TrustDecision::Trusted
        );
    }

    /// Version 1 stored a bare path. Those answers survive the upgrade, and
    /// they report that nothing recorded what they covered, so the caller can
    /// write down today's files instead of asking again.
    #[test]
    fn a_version_one_document_keeps_its_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let trusted = folder(&dir, "trusted");
        let rejected = folder(&dir, "rejected");
        fs::write(
            &store.document_path,
            serde_json::json!({
                "version": 1,
                "folders": [trusted.as_str()],
                "rejected": [rejected.as_str()],
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            store.decide(&trusted, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unrecorded
        );
        assert_eq!(
            store
                .decide(&rejected, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Rejected
        );

        store.add(&trusted, &[INIT_LUA]).unwrap();
        let upgraded = fs::read_to_string(&store.document_path).unwrap();
        assert!(upgraded.contains(r#""version": 2"#), "{upgraded}");
        assert_eq!(
            store.decide(&trusted, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Trusted
        );
        assert_eq!(
            store.decide(&trusted, &[MCP_TOML], &checkout_root).unwrap(),
            TrustDecision::Widened {
                added: vec![MCP_TOML.to_owned()],
            }
        );
    }

    /// The bypass a live session index would leave open: every entry point
    /// records a session in the folder it just refused to trust, so the next
    /// start would find that history and hand the folder trust with nobody
    /// asked. The snapshot is taken once, so only folders used before it
    /// counts.
    #[test]
    fn the_grandfather_set_is_frozen_at_the_first_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let used_before = checkout(&dir, "used-before");
        let used_after = checkout(&dir, "used-after");
        record_session(&dir, &used_before.path().join(SOURCE_DIR));

        assert_eq!(
            store
                .decide(&used_before, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Grandfathered
        );

        record_session(&dir, used_after.path());

        assert_eq!(
            store
                .decide(&used_after, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Unknown
        );
        assert_eq!(
            store
                .decide(&used_before, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Grandfathered
        );
    }

    /// The window a lock-guarded freeze left open. A run whose snapshot was
    /// blocked went on to record its own working directory anyway, and the next
    /// start froze that record as history nobody had ever been asked about.
    #[test]
    fn a_held_mutation_lock_cannot_put_this_run_into_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let refused = checkout(&dir, "refused");
        let held = store.mutation_lock().unwrap();

        assert_eq!(
            store.decide(&refused, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unknown
        );

        record_session(&dir, refused.path());
        drop(held);

        assert_eq!(
            store.decide(&refused, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unknown
        );
    }

    /// A session only says its own project root used to load `.maki` without a
    /// question. It says nothing about a directory that merely holds that
    /// checkout, and that directory can be one an attacker writes into.
    #[test]
    fn a_session_in_a_nested_checkout_grandfathers_only_that_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let parent = folder(&dir, "parent");
        let nested_path = parent.path().join("nested");
        fs::create_dir_all(nested_path.join(GIT_DIR)).unwrap();
        let nested = CanonicalFolder::resolve(&nested_path).unwrap();
        record_session(&dir, &nested_path.join(SOURCE_DIR));

        assert_eq!(
            store.decide(&nested, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Grandfathered
        );
        assert_eq!(
            store.decide(&parent, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unknown
        );
    }

    /// A snapshot left half written by an older Maki, or by a disk that filled
    /// up mid write, used to answer "nothing predates folder trust" for good.
    /// It is cleared instead, so the next start writes a whole one.
    #[test_case("" ; "empty_from_a_crash_between_create_and_write")]
    #[test_case("[\"/wo" ; "truncated_json")]
    fn a_damaged_snapshot_is_taken_again_rather_than_believed(damaged: &str) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let used_before = checkout(&dir, "used-before");
        record_session(&dir, &used_before.path().join(SOURCE_DIR));
        let snapshot = dir.path().join(SESSIONS_DIR).join(PRE_TRUST_FILE);
        fs::write(&snapshot, damaged).unwrap();

        assert_eq!(
            store
                .decide(&used_before, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Unknown,
            "a snapshot nobody can read must grandfather nothing"
        );
        assert!(!snapshot.exists(), "the damaged snapshot must be cleared");

        assert_eq!(
            store
                .decide(&used_before, &[INIT_LUA], &checkout_root)
                .unwrap(),
            TrustDecision::Grandfathered,
            "the next start takes the snapshot again"
        );
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&fs::read(&snapshot).unwrap()).unwrap(),
            vec![used_before.as_str().to_owned()]
        );
    }

    /// Nobody has session history on a first install, so the snapshot is empty
    /// and every folder is answered for by the user.
    #[test]
    fn a_store_that_never_saw_a_session_grandfathers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");

        assert_eq!(
            store.decide(&folder, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unknown
        );

        record_session(&dir, folder.path());

        assert_eq!(
            store.decide(&folder, &[INIT_LUA], &checkout_root).unwrap(),
            TrustDecision::Unknown
        );
    }

    #[test]
    fn writes_sorted_pretty_json_with_a_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let second = folder(&dir, "z-project");
        let first = folder(&dir, "a-project");

        store.add(&second, NOTHING).unwrap();
        store.add(&first, NOTHING).unwrap();

        let contents = fs::read_to_string(&store.document_path).unwrap();
        assert!(contents.ends_with('\n'));
        assert!(contents.find("a-project").unwrap() < contents.find("z-project").unwrap());
        assert_eq!(
            store.list().unwrap(),
            vec![
                FolderTrustDecision {
                    path: first.path,
                    status: TrustStatus::Trusted,
                },
                FolderTrustDecision {
                    path: second.path,
                    status: TrustStatus::Trusted,
                },
            ]
        );
    }

    #[test]
    fn malformed_and_unsupported_documents_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        fs::write(&store.document_path, "not json").unwrap();
        assert!(matches!(
            store.list(),
            Err(TrustedFoldersError::Json { .. })
        ));

        fs::write(&store.document_path, r#"{"version":3,"folders":[]}"#).unwrap();
        assert!(matches!(
            store.list(),
            Err(TrustedFoldersError::UnsupportedVersion { version: 3, .. })
        ));
    }

    #[test]
    fn unreadable_document_path_is_not_treated_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        fs::create_dir(&store.document_path).unwrap();

        assert!(matches!(store.list(), Err(TrustedFoldersError::Io { .. })));
    }

    /// A cosmetic defect somebody typed into the file must not brick the store.
    /// It grants nothing, because trust is an exact compare, and `remove` still
    /// clears it.
    #[test_case("relative" ; "not_absolute")]
    #[test_case("/a/../b" ; "not_normalized")]
    #[test_case("/a\nb" ; "control_character")]
    fn a_hand_edited_entry_loads_and_grants_nothing(entry: &str) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let real = folder(&dir, "project");
        fs::write(
            &store.document_path,
            serde_json::json!({"version": 1, "folders": [entry, real.as_str()]}).to_string(),
        )
        .unwrap();

        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.contains(&real).unwrap());
        assert_eq!(store.remove(real.path()).unwrap(), Change::Changed);
        assert!(!store.contains(&real).unwrap());
    }

    /// The reported way to brick the store: one trailing slash typed by hand
    /// used to make every `maki trust` command fail.
    #[test]
    fn remove_clears_an_entry_spelled_with_a_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let target = folder(&dir, "project");
        fs::write(
            &store.document_path,
            serde_json::json!({"version": 1, "folders": [format!("{}/", target.as_str())]})
                .to_string(),
        )
        .unwrap();

        assert_eq!(
            store.status(&target).unwrap(),
            TrustStatus::Unknown,
            "a defective entry must never grant trust"
        );
        assert_eq!(store.remove(target.path()).unwrap(), Change::Changed);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn add_rejects_missing_and_non_directory_paths() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(matches!(
            CanonicalFolder::resolve(&missing),
            Err(TrustedFoldersError::Missing { .. })
        ));

        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(matches!(
            CanonicalFolder::resolve(&file),
            Err(TrustedFoldersError::NotDirectory { .. })
        ));
    }

    #[test]
    fn remove_accepts_a_missing_absolute_path_but_not_a_relative_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert_eq!(
            store.remove(&dir.path().join("missing")).unwrap(),
            Change::Unchanged
        );
        assert!(matches!(
            store.remove(Path::new("missing")),
            Err(TrustedFoldersError::RelativeMissing { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn remove_uses_the_listed_identity_after_a_folder_is_replaced() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let original = folder(&dir, "original");
        let target = folder(&dir, "target");
        store.add(&original, NOTHING).unwrap();
        store.add(&target, NOTHING).unwrap();

        fs::remove_dir(original.path()).unwrap();
        symlink(target.path(), original.path()).unwrap();

        assert_eq!(store.remove(original.path()).unwrap(), Change::Changed);
        assert!(store.contains(&target).unwrap());
        assert_eq!(
            store.list().unwrap(),
            vec![FolderTrustDecision {
                path: target.path().to_path_buf(),
                status: TrustStatus::Trusted,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_resolves_to_the_same_identity() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = folder(&dir, "target");
        let alias = dir.path().join("alias");
        symlink(target.path(), &alias).unwrap();

        assert_eq!(CanonicalFolder::resolve(&alias).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn remove_accepts_a_symbolic_link_to_a_trusted_folder() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let target = folder(&dir, "target");
        let alias = dir.path().join("alias");
        symlink(target.path(), &alias).unwrap();
        store.add(&target, NOTHING).unwrap();

        assert_eq!(store.remove(&alias).unwrap(), Change::Changed);
        assert!(store.list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn remove_does_not_treat_other_metadata_errors_as_missing() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join(OsString::from_vec(b"bad\0path".to_vec()));

        assert!(matches!(
            store(&dir).remove(&invalid),
            Err(TrustedFoldersError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_refused() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let non_utf8 = dir.path().join(OsString::from_vec(vec![b'x', 0xff]));
        fs::create_dir(&non_utf8).unwrap();
        assert!(matches!(
            CanonicalFolder::resolve(&non_utf8),
            Err(TrustedFoldersError::NonUtf8 { .. })
        ));
    }

    #[test]
    fn control_character_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("line\nbreak");
        fs::create_dir(&control).unwrap();
        assert!(matches!(
            CanonicalFolder::resolve(&control),
            Err(TrustedFoldersError::ControlCharacter { .. })
        ));
    }

    #[test]
    fn mutation_lock_prevents_lost_updates_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let folder = folder(&dir, "project");
        let held = store.mutation_lock().unwrap();

        assert!(matches!(
            store.add(&folder, NOTHING),
            Err(TrustedFoldersError::Locked { .. })
        ));
        drop(held);
        assert_eq!(store.add(&folder, NOTHING).unwrap(), Change::Changed);
    }
}
