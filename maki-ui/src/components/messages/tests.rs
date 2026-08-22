use super::segment;
use super::*;
use crate::chat::{DONE_TEXT, ERROR_TEXT};
use crate::components::scrollbar::SCROLLBAR_THUMB;
use crate::repaint::expect::{OWED, QUIET};
use crate::selection::{Selection, SelectionZone};
use maki_agent::tools::{BASH_TOOL_NAME, GREP_TOOL_NAME, WRITE_TOOL_NAME};
use maki_agent::{
    GrepFileEntry, GrepMatchGroup, SnapshotLine, SnapshotSpan, SpanStyle, ToolInput, ToolOutput,
};
use ratatui::backend::TestBackend;
use std::collections::HashSet;
use std::time::Duration;
use test_case::test_case;

fn snap_line(text: &str) -> SnapshotLine {
    SnapshotLine {
        spans: vec![SnapshotSpan {
            text: text.into(),
            style: SpanStyle::Default,
        }],
    }
}

fn start(id: &str, tool: &str) -> ToolStartEvent {
    ToolStartEvent {
        id: id.into(),
        tool: tool.into(),
        summary: id.into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }
}

fn panel_with_tools(ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &(id, tool) in ids {
        panel.tool_start(start(id, tool));
    }
    panel
}

fn done(id: &str) -> ToolDoneEvent {
    ToolDoneEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    }
}

fn finish_with_live_buf(
    panel: &mut MessagesPanel,
    id: &str,
    text: &str,
    is_error: bool,
) -> Arc<maki_agent::SharedBuf> {
    let buf = Arc::new(maki_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line(text)]);
    panel.register_live_buf(id.into(), Arc::clone(&buf));
    let mut ev = start(id, BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_done(ToolDoneEvent {
        is_error,
        ..done(id)
    });
    buf
}

#[test_case(false, ToolStatus::Success ; "success_updates_start_to_success")]
#[test_case(true,  ToolStatus::Error   ; "error_updates_start_to_error")]
fn tool_done_updates_start_status(is_error: bool, expected: ToolStatus) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "bash"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error,
        annotation: None,
        written_path: None,
    });

    assert_eq!(panel.messages.len(), 1);
    assert!(matches!(&panel.messages[0].role, DisplayRole::Tool(t) if t.status == expected));
    assert!(panel.messages[0].text.contains("output"));
}

#[test_case(
    WRITE_TOOL_NAME,
    ToolOutput::WriteCode { path: "src/main.rs".into(), byte_count: 42, lines: vec!["fn main() {}".into()] },
    Some("42 bytes")
    ; "write_bytes"
)]
#[test_case(
    "grep",
    grep_output(2),
    Some("2 matches in 2 files")
    ; "grep_files"
)]
fn tool_done_sets_annotation(tool: &'static str, output: ToolOutput, expected: Option<&str>) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", tool));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: tool.into(),
        output,
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

#[test_case("line\n".repeat(200).as_str(), Some("2m timeout · 200 lines") ; "merges_start_and_output_annotations")]
#[test_case("ok",                           Some("2m timeout · 1 lines") ; "merges_start_and_short_output")]
fn tool_done_annotation_merge(output: &str, expected: Option<&str>) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut event = start("t1", BASH_TOOL_NAME);
    event.annotation = Some("2m timeout".into());
    panel.tool_start(event);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(output.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

fn grep_output(n_files: usize) -> ToolOutput {
    ToolOutput::GrepResult {
        entries: (0..n_files)
            .map(|i| GrepFileEntry {
                path: format!("{i}.rs"),
                groups: vec![GrepMatchGroup::single(1, "")],
            })
            .collect(),
    }
}

#[test]
fn tool_done_grep_shows_matches() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", GREP_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    let text = &panel.messages[0].text;
    assert!(!text.contains('\n'), "grep body should not be in msg.text");
    assert!(panel.messages[0].tool_output.is_some());
}

#[test]
fn tool_start_flushes_streaming_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer("partial response");

    panel.tool_start(start("t1", "read"));

    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.messages[0].role, DisplayRole::Assistant);
    assert!(matches!(panel.messages[1].role, DisplayRole::Tool(_)));
}

#[test]
fn thinking_delta_separate_from_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("reasoning");
    assert_eq!(panel.streaming_thinking, "reasoning");
    assert!(panel.streaming_text.is_empty());

    panel.text_delta("output");
    assert!(panel.streaming_thinking.is_empty());
    assert_eq!(panel.streaming_text, "output");
    assert_eq!(panel.messages[0].role, DisplayRole::Thinking);
    assert_eq!(panel.messages[0].text, "reasoning");
}

#[test]
fn scroll_up_pins_viewport_during_streaming() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);

    panel.scroll(1);
    panel.scroll(1);
    render(&mut panel, 80, 10);
    let pinned = panel.scroll_top;

    panel.text_delta("b\nb\nb\n");
    render(&mut panel, 80, 10);

    assert!(!panel.auto_scroll);
    assert_eq!(panel.scroll_top, pinned);
}

fn render_sel(
    panel: &mut MessagesPanel,
    width: u16,
    height: u16,
    has_selection: bool,
) -> ratatui::Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            panel.view(f, f.area(), has_selection);
        })
        .unwrap();
    terminal
}

fn render(panel: &mut MessagesPanel, width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
    render_sel(panel, width, height, false)
}

fn rebuild(panel: &mut MessagesPanel) {
    render(panel, 80, 24);
}

#[test]
fn ctrl_d_to_bottom_re_enables_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let half = panel.half_page();
    panel.scroll(half);
    render(&mut panel, 80, 10);
    assert!(!panel.auto_scroll);

    panel.scroll(-half);
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
}

#[test]
fn unknown_tool_id_is_noop() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_output("ghost", "data");
    panel.tool_done(ToolDoneEvent {
        id: "orphan".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert!(panel.messages.is_empty());
}

#[test]
fn fail_in_progress_except_preserves_excluded_tool() {
    let mut panel = panel_with_tools(&[("agent", "task"), ("shell", "bash")]);
    let excluded = HashSet::from(["shell".to_string()]);

    panel.fail_in_progress_except("missing completion".into(), &excluded);

    assert_eq!(panel.in_progress_count(), 1);
    assert_eq!(msg_status(&panel, "agent"), ToolStatus::Error);
    assert_eq!(msg_status(&panel, "shell"), ToolStatus::InProgress);
    assert!(panel.messages[0].text.contains("missing completion"));
}

#[test]
fn in_progress_tracking() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    assert_eq!(panel.in_progress_count(), 2);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 1);

    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "read".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 0);
}

