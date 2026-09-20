//! One walk of a directory tree, shared by everything that ranks files.
//!
//! The file picker and `maki.fs.fuzzy_files` both ask this module, so a repo
//! is walked once and ranked with one config however many readers there are.
//!
//! ```text
//! ignore::WalkBuilder -> Corpus (batches of paths) -> ArcSwap
//!                                                       |
//!                            file picker (nucleo) <-----+-----> maki.fs.fuzzy_files
//! ```
//!
//! A corpus is only ever appended to within one [`Corpus::generation`], and a
//! re-walk publishes a whole new generation. That is the contract a reader
//! holding a cursor into the list depends on: same generation, keep reading
//! from where you were; new generation, start over.

use std::cmp::Reverse;
use std::mem;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str, Utf32String, chars};
use tracing::{debug, error, warn};
use unicode_segmentation::UnicodeSegmentation;

/// Paths one `ignore` worker gathers before it takes the walk lock to hand
/// them over. Publishing is a clone of the batch pointers, so batching keeps
/// both that and the lock off the per-entry path.
const BATCH: usize = 1024;
/// A finished walk older than this is redone the next time anything asks for
/// the index, which is how a file created while maki runs turns up.
const STALE_AFTER: Duration = Duration::from_secs(20);
/// How soon a walk of a tree that moved under it, or one a tool marked, is
/// redone. The tools mark the tree on every write, so without a floor an
/// agent editing faster than a walk of the repo completes keeps one running
/// back to back for as long as the edits last.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// How soon a walk that died is tried again. A dead walker leaves a stamp
/// like any other ending, because a root whose walker cannot start would
/// otherwise be asked for another one by every frame that reads the index.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Roots indexed at once. A session per repo is normal, a dozen is not, so
/// the least recently asked for is dropped.
const MAX_ROOTS: usize = 4;
/// Paths one root may retain. The corpus outlives the walk that built it in a
/// process-wide registry, so without a ceiling a walk of a huge tree would pin
/// a list of every path in it for the rest of the session.
const MAX_ENTRIES: usize = 200_000;
/// Entries ranked between two looks at the cancel flag.
const CANCEL_EVERY: usize = 2048;
/// Walks running at once across the process. One is a thread plus a pool of
/// `ignore` workers, and a plugin loop over the subdirectories of a repo asks
/// for one root per iteration, so without a budget a single Lua loop is a few
/// hundred threads. A root that asks while the budget is spent is simply not
/// walked yet: nothing is published and no ending is left behind, so the
/// reader that wanted it asks again and gets the walk once a slot frees.
const MAX_WALKS: usize = 2;
const GIT_DIR_GLOB: &str = "!.git";
const WALKER_CRASHED: &str = "file index walker died before it reached the end of the tree";
const WALKER_UNSTARTABLE: &str = "file index walker failed to start";
const OVERRIDE_FAILED: &str = "file index could not build its .git override";
const CORPUS_FULL: &str = "file index hit its ceiling, the list it publishes is short";
const WALKS_BUSY: &str = "file index is already walking as many roots as it walks at once";
const NON_UTF8_SKIPPED: &str = "file index skipped paths that are not valid UTF-8";
const NOT_DETACHED: &str = "a corpus may only be published by hand on a detached file index";

static INDEXES: OnceLock<Mutex<Vec<FileIndex>>> = OnceLock::new();
static WALKS: AtomicUsize = AtomicUsize::new(0);
static WALK_OBSERVER: Mutex<Option<Arc<WalkObserver>>> = Mutex::new(None);

/// The scoring config every file ranking in maki uses. Exported so a caller
/// that runs its own matcher over the same paths cannot drift from this one.
pub const FILE_MATCH_CONFIG: Config = Config::DEFAULT.match_paths();

/// Parses {query} the way the file picker's search box does.
pub fn file_pattern(query: &str) -> Pattern {
    Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
}

/// The haystack for {path}, built the way the file picker's injector builds
/// the one it hands nucleo.
///
/// This is not `Utf32Str::new`: that takes its byte-indexed fast path for any
/// string whose graphemes all start with an ASCII codepoint, while a
/// `Utf32String` takes it only for a string that is ASCII. So `Utf32Str::new`
/// scores `"e\u{301}dit.rs"` as nine bytes with the combining mark between
/// `e` and `d`, and the picker scores it as seven graphemes. That is a
/// different score, a different place in the list and offsets that do not
/// name characters, so every ranking in maki builds its haystack here.
pub fn file_haystack<'a>(path: &'a str, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    if path.is_ascii() {
        return Utf32Str::Ascii(path.as_bytes());
    }
    buf.clear();
    buf.extend(chars::graphemes(path));
    Utf32Str::Unicode(buf)
}

/// The same haystack for a caller that has to own it, like the picker's
/// injector, which keeps one per path for as long as its list lives.
pub fn file_haystack_owned(path: &str) -> Utf32String {
    Utf32String::from(path)
}

/// Byte offset in {text} of every unit the matcher counts in, or `None` when
/// the matcher is already counting bytes.
///
/// [`file_haystack`] byte-indexes a haystack only for a string that is ASCII,
/// so the two cases are bytes and grapheme clusters with nothing in between.
/// Which one applies still has to be read off the value the matcher was
/// actually given rather than guessed, since only that value knows.
fn unit_starts(text: &str, haystack: Utf32Str<'_>) -> Option<Vec<usize>> {
    match haystack {
        Utf32Str::Ascii(_) => None,
        Utf32Str::Unicode(_) => Some(text.grapheme_indices(true).map(|(at, _)| at).collect()),
    }
}

/// The 1-based inclusive byte ranges of {text} that {indices} names, with
/// units that touch coalesced into one range, which is what a caller
/// highlighting a match needs. Ascending, non-overlapping and always on
/// character boundaries, so Lua's `sub(from, to)` hands back whole matched
/// characters however many bytes each of them took.
///
/// Ranges rather than the starts alone, because a start names nothing a
/// caller can slice: one grapheme is one to several bytes, and slicing a
/// single byte off an emoji path hands Lua a lone continuation byte.
///
/// {indices} is whatever `Pattern::indices` left behind for the same {text}
/// and {haystack}. It is sorted and deduplicated in place here so one buffer
/// can be reused for a whole list.
pub fn byte_highlights(
    text: &str,
    haystack: Utf32Str<'_>,
    indices: &mut Vec<u32>,
) -> Vec<(u32, u32)> {
    indices.sort_unstable();
    indices.dedup();
    let starts = unit_starts(text, haystack);
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for unit in indices.iter().map(|at| *at as usize) {
        let (from, to) = match &starts {
            Some(starts) => match starts.get(unit) {
                Some(from) => (*from, starts.get(unit + 1).copied().unwrap_or(text.len())),
                None => continue,
            },
            None => (unit, unit + 1),
        };
        // Every haystack maki matches against comes from [`file_haystack`],
        // which byte-indexes ASCII only. This is public though, and a
        // byte-indexed haystack over a string that is not ASCII names bytes
        // inside characters: Rust panics on slicing one and Lua would be
        // handed invalid UTF-8.
        if !text.is_char_boundary(from) || !text.is_char_boundary(to) {
            continue;
        }
        match ranges.last_mut() {
            // Inclusive and 1-based, so the range before this one ends on the
            // byte before {from} exactly when the two units touch.
            Some(last) if last.1 as usize == from => last.1 = to as u32,
            _ => ranges.push((from as u32 + 1, to as u32)),
        }
    }
    ranges
}

/// What a caller wants ranked.
pub struct FileQuery<'a> {
    /// What the user typed, parsed by [`file_pattern`].
    pub query: &'a str,
    /// How many paths to return at most.
    pub limit: usize,
    /// Also report where the query matched each returned path. Off by
    /// default: it is a second matcher pass, so a caller that only lists the
    /// paths never pays for ranges it would not draw.
    pub highlights: bool,
}

/// One ranked path.
pub struct FileMatch {
    /// Relative to the root, with a trailing separator on the directories.
    pub path: String,
    /// 1-based inclusive byte ranges into `path`, one per run of matched
    /// characters, ascending. Empty unless [`FileQuery::highlights`] asked
    /// for them.
    pub highlights: Vec<(u32, u32)>,
}

/// The answer to one [`FileQuery`].
pub struct Ranked {
    /// Best first.
    pub items: Vec<FileMatch>,
    /// The walk behind the corpus these were ranked against had finished. False
    /// means a walk is filling the list, so the same query asked again will
    /// find more, and an empty `items` is "not yet" rather than "nothing
    /// matches".
    pub complete: bool,
    /// That walk died partway through the tree, so the list is as much of it
    /// as the walker reached.
    pub crashed: bool,
    /// That walk stopped at the host's ceiling, so the list is the prefix of
    /// the tree it had read by then.
    pub truncated: bool,
}

/// How a walk ended, for the host that turns walks into plugin events.
pub struct WalkEnd {
    /// The canonical root that was walked.
    pub root: PathBuf,
    /// Paths the walk left in the corpus.
    pub files: usize,
    pub crashed: bool,
    pub truncated: bool,
}

type WalkObserver = dyn Fn(&WalkEnd) + Send + Sync;

/// Installs {observer}, called once for every walk that ends, whichever root
/// it was walking and however it ended. A cancelled walk is not an ending:
/// nothing was published and another walk is on its way.
///
/// Replaces whatever was installed before, because a plugin host rebuilt by
/// `/reload` is a new consumer of the same walks and the old one is gone.
pub fn on_walk_end(observer: impl Fn(&WalkEnd) + Send + Sync + 'static) {
    *lock(&WALK_OBSERVER) = Some(Arc::new(observer));
}

