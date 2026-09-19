use std::borrow::Cow;
use std::env;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use maki_storage::id::MakiId;
use maki_storage::paths;
use maki_storage::sessions::{SessionClaim, SessionError, SessionSummary};
use maki_storage::{StateDir, StorageError, now_epoch};
use maki_ui::AppSession;

/// Mirrors `AGE_UNITS` in the `sessions` picker plugin.
const AGE_UNITS: [(u64, &str); 6] = [
    (31_536_000, "y"),
    (2_592_000, "mo"),
    (604_800, "w"),
    (86_400, "d"),
    (3_600, "h"),
    (60, "m"),
];
const JUST_NOW: &str = "just now";
const HEADERS: [&str; 4] = ["Session ID", "Title", "Project", "Updated"];
const COLUMN_GAP: &str = "  ";
const RULE: char = '─';
const ELLIPSIS: &str = "…";
const MIN_TITLE_WIDTH: usize = 20;
const MIN_PROJECT_WIDTH: usize = 12;
const PROJECT_COLUMN: usize = 2;
const NO_SESSIONS: &str = "No sessions found";
const NO_SESSIONS_HERE: &str = "No sessions found in this directory (try --global)";
const NEEDS_TTY: &str = "refusing to delete without a terminal to confirm on, pass --force";
const CANCELLED: &str = "Cancelled";

pub fn list(global: bool, storage: &StateDir) -> Result<()> {
    let summaries = if global {
        AppSession::list_all(storage)
    } else {
        let cwd = env::current_dir().context("read current directory")?;
        AppSession::list(&cwd.to_string_lossy(), storage)
    }
    .context("list sessions")?;

    if summaries.is_empty() {
        if global {
            println!("{NO_SESSIONS}");
        } else {
            println!("{NO_SESSIONS_HERE}");
        }
        return Ok(());
    }
    let table = render_table(&summaries, now_epoch(), terminal_budget());
    print!("{table}");
    Ok(())
}

pub fn delete(session_id: &str, force: bool, storage: &StateDir) -> Result<()> {
    let id: MakiId = session_id
        .parse()
        .map_err(|e| eyre!("invalid session id {session_id:?}: {e}"))?;
    if !force && !confirm(id)? {
        println!("{CANCELLED}");
        return Ok(());
    }
    // Refusing to delete a session another maki is writing beats unlinking the
    // file it keeps appending to.
    let claim = SessionClaim::acquire(id, storage).context("claim session")?;
    match AppSession::delete(&claim, storage) {
        Ok(()) => println!("Deleted session {id}"),
        Err(SessionError::Storage(StorageError::NotFound(_))) => {
            bail!("session {session_id} not found")
        }
        Err(e) => return Err(e).context("delete session"),
    }
    Ok(())
}

/// A maki that already has the session open cannot notice the delete: it holds
/// an open handle, keeps appending to the unlinked file, and loses every turn
/// after that. The `/sessions` picker asks twice for the same reason.
fn confirm(id: MakiId) -> Result<bool> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        bail!(NEEDS_TTY);
    }
    print!("Delete session {id}? [y/N] ");
    io::stdout().flush().context("prompt")?;
    let mut answer = String::new();
    stdin.read_line(&mut answer).context("read answer")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Table budget when writing to a terminal. `terminal_width` answers from the
/// controlling tty even when stdout is redirected, so piped output keeps full
/// titles.
fn terminal_budget() -> Option<usize> {
    std::io::stdout()
        .is_terminal()
        .then(maki_ui::terminal_width)
        .flatten()
        .map(usize::from)
}

fn age(updated_at: u64, now: u64) -> String {
    let secs = now.saturating_sub(updated_at);
    for (unit_secs, suffix) in AGE_UNITS {
        if secs >= unit_secs {
            return format!("{}{} ago", secs / unit_secs, suffix);
        }
    }
    JUST_NOW.to_owned()
}

fn collapse_home(path: &str) -> String {
    match paths::home() {
        Some(home) => collapse_home_with(path, &home.to_string_lossy()),
        None => path.to_owned(),
    }
}

fn collapse_home_with(path: &str, home: &str) -> String {
    path.strip_prefix(home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_owned())
}

fn truncate_end(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.width());
    let mut used = 0;
    let end = s
        .char_indices()
        .find_map(|(i, c)| {
            used += c.width().unwrap_or(0);
            (used > budget).then_some(i)
        })
        .unwrap_or(s.len());
    Cow::Owned(format!("{}{ELLIPSIS}", &s[..end]))
}

/// Keeps the tail, which is the part of a path that identifies the checkout.
fn truncate_start(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.width());
    let mut used = 0;
    let start = s
        .char_indices()
        .rev()
        .find_map(|(i, c)| {
            used += c.width().unwrap_or(0);
            (used > budget).then_some(i + c.len_utf8())
        })
        .unwrap_or(0);
    Cow::Owned(format!("{ELLIPSIS}{}", &s[start..]))
}

/// Largest width that fits `allowed` without dropping below `floor`, unless
/// the natural width is already smaller than the floor.
fn fit(natural: usize, allowed: usize, floor: usize) -> usize {
    natural.min(allowed).max(natural.min(floor))
}