fn has_scrollbar_thumb(terminal: &ratatui::Terminal<TestBackend>) -> bool {
    let buf = terminal.backend().buffer();
    (0..buf.area.height).any(|y| {
        buf.cell((buf.area.width - 1, y))
            .is_some_and(|c: &ratatui::buffer::Cell| c.symbol() == SCROLLBAR_THUMB)
    })
}

#[test_case(40, true  ; "rendered_when_content_overflows")]
#[test_case(1,  false ; "hidden_when_content_fits")]
fn scrollbar_visibility(line_count: usize, expected: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel
        .streaming_text
        .set_buffer(&"line\n".repeat(line_count));
    let terminal = render(&mut panel, 80, 10);
    assert_eq!(has_scrollbar_thumb(&terminal), expected);
}

fn seg_text(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect()
}

fn msg_status(panel: &MessagesPanel, tool_id: &str) -> ToolStatus {
    panel
        .messages
        .iter()
        .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        .map(|m| match &m.role {
            DisplayRole::Tool(t) => t.status,
            _ => unreachable!(),
        })
        .unwrap()
}

fn has_seg(panel: &MessagesPanel, tool_id: &str) -> bool {
    panel
        .cache
        .segments()
        .iter()
        .any(|s| s.tool_id.as_deref() == Some(tool_id))
}

#[test]
fn events_before_cache_built_render_correctly() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "bash")]);
    panel.tool_output("t1", "early output");
    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("result".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    assert!(seg_text(&panel, "t1").contains("early output"));
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Success);
    assert!(seg_text(&panel, "t2").contains("result"));
}

fn bash_code_start(panel: &mut MessagesPanel, id: &str, code: &str) {
    panel.tool_start(ToolStartEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        summary: code.into(),
        annotation: None,
        input: Some(ToolInput::Code {
            language: "bash".into(),
            code: code.into(),
        }),
        raw_input: None,
        output: None,
        render_header: None,
    });
}

#[test]
fn bash_live_output_with_code_input() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo hello");
    rebuild(&mut panel);

    panel.tool_output("t1", "streaming");
    assert!(seg_text(&panel, "t1").contains("streaming"));

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("done".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    let text = seg_text(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("done"));
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test_case(true  ; "after_cache_built")]
#[test_case(false ; "before_cache_built")]
fn cancel_in_progress_marks_pending_as_error(cache_built: bool) {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    if cache_built {
        rebuild(&mut panel);
    }

    panel.cancel_in_progress();

    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(panel.cadence(), Cadence::IDLE);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Error);
}

const THINKING_TEXT: &str = "a long chain of reasoning";
const HIGHLIGHTED_CODE: &str = "fn main() {}";
const HIGHLIGHT_DEADLINE: Duration = Duration::from_secs(10);

/// Only `view` advances a typewriter, and collapsed thinking is never drawn,
/// so its reveal can never finish. Believing it would hold the loop at full
/// frame rate for as long as the model reasons.
#[test_case(true  => Cadence::SMOOTH ; "expanded_thinking_reveals")]
#[test_case(false => Cadence::IDLE   ; "collapsed_thinking_reveals_nothing")]
fn thinking_animates_only_while_it_is_on_screen(show_thinking: bool) -> Cadence {
    let config = UiConfig {
        show_thinking,
        ..UiConfig::default()
    };
    let mut panel = MessagesPanel::new(config, EventHandle::disconnected_for_test());

    panel.thinking_delta(THINKING_TEXT);

    assert!(
        panel.streaming_thinking.is_animating(),
        "the typewriter is mid-reveal, it just has nowhere to draw"
    );
    panel.cadence()
}

/// A waiting tool used to claim the whole screen was animating, which is what
/// pinned the loop at full frame rate. It draws one spinner glyph, so the
/// glyph rate is all it may ask for. Text arriving beside it earns the smooth
/// budget.
#[test_case(false => Cadence::SPINNER ; "waiting_tool_only_spins")]
#[test_case(true  => Cadence::SMOOTH  ; "streaming_text_beside_it_wins")]
fn cadence_while_a_tool_is_in_progress(text_streaming: bool) -> Cadence {
    let mut panel = panel_with_tools(&[("t1", BASH_TOOL_NAME)]);
    if text_streaming {
        panel.text_delta("an answer arriving while the tool still runs");
    }
    assert_eq!(
        panel.in_progress_count(),
        1,
        "the spinner source has to be live or this proves nothing"
    );
    panel.cadence()
}

/// Without the `show_idle_splash` gate the splash keeps asking for smooth
/// frames for the rest of the session, long after the first message pushed it
/// off screen.
#[test]
fn splash_stops_driving_cadence_once_a_message_exists() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    assert_eq!(
        panel.cadence(),
        Cadence::SMOOTH,
        "the starfield drifts while the splash is the only thing drawn"
    );

    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(done("t1"));

    assert_eq!(panel.cadence(), Cadence::IDLE, "the splash is gone");
}

/// `drain_highlights` moved out of `view`, so `tick` is the only thing feeding
/// the worker now. The wait is the worker's own round trip, not a sleep: the
/// loop ends the moment the result lands, and the deadline only turns a broken
/// drain into a failure instead of a hang.
#[test]
fn tick_drains_the_highlight_worker() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: ToolOutput::ReadCode {
            path: "file.rs".into(),
            start_line: 1,
            lines: vec![HIGHLIGHTED_CODE.into()],
            total_lines: 1,
            instructions: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let deadline = Instant::now() + HIGHLIGHT_DEADLINE;
    while panel.tick() == Dirty::NO {
        assert!(
            Instant::now() < deadline,
            "a highlighted tool stays unstyled until some unrelated repaint"
        );
        std::thread::yield_now();
    }

    assert!(
        seg_text(&panel, "t1").contains(HIGHLIGHTED_CODE),
        "the applied result replaces the highlight range in place"
    );
}

#[test]
fn new_tool_after_in_place_update() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    rebuild(&mut panel);
    panel.tool_output("t1", "streaming data");

    panel.tool_start(start("t2", "read"));
    rebuild(&mut panel);

    assert!(seg_text(&panel, "t1").contains("streaming data"));
    assert!(has_seg(&panel, "t2"));
}

#[test]
fn tool_done_after_cancel_in_progress_does_not_underflow() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.cancel_in_progress();
    assert_eq!(panel.in_progress_count(), 0);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("late".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test]
