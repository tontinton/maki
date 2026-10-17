use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::cancel::CancelToken;
use maki_agent::tools::ToolRegistry;
#[cfg(target_os = "linux")]
use rustix::event::{PollFd, PollFlags, Timespec, poll};
#[cfg(target_os = "linux")]
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use test_case::test_case;

use super::{CodeBlockRender, EventHandle, PluginHost};
use crate::api::slot::CODE_BLOCK_SLOT;
use crate::api::util::command::{UiAction, UiReply};
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::{MAX_CODE_BLOCK_SOURCE_BYTES, Request};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const LANGUAGE: &str = "mermaid";
const SOURCE: &str = "graph TD\nA --> B";
const WIDTH: u16 = 80;
const RENDERER: &str = "renderer";
const PARKED_BODY: &str = "maki.ui.input(); return { 'resumed' }";

fn host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
}

fn layer_source(body: &str) -> String {
    format!("maki.api.set_slot('{CODE_BLOCK_SLOT}', function(prev, ev) {body} end)")
}

fn layer(host: &PluginHost, name: &str, body: &str) {
    host.load_source(name, &layer_source(body)).unwrap();
}

fn submit(host: &PluginHost) -> CodeBlockRender {
    host.event_handle()
        .render_code_block(LANGUAGE.to_owned(), SOURCE.to_owned(), WIDTH)
}

fn answer(request: &CodeBlockRender) -> Option<Vec<String>> {
    request.reply.recv_timeout(TEST_TIMEOUT).unwrap()
}

fn parked_input(host: &PluginHost) -> flume::Sender<UiReply> {
    let UiAction::Input { reply_tx, .. } = host
        .ui_action_rx()
        .recv_timeout(TEST_TIMEOUT)
        .expect("the renderer must reach its input barrier")
    else {
        panic!("expected the renderer input barrier")
    };
    reply_tx
}

#[test]
fn code_block_event_and_literal_lines_reach_an_unprivileged_layer() {
    let host = host();
    host.load_source_with_permissions(
        RENDERER,
        &layer_source(
            r#"assert(ev.language == 'mermaid')
assert(ev.code == 'graph TD\nA --> B')
assert(ev.width == 80)
return { '  ┌──┐', '  │界│', '  └──┘' }"#,
        ),
        PluginPermissions::denied(),
    )
    .unwrap();

    assert_eq!(
        answer(&submit(&host)),
        Some(vec!["  ┌──┐".into(), "  │界│".into(), "  └──┘".into()])
    );
}

#[test]
fn display_slot_does_not_grant_process_permission() {
    let host = host();
    host.load_source_with_permissions(
        RENDERER,
        &layer_source(
            r#"local ok = pcall(maki.fn.jobstart, { 'true' })
assert(not ok, 'display layers must retain their permission limits')
return { 'safe' }"#,
        ),
        PluginPermissions::denied(),
    )
    .unwrap();
    assert_eq!(answer(&submit(&host)), Some(vec!["safe".into()]));
}

#[test]
fn code_block_layers_chain_in_registration_order() {
    let host = host();
    layer(&host, "inner", "return { 'inner' }");
    layer(&host, "outer", "return { 'outer:' .. prev(ev)[1] }");
    layer(&host, "defer", "return prev(ev)");
    assert_eq!(answer(&submit(&host)), Some(vec!["outer:inner".into()]));
}

#[test]
fn a_broken_layer_defers_to_the_remaining_chain() {
    let host = host();
    layer(&host, "inner", "return { 'inner' }");
    layer(&host, "broken", "error('broken renderer')");
    assert_eq!(answer(&submit(&host)), Some(vec!["inner".into()]));
}

#[test_case("return prev(ev)"; "deferred")]
#[test_case("return nil"; "declined")]
#[test_case("error('broken renderer')"; "error")]
#[test_case("return {}"; "empty")]
#[test_case("return { [1] = 'a', [3] = 'b' }"; "sparse")]
#[test_case("return { 'a', other = 'b' }"; "named_key")]
#[test_case("return { [0] = 'a', 'b' }"; "zero_key")]
#[test_case("return { 42 }"; "non_string_line")]
#[test_case("return 'text'"; "non_table")]
#[test_case("return { 'line\\nline' }"; "newline")]
#[test_case("return { 'tab\\tline' }"; "tab")]
#[test_case("return { string.char(27) .. '[31mred' }"; "escape")]
#[test_case("return { string.char(127) }"; "delete_control")]
#[test_case("return { string.char(255) }"; "invalid_utf8")]
#[test_case("return { string.rep('界', 41) }"; "display_cell_width")]
#[test_case("return table.create(257, '')"; "too_many_lines")]
fn declined_or_invalid_code_block_output_keeps_the_source(body: &str) {
    let host = host();
    layer(&host, RENDERER, body);
    assert_eq!(answer(&submit(&host)), None);
}