/// Tells the observer how one walk ended. The `Arc` is cloned out of the lock
/// first, so an observer that asks the index anything cannot deadlock against
/// the install.
fn notify(shared: &Shared, corpus: &Corpus) {
    let Some(observer) = lock(&WALK_OBSERVER).clone() else {
        return;
    };
    observer(&WalkEnd {
        root: shared.root.clone(),
        files: corpus.len,
        crashed: corpus.crashed,
        truncated: corpus.truncated,
    });
}

/// Paths under one root, relative to it, with a trailing separator on the
/// directories. Held behind an `Arc` so a reader keeps a consistent list for
/// as long as it needs one while the walk carries on.
#[derive(Default)]
pub struct Corpus {
    batches: Vec<Arc<[Box<str>]>>,
    len: usize,
    generation: u64,
    /// The walk that produced this reached the end of what it was going to
    /// read. A walk stopped by `MAX_ENTRIES` sets it too, because nothing
    /// more is coming and the list will not grow.
    pub complete: bool,
    /// The walk gave up: it panicked, or it never started. `complete` is set
    /// too, because nothing more is coming and nobody should keep waiting.
    pub crashed: bool,
    /// The walk stopped at `MAX_ENTRIES` with tree left to read, so this is a
    /// prefix of the tree rather than the whole of it. `complete` is set too,
    /// for the same reason.
    pub truncated: bool,
}

impl Corpus {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Which walk produced this list. Entries are only ever appended within
    /// one generation, so a reader that took the first N is safe to carry on
    /// from N for exactly as long as this does not move. A re-walk publishes a
    /// new generation, and a new generation means the old cursor names
    /// different entries and has to be thrown away.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.batches
            .iter()
            .flat_map(|batch| batch.iter().map(|p| &**p))
    }

    /// Everything past {from}, for a reader that already took the first
    /// {from} entries and is following the walk as it fills. Only meaningful
    /// against the generation the cursor was taken from.
    ///
    /// Found by batch rather than by skipping {from} entries: a reader
    /// following a walk asks on every tick, and skipping walks the whole
    /// prefix it has already seen each time.
    pub fn tail(&self, from: usize) -> impl Iterator<Item = &str> {
        let mut skip = from;
        let mut start = 0;
        for batch in &self.batches {
            if skip < batch.len() {
                break;
            }
            skip -= batch.len();
            start += 1;
        }
        self.batches[start..]
            .iter()
            .flat_map(|batch| batch.iter().map(|p| &**p))
            .skip(skip)
    }
}

/// How a walk ended: when, and how long what it left is served for before
/// anything asking for the index walks the tree again.
///
/// The two apart rather than one deadline, because how much an ending is
/// worth is exactly what the endings differ by, and a deadline built by
/// backdating the stamp instead has no answer on a host whose clock is
/// younger than the window it has to reach back through.
struct Ending {
    at: Instant,
    serve_for: Duration,
}

impl Ending {
    fn now(serve_for: Duration) -> Self {
        Self {
            at: Instant::now(),
            serve_for,
        }
    }

    /// When what this ending left stops being worth serving, which is what
    /// two endings asking for a walk at different moments are compared by.
    fn due(&self) -> Instant {
        self.at + self.serve_for
    }

    /// Whether what this ending left has been served for as long as it is
    /// worth, so the next thing to ask walks the tree again.
    fn stale(&self) -> bool {
        Instant::now() >= self.due()
    }
}

#[derive(Default)]
struct Walk {
    running: bool,
    /// A reader asked for a walk while this one was being cancelled. It could
    /// not start one itself, so the walker hands over to a fresh walk on its
    /// way out instead of leaving the root cancelled behind it.
    restart: bool,
    /// The tree moved while this walk was reading it. Its workers may have
    /// passed the directory that changed before the change happened, and
    /// nothing can tell, so the list it ends with describes neither the tree
    /// it started on nor the one it ends on. A walk no walker survived is
    /// marked the same way, for the same reason, and the mark is what makes
    /// a reader ask for another walk once the ending has been served.
    dirty: bool,
    /// How the last walk ended. Unset until one has, which is how a reader
    /// asks for a walk.
    ended: Option<Ending>,
    generation: u64,
    batches: Vec<Arc<[Box<str>]>>,
    len: usize,
    /// The walk gave up on the rest of the tree at `MAX_ENTRIES`.
    truncated: bool,
}

struct Shared {
    root: PathBuf,
    /// Nothing may start a walker behind this index: its corpus is published
    /// by hand, so a test driving a walk through an exact sequence of states
    /// cannot have a real one race it.
    detached: bool,
    corpus: ArcSwap<Corpus>,
    walk: Mutex<Walk>,
    /// Readers holding a [`FileReader`]. The walk is shared, so it is only
    /// cancelled once the last of them has let go.
    readers: AtomicUsize,
    /// Entries the running walk could not name, counted so the walk logs once
    /// on its way out instead of once per entry.
    skipped: AtomicUsize,
    cancel: AtomicBool,
    /// A worker acted on `cancel`. Latched, and cleared only by the start of
    /// a walk, because `refresh` may clear `cancel` at any moment while the
    /// walk that cancel stopped is still winding down.
    quit: AtomicBool,
}

/// One of the `MAX_WALKS` slots, held for as long as a walk runs.
struct WalkPermit;