fn render_table(summaries: &[SessionSummary], now: u64, budget: Option<usize>) -> String {
    let rows: Vec<[String; 4]> = summaries.iter().map(|s| row_cells(s, now)).collect();
    let mut widths = HEADERS.map(UnicodeWidthStr::width);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.width());
        }
    }
    let gaps = COLUMN_GAP.len() * (HEADERS.len() - 1);
    if let Some(budget) = budget {
        let rest = budget.saturating_sub(widths[0] + widths[3] + gaps);
        let project = rest.saturating_sub(MIN_TITLE_WIDTH);
        widths[2] = fit(widths[2], project, MIN_PROJECT_WIDTH);
        let title = rest.saturating_sub(widths[2]);
        widths[1] = fit(widths[1], title, MIN_TITLE_WIDTH);
    }

    let total: usize = widths.iter().sum::<usize>() + gaps;
    let mut out = String::new();
    let _ = writeln!(out, "{}", format_row(HEADERS, &widths));
    let _ = writeln!(out, "{}", RULE.to_string().repeat(total));
    for row in &rows {
        let cells = std::array::from_fn(|i| row[i].as_str());
        let _ = writeln!(out, "{}", format_row(cells, &widths));
    }
    out
}

fn row_cells(s: &SessionSummary, now: u64) -> [String; 4] {
    [
        s.id.to_string(),
        s.title.clone(),
        collapse_home(&s.cwd),
        age(s.updated_at, now),
    ]
}

fn format_row(cells: [&str; 4], widths: &[usize; 4]) -> String {
    let mut line = String::new();
    let last = cells.len() - 1;
    for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
        if i > 0 {
            line.push_str(COLUMN_GAP);
        }
        let cell = if i == PROJECT_COLUMN {
            truncate_start(cell, *w)
        } else {
            truncate_end(cell, *w)
        };
        line.push_str(&cell);
        if i < last {
            for _ in cell.width()..*w {
                line.push(' ');
            }
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn summary(title: &str, cwd: &str, updated_at: u64) -> SessionSummary {
        SessionSummary {
            id: MakiId::generate(),
            title: title.to_owned(),
            cwd: cwd.to_owned(),
            updated_at,
        }
    }

    #[test_case(0, JUST_NOW)]
    #[test_case(59, JUST_NOW)]
    #[test_case(60, "1m ago")]
    #[test_case(3_599, "59m ago")]
    #[test_case(3_600, "1h ago")]
    #[test_case(86_399, "23h ago")]
    #[test_case(86_400, "1d ago")]
    #[test_case(604_800, "1w ago")]
    #[test_case(2_592_000, "1mo ago")]
    #[test_case(31_536_000, "1y ago")]
    fn age_formats_elapsed(secs: u64, expected: &str) {
        assert_eq!(age(0, secs), expected);
    }

    #[test_case("/home/tony/c/maki", "/home/tony", "~/c/maki" ; "under_home")]
    #[test_case("/workspace/maki", "/home/tony", "/workspace/maki" ; "outside_home")]
    #[test_case("/home/tony", "/home/tony", "~" ; "home_itself")]
    fn collapse_home_rewrites_prefix(cwd: &str, home: &str, expected: &str) {
        assert_eq!(collapse_home_with(cwd, home), expected);
    }

    #[test_case("short", 10, "short" ; "fits_untouched")]
    #[test_case("exact", 5, "exact" ; "exact_fit")]
    #[test_case("longer", 5, "long…" ; "ascii")]
    #[test_case("文档文档", 5, "文档…" ; "cjk_stops_before_overflow")]
    #[test_case("🚀🚀🚀", 4, "🚀…" ; "emoji")]
    fn truncate_end_caps_at_width(input: &str, max: usize, expected: &str) {
        assert_eq!(truncate_end(input, max), expected);
        assert!(truncate_end(input, max).width() <= max);
    }

    #[test_case("~/c/maki", 10, "~/c/maki" ; "fits_untouched")]
    #[test_case("~/c/maki", 5, "…maki" ; "ascii_tail")]
    #[test_case("~/文档/proj", 6, "…/proj" ; "cjk_path")]
    fn truncate_start_keeps_tail(input: &str, max: usize, expected: &str) {
        assert_eq!(truncate_start(input, max), expected);
        assert!(truncate_start(input, max).width() <= max);
    }

    #[test]
    fn render_table_without_budget_keeps_full_title() {
        let long_title = "t".repeat(60);
        let table = render_table(&[summary(&long_title, "/workspace/maki", 0)], 100, None);
        assert!(table.contains(&long_title));
        assert!(table.contains("/workspace/maki"));
        assert!(table.contains("1m ago"));
    }

    #[test]
    fn render_table_fits_budget_and_ellipsizes() {
        let long_title = "t".repeat(60);
        let table = render_table(&[summary(&long_title, "/workspace/maki", 0)], 100, Some(80));
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Session ID"));
        assert_eq!(lines[0].width(), 80);
        assert_eq!(lines[1], RULE.to_string().repeat(80));
        assert!(lines[2].contains(ELLIPSIS));
        assert!(lines[2].ends_with("1m ago"));
        assert!(lines[2].width() <= 80);
    }

    #[test]
    fn render_table_fits_budget_with_wide_chars() {
        let table = render_table(
            &[summary(&"文".repeat(40), "/workspace/文档", 0)],
            100,
            Some(80),
        );
        for line in table.lines() {
            assert!(line.width() <= 80);
        }
    }
}