#[test]
fn code_block_output_accepts_the_exact_resource_limits() {
    let host = host();
    layer(
        &host,
        RENDERER,
        "return table.create(256, string.rep('a', 256))",
    );
    let request = host
        .event_handle()
        .render_code_block(LANGUAGE.into(), SOURCE.into(), 256);
    assert_eq!(answer(&request), Some(vec!["a".repeat(256); 256]));
}

#[test]
fn code_block_byte_limit_applies_even_when_each_line_fits() {
    let host = host();
    layer(
        &host,
        RENDERER,
        "return table.create(256, string.rep('a', 257))",
    );
    let request = host
        .event_handle()
        .render_code_block(LANGUAGE.into(), SOURCE.into(), 257);
    assert_eq!(answer(&request), None);
}

#[test]
fn zero_width_and_large_sources_are_declined_before_submission() {
    let (tx, rx) = flume::unbounded();
    let events = EventHandle::probed_for_test(tx).layering(&[CODE_BLOCK_SLOT]);
    for (code, width) in [
        (SOURCE.to_owned(), 0),
        ("a".repeat(MAX_CODE_BLOCK_SOURCE_BYTES + 1), WIDTH),
    ] {
        let request = events.render_code_block(LANGUAGE.into(), code, width);
        assert_eq!(request.try_recv(), Ok(None));
        assert!(rx.is_empty());
    }
}

#[test]
fn the_exact_source_limit_reaches_the_renderer() {
    let host = host();
    layer(
        &host,
        RENDERER,
        &format!("assert(#ev.code == {MAX_CODE_BLOCK_SOURCE_BYTES}); return {{ 'accepted' }}"),
    );
    let request = host.event_handle().render_code_block(
        LANGUAGE.into(),
        "a".repeat(MAX_CODE_BLOCK_SOURCE_BYTES),
        WIDTH,
    );
    assert_eq!(answer(&request), Some(vec!["accepted".into()]));
}

#[test]
fn a_parked_renderer_does_not_block_the_request_dispatcher() {
    let host = host();
    layer(&host, RENDERER, PARKED_BODY);
    let request = submit(&host);
    let reply = parked_input(&host);

    host.event_handle().collect_prompt_slots();
    reply.send(Ok(serde_json::Value::Null)).unwrap();
    assert_eq!(answer(&request), Some(vec!["resumed".into()]));
}

#[test]
fn dropping_a_request_cancels_its_parked_callback() {
    let host = host();
    layer(&host, RENDERER, PARKED_BODY);
    let request = submit(&host);
    let reply = parked_input(&host);

    drop(request);
    host.unload(RENDERER).unwrap();
    assert!(
        reply.is_disconnected(),
        "cancel must drop the parked future"
    );
}

#[test]
fn a_parked_renderer_times_out_and_releases_its_callback() {
    let host = host();
    layer(&host, RENDERER, PARKED_BODY);
    let request = submit(&host);
    let reply = parked_input(&host);

    assert_eq!(answer(&request), None);
    assert!(
        reply.is_disconnected(),
        "timeout must drop the parked future"
    );
}

#[test]
fn the_watchdog_stops_a_renderer_that_does_not_yield() {
    let host = host();
    layer(&host, RENDERER, "while true do end");
    assert_eq!(answer(&submit(&host)), None);
    host.event_handle().collect_prompt_slots();
}