fn selection_freezes_viewport_during_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
    let scroll_before = panel.scroll_top;
    assert!(scroll_before > 0);

    panel.streaming_text.set_buffer(&"a\n".repeat(35));
    render_sel(&mut panel, 80, 10, true);
    assert_eq!(panel.scroll_top, scroll_before);
    assert!(panel.auto_scroll);

    render_sel(&mut panel, 80, 10, false);
    assert!(panel.scroll_top > scroll_before);
    assert!(panel.auto_scroll);
}

fn seg_search(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .search_text
        .clone()
}

#[test]
fn search_text_grep_result_includes_structured_output() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "grep"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "grep".into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("0.rs:") && text.contains("1.rs:"));
}

#[test]
fn search_text_diff_output_includes_hunks() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "edit"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "edit".into(),
        output: ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "old\n".into(),
            after: "new\n".into(),
            summary: "1 edit".into(),
        },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("- old") && text.contains("+ new"));
}

#[test]
fn search_text_bash_with_code_input() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo hello");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("hello".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("hello"));
}

#[test]
fn search_text_includes_role_prefix() {
    let md = "# Heading\n\nSome **bold** text";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, md.into()));
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "hmm".into()));
    rebuild(&mut panel);
    let texts = panel.segment_search_texts();
    assert_eq!(texts[0], "you> hello");
    assert_eq!(texts[2], format!("maki> {md}"));
    assert_eq!(texts[4], "thinking> hmm");
}

#[test_case(&["short", &"x".repeat(200)], 80, 4 ; "long_line_wraps")]
#[test_case(&["", "a", ""],                 40, 3 ; "empty_lines_count_as_one")]
#[test_case(&[&"a".repeat(80)],              80, 1 ; "exactly_width_no_wrap")]
#[test_case(&[&"a".repeat(81)],              80, 2 ; "one_over_width_wraps")]
#[test_case(&["hello", "world"],              0, 2 ; "zero_width_returns_line_count")]
#[test_case(&["aaaa bbbb cccc dddd"],         10, 2 ; "word_boundary_wrap")]
#[test_case(&["aaaaaa bbbbbbbbb"],            10, 2 ; "word_straddles_boundary")]
fn wrapped_line_count_cases(input: &[&str], width: u16, expected: u16) {
    let lines: Vec<Line<'static>> = input
        .iter()
        .map(|s| Line::from(Span::raw(s.to_string())))
        .collect();
    assert_eq!(wrapped_line_count(&lines, width), expected);
}

#[test]
fn update_tool_model_sets_annotation() {
    let mut panel = panel_with_tools(&[("t1", "task"), ("t2", "bash")]);
    rebuild(&mut panel);

    panel.update_tool_model("t1", "anthropic/claude-sonnet-4-20250514");

    let msg = &panel.messages[0];
    assert_eq!(
        msg.annotation.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test]
fn set_tool_turn_usage_updates_exact_tool_and_keeps_annotation() {
    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const USAGE: &str = "1.2k↑ 345↓ $0.010";

    let mut panel = panel_with_tools(&[("t1", "task"), ("t2", "task")]);
    panel.update_tool_model("t1", MODEL);

    panel.set_tool_turn_usage("t1", USAGE.into());

    assert_eq!(panel.tool_turn_usage("t1"), Some(USAGE));
    assert_eq!(panel.tool_turn_usage("t2"), None);
    assert_eq!(panel.messages[0].annotation.as_deref(), Some(MODEL));
}

#[test]
fn win_view_clamps_a_restored_offset_past_the_end() {
    const LINES: u16 = 15;
    const HEIGHT: u16 = 10;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel
        .streaming_text
        .set_buffer(&"a\n".repeat(LINES as usize));
    render(&mut panel, 80, HEIGHT);

    panel.restore_scroll(u16::MAX, true);

    let view = panel.win_view();
    assert_eq!(view.scroll_top, panel.max_scroll());
    assert_eq!(view.line_count, LINES);
    assert_eq!(view.height, HEIGHT);
    assert!(view.auto_scroll);
}

#[test]
fn scroll_clamps_to_max_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(15));
    render(&mut panel, 80, 10);
    let max = panel.max_scroll();

    panel.scroll(-3);
    assert_eq!(panel.scroll_top, max);
}

#[test_case("bash", 1, 1 ; "known_tool_creates_message")]
#[test_case("nonexistent_tool", 1, 1 ; "unknown_tool_accepted")]
fn tool_pending(tool: &str, expected_msgs: usize, expected_in_progress: usize) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), tool);
    assert_eq!(panel.messages.len(), expected_msgs);
    assert_eq!(panel.in_progress_count(), expected_in_progress);
}

#[test]
fn tool_start_upgrades_pending_in_place() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), "bash");
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);

    let mut event = start("t1", BASH_TOOL_NAME);
    event.annotation = Some("note".into());
    panel.tool_start(event);

    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);
    assert_eq!(panel.messages[0].text, "t1");
    assert_eq!(panel.messages[0].annotation.as_deref(), Some("note"));
}

#[test]
fn stream_reset_clears_streaming_and_fails_tools() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    panel.streaming_thinking.set_buffer("partial thinking");
    panel.streaming_text.set_buffer("partial text");
    rebuild(&mut panel);

    panel.stream_reset();

    assert!(panel.streaming_thinking.is_empty());
    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Error);
}

const MAKI_PREFIX_LEN: u16 = 6;

fn make_sel(area: Rect, anchor: (u32, u16), cursor: (u32, u16)) -> Selection {
    let mut sel = Selection::start(
        area.y + anchor.0 as u16,
        anchor.1,
        area,
        SelectionZone::Messages,
        0,
    );
    sel.update(area.y + cursor.0 as u16, cursor.1, 0);
    sel
}

fn panel_with_msgs(texts: &[&str], width: u16, height: u16) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &text in texts {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, width, height);
    panel
}

#[test]
fn extract_partial_column_selection() {
    let panel = panel_with_msgs(&["Hello world"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let world_start = MAKI_PREFIX_LEN + "Hello ".len() as u16;
    let sel = make_sel(area, (0, world_start), (0, world_start + 4));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text, "world");
}

#[test]
fn extract_skips_out_of_range_segments() {
    let panel = panel_with_msgs(&["seg0", "seg1", "seg2"], 80, 24);
    let heights = panel.segment_heights();
    let total: u16 = heights.iter().sum();
    let mid = total / 2;
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (mid as u32, 0), (mid as u32, 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("seg1"));
    assert!(!text.contains("seg0"));
    assert!(!text.contains("seg2"));
}

#[test]
fn extract_off_screen_rows_via_temp_buffer() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let text = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    panel.push(DisplayMessage::new(DisplayRole::Assistant, text));
    render(&mut panel, 80, 5);

    let total: u16 = panel.segment_heights().iter().sum();
    assert!(total > 5, "content must exceed viewport");
    let sel_area = Rect::new(0, 0, 80, total);
    let sel = make_sel(sel_area, (1, 0), ((total - 1) as u32, 79));

    let extracted = panel.extract_selection_text(&sel, sel_area);
    assert!(!extracted.contains("line 0"), "first line excluded");
    assert!(extracted.contains("line 1") && extracted.contains("line 19"));
}

