use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use memchr::memmem;
use tree_sitter::{
    Node, ParseOptions, Parser, QueryCursor, QueryCursorOptions, StreamingIterator, Tree,
};

use super::language::{RefLanguage, Reference, ReferenceKind, SearchScope};
use crate::helpers::{context_line, is_word_byte, node_text};

const DEFAULT_MAX_RESULTS: usize = 500;
const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;
const GREP_HIT_MULTIPLIER: usize = 4;

#[derive(Debug)]
pub struct SearchStats {
    pub files_grepped: usize,
    pub files_parsed: usize,
}

pub fn find_symbol_on_line(
    tree: &Tree,
    line: usize,
    symbol: &str,
    occurrence: usize,
    source: &[u8],
    identifier_kinds: &[&str],
) -> Option<usize> {
    let row = line - 1;
    let mut matches = Vec::new();
    collect_identifiers_on_line(
        tree.root_node(),
        row,
        symbol,
        source,
        identifier_kinds,
        &mut matches,
    );
    matches
        .into_iter()
        .nth(occurrence - 1)
        .map(|n| n.start_byte())
}

fn collect_identifiers_on_line<'tree>(
    node: Node<'tree>,
    row: usize,
    symbol: &str,
    source: &[u8],
    identifier_kinds: &[&str],
    out: &mut Vec<Node<'tree>>,
) {
    if row < node.start_position().row || row > node.end_position().row {
        return;
    }

    if identifier_kinds.contains(&node.kind())
        && node.start_position().row == row
        && node_text(node, source) == symbol
    {
        out.push(node);
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers_on_line(child, row, symbol, source, identifier_kinds, out);
    }
}

pub struct SearchParams<'a> {
    pub lang: &'a dyn RefLanguage,
    pub ts_lang: &'a tree_sitter::Language,
    pub scope: &'a SearchScope,
    pub symbol: &'a str,
    pub source: Option<&'a [u8]>,
    pub existing_tree: Option<&'a Tree>,
    pub project_root: &'a Path,
    pub max_results: Option<usize>,
}

pub fn search_scope(params: &SearchParams) -> (Vec<Reference>, SearchStats) {
    match params.scope {
        SearchScope::Local { file, range } => {
            let owned;
            let bytes = match params.source {
                Some(s) => s,
                None => {
                    owned = std::fs::read(file).unwrap_or_default();
                    &owned
                }
            };
            let refs = search_local(
                params.lang,
                params.ts_lang,
                bytes,
                file,
                params.symbol,
                range.as_ref(),
                params.existing_tree,
            );
            (
                refs,
                SearchStats {
                    files_grepped: 1,
                    files_parsed: if params.existing_tree.is_some() { 0 } else { 1 },
                },
            )
        }
        SearchScope::Project { subtree } => {
            let dir = subtree.as_deref().unwrap_or(params.project_root);
            search_project(
                params.lang,
                params.ts_lang,
                dir,
                params.symbol,
                params.max_results.unwrap_or(DEFAULT_MAX_RESULTS),
            )
        }
    }
}

fn search_local(
    lang: &dyn RefLanguage,
    ts_lang: &tree_sitter::Language,
    bytes: &[u8],
    path: &Path,
    symbol: &str,
    byte_range: Option<&std::ops::Range<usize>>,
    existing_tree: Option<&Tree>,
) -> Vec<Reference> {
    let owned_tree;
    let tree = match existing_tree {
        Some(t) => t,
        None => {
            let mut parser = Parser::new();
            if parser.set_language(ts_lang).is_err() {
                return Vec::new();
            }
            match parser.parse(bytes, None) {
                Some(t) => {
                    owned_tree = t;
                    &owned_tree
                }
                None => return Vec::new(),
            }
        }
    };

    let search_node = byte_range
        .and_then(|r| find_node_covering(tree, r))
        .unwrap_or_else(|| tree.root_node());

    collect_refs(lang, &search_node, bytes, path, symbol, byte_range)
}

fn find_node_covering<'a>(tree: &'a Tree, range: &std::ops::Range<usize>) -> Option<Node<'a>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    root.children(&mut cursor)
        .find(|child| child.start_byte() <= range.start && child.end_byte() >= range.end)
}

fn collect_refs(
    lang: &dyn RefLanguage,
    search_node: &Node,
    bytes: &[u8],
    path: &Path,
    symbol: &str,
    byte_range: Option<&std::ops::Range<usize>>,
) -> Vec<Reference> {
    let query = lang.reference_query();
    let mut cursor = QueryCursor::new();
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let mut matches = cursor.matches(query, *search_node, bytes);

    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = query.capture_names()[capture.index as usize];

            if let Some(range) = byte_range
                && (node.start_byte() < range.start || node.end_byte() > range.end)
            {
                continue;
            }

            if let Some(r) = classify_node(lang, capture_name, node, bytes, path, symbol, &mut seen)
            {
                refs.push(r);
            }
        }
    }

    refs
}

fn classify_node(
    lang: &dyn RefLanguage,
    capture_name: &str,
    node: Node,
    bytes: &[u8],
    path: &Path,
    symbol: &str,
    seen: &mut HashSet<usize>,
) -> Option<Reference> {
    let result = lang.classify_capture(capture_name, node, bytes, symbol)?;

    if result.text_search {
        let text = node_text(node, bytes);
        if !text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|word| word == symbol)
        {
            return None;
        }
    } else if node_text(node, bytes) != symbol {
        return None;
    }

    if !seen.insert(node.start_byte()) {
        return None;
    }
    Some(make_ref(path, node, result.kind, bytes))
}