#[test]
fn an_expired_queued_request_never_enters_the_callback() {
    let host = host();
    layer(
        &host,
        RENDERER,
        "maki.notify('unexpected'); return { 'late' }",
    );
    let (reply, rx) = flume::bounded(1);
    host.inner
        .tx
        .send(Request::RenderCodeBlock {
            language: LANGUAGE.into(),
            code: SOURCE.into(),
            width: WIDTH,
            generation: host.event_handle().code_block_generation(),
            cancel: CancelToken::none(),
            deadline: Instant::now(),
            reply,
        })
        .unwrap();
    assert_eq!(rx.recv_timeout(TEST_TIMEOUT).unwrap(), None);
    assert!(host.ui_action_rx().is_empty());
}

#[test]
fn slot_changes_invalidate_an_already_completed_reply() {
    let host = host();
    let events = host.event_handle();
    assert!(!events.code_block_layered());
    let initial_generation = events.code_block_generation();
    layer(
        &host,
        RENDERER,
        "maki.ui.flash('completed'); return { 'old' }",
    );
    assert!(events.code_block_layered());
    assert!(events.code_block_generation() > initial_generation);
    let request = submit(&host);
    assert!(matches!(
        host.ui_action_rx().recv_timeout(TEST_TIMEOUT).unwrap(),
        UiAction::Flash(message) if message == "completed"
    ));
    host.unload(RENDERER).unwrap();

    assert!(!events.code_block_layered());
    assert_eq!(request.try_recv(), Ok(None));
    layer(&host, RENDERER, "return { 'new' }");
    assert_eq!(answer(&submit(&host)), Some(vec!["new".into()]));
}

#[test]
fn reload_drains_a_pending_renderer_and_prevents_its_old_reply() {
    let host = host();
    layer(&host, RENDERER, PARKED_BODY);
    let request = submit(&host);
    let reply = parked_input(&host);

    layer(&host, RENDERER, "return { 'new' }");
    assert!(reply.is_disconnected());
    assert_eq!(request.try_recv(), Ok(None));
    assert_eq!(answer(&submit(&host)), Some(vec!["new".into()]));
}

#[cfg(target_os = "linux")]
#[test_case("completed")]
#[test_case("cancelled")]
#[test_case("timed_out")]
#[test_case("reloaded")]
fn renderer_task_processes_end_with_the_request(outcome: &str) {
    let host = host();
    layer(
        &host,
        RENDERER,
        r#"local job = assert(maki.fn.jobstart({ 'sleep', '30' }))
local info = assert(maki.fn.jobinfo(job))
maki.ui.flash(tostring(info.pid))
maki.ui.input()
return { 'done' }"#,
    );
    let request = submit(&host);
    let UiAction::Flash(pid) = host.ui_action_rx().recv_timeout(TEST_TIMEOUT).unwrap() else {
        panic!("the renderer must identify its task process")
    };
    let pid = Pid::from_raw(pid.parse().unwrap()).unwrap();
    let process = pidfd_open(pid, PidfdFlags::empty()).unwrap();
    let reply = parked_input(&host);
    let mut process_exit = [PollFd::new(&process, PollFlags::IN)];
    assert_eq!(
        poll(&mut process_exit, Some(&Timespec::default())).unwrap(),
        0
    );

    match outcome {
        "completed" => {
            reply.send(Ok(serde_json::Value::Null)).unwrap();
            assert_eq!(answer(&request), Some(vec!["done".into()]));
        }
        "cancelled" => {
            drop(request);
            host.unload(RENDERER).unwrap();
            assert!(reply.is_disconnected());
        }
        "timed_out" => {
            assert_eq!(answer(&request), None);
            assert!(reply.is_disconnected());
        }
        "reloaded" => {
            layer(&host, RENDERER, "return { 'new' }");
            assert_eq!(request.try_recv(), Ok(None));
            assert!(reply.is_disconnected());
        }
        _ => unreachable!(),
    }

    let timeout = Timespec::try_from(TEST_TIMEOUT).unwrap();
    assert_eq!(
        poll(&mut process_exit, Some(&timeout)).unwrap(),
        1,
        "the renderer task must kill its process"
    );
}

#[test]
fn no_layers_and_a_stopped_host_keep_the_source() {
    let host = host();
    assert_eq!(submit(&host).try_recv(), Ok(None));
    layer(&host, RENDERER, "return { 'rendered' }");
    let events = host.event_handle();
    drop(host);
    assert!(matches!(
        events
            .render_code_block(LANGUAGE.into(), SOURCE.into(), WIDTH)
            .try_recv(),
        Err(flume::TryRecvError::Disconnected)
    ));
}