#[test]
fn extract_mixed_fully_enclosed_and_partial() {
    let panel = panel_with_msgs(&["full segment", "partial here"], 80, 24);
    let heights = panel.segment_heights().to_vec();
    let area = Rect::new(0, 0, 80, 24);
    let seg1_start = heights[0] + heights[1];
    let sel = make_sel(area, (0, 0), (seg1_start as u32, MAKI_PREFIX_LEN + 6));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("full segment"));
    assert!(text.contains("partial"));
}

#[test_case(&["line-0\nline-1\nline-2\nline-3"], "line-0", "line-3" ; "single_segment")]
#[test_case(&["seg-A-text", "seg-B-text"],      "seg-A-text", "seg-B-text" ; "across_segments")]
fn extract_partial_col_symmetric(msgs: &[&str], expect_start: &str, expect_end: &str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &text in msgs {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let down = make_sel(area, (0, MAKI_PREFIX_LEN), ((total - 1) as u32, 79));
    let up = make_sel(area, ((total - 1) as u32, 79), (0, MAKI_PREFIX_LEN));
    let text_down = panel.extract_selection_text(&down, area);
    let text_up = panel.extract_selection_text(&up, area);
    assert!(text_down.contains(expect_start));
    assert!(text_down.contains(expect_end));
    assert_eq!(text_down, text_up, "direction should not affect result");
}

#[test_case("```\n{L}\n```", (0, 1)  ; "wrapped_code_block")]
#[test_case("short\n{L}",   (0, 0)  ; "wrapped_long_line")]
fn extract_wrapped_no_soft_breaks(template: &str, anchor: (u32, u16)) {
    let long = "x".repeat(200);
    let msg = template.replace("{L}", &long);
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Assistant, msg));
    render(&mut panel, 40, 30);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 40, 30);
    let sel = make_sel(area, anchor, ((total - 1) as u32, 39));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        text.contains(&long),
        "wrapped line must be copied without newlines: {text:?}"
    );
}

#[test]
fn extract_partial_last_line_truncated() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "first\nABCDEFGHIJKLMNOP".into(),
    ));
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let last_row = (total - 1) as u32;
    let sel = make_sel(area, (0, 0), (last_row, 3));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text.lines().last().unwrap(), "ABCD");
}

fn panel_with_long_tool(line_count: usize) -> MessagesPanel {
    let body = (0..line_count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        summary: "cmd".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(body.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_truncated_tool() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));
}

#[test]
fn extract_selection_copies_visible_content_only() {
    let panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let sel = make_sel(area, (0, 0), ((total - 1) as u32, 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        !text.contains("line 50"),
        "truncated line should not be copied"
    );
}

#[test]
fn toggle_returns_false_for_non_expandable() {
    let mut panel = panel_with_long_tool(3);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.toggle_expansion_at(area.y, area));
}

fn panel_with_grep_tool(match_count: usize) -> MessagesPanel {
    let entries = vec![GrepFileEntry {
        path: "src/main.rs".into(),
        groups: (1..=match_count)
            .map(|i| GrepMatchGroup::single(i, format!("match_{i}")))
            .collect(),
    }];
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        summary: "grep pattern".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        output: ToolOutput::GrepResult { entries },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_grep_tool() {
    let mut panel = panel_with_grep_tool(8);
    let area = Rect::new(0, 0, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));
}

fn buffer_text(terminal: &ratatui::Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.push('\n');
    }
    text
}

#[test]
fn streaming_with_cached_segments_shows_end_on_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "a\n".repeat(20).trim().into(),
    ));
    panel.streaming_text.set_buffer(
        &(0..50)
            .map(|i| format!("stream_{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let terminal = render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let screen = buffer_text(&terminal);
    assert!(screen.contains("stream_49"), "should show end");
    assert!(!screen.contains("stream_0 "), "should not show beginning");
}

#[test]
fn search_text_includes_truncated_bash_output() {
    let full_output = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo lines");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(full_output.clone().into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    assert!(seg_search(&panel, "t1").contains(&full_output));
}

fn instruction_blocks() -> Vec<InstructionBlock> {
    vec![InstructionBlock {
        path: "agents.md".into(),
        content: "follow style guide".into(),
    }]
}

fn read_code_with_instructions(blocks: Vec<InstructionBlock>) -> ToolOutput {
    ToolOutput::ReadCode {
        path: "file.rs".into(),
        start_line: 1,
        lines: vec!["fn main() {}".into()],
        total_lines: 1,
        instructions: Some(blocks),
    }
}

fn prev_segment_is_spacer(panel: &MessagesPanel, tool_id: &str) -> bool {
    let idx = panel.cache.find_by_tool_id(tool_id).unwrap();
    panel.cache.get(idx - 1).unwrap().tool_id.is_none()
}

#[test]
fn instruction_segment_has_spacer_before_it() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let inst_id = segment::instruction_id("t1");
    assert!(prev_segment_is_spacer(&panel, &inst_id));
}

fn seg_line_count(panel: &MessagesPanel, tool_id: &str) -> usize {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .len()
}

#[test]
fn toggle_instruction_segment_expands_and_collapses() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let blocks = vec![InstructionBlock {
        path: "agents.md".into(),
        content: "x\n".repeat(100),
    }];
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(blocks),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let inst_id = segment::instruction_id("t1");
    let collapsed = seg_line_count(&panel, &inst_id);

    panel.toggle_expansion(&inst_id);
    assert!(seg_line_count(&panel, &inst_id) > collapsed);

    panel.toggle_expansion(&inst_id);
    assert_eq!(seg_line_count(&panel, &inst_id), collapsed);
}

#[test]
fn handle_click_returns_nothing_when_no_segment_at_row() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(23, area));
}

#[test]
fn handle_click_on_done_tool_records_click_row() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t1").map(Vec::len), Some(1));
}

#[test]
fn handle_click_on_running_tool_forwards_live_without_recording() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("streaming")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert!(panel.lua_clicks.is_empty());
}

#[test]
fn handle_click_returns_toggled_for_truncated_tool_without_snapshot() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
}

#[test]
fn handle_click_non_tool_segment_returns_nothing() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "user message".into(),
    ));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(area.y, area));
}