impl WalkPermit {
    fn take() -> Option<Self> {
        WALKS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |walking| {
                (walking < MAX_WALKS).then_some(walking + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for WalkPermit {
    fn drop(&mut self) {
        WALKS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A handle on the index for one root. Cloning shares the walk and its
/// results.
#[derive(Clone)]
pub struct FileIndex {
    shared: Arc<Shared>,
}

/// The index for {root}, walked on first use and re-walked when it goes
/// stale. Callers get the same index for the same root, so the walk happens
/// once however many of them there are.
///
/// {root} is canonicalised first: `/repo`, `/repo/`, `/repo/./` and a
/// symlinked spelling all name one tree, and each extra spelling would
/// otherwise be another index and another full walk of it.
pub fn file_index(root: &Path) -> FileIndex {
    resolved_index(canonical_root(root))
}

/// The index for a {root} the caller has already resolved, keyed on exactly
/// the `PathBuf` it holds.
///
/// A caller that confines a root to a directory has to resolve it to check
/// it, and resolving it a second time here would be a second answer: a
/// component swapped for a symlink between the two lets the walk run
/// somewhere the check never saw. So the path that was checked is the path
/// that is walked.
pub fn resolved_index(root: PathBuf) -> FileIndex {
    let mut roots = lock(INDEXES.get_or_init(Mutex::default));
    let index = match roots.iter().position(|i| i.shared.root == root) {
        Some(at) => roots.remove(at),
        None => FileIndex::new(root),
    };
    roots.insert(0, index.clone());
    evict(&mut roots);
    drop(roots);
    index.refresh();
    index
}

/// Stops every walk in the process. Readers are on their way out by the time
/// this is called, and a walk of a large tree has no business burning cores
/// through shutdown.
pub fn cancel_walks() {
    for index in lock(INDEXES.get_or_init(Mutex::default)).iter() {
        index.cancel();
    }
}

/// Forgets the recent walk of every indexed root {path} lies under, so the
/// next reader gets a fresh one. Called from the tools that create and remove
/// paths: without it the index is a list of the tree as it was up to
/// `STALE_AFTER` ago, and a file the agent has just written is a file the
/// user is about to look for.
///
/// Nothing is walked here. The re-walk starts when something next asks for
/// the index, so a turn that writes a hundred files costs one walk and only
/// if anybody reads it.
///
/// Roots are matched against {path} by prefix, and they are canonical, so a
/// caller has to hand over a path that is too: a spelling that reaches the
/// same file through a symlink matches nothing, and one that still holds a
/// `..` matches the root it climbed out of.
pub fn invalidate_for(path: &Path) {
    // Never asked for an index, so there is nothing to forget and no reason
    // to build the registry from a write path.
    let Some(indexes) = INDEXES.get() else {
        return;
    };
    for index in lock(indexes)
        .iter()
        .filter(|index| path.starts_with(&index.shared.root))
    {
        index.invalidate();
    }
}

/// A root nobody is reading any more is dropped once the registry is over
/// `MAX_ROOTS`. Dropping one a reader still holds would leave that reader on a
/// `Shared` no lookup can find again, so the next lookup for the same root
/// would build a second one and walk the same tree twice into two lists that
/// then disagree. The entries that survive over the cap are the ones a reader
/// or a running walk is holding, and `MAX_WALKS` bounds the latter.
fn evict(roots: &mut Vec<FileIndex>) {
    while roots.len() > MAX_ROOTS {
        let Some(at) = roots
            .iter()
            .rposition(|i| Arc::strong_count(&i.shared) == 1)
        else {
            return;
        };
        roots.remove(at).cancel();
    }
}

/// The key a root is remembered under. A root that cannot be canonicalised is
/// kept as given: it is most likely a directory that does not exist, and the
/// caller still wants an index that says so rather than an error.
fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// A poisoned lock here means a walker thread panicked mid-batch. The list it
/// left behind is still a list of paths, so carry on with it instead of
/// taking the whole app down over a stale file name.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FileIndex {
    fn build(root: PathBuf, detached: bool) -> Self {
        Self {
            shared: Arc::new(Shared {
                root,
                detached,
                corpus: ArcSwap::default(),
                walk: Mutex::new(Walk::default()),
                readers: AtomicUsize::new(0),
                skipped: AtomicUsize::new(0),
                cancel: AtomicBool::new(false),
                quit: AtomicBool::new(false),
            }),
        }
    }

    fn new(root: PathBuf) -> Self {
        Self::build(root, false)
    }

    /// An index with no walk behind it, whose corpus the caller publishes by
    /// hand. A test that needs a walk stopped at an exact point drives it
    /// through this instead of racing a real one.
    pub fn detached(root: impl Into<PathBuf>) -> Self {
        Self::build(root.into(), true)
    }

    /// Ends the walk the way a walker that died ends it. Pairs with
    /// `detached`.
    pub fn crash(&self) {
        if !self.hand_published() {
            return;
        }
        abandon(&self.shared);
    }

    /// Replaces everything the index knows, the way a finished re-walk does:
    /// a new generation, so readers start over rather than trusting a cursor
    /// into the list it replaced. Pairs with `detached`.
    pub fn publish(&self, paths: Vec<String>, complete: bool) {
        if !self.hand_published() {
            return;
        }
        let mut walk = lock(&self.shared.walk);
        walk.generation += 1;
        walk.len = paths.len();
        walk.batches = vec![paths.into_iter().map(String::into_boxed_str).collect()];
        self.shared.corpus.store(Arc::new(Corpus {
            batches: walk.batches.clone(),
            len: walk.len,
            generation: walk.generation,
            complete,
            crashed: false,
            truncated: false,
        }));
    }

    /// Appends to what the index knows, the way a walk in progress does,
    /// leaving readers' cursors valid. Pairs with `detached`.
    pub fn extend(&self, paths: Vec<String>, complete: bool) {
        if !self.hand_published() {
            return;
        }
        let mut walk = lock(&self.shared.walk);
        walk.len += paths.len();
        walk.batches
            .push(paths.into_iter().map(String::into_boxed_str).collect());
        self.shared.corpus.store(Arc::new(Corpus {
            batches: walk.batches.clone(),
            len: walk.len,
            generation: walk.generation,
            complete,
            crashed: false,
            truncated: false,
        }));
    }

    /// Whether a corpus may be published by hand here. A real walk owns its
    /// generation and its batches, so publishing over one would replace the
    /// list the walker is still appending to and leave every reader's cursor
    /// naming entries that no longer exist.
    fn hand_published(&self) -> bool {
        if self.shared.detached {
            return true;
        }
        warn!(root = %self.shared.root.display(), "{NOT_DETACHED}");
        false
    }

    /// What the index knows right now. The first walk publishes as it goes, so
    /// this can be a partial list with `complete` unset.
    pub fn corpus(&self) -> Arc<Corpus> {
        self.shared.corpus.load_full()
    }

    /// Stops the walk behind this index. The walk is shared, so the last
    /// reader leaving has to say so explicitly, or a walk of a huge tree
    /// outlives every consumer of it.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// Registers interest in the walk. A reader that stops reading drops the
    /// guard, and the walk is cancelled only once the last of them has, so
    /// closing the file picker no longer throws away a walk a plugin is
    /// following.
    pub fn reader(self) -> FileReader {
        self.shared.readers.fetch_add(1, Ordering::Relaxed);
        FileReader { index: self }
    }

    /// Whether a walk still owes this index a list, starting it when none is
    /// running.
    ///
    /// An ending is what a walk leaves however it ended, and a walk is owed
    /// when there is none: a first walk has not left one, [`Self::rescan`]
    /// takes it back, and a walk the host had no slot for never got to leave
    /// one. A walk that died leaves one too, or a root nothing can walk is
    /// asked for a walker by every frame and the reader is never told the
    /// walk it is waiting for is gone.
    /// Asking is part of the answer because nothing else will ask: the reader
    /// waiting for the list is the only thing that knows it is still waiting.
    ///
    /// A marked tree is the other half: the ending of a walk the tree moved
    /// under, or of one no walker survived, is served for its own window and
    /// no longer, so the reader that asks after that gets the walk the mark
    /// asked for. That is how a file the agent writes reaches a picker that
    /// is already open, and how a root whose walker died is tried again
    /// without a walker per reader that asks in between. A tree nothing has
    /// marked is never re-walked from here, so a picker open for minutes
    /// leaves the repo alone.
    ///
    /// A hand-published index has no walker to owe it anything, and its
    /// corpus already says for itself whether more is coming.
    fn walk_owed(&self) -> bool {
        if self.shared.detached {
            return false;
        }
        // Dropped before the walk this may start takes the same lock.
        let served = {
            let walk = lock(&self.shared.walk);
            walk.ended
                .as_ref()
                .is_some_and(|ended| !walk.dirty || !ended.stale())
        };
        if served {
            return false;
        }
        self.refresh();
        true
    }

    /// Says that this root moved, so the walk that read it is served for
    /// `DEBOUNCE` rather than `STALE_AFTER`. The list stays: a reader keeps
    /// ranking the paths it has while the fresh walk runs.
    ///
    /// `DEBOUNCE` rather than nothing at all because every write the agent
    /// makes lands here, and a reader that asks between two of them would
    /// otherwise start a walk of the whole tree for each. Only the first mark
    /// since the walk ended moves the ending, so a run of writes is re-walked
    /// `DEBOUNCE` after the first of them instead of being pushed back by
    /// each one, and an ending already asking for a walk of its own keeps the
    /// window it asked for. The sooner of the two deadlines stands either
    /// way: a mark landing on an ending that is nearly stale would otherwise
    /// push the walk it asks for out past the one that was already due.
    ///
    /// A walk already running is marked as well. Its workers may have read
    /// the directory that just changed before it changed, and no walk can
    /// tell, so it ends with the short window rather than the one that would
    /// serve its list for `STALE_AFTER`.
    pub fn invalidate(&self) {
        let mut walk = lock(&self.shared.walk);
        if mem::replace(&mut walk.dirty, true) {
            return;
        }
        if let Some(ended) = &mut walk.ended {
            let debounced = Ending::now(DEBOUNCE);
            if debounced.due() < ended.due() {
                *ended = debounced;
            }
        }
    }

    /// Brings the wait a mark left forward, so the walk it asked for is due
    /// now rather than a debounce or a retry from now. A test about what
    /// happens once one of those waits is over drives it from here instead of
    /// sleeping through the wait.
    ///
    /// A tree nothing has marked is left alone, so a test that expects a mark
    /// still fails when there was none, and this can never turn a list the
    /// index is happy with into a walk of the whole repo. A walk still running
    /// has left no ending to bring forward either, so a caller waits it out
    /// first or gets nothing from here.
    ///
    /// For tests only, and hidden because of it: anything in maki calling this
    /// per frame would collapse the debounce to nothing and put the tree back
    /// to being walked back to back for as long as the agent keeps writing.
    #[doc(hidden)]
    pub fn expire(&self) {
        let mut walk = lock(&self.shared.walk);
        if !walk.dirty {
            return;
        }
        if let Some(ended) = &mut walk.ended {
            ended.serve_for = Duration::ZERO;
        }
    }

    /// Walks now, however recent the last walk was. A user opening the file
    /// picker is asking to see the tree as it is, and the last walk can be
    /// `STALE_AFTER` old.
    ///
    /// Not [`Self::invalidate`]: nothing here claims the tree has moved, so a
    /// walk already running is the walk being asked for and the list it ends
    /// with is as fresh as any. Marking that one would cost every first open
    /// of the picker a second walk of the whole repo.
    pub fn rescan(&self) {
        {
            let mut walk = lock(&self.shared.walk);
            if walk.running {
                return;
            }
            walk.ended = None;
        }
        self.refresh();
    }

    /// Whether a cancel is pending. A walk starting from here clears it, so a
    /// cancelled walk cannot poison the next one.
    pub fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    /// Starts a walk unless one is running or the last one is recent enough.
    /// [`Self::invalidate`] is how a caller that knows the tree moved, or a
    /// user who just asked to see it, gets one before then.
    ///
    /// A first walk streams its batches, because there is nothing else to
    /// show. A re-walk builds alongside the list on show and replaces it in
    /// one go when it lands, so a reader keeps a whole tree to rank against
    /// throughout and a re-walk that is stopped costs it nothing.
    pub fn refresh(&self) {
        if self.shared.detached {
            return;
        }
        let permit = {
            let mut walk = lock(&self.shared.walk);
            if walk.running {
                // A reader arrived while a previous reader's cancel was still
                // in flight. The walk that cancel was aimed at is the walk
                // this one wants, so take the cancel back - but only a cancel
                // no worker has acted on yet can be taken back, because the
                // first `WalkState::Quit` makes `ignore` quit the whole pool.
                // Either way a fresh walk is queued behind this one, and the
                // walker runs it if it ends up throwing its own list away.
                if self.shared.cancel.swap(false, Ordering::Relaxed)
                    || self.shared.quit.load(Ordering::Relaxed)
                {
                    walk.restart = true;
                }
                return;
            }
            if walk.ended.as_ref().is_some_and(|ended| !ended.stale()) {
                return;
            }
            let Some(permit) = WalkPermit::take() else {
                debug!(root = %self.shared.root.display(), max = MAX_WALKS, "{WALKS_BUSY}");
                return;
            };
            begin(&self.shared, &mut walk);
            permit
        };
        let shared = Arc::clone(&self.shared);
        if let Err(e) = thread::Builder::new()
            .name("file-index".into())
            .spawn(move || walk_tree(shared, permit))
        {
            error!(root = %self.shared.root.display(), error = %e, "{WALKER_UNSTARTABLE}");
            abandon(&self.shared);
        }
    }

    /// The best `request.limit` paths for `request.query`, in the order the
    /// file picker would list them, and whether the walk they were ranked
    /// against had finished. Returns `None` once {cancel} is set, so an
    /// overtaken keystroke stops paying for an answer it must never hand back.
    ///
    /// Only the results are built, never a copy of the corpus, so the cost is
    /// bounded by the limit and not by the size of the tree. Offsets add a
    /// second matcher pass over the paths that made the cut, and none at all
    /// when they were not asked for.
    pub fn query(&self, request: &FileQuery<'_>, cancel: &AtomicBool) -> Option<Ranked> {
        let corpus = self.corpus();
        let answer = |items: Vec<FileMatch>| Ranked {
            items,
            complete: corpus.complete,
            crashed: corpus.crashed,
            truncated: corpus.truncated,
        };
        if request.limit == 0 {
            return Some(answer(Vec::new()));
        }
        let pattern = file_pattern(request.query);
        let mut matcher = Matcher::new(FILE_MATCH_CONFIG);
        let mut buf = Vec::new();
        // nucleo scores and sorts nothing at all for an empty pattern: its
        // snapshot is the order the paths were injected in. Ranking one by
        // haystack length here would put a plugin and the picker in a
        // different order before the user has typed anything.
        let sorted = !pattern.atoms.is_empty();
        // What nucleo sorts a snapshot by: the best score first, and a tie
        // broken by the shorter haystack and then by insertion order. Short
        // queries tie constantly, so score alone is not the same ranking.
        let rank = |score: u32, units: usize| match sorted {
            true => (Reverse(score), units),
            false => (Reverse(score), 0),
        };
        let mut best: Vec<((Reverse<u32>, usize), &str)> = Vec::with_capacity(request.limit);

        for (n, path) in corpus.iter().enumerate() {
            if n % CANCEL_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let haystack = file_haystack(path, &mut buf);
            let Some(score) = pattern.score(haystack, &mut matcher) else {
                continue;
            };
            let key = rank(score, haystack.len());
            if best.len() == request.limit && key >= best[request.limit - 1].0 {
                continue;
            }
            // Past the last entry that ranks the same, since walk order is
            // insertion order and nucleo's last tie break is insertion order.
            let at = best.partition_point(|(ranked, _)| *ranked <= key);
            best.insert(at, (key, path));
            best.truncate(request.limit);
        }
        // The loop only looks every `CANCEL_EVERY` entries and a small corpus
        // never looks twice, so the promise that a superseded caller gets
        // nothing is only kept if the last word is here.
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let mut indices = Vec::new();
        let items = best
            .into_iter()
            .map(|(_, path)| FileMatch {
                highlights: match request.highlights {
                    true => {
                        indices.clear();
                        let haystack = file_haystack(path, &mut buf);
                        pattern.indices(haystack, &mut matcher, &mut indices);
                        byte_highlights(path, haystack, &mut indices)
                    }
                    false => Vec::new(),
                },
                path: path.to_owned(),
            })
            .collect();
        Some(answer(items))
    }
}

/// One reader's interest in a walk, handed out by [`FileIndex::reader`].
///
/// Reading only. The walk behind it is shared, so a reader that could reach
/// [`FileIndex::cancel`] could stop it for every other reader of the same
/// root, which is the thing the reader count exists to prevent, and one that
/// could reach [`FileIndex::publish`] could move the generation under the
/// cursors of the rest.
pub struct FileReader {
    index: FileIndex,
}

impl FileReader {
    /// What the index knows right now. The first walk publishes as it goes,
    /// so this can be a partial list with `complete` unset.
    pub fn corpus(&self) -> Arc<Corpus> {
        self.index.corpus()
    }

    /// Whether a walk still owes this reader a list, asking for it when none
    /// is running.
    ///
    /// Asking is part of it because nothing else will: the host walks a
    /// bounded number of trees at once, and a reader that arrived while that
    /// budget was spent is the only thing that knows it is still waiting. A
    /// list that has landed and has not been marked stale since answers false
    /// and starts nothing, so a reader open for minutes does not re-walk the
    /// tree every `STALE_AFTER`.
    ///
    /// Says nothing about a walk that is filling the list right now: that is
    /// [`Corpus::complete`], and a reader that cares about either has to read
    /// both.
    pub fn scanning(&self) -> bool {
        self.index.walk_owed()
    }

    /// The best `request.limit` paths for `request.query`. See
    /// [`FileIndex::query`].
    pub fn query(&self, request: &FileQuery<'_>, cancel: &AtomicBool) -> Option<Ranked> {
        self.index.query(request, cancel)
    }
}

/// The last reader leaving stops the walk. Any earlier one leaving must not:
/// the picker and a completion plugin read the same walk, and the picker
/// closing used to throw away the prefix the plugin was waiting on.
impl Drop for FileReader {
    fn drop(&mut self) {
        if self.index.shared.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.index.cancel();
        }
    }
}

/// Clears `running` however the walk it guards leaves. A panic in the
/// `ignore` visitor would otherwise leave the root marked busy for the rest
/// of the process, so no later `refresh` could start a walk and the picker
/// would scan forever.
struct WalkGuard {
    shared: Arc<Shared>,
    generation: u64,
}

impl Drop for WalkGuard {
    fn drop(&mut self) {
        let walk = lock(&self.shared.walk);
        // A walk that ended cleared `running` itself. Another walk may have
        // set it again since, and ending that one is not this guard's to do.
        if !walk.running || walk.generation != self.generation {
            return;
        }
        drop(walk);
        error!(root = %self.shared.root.display(), "{WALKER_CRASHED}");
        abandon(&self.shared);
    }
}

/// The state every walk starts from: a generation of its own, so a reader
/// holding a cursor knows the list was replaced, and neither of the cancel
/// flags whatever the walk before it left behind.
fn begin(shared: &Shared, walk: &mut Walk) {
    // Whatever moved the tree before this moment is a change this walk is
    // about to read, so the mark it left is not this walk's to carry.
    *walk = Walk {
        running: true,
        generation: walk.generation + 1,
        ..Walk::default()
    };
    shared.skipped.store(0, Ordering::Relaxed);
    shared.cancel.store(false, Ordering::Relaxed);
    shared.quit.store(false, Ordering::Relaxed);
}

/// Whether a worker should stop, latching the cancel it acts on.
///
/// The latch is the whole point: `refresh` clears `cancel` when it finds a
/// walk running, and by then `ignore` has already dominoed this worker's
/// `WalkState::Quit` through the rest of the pool. Reading the flag back at
/// the end of the walk would say "not cancelled" and publish the arbitrary
/// prefix of the tree the workers had reached as the whole tree.
fn stop_requested(shared: &Shared) -> bool {
    if !shared.cancel.load(Ordering::Relaxed) {
        return false;
    }
    shared.quit.store(true, Ordering::Relaxed);
    true
}

/// Whether the walk that just ended was stopped rather than finished.
fn was_stopped(shared: &Shared) -> bool {
    shared.quit.load(Ordering::Relaxed) || shared.cancel.load(Ordering::Relaxed)
}

/// Takes over for the reader that asked for a walk while this one was being
/// cancelled, since that reader could not start one itself while this walk
/// held the root. Answers whether there is a walk to run, having reset the
/// state it starts from.
fn take_restart(shared: &Shared) -> bool {
    let mut walk = lock(&shared.walk);
    if !mem::take(&mut walk.restart) {
        return false;
    }
    begin(shared, &mut walk);
    true
}

fn walk_tree(shared: Arc<Shared>, _permit: WalkPermit) {
    while walk_once(&shared) {}
}

/// One pass over the tree, answering whether another was asked for while this
/// one was being cancelled. The same thread runs it: it already holds the
/// walk permit, and this root was marked running for the whole of the pass.
fn walk_once(shared: &Arc<Shared>) -> bool {
    let _guard = WalkGuard {
        shared: Arc::clone(shared),
        generation: lock(&shared.walk).generation,
    };
    let root = shared.root.clone();
    let overrides = match OverrideBuilder::new(&root)
        .add(GIT_DIR_GLOB)
        .and_then(|b| b.build())
    {
        Ok(overrides) => overrides,
        // Silently walking `.git` instead would bury every object file in
        // every list maki ranks, so this ends the walk and says why.
        Err(e) => {
            error!(root = %root.display(), error = %e, "{OVERRIDE_FAILED}");
            return false;
        }
    };
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        // Depth 0 is the root, which strips to an empty name: a bare
        // separator at the top of every list, selected by default.
        .min_depth(Some(1))
        .overrides(overrides);
    builder.build_parallel().run(|| {
        let shared = Arc::clone(shared);
        let root = root.clone();
        let mut batch = Batcher::new(Arc::clone(&shared));
        Box::new(move |entry| {
            if stop_requested(&shared) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            let Some(kind) = entry.file_type() else {
                return WalkState::Continue;
            };
            if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
                return WalkState::Continue;
            }
            let path = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            // A lossy name is a name the filesystem does not have: the picker
            // would offer it, Enter would insert it and the editor would be
            // asked to open it, and a plugin would be handed it as a path it
            // can read. Counted rather than logged, so a tree full of them
            // costs one line at the end of the walk.
            let Some(name) = path.to_str() else {
                shared.skipped.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            let mut name = name.to_owned();
            if kind.is_dir() {
                name.push(MAIN_SEPARATOR);
            }
            batch.push(name.into_boxed_str())
        })
    });
    if !was_stopped(shared) {
        finish(shared);
        return false;
    }
    abort(shared);
    take_restart(shared)
}

/// One `ignore` worker's share of the walk, gathered here and handed over a
/// batch at a time.
///
/// The whole pool appends through the one walk lock that the render thread
/// takes on every frame to read the list, so a lock per path is hundreds of
/// thousands of acquisitions a second contending with the UI. A batch is
/// pushed under `readdir` and `stat` costs the worker pays anyway.
struct Batcher {
    shared: Arc<Shared>,
    pending: Vec<Box<str>>,
}

impl Batcher {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            pending: Vec::with_capacity(BATCH),
        }
    }

    fn push(&mut self, name: Box<str>) -> WalkState {
        self.pending.push(name);
        if self.pending.len() < BATCH {
            return WalkState::Continue;
        }
        self.hand_over()
    }

    /// Moves what this worker has gathered into the shared list, answering
    /// whether the walk has room for more.
    fn hand_over(&mut self) -> WalkState {
        if self.pending.is_empty() {
            return WalkState::Continue;
        }
        push(&self.shared, mem::take(&mut self.pending))
    }
}

/// A worker ends with a batch it never filled, and `ignore` drops its visitor
/// on the way out of the pool, before the walk is finished. Without this the
/// tail of every worker's share of the tree is dropped on the floor.
impl Drop for Batcher {
    fn drop(&mut self) {
        self.hand_over();
    }
}

/// Appends one worker's {batch}, answering whether the walk has room for
/// more. It stays a lock: one shared list with one generation is the point of
/// the module, where an injector per reader is a walk per reader.
fn push(shared: &Shared, mut batch: Vec<Box<str>>) -> WalkState {
    let mut walk = lock(&shared.walk);
    let room = MAX_ENTRIES.saturating_sub(walk.len);
    let full = batch.len() >= room;
    if full {
        // One line per walk: every worker in the pool reaches the full list,
        // and each of them comes back through here to flush its tail on the
        // way out of the walk.
        if !mem::replace(&mut walk.truncated, true) {
            warn!(root = %shared.root.display(), max = MAX_ENTRIES, "{CORPUS_FULL}");
        }
        batch.truncate(room);
    }
    if !batch.is_empty() {
        walk.len += batch.len();
        walk.batches.push(batch.into());
        publish_progress(shared, &walk);
    }
    match full {
        true => WalkState::Quit,
        false => WalkState::Continue,
    }
}

/// Shows what the walk has read so far, unless there is a finished list to
/// lose by doing so.
///
/// A first walk publishes every batch, because the alternative is a picker
/// staring at nothing for as long as the tree takes to read. A walk over a
/// list that is already complete publishes nothing until it ends: the list on
/// show stays whole and is replaced in one go, so the user never watches it
/// collapse to a prefix and grow back, and a walk stopped partway through
/// leaves the index holding the answer it had rather than an arbitrary piece
/// of the tree.
fn publish_progress(shared: &Shared, walk: &Walk) {
    if shared.corpus.load().complete {
        return;
    }
    shared.corpus.store(Arc::new(Corpus {
        batches: walk.batches.clone(),
        len: walk.len,
        generation: walk.generation,
        complete: false,
        crashed: false,
        truncated: false,
    }));
}

fn finish(shared: &Shared) {
    let mut walk = lock(&shared.walk);
    let corpus = Arc::new(Corpus {
        batches: mem::take(&mut walk.batches),
        len: walk.len,
        generation: walk.generation,
        complete: true,
        crashed: false,
        truncated: walk.truncated,
    });
    shared.corpus.store(Arc::clone(&corpus));
    walk.running = false;
    // A tree that moved under the walk leaves a list of neither the tree it
    // started on nor the one it ended on, so the reader after the debounce
    // gets a walk of its own instead of this list for `STALE_AFTER`.
    let serve_for = match walk.dirty {
        true => DEBOUNCE,
        false => STALE_AFTER,
    };
    walk.ended = Some(Ending::now(serve_for));
    drop(walk);
    let skipped = shared.skipped.load(Ordering::Relaxed);
    if skipped > 0 {
        warn!(root = %shared.root.display(), skipped, "{NON_UTF8_SKIPPED}");
    }
    notify(shared, &corpus);
}

/// A cancelled walk keeps nothing of its own: its list is a prefix of the
/// tree in whatever order the threads got there, and the reader that wanted
/// it is gone. What stays on show is the finished list it was building
/// alongside, because a re-walk publishes nothing until it ends. It leaves no
/// ending, so the next reader walks afresh instead of being served a twenty
/// second old list it has already asked to replace.
fn abort(shared: &Shared) {
    let mut walk = lock(&shared.walk);
    walk.running = false;
    walk.ended = None;
    walk.batches.clear();
    walk.len = 0;
}

/// Ends a walk that will never reach the end of the tree. The index keeps the
/// list and the generation a previous walk left, so a dead walker costs
/// readers neither their results nor their cursors. `crashed` is the signal
/// the picker turns into a message, and the ending is served for
/// `RETRY_AFTER`: a failure that was transient is walked again soon, and one
/// that is not costs a walker every `RETRY_AFTER` rather than one per reader
/// that asks.
///
/// Marked like a tree that moved, because that is what makes a reader ask for
/// the retry at all. Without it the ending is one a reader is served for as
/// long as the process lives, and a picker holding the only handle on this
/// root would never see the tree walked again.
fn abandon(shared: &Shared) {
    let mut walk = lock(&shared.walk);
    let previous = shared.corpus.load();
    let corpus = Arc::new(Corpus {
        batches: previous.batches.clone(),
        len: previous.len,
        generation: previous.generation,
        complete: true,
        crashed: true,
        truncated: previous.truncated,
    });
    shared.corpus.store(Arc::clone(&corpus));
    walk.running = false;
    walk.dirty = true;
    walk.ended = Some(Ending::now(RETRY_AFTER));
    walk.batches.clear();
    walk.len = 0;
    drop(walk);
    notify(shared, &corpus);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_case::test_case;

    static NEVER: AtomicBool = AtomicBool::new(false);
    /// Small enough that the corpora it is asked against never reach a second
    /// look at the cancel flag inside the ranking loop.
    const SMALL_QUERY: FileQuery<'static> = FileQuery {
        query: "a",
        limit: 5,
        highlights: false,
    };
    const WAIT_SECS: u64 = 10;
    /// Longer than any wait the module sits on, so an ending held for this
    /// long is one no test can watch go stale.
    const NEVER_DUE: Duration = Duration::from_secs(60 * 60);
    const MANY: usize = 200;
    const DETACHED_ROOT: &str = "/detached";
    const WALKER_PANIC: &str = "the walker fell over";
    const NO_PERMIT: &str = "the whole walk budget was supposed to be free";
    const NEVER_SETTLED: &str = "the walk never reached the state the test waits for";
    const MAIN_PATH: &str = "src/main.rs";
    const MAIN_QUERY: &str = "main";
    /// `e` plus a combining acute: one grapheme, two codepoints, three bytes,
    /// and the first codepoint of every cluster is ASCII, so `Utf32Str::new`
    /// byte-indexes a string that is not ASCII while the picker's
    /// `Utf32String` holds one entry per grapheme.
    const COMBINING_PATH: &str = "e\u{301}dit.rs";
    /// A base emoji plus a skin tone modifier: one grapheme, two codepoints.
    const SKIN_TONE_PATH: &str = "\u{1f9d1}\u{1f3fd}.rs";
    /// Three emoji joined by zero width joiners: one grapheme, five codepoints.
    const ZWJ_PATH: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}.rs";
    /// Two ASCII letters that appear once in each path above, in its suffix.
    const UNICODE_QUERY: &str = "rs";
    /// A query whose first character matches a grapheme of three bytes, so the
    /// range it produces has to cover the whole cluster.
    const COMBINING_QUERY: &str = "edit";
    const COMBINING_MATCHED: &str = "e\u{301}dit";
    const OFF_CHARACTER: &str = "a highlight range landed inside a character";
    const NOTHING_MATCHED: &str = "the query was supposed to match the path";
    const RESPAWNED: &str = "a dead walk was started again by a reader asking for it";
    const NO_RETRY_FLOOR: &str = "a dead walk was due to be walked again at once";
    const NO_DEBOUNCE: &str = "a marked tree was due to be walked again at once";
    const NO_STAMP: &str = "the walk ended without saying when";
    /// Enough asks to tell a walk started once from one started per ask.
    const ASKS: usize = 3;
    const NEW_FILE: &str = "new_thing.rs";
    const GONE_FILE: &str = "gone.rs";
    /// Not valid UTF-8, so it has no name the picker could draw or the editor
    /// could open.
    #[cfg(all(unix, not(target_os = "macos")))]
    const NON_UTF8_NAME: &[u8] = b"bad\xff.rs";
    const HAND_PUBLISHED: &str = "made_up.rs";

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        dir
    }

    /// The walk runs on its own thread, so every test that reads results has
    /// to wait for it before asserting.
    fn wait_for(index: &FileIndex, ready: impl Fn(&Corpus) -> bool) -> Arc<Corpus> {
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECS);
        loop {
            let corpus = index.corpus();
            if ready(&corpus) {
                return corpus;
            }
            assert!(Instant::now() < deadline, "{NEVER_SETTLED}");
            thread::yield_now();
        }
    }

    fn walked(index: &FileIndex) -> Arc<Corpus> {
        wait_for(index, |c| c.complete)
    }

    fn ask(index: &FileIndex, query: &str, limit: usize, highlights: bool) -> Ranked {
        index
            .query(
                &FileQuery {
                    query,
                    limit,
                    highlights,
                },
                &NEVER,
            )
            .unwrap()
    }

    fn paths(ranked: Ranked) -> Vec<String> {
        ranked.items.into_iter().map(|m| m.path).collect()
    }

    fn ranked(index: &FileIndex, query: &str, limit: usize) -> Vec<String> {
        walked(index);
        paths(ask(index, query, limit, false))
    }

    fn one(index: &FileIndex, path: &str, complete: bool) {
        index.publish(vec![path.to_owned()], complete);
    }

    /// How long what the last walk left is served for, as the index recorded
    /// it rather than as the clock has moved since. A remainder measured
    /// against the clock would make every assert on it an assert on the test
    /// thread not being descheduled.
    fn served_for(index: &FileIndex) -> Duration {
        lock(&index.shared.walk)
            .ended
            .as_ref()
            .expect(NO_STAMP)
            .serve_for
    }

    /// When the last walk ended, which with [`served_for`] is the whole of
    /// when the next one is due.
    fn ended_at(index: &FileIndex) -> Instant {
        lock(&index.shared.walk).ended.as_ref().expect(NO_STAMP).at
    }

    /// Serves what the last walk left for longer than any stall this process
    /// could suffer, so an assert about what a reader gets while an ending is
    /// still fresh is answered by the index rather than by the scheduler.
    fn hold(index: &FileIndex) {
        lock(&index.shared.walk)
            .ended
            .as_mut()
            .expect(NO_STAMP)
            .serve_for = NEVER_DUE;
    }

    /// Marks the root busy the way a walk in flight does, under a generation
    /// of its own.
    ///
    /// The generation is the whole point: [`WalkGuard`] ends a walk it finds
    /// still running under the generation it was built with, and the walk a
    /// test really ran drops its guard after the list it published is already
    /// visible. A faked `running` carrying that same generation is a walk that
    /// guard would abandon a second time, re-stamping the ending under the
    /// test. Every real walk gets its generation from [`begin`], so this does
    /// too.
    fn walking(index: &FileIndex) {
        let mut walk = lock(&index.shared.walk);
        walk.running = true;
        walk.generation += 1;
    }

    /// Waits out a walk a test started and never reads, so the tree it is
    /// reading is not unlinked under it by the end of the test.
    ///
    /// Only `running` is waited on, which is as much as anything outside the
    /// walker can see: the walk slot is let go a moment later, when the walker
    /// thread returns. A test that waited for a corpus has waited for this
    /// much already, since `finish` clears `running` under the lock it
    /// publishes the list with.
    fn settled(index: &FileIndex) {
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECS);
        while lock(&index.shared.walk).running {
            assert!(Instant::now() < deadline, "{NEVER_SETTLED}");
            thread::yield_now();
        }
    }

    #[test]
    fn the_index_lists_every_file_under_the_root() {
        let dir = tree(&["a.rs", "sub/b.rs"]);
        let index = file_index(dir.path());
        let corpus = walked(&index);
        let mut got: Vec<String> = corpus.iter().map(str::to_owned).collect();
        got.sort_unstable();
        let want = [
            "a.rs".to_owned(),
            format!("sub{MAIN_SEPARATOR}"),
            format!("sub{MAIN_SEPARATOR}b.rs"),
        ];
        assert_eq!(got, want, "files and directories, relative to the root");
    }

    #[test]
    fn a_query_ranks_the_closest_path_first() {
        let dir = tree(&["vendor/other.rs", "src/main.rs"]);
        let index = file_index(dir.path());
        assert_eq!(ranked(&index, "mainrs", 5).first().unwrap(), "src/main.rs");
    }

    #[test]
    fn a_query_returns_nothing_it_could_not_match() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        assert!(ranked(&index, "zzzz", 5).is_empty());
    }

    #[test]
    fn a_query_never_returns_more_than_the_limit() {
        let dir = tree(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let index = file_index(dir.path());
        assert_eq!(ranked(&index, "rs", 2).len(), 2);
    }

    /// The point of the limit is that a caller in another language never sees
    /// a list that grows with the repo.
    #[test]
    fn a_query_costs_the_limit_rather_than_the_tree() {
        let names: Vec<String> = (0..MANY).map(|i| format!("pkg/file{i}.rs")).collect();
        let dir = tree(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let index = file_index(dir.path());
        let corpus = walked(&index);
        assert!(corpus.len() >= names.len());
        assert_eq!(ranked(&index, "", 10).len(), 10);
    }

    /// A corpus smaller than `CANCEL_EVERY` never reaches a second look at the
    /// flag inside the loop, and every realistic completion popup query is
    /// against one of those. Answering it anyway would let a plugin paint the
    /// results of a keystroke the user has already moved past.
    #[test]
    fn a_cancelled_query_answers_with_nothing() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        assert!(walked(&index).len() < CANCEL_EVERY);
        let cancel = AtomicBool::new(true);
        assert!(index.query(&SMALL_QUERY, &cancel).is_none());
    }

    /// The flag can be set while the loop is already inside it, so the check
    /// that matters is the one after the last entry.
    #[test]
    fn a_query_cancelled_after_its_last_entry_still_answers_with_nothing() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        let cancel = AtomicBool::new(false);
        let got = index.query(&SMALL_QUERY, &cancel);
        assert!(got.is_some(), "an uncancelled query answers");
        cancel.store(true, Ordering::Relaxed);
        assert!(index.query(&SMALL_QUERY, &cancel).is_none());
    }

    /// The whole point of the module: asking twice must not walk twice.
    #[test]
    fn asking_again_reuses_the_walk() {
        let dir = tree(&["a.rs"]);
        let first = file_index(dir.path());
        walked(&first);
        fs::write(dir.path().join("b.rs"), "x").unwrap();
        let second = file_index(dir.path());
        let corpus = second.corpus();
        assert!(
            corpus.complete,
            "the fresh handle answered from the old walk"
        );
        assert_eq!(
            corpus.len(),
            1,
            "no second walk ran inside the stale window"
        );
    }

    /// Four spellings of one tree would otherwise be four indexes, four walks
    /// and four retained copies of the same list, which a plugin can ask for
    /// with four `maki.fs.fuzzy_files` calls.
    #[test]
    fn every_spelling_of_one_root_is_one_index() {
        let dir = tree(&["a.rs", "sub/b.rs"]);
        let root = dir.path().canonicalize().unwrap();
        let spellings = [
            root.clone(),
            root.join("."),
            PathBuf::from(format!("{}{MAIN_SEPARATOR}", root.display())),
            root.join("sub").join(".."),
        ];
        let first = file_index(&root);
        walked(&first);
        for spelling in &spellings {
            assert!(
                Arc::ptr_eq(&file_index(spelling).shared, &first.shared),
                "{} reached a second index",
                spelling.display()
            );
        }
    }

    /// A reader still holding a handle keeps its `Shared` alive whatever the
    /// registry does, so evicting its entry would leave the next lookup
    /// building a second index and walking the same tree again.
    #[test]
    fn eviction_leaves_roots_their_readers_are_still_holding() {
        let dirs: Vec<_> = (0..MAX_ROOTS + 2).map(|_| tree(&["a.rs"])).collect();
        let held: Vec<FileIndex> = dirs.iter().map(|d| file_index(d.path())).collect();
        for (dir, index) in dirs.iter().zip(&held) {
            assert!(
                Arc::ptr_eq(&file_index(dir.path()).shared, &index.shared),
                "the held root was evicted, so it is about to be walked twice"
            );
        }
    }

    /// `running` used to be cleared only by the normal return of `walk_tree`,
    /// so a panic in the visitor left the root permanently busy: no walk could
    /// restart and nothing could ever say the list was finished.
    #[test]
    fn a_panicking_walker_leaves_the_root_walkable_again() {
        let index = FileIndex::detached(DETACHED_ROOT);
        index.extend(vec!["a.rs".to_owned()], false);
        walking(&index);

        let shared = Arc::clone(&index.shared);
        let generation = lock(&index.shared.walk).generation;
        let died = thread::spawn(move || {
            let _guard = WalkGuard { shared, generation };
            panic!("{WALKER_PANIC}");
        })
        .join();

        assert!(died.is_err(), "the walker really did unwind");
        assert!(!lock(&index.shared.walk).running, "the root is free again");
        let corpus = index.corpus();
        assert!(corpus.crashed, "whoever has to say why can tell");
        assert!(corpus.complete, "nobody is left waiting for more entries");
        assert_eq!(corpus.len(), 1, "the list it had built survived the crash");
    }

    /// A crash used to be terminal for the process. It has to leave the root
    /// walkable again, on a timer rather than at once, and through the reader
    /// that is holding it: a picker asks `scanning` on every frame and never
    /// touches `refresh` itself, so a retry only `refresh` could reach is a
    /// retry that never happens while the picker is open.
    #[test]
    fn a_crashed_root_walks_again_once_the_retry_is_due() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        let reader = index.clone().reader();
        walking(&index);
        abandon(&index.shared);

        assert!(index.corpus().crashed);
        assert_eq!(served_for(&index), RETRY_AFTER, "{NO_RETRY_FLOOR}");
        hold(&index);
        assert!(!reader.scanning(), "{NO_RETRY_FLOOR}");

        index.expire();
        assert!(reader.scanning(), "and the retry due, the walk is asked");
        let corpus = wait_for(&index, |c| c.complete && !c.crashed);
        assert_eq!(corpus.len(), 1, "a fresh walk answered");
    }

    /// `abandon` left no stamp at all, and a missing stamp is how a reader
    /// asks for a walk, so a root whose walker had died spawned another one
    /// on every frame that drew the picker and the picker was never told the
    /// walk was gone. Every other test of this drives a detached index, which
    /// answers before the stamp is ever looked at, so this one walks a real
    /// tree.
    #[test]
    fn a_dead_walker_is_not_started_again_by_every_reader_that_asks() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        walking(&index);
        abandon(&index.shared);

        let reader = index.clone().reader();
        hold(&index);
        for _ in 0..ASKS {
            assert!(!reader.scanning(), "{RESPAWNED}");
            assert!(!lock(&index.shared.walk).running, "{RESPAWNED}");
        }
        assert!(
            reader.corpus().crashed,
            "so the reader gets to say the walk died"
        );
    }

    /// A walker that never started used to publish an empty corpus over a
    /// perfectly good one, so the picker announced an unreadable directory for
    /// a directory it had just listed.
    #[test]
    fn a_walk_that_could_not_start_keeps_the_list_it_had() {
        let index = FileIndex::detached(DETACHED_ROOT);
        index.publish(vec!["a.rs".to_owned(), "b.rs".to_owned()], true);
        walking(&index);
        abandon(&index.shared);
        assert_eq!(index.corpus().len(), 2);
    }

    /// `MAX_ROOTS` bounds how many trees are indexed, not how big they are.
    /// Without this a walk of a large tree pins a path list the size of the
    /// tree in a process-wide static until maki exits.
    #[test]
    fn the_corpus_stops_growing_at_its_ceiling() {
        let index = FileIndex::detached(DETACHED_ROOT);
        walking(&index);
        let batch = |at: usize| -> Vec<Box<str>> {
            (at..at + BATCH)
                .map(|i| format!("f{i}").into_boxed_str())
                .collect()
        };
        let quit_at = (0..)
            .step_by(BATCH)
            .find(|at| push(&index.shared, batch(*at)) == WalkState::Quit)
            .expect("the walk was never told to stop");
        assert!(
            quit_at < MAX_ENTRIES && quit_at + BATCH > MAX_ENTRIES,
            "the batch that overran the ceiling is the one that stopped the walk"
        );

        finish(&index.shared);
        let corpus = index.corpus();
        assert_eq!(
            corpus.len(),
            MAX_ENTRIES,
            "the batch that overran it was cut to fit rather than dropped"
        );
        // A truncated list is not a partial one. The walk behind it is over,
        // so a reader waiting for the rest of the tree would wait forever, and
        // `complete` has to say so.
        assert!(corpus.complete, "a capped walk is finished, not paused");
        assert!(!corpus.crashed, "hitting the ceiling is not a crash");
        // Which leaves `truncated` as the only way a caller can tell a short
        // list from a small repo.
        assert!(corpus.truncated, "and it says the tree outgrew the list");
        let ranked = ask(&index, "", 1, false);
        assert!(ranked.complete);
        assert!(ranked.truncated);
    }

    /// The picker owned its walker and killed it on close. Now that the walk
    /// is shared, the last reader leaving has to be able to say so, or closing
    /// the picker leaves a walk of the whole tree running behind it.
    #[test]
    fn a_cancelled_walk_frees_the_root_for_a_fresh_one() {
        let index = FileIndex::detached(DETACHED_ROOT);
        walking(&index);
        index.cancel();
        assert!(index.cancelled());
        abort(&index.shared);

        let walk = lock(&index.shared.walk);
        assert!(!walk.running);
        assert!(
            walk.ended.is_none(),
            "a prefix of the tree is not an answer to serve for the next 20 seconds"
        );
    }

    /// A cancel left over from a reader that has gone would otherwise stop the
    /// next walk on its first entry.
    #[test]
    fn a_stale_cancel_does_not_poison_the_next_walk() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        index.cancel();
        lock(&index.shared.walk).ended = None;

        index.refresh();
        assert!(!index.cancelled(), "starting a walk clears the flag");
        assert_eq!(walked(&index).len(), 1);
    }

    /// Ranking has to agree with what the picker does with the same corpus, or
    /// a plugin picker and the built-in one disagree on the same query. The
    /// order is nucleo's: score, then the shorter haystack, then walk order.
    #[test]
    fn ranking_matches_a_matcher_built_the_way_the_picker_builds_one() {
        let dir = tree(&["vendor/main/x.rs", "src/main.rs", "README.md"]);
        let index = file_index(dir.path());
        let corpus = walked(&index);

        let pattern = file_pattern(MAIN_QUERY);
        let mut matcher = Matcher::new(FILE_MATCH_CONFIG);
        let mut buf = Vec::new();
        let mut by_hand: Vec<((Reverse<u32>, usize), &str)> = corpus
            .iter()
            .filter_map(|p| {
                let haystack = file_haystack(p, &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some(((Reverse(score), haystack.len()), p))
            })
            .collect();
        by_hand.sort_by_key(|entry| entry.0);

        let want: Vec<String> = by_hand
            .iter()
            .take(3)
            .map(|(_, p)| (*p).to_owned())
            .collect();
        assert_eq!(paths(ask(&index, MAIN_QUERY, 3, false)), want);
    }

    /// The contract readers hold a cursor against: appending leaves it valid,
    /// replacing does not, and the generation is how they tell.
    #[test]
    fn a_replaced_corpus_gets_a_new_generation_and_an_extended_one_does_not() {
        let index = FileIndex::detached(DETACHED_ROOT);
        index.extend(vec!["a.rs".to_owned()], false);
        let first = index.corpus().generation();
        index.extend(vec!["b.rs".to_owned()], false);
        assert_eq!(index.corpus().generation(), first, "a walk still filling");
        assert_eq!(index.corpus().len(), 2);

        index.publish(vec!["c.rs".to_owned()], true);
        assert_ne!(
            index.corpus().generation(),
            first,
            "a walk that replaced it"
        );
        assert_eq!(index.corpus().len(), 1);
    }

    /// The picker builds its haystacks with `Utf32String` and the index builds
    /// them here, and a haystack of different units is a different score, a
    /// different place in the list and offsets that name something else.
    #[test_case(MAIN_PATH      ; "an_ascii_path")]
    #[test_case(COMBINING_PATH ; "a_combining_mark")]
    #[test_case(SKIN_TONE_PATH ; "a_skin_tone_emoji")]
    #[test_case(ZWJ_PATH       ; "a_zwj_sequence")]
    fn the_haystack_is_the_one_the_picker_injects(path: &str) {
        let mut buf = Vec::new();
        assert_eq!(
            file_haystack(path, &mut buf),
            file_haystack_owned(path).slice(..)
        );
    }

    /// A caller slices the path with the range it is handed, so a range has
    /// to cover whole characters however many bytes the matched grapheme took.
    /// Lua's `sub(from, to)` is these bytes, which is why they are 1-based and
    /// inclusive.
    #[test_case(COMBINING_PATH, COMBINING_QUERY, COMBINING_MATCHED ; "a_combining_mark")]
    #[test_case(SKIN_TONE_PATH, UNICODE_QUERY,   UNICODE_QUERY     ; "a_skin_tone_emoji")]
    #[test_case(ZWJ_PATH,       UNICODE_QUERY,   UNICODE_QUERY     ; "a_zwj_sequence")]
    fn a_highlight_range_slices_whole_characters(path: &str, query: &str, want: &str) {
        let index = FileIndex::detached(DETACHED_ROOT);
        one(&index, path, true);

        let ranked = ask(&index, query, 1, true);
        let item = ranked.items.first().expect(NOTHING_MATCHED);
        let matched: String = item
            .highlights
            .iter()
            .map(|(from, to)| {
                let (from, to) = (*from as usize - 1, *to as usize);
                assert!(item.path.is_char_boundary(from), "{OFF_CHARACTER}: {from}");
                assert!(item.path.is_char_boundary(to), "{OFF_CHARACTER}: {to}");
                &item.path[from..to]
            })
            .collect();
        assert_eq!(
            matched, want,
            "the ranges spell out exactly the text the query matched"
        );
    }

    /// Units that touch are one range, because a run of matched characters is
    /// one thing to paint and a caller has no use for the seams.
    #[test]
    fn a_run_of_matched_characters_is_one_range() {
        let index = FileIndex::detached(DETACHED_ROOT);
        one(&index, MAIN_PATH, true);

        let ranked = ask(&index, MAIN_QUERY, 1, true);
        let item = ranked.items.first().expect(NOTHING_MATCHED);
        assert_eq!(item.highlights, [(5, 8)], "`main`, 1-based and inclusive");
        assert_eq!(&item.path[4..8], MAIN_QUERY);
    }

    /// Highlights are a second matcher pass over the paths that made the cut,
    /// so a caller that only lists paths must not be paying for them.
    #[test]
    fn highlights_cost_nothing_until_they_are_asked_for() {
        let index = FileIndex::detached(DETACHED_ROOT);
        one(&index, MAIN_PATH, true);
        assert!(
            ask(&index, MAIN_QUERY, 1, false).items[0]
                .highlights
                .is_empty()
        );
        assert!(
            !ask(&index, MAIN_QUERY, 1, true).items[0]
                .highlights
                .is_empty()
        );
    }

    /// A caller polling as the user types has to tell "nothing matches" from
    /// "the walk has not reached it yet", or it stops asking on an empty
    /// answer that was only ever going to be empty for another moment.
    #[test]
    fn a_ranking_reports_whether_the_walk_behind_it_had_finished() {
        let index = FileIndex::detached(DETACHED_ROOT);
        index.extend(vec![MAIN_PATH.to_owned()], false);
        assert!(
            !ask(&index, MAIN_QUERY, 1, false).complete,
            "a first walk still filling"
        );

        one(&index, MAIN_PATH, true);
        assert!(
            ask(&index, MAIN_QUERY, 1, false).complete,
            "the walk reached the end of the tree"
        );
    }

    /// `refresh` takes a pending cancel back when it finds a walk running, on
    /// the theory that the walk it would have stopped is the walk the new
    /// reader wants. A worker that already acted on that cancel cannot be
    /// called back: `ignore` quits the whole pool behind its `Quit`. Reading
    /// the flag back at the end of the walk would then call an arbitrary
    /// prefix of the tree a finished list and serve it for `STALE_AFTER`.
    #[test]
    fn a_cancel_a_worker_acted_on_outlives_being_taken_back() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        walking(&index);

        index.cancel();
        assert!(stop_requested(&index.shared), "the worker quits");
        index.refresh();

        assert!(!index.cancelled(), "the flag itself was taken back");
        assert!(
            was_stopped(&index.shared),
            "the walk still has to throw its list away"
        );
        assert!(
            take_restart(&index.shared),
            "and hand over to the walk the reader asked for"
        );
        assert!(!index.cancelled(), "which starts uncancelled");
    }

    /// Opening the picker closes it first, so a cancel and an ask land back to
    /// back on the same root, and every plugin keystroke lands another. The
    /// only list published as complete has to be the whole tree, whichever of
    /// the two the walkers see first.
    #[test]
    fn a_cancel_and_an_ask_back_to_back_still_walk_the_whole_tree() {
        let names: Vec<String> = (0..MANY).map(|i| format!("pkg/file{i}.rs")).collect();
        let dir = tree(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let index = file_index(dir.path());

        index.cancel();
        index.refresh();

        let corpus = walked(&index);
        assert_eq!(
            corpus.len(),
            names.len() + 1,
            "every file plus the directory holding them"
        );
    }

    /// A plugin looping over the subdirectories of a repo asks for one root
    /// per iteration, and each root used to be a thread plus a pool of walker
    /// threads however many were already going.
    #[test]
    fn a_root_that_asks_while_the_walk_budget_is_spent_waits_for_a_later_ask() {
        let dir = tree(&["a.rs"]);
        let mut spent = Vec::new();
        while let Some(permit) = WalkPermit::take() {
            spent.push(permit);
        }
        assert_eq!(spent.len(), MAX_WALKS, "{NO_PERMIT}");

        let index = file_index(dir.path());
        assert!(!lock(&index.shared.walk).running, "no walker started");
        assert!(index.corpus().is_empty(), "and nothing was published");

        drop(spent);
        index.refresh();
        assert_eq!(walked(&index).len(), 1, "the next ask got the walk");
    }

    /// The tools write files the user then goes looking for. Without a word
    /// from them the index is the tree as it was up to `STALE_AFTER` ago, and
    /// it calls that list complete, so there is not even a hint that the file
    /// they just watched the agent write is missing from it.
    #[test]
    fn a_path_created_after_the_walk_turns_up_once_a_tool_says_so() {
        let dir = tree(&["a.rs"]);
        let root = dir.path().canonicalize().unwrap();
        let index = file_index(&root);
        assert_eq!(walked(&index).len(), 1);

        let made = root.join(NEW_FILE);
        fs::write(&made, "x").unwrap();
        invalidate_for(&made);
        index.expire();
        index.refresh();

        let corpus = wait_for(&index, |c| c.complete && c.len() == 2);
        assert!(
            corpus.iter().any(|p| p == NEW_FILE),
            "the new file is there"
        );
    }

    /// And the other direction, which is worse: the picker offers a path that
    /// is gone, Enter puts it in the chat input and asks the editor to open a
    /// file that is not there.
    #[test]
    fn a_path_removed_after_the_walk_stops_being_offered() {
        let dir = tree(&["a.rs", GONE_FILE]);
        let root = dir.path().canonicalize().unwrap();
        let index = file_index(&root);
        assert_eq!(walked(&index).len(), 2);

        let gone = root.join(GONE_FILE);
        fs::remove_file(&gone).unwrap();
        invalidate_for(&gone);
        index.expire();
        index.refresh();

        let corpus = wait_for(&index, |c| c.complete && c.len() == 1);
        assert!(
            !corpus.iter().any(|p| p == GONE_FILE),
            "the deleted file is gone from the list too"
        );
    }

    /// A path under no indexed root is nothing to forget, and a write is on
    /// the hot path of every turn.
    #[test]
    fn a_write_outside_every_indexed_root_leaves_the_walks_alone() {
        let dir = tree(&["a.rs"]);
        let root = dir.path().canonicalize().unwrap();
        let index = file_index(&root);
        walked(&index);

        let stamped = ended_at(&index);
        invalidate_for(Path::new(DETACHED_ROOT));
        assert_eq!(ended_at(&index), stamped, "nothing moved the ending");
        assert_eq!(
            served_for(&index),
            STALE_AFTER,
            "the walk still counts as recent"
        );
    }

    /// The floor under re-walks. A mark used to take the finish stamp away
    /// entirely, and a missing stamp is how a reader asks for a walk, so an
    /// agent writing faster than a walk of the tree completes kept one
    /// running for as long as its edits lasted. The file a write just made
    /// still has to turn up in a picker that is already open, so the floor is
    /// a debounce rather than the whole staleness window.
    #[test]
    fn a_marked_tree_is_re_walked_on_a_floor_rather_than_at_once() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);
        let reader = index.clone().reader();

        index.invalidate();
        let marked = ended_at(&index);
        assert_eq!(served_for(&index), DEBOUNCE, "{NO_DEBOUNCE}");

        index.invalidate();
        assert_eq!(
            (ended_at(&index), served_for(&index)),
            (marked, DEBOUNCE),
            "a second write does not push the re-walk further out"
        );

        hold(&index);
        assert!(!reader.scanning(), "{NO_DEBOUNCE}");
        index.expire();
        assert!(
            reader.scanning(),
            "and the debounce over, the walk is asked"
        );
        settled(&index);
    }

    /// A mark asks for a walk sooner, never later. A write landing just before
    /// the last walk went stale used to replace a wait that was nearly over
    /// with a whole fresh debounce, so saying the tree had moved put the walk
    /// that finds the new file further out than saying nothing at all.
    #[test]
    fn a_mark_never_pushes_a_re_walk_out_past_what_was_already_due() {
        let index = FileIndex::detached(DETACHED_ROOT);
        lock(&index.shared.walk).ended = Some(Ending::now(Duration::ZERO));
        let due = (ended_at(&index), served_for(&index));

        index.invalidate();

        assert_eq!(
            (ended_at(&index), served_for(&index)),
            due,
            "an ending already asking for a walk keeps the deadline it had"
        );
        assert!(lock(&index.shared.walk).dirty, "and the mark still landed");
    }

    /// A lossy name is a name nothing else in maki can use: the picker draws
    /// it, Enter inserts it, and the editor is asked to open a path the
    /// filesystem does not have.
    // APFS rejects non-UTF-8 names.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_path_that_is_not_utf8_never_reaches_the_corpus() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tree(&["a.rs"]);
        fs::write(dir.path().join(OsStr::from_bytes(NON_UTF8_NAME)), "x").unwrap();

        let index = file_index(dir.path());
        let corpus = walked(&index);
        let got: Vec<&str> = corpus.iter().collect();
        assert_eq!(got, ["a.rs"], "only the paths that have a name");
    }

    /// The picker and a completion plugin read one walk. The picker used to
    /// cancel it on its way out whoever else was still reading, and `abort`
    /// throws the streamed prefix away, so the plugin's walk restarted from
    /// nothing because the user pressed Esc.
    #[test]
    fn a_walk_outlives_one_of_its_readers_letting_go() {
        let index = FileIndex::detached(DETACHED_ROOT);
        let plugin = index.clone().reader();
        let picker = index.clone().reader();

        drop(picker);
        assert!(!index.cancelled(), "someone is still reading this walk");

        drop(plugin);
        assert!(index.cancelled(), "and the last reader stops it");
    }

    /// The list is what the index is for. A re-walk that published its prefix
    /// as it went threw the finished tree away the moment anything asked for
    /// a fresh one, so the picker closing mid-re-walk left the index holding
    /// an arbitrary piece of the tree and the next open walked it all again.
    #[test]
    fn a_re_walk_stopped_by_its_last_reader_leaves_the_finished_list_up() {
        let index = FileIndex::detached(DETACHED_ROOT);
        index.publish(vec!["a.rs".to_owned(), "b.rs".to_owned()], true);
        let reader = index.clone().reader();

        begin(&index.shared, &mut lock(&index.shared.walk));
        let batch: Vec<Box<str>> = (0..BATCH)
            .map(|i| format!("f{i}.rs").into_boxed_str())
            .collect();
        assert_eq!(push(&index.shared, batch), WalkState::Continue);
        assert!(
            index.corpus().iter().eq(["a.rs", "b.rs"]),
            "the fresh walk builds alongside the list on show"
        );

        drop(reader);
        assert!(index.cancelled(), "the last reader stopped the walk");
        abort(&index.shared);

        let corpus = index.corpus();
        assert!(corpus.complete, "and the list it had is still an answer");
        assert!(corpus.iter().eq(["a.rs", "b.rs"]));
    }

    /// A write that lands while the walk is running is a write the walkers may
    /// already have read past, and no walk can tell. The stamp was the same
    /// either way, so the index called that list fresh and served it for the
    /// whole of `STALE_AFTER` with the file the agent had just written missing
    /// from it. A write before the walk started is one the walk goes on to
    /// find, and costs it nothing.
    #[test_case(true  => DEBOUNCE    ; "a_write_during_the_walk_is_one_it_may_have_missed")]
    #[test_case(false => STALE_AFTER ; "a_write_before_it_started_is_one_it_finds")]
    fn a_finished_walk_counts_as_fresh_only_if_the_tree_held_still(during: bool) -> Duration {
        let index = FileIndex::detached(DETACHED_ROOT);
        if !during {
            index.invalidate();
        }
        begin(&index.shared, &mut lock(&index.shared.walk));
        if during {
            index.invalidate();
        }
        finish(&index.shared);

        assert!(
            !lock(&index.shared.walk).running,
            "the walk is over either way"
        );
        served_for(&index)
    }

    /// The signal a plugin waits on instead of polling. One per walk that
    /// ends, not one per batch, and it says how much of the tree it got.
    #[test]
    fn a_walk_that_lands_tells_the_host_once() {
        let dir = tree(&["a.rs", "b.rs"]);
        let root = dir.path().canonicalize().unwrap();
        let ends: Arc<Mutex<Vec<(usize, bool, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ends);
        // Every walk in the process reaches the observer, and other tests
        // walk their own trees, so only this root counts.
        on_walk_end(move |end| {
            if end.root == root {
                lock(&seen).push((end.files, end.crashed, end.truncated));
            }
        });

        let index = file_index(dir.path());
        walked(&index);
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECS);
        while lock(&ends).is_empty() {
            assert!(Instant::now() < deadline, "{NEVER_SETTLED}");
            thread::yield_now();
        }

        let ends = lock(&ends);
        assert_eq!(ends.len(), 1, "one walk, one event");
        assert_eq!(ends[0], (2, false, false), "the whole tree, intact");
    }

    /// `publish` drives tests through an exact sequence of states. Aimed at a
    /// root with a real walk behind it, it would replace the list the walker
    /// is still appending to and move the generation under every cursor into
    /// it.
    #[test]
    fn a_corpus_published_by_hand_over_a_live_index_is_refused() {
        let dir = tree(&["a.rs"]);
        let index = file_index(dir.path());
        walked(&index);

        index.publish(vec![HAND_PUBLISHED.to_owned()], true);

        let corpus = index.corpus();
        assert_eq!(corpus.len(), 1);
        assert!(corpus.iter().eq(["a.rs"]), "the walk's own list, untouched");
    }
}
