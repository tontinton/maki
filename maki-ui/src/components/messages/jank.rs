//! Frame-replay harness for measuring how smoothly the transcript grows.
//!
//! A fixed script of deltas and tool events drives the panel one frame at a
//! time. Each frame records the drawn document height, so a run yields the
//! number a "smooth" claim needs: the most rows any one frame adds. Counting
//! drawn rows rather than pixels is deliberate: a bottom-pinned transcript
//! shifts every row up when one is added, so a pixel diff would report the
//! whole screen as new every frame.

use super::*;
use maki_agent::tools::BASH_TOOL_NAME;
use std::time::Duration;

const VIEW_WIDTH: u16 = 80;
const VIEW_HEIGHT: u16 = 24;

/// One frame at 60 Hz, the elapsed time the reveal clock is credited.
const FRAME: Duration = Duration::from_millis(16);

/// Rows any one frame may add. Two allows a row to cross while the next is
/// partially typed without demanding a sub-row rate.
const MAX_ROWS_PER_FRAME: usize = 2;

/// Rows one frame may add while the typewriter drains a large backlog. A burst
/// larger than the base rate can carry is spread over `BACKLOG_WINDOW_MS`, so a
/// frame reveals `backlog / window` chars: a few rows here rather than the whole
/// burst, which is tens of rows. The bound allows that catch-up frame.
const MAX_BURST_ROWS_PER_FRAME: usize = 8;

fn render(panel: &mut MessagesPanel, width: u16, height: u16) {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            panel.view(f, f.area(), false, true);
        })
        .unwrap();
}

/// Rows the document currently draws: the cursored heights, not the full
/// document, because the cursor is what paces what appears on screen.
fn drawn_rows(panel: &mut MessagesPanel) -> u32 {
    render(panel, VIEW_WIDTH, VIEW_HEIGHT);
    panel.layout().drawn_total()
}

/// A step in a replay script: append text, land a tool block, or seal the
/// stream. Each step is one frame.
enum Step<'a> {
    Text(&'a str),
    /// A tool call whose output is `lines` lines of text.
    Tool {
        id: &'a str,
        lines: usize,
    },
    Seal,
}

fn apply(panel: &mut MessagesPanel, step: &Step<'_>) {
    match step {
        Step::Text(text) => panel.text_delta(text),
        Step::Tool { id, lines } => {
            panel.tool_pending((*id).into(), BASH_TOOL_NAME);
            let output = "line of output\n".repeat(*lines);
            panel.tool_done(ToolDoneEvent {
                call: None,
                id: (*id).into(),
                tool: BASH_TOOL_NAME.into(),
                output: Arc::new(ToolOutput::Plain(output.into())),
                is_error: false,
                annotation: None,
                written_path: None,
            });
        }
        Step::Seal => panel.flush(),
    }
}

/// Drive each step, then tick and render one frame — the order the app loop
/// uses, so the reveal clock and the draw see the same state a user would.
/// Returns the most rows any one frame added, the jump a smooth reveal avoids.
fn replay(panel: &mut MessagesPanel, script: &[Step<'_>]) -> usize {
    let mut max_appeared = 0;
    let mut prev = drawn_rows(panel);
    for step in script {
        apply(panel, step);
        let _ = panel.tick_for_test(FRAME);
        let now = drawn_rows(panel);
        max_appeared = max_appeared.max(now.saturating_sub(prev) as usize);
        prev = now;
    }
    max_appeared
}

#[test]
fn text_grows_without_a_frame_dumping_many_rows() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let script: Vec<Step> = (0..20).map(|_| Step::Text("line of text\n")).collect();
    let appeared = replay(&mut panel, &script);
    assert!(
        appeared <= MAX_ROWS_PER_FRAME,
        "a frame must not dump text: {appeared} rows"
    );
}

#[test]
fn a_tool_block_reveals_a_row_at_a_time() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let script = vec![
        Step::Text("thinking about it\n"),
        Step::Tool {
            id: "t1",
            lines: 12,
        },
    ];
    let appeared = replay(&mut panel, &script);
    assert!(
        appeared <= MAX_ROWS_PER_FRAME,
        "a tool block must not land whole: {appeared} rows"
    );
}

/// A burst larger than the base rate can carry must not land whole. The
/// typewriter drains it over `BACKLOG_WINDOW_MS`, so its worst frame adds the
/// catch-up rate's rows, not the whole burst, which is tens of rows. Sealing in
/// the same frame adds none.
#[test]
fn a_burst_does_not_land_whole() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let burst = "line of text\n".repeat(60);
    let script = vec![Step::Text(&burst), Step::Seal];
    let appeared = replay(&mut panel, &script);
    assert!(
        appeared <= MAX_BURST_ROWS_PER_FRAME,
        "the burst landed whole: {appeared} rows"
    );
}

/// A tool starting flushes the streaming text into a cached segment, which the
/// cursor paces. Committed text was already on screen, so the flush must not
/// hide it and reveal it again: the frame that commits it adds no rows.
#[test]
fn a_flush_does_not_reveal_committed_text_again() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut script: Vec<Step> = (0..15).map(|_| Step::Text("line of text\n")).collect();
    script.push(Step::Tool { id: "t1", lines: 2 });
    let appeared = replay(&mut panel, &script);
    assert!(
        appeared <= MAX_ROWS_PER_FRAME,
        "a flush must not re-reveal committed text: {appeared} rows"
    );
}

/// A re-wrap lays the same text out taller or shorter, and those rows were
/// already read. Only growth is paced, so a resize must not reveal the
/// re-wrapped document again a row at a time.
#[test]
fn a_resize_does_not_reveal_the_transcript_again() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for _ in 0..40 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            "a fairly long line of text that wraps around".into(),
        ));
    }
    render(&mut panel, 120, 20);
    let wide = drawn_rows(&mut panel);

    // Shrink until the re-wrap is taller, then one tick must land the whole
    // re-wrapped document rather than pacing it back in.
    render(&mut panel, 40, 20);
    let _ = panel.tick_for_test(FRAME);
    let narrow = drawn_rows(&mut panel);

    assert!(
        narrow >= wide,
        "the narrower width must wrap taller: {wide} -> {narrow}"
    );
    assert_eq!(
        narrow,
        panel.layout().total_rows(),
        "the resize must not leave rows hidden behind the cursor"
    );
}