#[test]
fn tool_done_removes_live_buf_and_snapshots_dirty() {
    let buf = Arc::new(maki_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("dirty content")]);

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "dirty content"
    );
}

/// The handler's buf must supersede the `start` preview: the UI keeps only
/// the last registered buf per tool_use_id.
#[test]
fn second_register_live_buf_replaces_first() {
    let preview = Arc::new(maki_agent::SharedBuf::new());
    preview.set_lines(vec![snap_line("preview")]);
    let handler = Arc::new(maki_agent::SharedBuf::new());
    handler.set_lines(vec![snap_line("handler")]);

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&preview));
    panel.register_live_buf("t1".into(), Arc::clone(&handler));
    let _ = panel.poll_live_bufs();

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "handler"
    );
}

/// Every finished-tool click on a watched buf carries the full recorded
/// click list as a restore fallback: the runtime serves it warm when it
/// can and restores otherwise, so the UI never guesses runtime state.
#[test_case(false ; "success")]
#[test_case(true ; "error_finish")]
fn handle_click_on_watched_tool_sends_click_with_fallback(is_error: bool) {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "body", is_error);
    assert!(panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t1"].clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(probe.try_recv(), Some(("click_fallback", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_moves_live_buf_to_watched_polled_but_not_animating() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let buf = finish_with_live_buf(&mut panel, "t1", "before", false);
    assert!(panel.watching("t1"));
    assert_eq!(
        panel.cadence(),
        Cadence::IDLE,
        "a finished tool must not leave a spinner running"
    );

    buf.set_lines(vec![snap_line("after")]);
    assert_eq!(panel.poll_live_bufs(), Dirty::YES, "{OWED}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after"
    );
}

#[test]
fn watched_fifo_evicts_oldest_which_stops_polling_and_restores_with_recorded_clicks() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t0", "before", false);

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t0").map(Vec::len), Some(1));
    assert_eq!(
        probe.try_recv(),
        Some(("click_fallback", panel.lua_clicks["t0"].clone()))
    );

    for i in 1..=WARM_TOOL_CAP {
        finish_with_live_buf(&mut panel, &format!("t{i}"), "body", false);
    }
    assert_eq!(panel.watched_bufs.len(), WARM_TOOL_CAP);
    assert!(!panel.watching("t0"));

    buf.set_lines(vec![snap_line("after-eviction")]);
    assert_eq!(
        panel.poll_live_bufs(),
        Dirty::NO,
        "evicted buf must no longer be polled"
    );
    let msg = panel.find_tool_msg_mut("t0").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "before",
        "evicted buf must no longer be polled"
    );

    render(&mut panel, 80, 24);
    panel.scroll_to_top();
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t0"].clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(probe.try_recv(), Some(("restore", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_without_live_buf_is_not_watched_and_click_restores() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let mut ev = start("t1", BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("body")])),
        None,
    );
    panel.tool_done(done("t1"));
    assert!(!panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// The stale-run_id filter drops ToolDone events after a cancel, so the
/// cancel path itself must retire live bufs: no spinner left running, and
/// the tool stays clickable through the warm path.
#[test]
fn cancel_in_progress_retires_live_buf_to_watched() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = Arc::new(maki_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("body")]);
    let mut ev = start("t1", BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    panel.cancel_in_progress();
    assert_eq!(
        panel.cadence(),
        Cadence::IDLE,
        "cancel must not leave a tool marked in progress"
    );
    assert!(panel.watching("t1"));

    buf.set_lines(vec![snap_line("after-cancel")]);
    // The tool hands that same repaint to the host as its reply body, and the
    // stale-run_id filter drops it. Taking a body must not cost the screen the
    // last thing a cancelled tool painted.
    let _dropped_reply = buf.take();
    assert_eq!(panel.poll_live_bufs(), Dirty::YES, "{OWED}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after-cancel"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(probe.try_recv(), Some(("click", vec![])));
    assert_eq!(probe.try_recv(), None);
}

/// A restore reply supersedes the old live view: the buf must stop
/// being watched so its stale content can't overwrite the fresh
/// snapshot, and later clicks must go through restore.
#[test]
fn restore_reply_stops_watching_buf() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rebaked")])),
        Some(baked_gen),
    );
    assert!(!panel.watching("t1"));

    buf.set_lines(vec![snap_line("stale-mutation")]);
    assert_eq!(
        panel.poll_live_bufs(),
        Dirty::NO,
        "unwatched buf must no longer be polled"
    );
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "rebaked",
        "unwatched buf must not overwrite the restored snapshot"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// Requesting a rebake already stops watching: clicks inside the
/// request/reply window must restore (with the new theme) instead of
/// mutating the old-theme buf.
#[test]
fn rebake_request_stops_watching_buf() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let next_gen = panel.snapshot_gen_of("t1").unwrap() + 1;
    panel.rebake_stale_snapshots(next_gen);
    assert!(!panel.watching("t1"));
    assert_eq!(probe.try_recv(), Some(("restore", vec![])));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn live_buf_streams_across_clean_polls() {
    let buf = Arc::new(maki_agent::SharedBuf::new());
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    buf.append(snap_line("first"));
    assert_eq!(panel.poll_live_bufs(), Dirty::YES);
    assert_eq!(panel.poll_live_bufs(), Dirty::NO, "{QUIET}");

    buf.append(snap_line("second"));
    assert_eq!(panel.poll_live_bufs(), Dirty::YES);

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    let snapshot = msg.render_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.lines.len(), 2);
}

#[test]
fn tool_done_without_live_buf_preserves_existing_snapshot() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("pre-existing")])),
        None,
    );
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "pre-existing"
    );
}

#[test]
fn tool_done_clean_live_buf_does_not_snapshot() {
    let buf = Arc::new(maki_agent::SharedBuf::new());

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(
        msg.render_snapshot.is_none(),
        "clean (never-written) live buf should not produce a snapshot"
    );
}

const REQUEST_RECORDED_MSG: &str = "a fired re-bake records the requested generation";
const NOT_RESTAMPED_MSG: &str =
    "the re-bake walk must not optimistically stamp the displayed generation";
const NO_REQUEST_MSG: &str = "snapshot-free message must not trigger a re-bake request";
const SUPERSEDED_DROP_MSG: &str =
    "a re-bake reply older than the applied generation must be dropped (monotonic)";

