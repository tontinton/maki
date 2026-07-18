use super::segment;
use super::*;
use crate::components::scrollbar::SCROLLBAR_THUMB;
use crate::selection::{Selection, SelectionZone};
use maki_agent::tools::{BASH_TOOL_NAME, GREP_TOOL_NAME, WRITE_TOOL_NAME};
use maki_agent::{
    GrepFileEntry, GrepMatchGroup, SnapshotLine, SnapshotSpan, SpanStyle, ToolInput, ToolOutput,
};
use ratatui::backend::TestBackend;
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.streaming_text.set_buffer("partial response");

    panel.tool_start(start("t1", "read"));

    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.messages[0].role, DisplayRole::Assistant);
    assert!(matches!(panel.messages[1].role, DisplayRole::Tool(_)));
}

#[test]
fn thinking_delta_separate_from_text() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    assert!(!panel.is_animating());
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Error);
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
fn scroll_clamps_to_max_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.streaming_text.set_buffer(&"a\n".repeat(15));
    render(&mut panel, 80, 10);
    let max = panel.max_scroll();

    panel.scroll(-3);
    assert_eq!(panel.scroll_top, max);
}

#[test_case("bash", 1, 1 ; "known_tool_creates_message")]
#[test_case("nonexistent_tool", 1, 1 ; "unknown_tool_accepted")]
fn tool_pending(tool: &str, expected_msgs: usize, expected_in_progress: usize) {
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.tool_pending("t1".into(), tool);
    assert_eq!(panel.messages.len(), expected_msgs);
    assert_eq!(panel.in_progress_count(), expected_in_progress);
}

#[test]
fn tool_start_upgrades_pending_in_place() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
fn extract_fully_selected_message_copies_raw_text() {
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "some **markdown** text".into(),
    ));
    render(&mut panel, 80, 24);

    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (0, 0), ((total - 1) as u32, 79));
    let text = panel.extract_selection_text(&sel, area);

    assert_eq!(text, "some **markdown** text");
}

#[test]
fn extract_fully_selected_tool_copies_raw_output() {
    let mut panel = MessagesPanel::new(UiConfig::default());
    let table = "| a | b |\n|---|---|\n| 1 | 2 |";
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Markdown(table.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (0, 0), ((total - 1) as u32, 79));
    let text = panel.extract_selection_text(&sel, area);

    assert!(
        text.contains("| a | b |"),
        "expected raw markdown table, got: {text:?}"
    );
    assert!(
        !text.contains('─'),
        "copied text should not contain rendered table borders: {text:?}"
    );
}

#[test]
fn extract_partial_last_line_truncated() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(23, area));
}

#[test]
fn handle_click_on_done_tool_records_click_row() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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

    let mut panel = MessagesPanel::new(UiConfig::default());
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

    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&preview));
    panel.register_live_buf("t1".into(), Arc::clone(&handler));
    panel.poll_live_bufs();

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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    let buf = finish_with_live_buf(&mut panel, "t1", "before", false);
    assert!(panel.watching("t1"));
    assert!(
        !panel.is_animating(),
        "watched bufs must not keep the UI animating"
    );

    buf.set_lines(vec![snap_line("after")]);
    panel.poll_live_bufs();
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
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
    panel.poll_live_bufs();
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
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
/// cancel path itself must retire live bufs: no `is_animating` pin, and
/// the tool stays clickable through the warm path.
#[test]
fn cancel_in_progress_retires_live_buf_to_watched() {
    let (eh, probe) = maki_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    let buf = Arc::new(maki_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("body")]);
    let mut ev = start("t1", BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    panel.cancel_in_progress();
    assert!(
        !panel.is_animating(),
        "cancel must not leak live bufs that pin animation"
    );
    assert!(panel.watching("t1"));

    buf.set_lines(vec![snap_line("after-cancel")]);
    panel.poll_live_bufs();
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
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
    panel.poll_live_bufs();
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    buf.append(snap_line("first"));
    panel.poll_live_bufs();
    panel.poll_live_bufs();

    buf.append(snap_line("second"));
    panel.poll_live_bufs();

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    let snapshot = msg.render_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.lines.len(), 2);
}

#[test]
fn tool_done_without_live_buf_preserves_existing_snapshot() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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

    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    panel.set_restore_channel(
        Some(maki_lua::EventHandle::disconnected_for_test()),
        Some(test_event_sender()),
    );

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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig::default());
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_header_snapshot("t1", rendered_snapshot(), Some(5));

    assert_eq!(panel.snapshot_gen_of("t1"), Some(5), "{HEADER_GEN_MSG}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(msg.render_header.is_some(), "render_header must be set");
}

#[test]
fn live_snapshot_uses_panel_generation() {
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
    let mut panel = MessagesPanel::new(UiConfig::default());
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
    let mut panel = MessagesPanel::new(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
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