fn make_ref(path: &Path, node: Node, kind: ReferenceKind, bytes: &[u8]) -> Reference {
    Reference {
        path: path.to_path_buf(),
        line: node.start_position().row + 1,
        col: node.start_position().column + 1,
        kind,
        context: context_line(bytes, node.start_byte()),
    }
}

fn search_project(
    lang: &dyn RefLanguage,
    ts_lang: &tree_sitter::Language,
    dir: &Path,
    symbol: &str,
    max_results: usize,
) -> (Vec<Reference>, SearchStats) {
    struct Shared {
        finder: memmem::Finder<'static>,
        result_count: AtomicUsize,
        results: Mutex<Vec<Reference>>,
        files_parsed: AtomicUsize,
        files_grepped: AtomicUsize,
    }

    let shared = Arc::new(Shared {
        finder: memmem::Finder::new(symbol.as_bytes()).into_owned(),
        result_count: AtomicUsize::new(0),
        results: Mutex::new(Vec::new()),
        files_parsed: AtomicUsize::new(0),
        files_grepped: AtomicUsize::new(0),
    });

    let mut walker = WalkBuilder::new(dir);
    walker.hidden(false);
    walker.max_filesize(Some(MAX_FILE_SIZE));

    let mut overrides = OverrideBuilder::new(dir);
    let _ = overrides.add("!.git");
    for ext in lang.extensions() {
        let _ = overrides.add(&format!("*.{ext}"));
    }
    let Ok(built) = overrides.build() else {
        return (
            Vec::new(),
            SearchStats {
                files_grepped: 0,
                files_parsed: 0,
            },
        );
    };
    walker.overrides(built);

    let ts_lang = ts_lang.clone();
    let query = lang.reference_query();

    walker.build_parallel().run(|| {
        let shared = Arc::clone(&shared);
        let symbol = symbol.to_string();
        let ts_lang = ts_lang.clone();

        let mut parser = Parser::new();
        if parser.set_language(&ts_lang).is_err() {
            return Box::new(move |_| ignore::WalkState::Continue);
        }
        let mut cursor = QueryCursor::new();

        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            if shared.result_count.load(Ordering::Relaxed) >= max_results {
                return ignore::WalkState::Quit;
            }

            let path = entry.into_path();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return ignore::WalkState::Continue,
            };

            if !contains_word(&shared.finder, &bytes) {
                return ignore::WalkState::Continue;
            }

            let grepped = shared.files_grepped.fetch_add(1, Ordering::Relaxed) + 1;
            if grepped > max_results * GREP_HIT_MULTIPLIER {
                return ignore::WalkState::Quit;
            }

            let should_cancel = || shared.result_count.load(Ordering::Relaxed) >= max_results;

            let mut cancel = |_: &tree_sitter::ParseState| {
                if should_cancel() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let opts = ParseOptions::new().progress_callback(&mut cancel);

            let tree = match parser.parse_with_options(
                &mut |i, _| if i < bytes.len() { &bytes[i..] } else { &[] },
                None,
                Some(opts),
            ) {
                Some(t) => t,
                None => return ignore::WalkState::Continue,
            };

            shared.files_parsed.fetch_add(1, Ordering::Relaxed);

            let mut cancel_q = |_: &tree_sitter::QueryCursorState| {
                if should_cancel() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let q_opts = QueryCursorOptions::new().progress_callback(&mut cancel_q);

            let mut local_refs = Vec::new();
            let mut seen = HashSet::new();
            let mut matches =
                cursor.matches_with_options(query, tree.root_node(), bytes.as_slice(), q_opts);

            while let Some(m) = matches.next() {
                if should_cancel() {
                    break;
                }

                for capture in m.captures {
                    let node = capture.node;
                    let capture_name = query.capture_names()[capture.index as usize];

                    if let Some(r) =
                        classify_node(lang, capture_name, node, &bytes, &path, &symbol, &mut seen)
                    {
                        local_refs.push(r);
                    }
                }
            }

            if !local_refs.is_empty() {
                let count = local_refs.len();
                if let Ok(mut locked) = shared.results.lock() {
                    locked.extend(local_refs);
                }
                if shared.result_count.fetch_add(count, Ordering::Relaxed) + count >= max_results {
                    return ignore::WalkState::Quit;
                }
            }

            ignore::WalkState::Continue
        })
    });

    let (results, grepped, parsed) = match Arc::try_unwrap(shared) {
        Ok(s) => (
            s.results.into_inner().unwrap_or_default(),
            s.files_grepped.into_inner(),
            s.files_parsed.into_inner(),
        ),
        Err(arc) => (
            arc.results
                .lock()
                .ok()
                .map(|mut g| std::mem::take(&mut *g))
                .unwrap_or_default(),
            arc.files_grepped.load(Ordering::Relaxed),
            arc.files_parsed.load(Ordering::Relaxed),
        ),
    };
    (
        results,
        SearchStats {
            files_grepped: grepped,
            files_parsed: parsed,
        },
    )
}

fn contains_word(finder: &memmem::Finder<'_>, haystack: &[u8]) -> bool {
    let needle_len = finder.needle().len();
    let mut start = 0;
    while let Some(pos) = finder.find(&haystack[start..]) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(haystack[abs - 1]);
        let after_ok =
            abs + needle_len >= haystack.len() || !is_word_byte(haystack[abs + needle_len]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}