fn bash_tool_with_snapshot(id: &str) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(id, BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    panel.tool_snapshot(
        id,
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    panel
}

fn rendered_snapshot() -> BufferSnapshot {
    BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")]))
}

#[test]
fn rebake_walk_requests_without_stamping_displayed_generation() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({ "command": "echo" })));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "plain".into()));
    panel.set_restore_channel(Some(test_event_sender()));

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    let next_gen = baked_gen + 1;
    panel.rebake_stale_snapshots(next_gen);

    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(baked_gen),
        "{NOT_RESTAMPED_MSG}"
    );
    assert_eq!(
        panel.rebake_requested_gen("t1"),
        Some(next_gen),
        "{REQUEST_RECORDED_MSG}"
    );
    assert_eq!(panel.messages[1].snapshot_theme_gen, 0, "{NO_REQUEST_MSG}");
}

#[test]
fn superseded_rebake_reply_is_dropped() {
    let mut panel = bash_tool_with_snapshot("t1");
    let baked = panel.snapshot_gen_of("t1").unwrap();
    let newer = baked + 3;
    panel.tool_snapshot("t1", rendered_snapshot(), Some(newer));
    panel.tool_snapshot("t1", rendered_snapshot(), Some(baked + 1));
    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(newer),
        "{SUPERSEDED_DROP_MSG}"
    );
}

fn test_event_sender() -> maki_agent::EventSender {
    let (tx, _rx) = flume::unbounded();
    maki_agent::EventSender::new(tx, 0)
}

const RAW_INPUT_SET_MSG: &str = "tool_raw_input must be set from event payload";
const HEADER_GEN_MSG: &str = "header snapshot must stamp the provided generation";
const LIVE_PANEL_GEN_MSG: &str = "live snapshot (None gen) must stamp with panel theme_generation";
const REBAKE_NOOP_MSG: &str = "rebake without channel must be a no-op (no requested gen)";

#[test_case(false ; "fresh_start")]
#[test_case(true  ; "upgrade_from_pending")]
fn tool_start_propagates_raw_input(pre_pending: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    if pre_pending {
        panel.tool_pending("t1".into(), BASH_TOOL_NAME);
    }
    let mut event = start("t1", BASH_TOOL_NAME);
    event.raw_input = Some(serde_json::json!({"command": "echo"}));
    panel.tool_start(event);

    let raw = panel
        .find_tool_msg_mut("t1")
        .unwrap()
        .tool_raw_input
        .as_ref();
    assert!(raw.is_some(), "{RAW_INPUT_SET_MSG}");
    assert_eq!(
        raw.unwrap().as_ref(),
        &serde_json::json!({"command": "echo"}),
        "{RAW_INPUT_SET_MSG}"
    );
}

#[test]
fn header_snapshot_stamps_gen_on_top_level() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_header_snapshot("t1", rendered_snapshot(), Some(5));

    assert_eq!(panel.snapshot_gen_of("t1"), Some(5), "{HEADER_GEN_MSG}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(msg.render_header.is_some(), "render_header must be set");
}

#[test]
fn live_snapshot_uses_panel_generation() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_snapshot("t1", rendered_snapshot(), None);

    assert_eq!(panel.snapshot_gen_of("t1"), Some(0), "{LIVE_PANEL_GEN_MSG}");
}

#[test]
fn rebake_without_channel_is_noop() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({"command": "echo"})));
    let baked_gen = panel.snapshot_gen_of("t1").unwrap();

    panel.rebake_stale_snapshots(baked_gen + 1);

    assert!(
        panel.rebake_requested_gen("t1").is_none(),
        "{REBAKE_NOOP_MSG}"
    );
}

#[test]
fn hide_collapses_streaming_thinking() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel
        .streaming_thinking
        .set_buffer("line one\nline two\nline three");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("thinking> ..."),
        "collapsed view should show hint; got: {text}"
    );
    assert!(
        text.contains("3 lines"),
        "should show live line counter; got: {text}"
    );
    assert!(
        text.contains("click to expand"),
        "should hint click-to-expand; got: {text}"
    );
    assert!(
        !text.contains("line one"),
        "reasoning must stay hidden; got: {text}"
    );
}

#[test]
fn hide_click_expands_streaming_thinking() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.streaming_thinking.set_buffer("secret reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, 80, 10);
    assert!(
        panel.handle_click(0, area),
        "clicking collapsed thinking should toggle expand"
    );
    assert!(!panel.thinking_collapsed);
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("secret reasoning"),
        "expanded view should show reasoning; got: {text}"
    );
    assert!(
        !text.contains("click to expand"),
        "collapsed hint should not appear after expand; got: {text}"
    );
}

#[test]
fn hide_keeps_cached_thinking_as_indicator() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("reasoning here");
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("thinking> ..."),
        "cached thinking should persist as an indicator, not hide; got: {text}"
    );
    assert!(
        text.contains("(1 lines)"),
        "footer always shows the line count; got: {text}"
    );
    assert!(
        text.contains("click to expand"),
        "footer should hint click-to-expand; got: {text}"
    );
    assert!(
        !text.contains("reasoning here"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn full_default_renders_streaming_thinking() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_thinking.set_buffer("visible reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("visible reasoning"),
        "default config renders reasoning; got: {text}"
    );
}

#[test]
fn hide_cached_thinking_persists_as_indicator() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    let lines: Vec<String> = (1..=7).map(|n| format!("cached line {n}")).collect();
    panel.thinking_delta(&lines.join("\n"));
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("thinking> ..."),
        "cached thinking should persist as an indicator, not hide; got: {text}"
    );
    assert!(text.contains("(7 lines)"), "footer line count; got: {text}");
    assert!(
        text.contains("click to expand"),
        "footer should hint click-to-expand; got: {text}"
    );
    assert!(
        !text.contains("cached line 7"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
    assert!(
        !text.contains("cached line 1"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn hide_cached_thinking_click_expands() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("hidden cached reasoning");
    panel.flush();
    let area = Rect::new(0, 0, 80, 12);
    render(&mut panel, 80, 12);
    assert!(
        panel.handle_click(0, area),
        "clicking persisted thinking should toggle expand"
    );
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("hidden cached reasoning"),
        "expanded view shows full reasoning; got: {text}"
    );
    assert!(
        !text.contains("click to expand"),
        "footer should disappear when expanded; got: {text}"
    );
}

#[test]
fn stream_reset_clears_thinking_expand_state() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.streaming_thinking.set_buffer("secret reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, 80, 10);
    assert!(
        panel.handle_click(0, area),
        "clicking collapsed thinking should toggle expand"
    );
    assert!(!panel.thinking_collapsed);
    panel.stream_reset();
    assert!(
        panel.thinking_collapsed,
        "stream_reset must restore the collapsed default so it does not leak into retries"
    );
    panel.streaming_thinking.set_buffer("fresh reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("thinking> ..."),
        "new stream after reset should collapse again; got: {text}"
    );
    assert!(
        !text.contains("fresh reasoning"),
        "new stream must stay hidden; got: {text}"
    );
}

#[test]
fn stale_height_keeps_the_old_width_but_drawn_height_does_not() {
    let long_line = Line::from("x".repeat(80));
    let mut seg = Segment::with_lines(vec![long_line.clone()], "test".into(), None);

    let h_wide = seg.height(80);
    assert_eq!(h_wide, 1, "80 chars at width 80 fits on one line");

    // Keeping the old height is what keeps a resize cheap: the document
    // layout stays put until the segment is really reflowed.
    seg.stale = true;
    assert_eq!(
        seg.height(40),
        h_wide,
        "stale segment should return old cached height, not recompute"
    );
    // Callers that re-wrap the lines themselves need the real number.
    assert_eq!(
        seg.drawn_height(40),
        2,
        "drawn_height must report what the lines really take at the new width"
    );

    seg.set_lines(vec![long_line]);
    assert_eq!(seg.height(40), 2, "80 chars at width 40 wraps to two lines");
}

#[test]
fn copy_after_resize_keeps_offscreen_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let body = "x".repeat(60);
    for i in 0..30 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("m{i:02}{body}"),
        ));
    }
    render(&mut panel, 80, 10);
    render(&mut panel, 40, 10);

    let total: u32 = panel.segment_heights().iter().map(|&h| h as u32).sum();
    let area = Rect::new(0, 0, 40, 10);
    let sel = make_sel(area, (0, 0), (total - 1, 39));
    let text = panel.extract_selection_text(&sel, area);

    // The top of the transcript is far off-screen and never gets reflowed.
    // Selection sizes its buffer from `height` and then re-wraps, so a height
    // measured at the old width would clip every line it copies.
    assert!(
        text.contains(&format!("m00{body}")),
        "off-screen message was truncated in the copy: {text:?}"
    );
}

#[test]
fn resize_reflows_only_viewport_segments() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    // 30 messages, each ~60 chars beyond the label — enough to exceed
    // viewport (10) + reflow margin (1 * 10 = 10 lines) so top segments
    // stay out of the reflow range when auto-scrolled to the bottom.
    for i in 0..30 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, 10);
    let seg_count_before = panel.cache.len();
    assert!(seg_count_before > 0);

    render(&mut panel, 40, 10);

    // Cache preserved — no nuke
    assert_eq!(
        panel.cache.len(),
        seg_count_before,
        "resize must not clear the segment cache"
    );

    let segs = panel.cache.segments();

    // Bottom segments (near the auto-scrolled viewport) are reflowed
    let bottom_fresh = segs
        .iter()
        .filter(|s| s.msg_index.is_some())
        .rev()
        .take(5)
        .all(|s| !s.stale);
    assert!(
        bottom_fresh,
        "viewport segments should be reflowed to new width"
    );

    // Top segments (far above the viewport) remain width-stale
    let top_stale = segs
        .iter()
        .filter(|s| s.msg_index.is_some())
        .take(5)
        .all(|s| s.stale);
    assert!(
        top_stale,
        "off-viewport segments should stay width-stale after resize"
    );
}

fn msg_seg_text(panel: &MessagesPanel, msg_idx: usize) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none())
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect()
}

#[test]
fn reflow_rebuilds_collapsed_thinking_instead_of_only_stamping() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.show_thinking = false;
    let mut m = DisplayMessage::new(DisplayRole::Thinking, "one\ntwo".to_string());
    m.thinking_collapsed = true;
    panel.push(m);
    render(&mut panel, 80, 10);
    assert!(
        msg_seg_text(&panel, 0).contains("(2 lines)"),
        "indicator should report the initial line count"
    );

    // Change what the indicator renders, then mark it stale the way a theme
    // change does. Clearing the flag without rebuilding keeps the old spans.
    panel.messages[0].text = "one\ntwo\nthree\nfour".to_string();
    panel.cache.mark_all_width_stale();
    render(&mut panel, 80, 10);

    assert!(
        msg_seg_text(&panel, 0).contains("(4 lines)"),
        "stale collapsed-thinking segment must be rebuilt, not just stamped; got: {}",
        msg_seg_text(&panel, 0)
    );
}

#[test]
fn reflow_runs_without_a_width_or_scroll_change() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..5 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i}"),
        ));
    }
    render(&mut panel, 80, 10);

    // Segments go stale between frames without either trigger firing.
    panel.cache.mark_all_width_stale();
    render(&mut panel, 80, 10);

    assert!(
        panel.cache.segments().iter().all(|s| !s.stale),
        "visible segments must be reflowed even when width and scroll_top are unchanged"
    );
}

#[test]
fn resize_reflows_tool_segment_and_keeps_instruction_segment() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 10);

    // Tool + spacer + instruction segments are built up front.
    let seg_count = panel.cache.len();
    assert!(seg_count >= 3);

    render(&mut panel, 40, 10);

    // The instruction segment already exists, so reflowing the tool segment
    // updates it in place rather than re-inserting (exercises the to_reflow
    // index path through `rebuild_tool_segment` and the upsert).
    assert_eq!(
        panel.cache.len(),
        seg_count,
        "reflow must reuse the existing instruction segment, not re-insert"
    );
    // The viewport auto-scrolls to the bottom, where the tool and instruction
    // segments sit, so neither stays stale after the resize.
    assert!(
        panel.cache.segments().iter().all(|s| !s.stale),
        "tool and instruction segments in the viewport must be reflowed, not left stale"
    );
}

#[test]
fn big_widen_keeps_no_stale_segment_in_the_viewport() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..40 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(150)),
        ));
    }
    render(&mut panel, 80, 30);
    render(&mut panel, 240, 30);

    // 3x widen: content shrinks and the bottom pin pulls up, so a single
    // pre-reflow pass would leave stale segments in the viewport.
    let vh = 30u32;
    let top = panel.scroll_top() as u32;
    let mut offset: u32 = 0;
    for seg in panel.cache.segments() {
        let h = seg.height(240) as u32;
        let in_view = offset < top.saturating_add(vh) && offset + h > top;
        assert!(
            !(in_view && seg.stale),
            "a stale segment overlaps the viewport after a big widen"
        );
        offset += h;
    }
    assert!(
        panel.cache.segments().iter().any(|s| s.stale),
        "off-viewport segments must stay stale so the test exercises convergence"
    );
}

/// `scroll_top` sits inside the anchor segment, so a downward window measured
/// from that segment's start can be consumed entirely by rows above the first
/// visible one, leaving the screen full of segments still wrapped at the old
/// width.
#[test]
fn resize_low_in_a_tall_segment_leaves_no_stale_segment_in_the_viewport() {
    const VIEWPORT_HEIGHT: u16 = 10;
    const TALL_LINES: usize = 100;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let tall = (0..TALL_LINES)
        .map(|i| format!("line {i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    panel.push(DisplayMessage::new(DisplayRole::Assistant, tall));
    for i in 0..8 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("tail {i} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, VIEWPORT_HEIGHT);

    // Five rows from the bottom of the tall first segment: the rest of the
    // viewport is filled by the segments after it.
    panel.set_scroll_top(TALL_LINES as u16 - 5);
    render(&mut panel, 80, VIEWPORT_HEIGHT);
    assert!(
        !panel.auto_scroll(),
        "the test must start anchored deep inside the tall segment"
    );

    render(&mut panel, 40, VIEWPORT_HEIGHT);

    let top = panel.scroll_top() as u32;
    let mut offset: u32 = 0;
    for seg in panel.cache.segments() {
        let h = seg.height(39) as u32;
        let in_view = offset < top + VIEWPORT_HEIGHT as u32 && offset + h > top;
        assert!(
            !(in_view && seg.stale),
            "a stale segment overlaps the viewport after resizing low in a tall segment"
        );
        offset += h;
    }
}

#[test]
fn anchored_resize_keeps_the_topmost_visible_segment() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..40 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, 10);
    panel.set_scroll_top(panel.max_scroll() / 2); // mid-transcript, unpins
    render(&mut panel, 80, 10);
    assert!(
        !panel.auto_scroll(),
        "the test must start anchored, not pinned"
    );
    let before = panel
        .cache
        .anchor_at(panel.scroll_top() as u32, 79)
        .expect("scroll_top lands inside a segment");

    render(&mut panel, 40, 10);

    let after = panel
        .cache
        .anchor_at(panel.scroll_top() as u32, 39)
        .expect("scroll_top still lands inside a segment after the resize");
    assert_eq!(
        after.0, before.0,
        "narrowing must not slide the anchored topmost segment off the viewport"
    );
    assert!(
        !panel.auto_scroll(),
        "an anchored mid-transcript resize must not flip to the bottom pin"
    );
}

const THEME_CODE: &str = "fn main() { let x = 1; }";
const THEME_CODE_KEYWORDS: [&str; 3] = ["fn", "main", "let"];

fn code_span_styles(panel: &MessagesPanel, tool_id: &str) -> Vec<(String, Style)> {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| THEME_CODE_KEYWORDS.contains(&s.content.trim()))
        .map(|s| (s.content.to_string(), s.style))
        .collect()
}

fn drain_highlight_worker(panel: &mut MessagesPanel) {
    let deadline = Instant::now() + HIGHLIGHT_DEADLINE;
    while panel.tick() == Dirty::NO {
        assert!(
            Instant::now() < deadline,
            "the highlight worker never delivered a result"
        );
        std::thread::yield_now();
    }
    render(panel, 80, 20);
}

/// The unit test above only proves two generations make two keys. This is the
/// wiring: drop `theme_gen` at a call site and the old palette gets spliced
/// straight back in with no test to catch it.
#[test]
fn theme_switch_repaints_highlighted_code() {
    theme::set(theme::load_by_name("dracula").unwrap());
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: ToolOutput::ReadCode {
            path: "file.rs".into(),
            start_line: 1,
            lines: vec![THEME_CODE.into()],
            total_lines: 1,
            instructions: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 20);
    drain_highlight_worker(&mut panel);
    let dracula = code_span_styles(&panel, "t1");
    assert!(!dracula.is_empty(), "no highlighted keywords to compare");

    theme::set(theme::load_by_name("tokyonight").unwrap());
    render(&mut panel, 80, 20);
    drain_highlight_worker(&mut panel);

    assert_ne!(
        dracula,
        code_span_styles(&panel, "t1"),
        "a theme switch must re-highlight, not splice old-palette lines back"
    );
}

const FIRST_TEXT: &str = "run the migration";
const FOLLOW_UP_TEXT: &str = "and then deploy";
const STALE_BUBBLE_MSG: &str = "the superseded bubble must disappear from the viewport";
const UNTOUCHED_MSG: &str = "a rejected replace must leave the transcript untouched";

fn style_of(terminal: &ratatui::Terminal<TestBackend>, text: &str) -> Style {
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol()))
            .collect();
        if let Some(col) = row.find(text) {
            return buf.cell((col as u16, y)).unwrap().style();
        }
    }
    panic!("{text} was never rendered");
}

/// `Chat::mark_finished` corrects a bubble long after it was drawn, with the
/// transcript still growing in between. Unless `replace` throws the baked
/// segments away, the viewport keeps painting a green "Done!" the message
/// vector no longer holds.
#[test]
fn replace_repaints_the_corrected_bubble_in_place() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, FIRST_TEXT.into()));
    let bubble = panel.push(DisplayMessage::new(DisplayRole::Done, DONE_TEXT.into()));
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        FOLLOW_UP_TEXT.into(),
    ));
    let done_style = style_of(&render(&mut panel, 80, 24), DONE_TEXT);

    panel.replace(
        bubble,
        DisplayMessage::new(DisplayRole::Error, ERROR_TEXT.into()),
    );

    let rendered = render(&mut panel, 80, 24);
    let text = buffer_text(&rendered);
    let texts: Vec<&str> = panel.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, [FIRST_TEXT, ERROR_TEXT, FOLLOW_UP_TEXT]);
    assert!(text.contains(ERROR_TEXT), "got: {text}");
    assert!(text.contains(FOLLOW_UP_TEXT), "got: {text}");
    assert!(!text.contains(DONE_TEXT), "{STALE_BUBBLE_MSG}: {text}");
    assert_ne!(
        style_of(&rendered, ERROR_TEXT),
        done_style,
        "the corrected bubble kept the success styling"
    );
}

#[test]
fn replace_past_the_end_is_a_noop() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Done, DONE_TEXT.into()));
    rebuild(&mut panel);

    panel.replace(
        panel.message_count(),
        DisplayMessage::new(DisplayRole::Error, ERROR_TEXT.into()),
    );

    assert_eq!(panel.message_count(), 1, "{UNTOUCHED_MSG}");
    let text = buffer_text(&render(&mut panel, 80, 10));
    assert!(text.contains(DONE_TEXT), "{UNTOUCHED_MSG}: {text}");
    assert!(!text.contains(ERROR_TEXT), "{UNTOUCHED_MSG}: {text}");
}
