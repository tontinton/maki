use std::sync::Arc;
use std::thread;
use std::time::Duration;

use maki_agent::tools::ToolRegistry;
use test_case::test_case;

use super::{CodeBlockOutcome, CodeBlockRender, EventHandle, PluginHost};
use crate::api::slot::code_block_slot;
use crate::api::util::command::{UiAction, UiReply};
use crate::plugin_permissions::{Permission, PluginPermissions};
use crate::runtime::MAX_CODE_BLOCK_SOURCE_BYTES;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A budget no test outlives, so only a cancel or an unload can end the render.
const UNREACHED_BUDGET: Duration = Duration::from_secs(3600);
/// Requests the Lua thread answers in order, to let a cancelled render settle.
const BARRIERS: usize = 3;
const LANGUAGE: &str = "mermaid";
const SOURCE: &str = "graph TD\nA --> B";
const WIDTH: u16 = 80;
const RENDERER: &str = "renderer";
const OTHER_PLUGIN: &str = "other";
const PARKED_BODY: &str = "maki.ui.input(); return { 'resumed' }";
/// Parks with a cancel hook on its first run and reports the hook on its next.
const HOOKED_BODY: &str = "if parked then return { tostring(hooked) } end \
    parked = true maki.async.on_cancel(function() hooked = true end) \
    maki.ui.input() return { 'resumed' }";
const HOOK_RAN: &str = "true";
const DRAWN: &str = "drawn";
const UNLOAD_WAITED: &str = "unload must not wait for a parked render";

fn host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
}

fn layer_source(body: &str) -> String {
    format!(
        "maki.api.set_slot('{}', function(prev, ev) {body} end)",
        code_block_slot(LANGUAGE)
    )
}

fn layer(host: &PluginHost, body: &str) {
    host.load_source(RENDERER, &layer_source(body)).unwrap();
}

fn submit(host: &PluginHost, width: u16) -> CodeBlockRender {
    host.event_handle()
        .render_code_block(Arc::from(LANGUAGE), SOURCE.to_owned(), width)
}

fn submit_within(host: &PluginHost, budget: Duration) -> CodeBlockRender {
    host.event_handle().render_code_block_within(
        Arc::from(LANGUAGE),
        SOURCE.to_owned(),
        WIDTH,
        budget,
    )
}

fn answer(request: &CodeBlockRender) -> CodeBlockOutcome {
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

#[test_case(false, false ; "no_grants")]
#[test_case(false, true ; "network_access_does_not_grant_chat_access")]
#[test_case(true, false ; "session_read_is_sufficient")]
fn code_block_rendering_requires_session_read(can_read: bool, can_network: bool) {
    let host = host();
    let mut permissions = PluginPermissions::denied();
    permissions.set(Permission::SessionRead, can_read);
    permissions.set(Permission::Net, can_network);
    host.load_source_with_permissions(
        RENDERER,
        &layer_source(
            r#"assert(ev.language == 'mermaid' and ev.code == 'graph TD\nA --> B' and ev.width == 80)
return { '  ┌──┐', '  │界│', '  └──┘' }"#,
        ),
        permissions,
    )
    .unwrap();

    let expected = if can_read {
        CodeBlockOutcome::Rendered(vec!["  ┌──┐".into(), "  │界│".into(), "  └──┘".into()])
    } else {
        CodeBlockOutcome::Declined
    };
    assert_eq!(answer(&submit(&host, WIDTH)), expected);
}

#[test_case("ui.code_block"; "bare_slot")]
#[test_case("ui.code_block.*"; "wildcard_slot")]
#[test_case("ui.code_block.rust"; "another_language")]
fn unclaimed_languages_are_declined_before_submission(slot: &str) {
    let (tx, rx) = flume::unbounded();
    let events = EventHandle::probed_for_test(tx).layering(&[slot]);
    assert_eq!(events.claimed_code_block(LANGUAGE, SOURCE, WIDTH), None);
    let request = events.render_code_block(Arc::from(LANGUAGE), SOURCE.into(), WIDTH);
    assert_eq!(request.try_recv(), Ok(CodeBlockOutcome::Declined));
    assert!(rx.is_empty());
}

#[test_case("mermaid"; "exact")]
#[test_case("Mermaid"; "capitalised")]
#[test_case("mermaid title=\"Flow\""; "with_attributes")]
fn the_first_word_of_the_info_string_claims_a_language(info: &str) {
    let (tx, _rx) = flume::unbounded();
    let events = EventHandle::probed_for_test(tx).layering(&[&code_block_slot(LANGUAGE)]);
    assert_eq!(
        events.claimed_code_block(info, SOURCE, WIDTH).as_deref(),
        Some(LANGUAGE)
    );
}

#[test]
fn zero_width_and_large_sources_are_declined_before_submission() {
    let (tx, rx) = flume::unbounded();
    let events = EventHandle::probed_for_test(tx).layering(&[&code_block_slot(LANGUAGE)]);
    for (code, width) in [
        (SOURCE.to_owned(), 0),
        ("a".repeat(MAX_CODE_BLOCK_SOURCE_BYTES + 1), WIDTH),
    ] {
        let request = events.render_code_block(Arc::from(LANGUAGE), code, width);
        assert_eq!(request.try_recv(), Ok(CodeBlockOutcome::Declined));
        assert!(rx.is_empty());
    }
}

#[test_case("return prev(ev)"; "deferred")]
#[test_case("error('broken renderer')"; "error")]
#[test_case("return {}"; "empty")]
#[test_case("return { 42 }"; "non_string_line")]
#[test_case("return 'text'"; "non_table")]
#[test_case("return { string.char(27) .. '[31mred' }"; "control_character")]
#[test_case("return { string.char(255) }"; "invalid_utf8")]
#[test_case("return { string.rep('界', 41) }"; "display_cell_width")]
#[test_case("return table.create(257, '')"; "too_many_lines")]
fn declined_or_invalid_code_block_output_keeps_the_source(body: &str) {
    let host = host();
    layer(&host, body);
    assert_eq!(answer(&submit(&host, WIDTH)), CodeBlockOutcome::Declined);
}

#[test_case(256, true ; "at_the_limits")]
#[test_case(257, false ; "over_the_byte_limit")]
fn code_block_output_byte_limit(line_bytes: u16, accepted: bool) {
    let host = host();
    layer(
        &host,
        &format!("return table.create(256, string.rep('a', {line_bytes}))"),
    );
    let expected = if accepted {
        CodeBlockOutcome::Rendered(vec!["a".repeat(line_bytes.into()); 256])
    } else {
        CodeBlockOutcome::Declined
    };
    assert_eq!(answer(&submit(&host, line_bytes)), expected);
}

#[test]
fn a_parked_renderer_does_not_block_the_request_dispatcher() {
    let host = host();
    layer(&host, PARKED_BODY);
    let request = submit(&host, WIDTH);
    let reply = parked_input(&host);

    host.event_handle().collect_prompt_slots();
    reply.send(Ok(serde_json::Value::Null)).unwrap();
    assert_eq!(
        answer(&request),
        CodeBlockOutcome::Rendered(vec!["resumed".into()])
    );
}

#[test]
fn dropping_a_request_ends_its_parked_render_at_once() {
    let host = host();
    layer(&host, PARKED_BODY);
    let request = submit_within(&host, UNREACHED_BUDGET);
    let reply = parked_input(&host);

    drop(request);
    for _ in 0..BARRIERS {
        if reply.is_disconnected() {
            break;
        }
        host.event_handle().collect_prompt_slots();
    }
    assert!(
        reply.is_disconnected(),
        "cancel must drop the parked future"
    );
}

#[test]
fn a_cancelled_render_runs_its_cancel_hooks() {
    let host = host();
    layer(&host, HOOKED_BODY);
    let request = submit_within(&host, UNREACHED_BUDGET);
    let reply = parked_input(&host);

    drop(request);
    for _ in 0..BARRIERS {
        if reply.is_disconnected() {
            break;
        }
        host.event_handle().collect_prompt_slots();
    }
    assert_eq!(
        answer(&submit(&host, WIDTH)),
        CodeBlockOutcome::Rendered(vec![HOOK_RAN.into()])
    );
}

/// The watchdog grants a yielding task a grace past its deadline, so a render
/// that answered without being killed counts, however late it is collected.
#[test]
fn a_render_answered_after_its_deadline_is_kept() {
    let host = host();
    layer(&host, &format!("return {{ '{DRAWN}' }}"));
    assert_eq!(
        answer(&submit_within(&host, Duration::ZERO)),
        CodeBlockOutcome::Rendered(vec![DRAWN.into()])
    );
}

#[test]
fn a_parked_renderer_times_out_and_releases_its_callback() {
    let host = host();
    layer(&host, PARKED_BODY);
    let request = submit_within(&host, Duration::ZERO);
    let reply = parked_input(&host);

    assert_eq!(answer(&request), CodeBlockOutcome::TimedOut);
    assert!(
        reply.is_disconnected(),
        "timeout must drop the parked future"
    );
}

#[test]
fn a_renderer_that_never_yields_times_out() {
    let host = host();
    layer(&host, "while true do end");
    assert_eq!(
        answer(&submit_within(&host, Duration::ZERO)),
        CodeBlockOutcome::TimedOut
    );
}

#[test]
fn a_parked_render_does_not_hold_back_an_unload() {
    let host = Arc::new(host());
    layer(&host, PARKED_BODY);
    let _request = submit_within(&host, UNREACHED_BUDGET);
    let _reply = parked_input(&host);

    let (done, unloaded) = flume::bounded(1);
    let unloader = Arc::clone(&host);
    thread::spawn(move || done.send(unloader.unload(RENDERER).is_ok()));
    assert_eq!(
        unloaded.recv_timeout(TEST_TIMEOUT),
        Ok(true),
        "{UNLOAD_WAITED}"
    );
}

#[test]
fn the_code_block_revision_moves_only_with_code_block_layers() {
    let host = host();
    let events = host.event_handle();
    let initial = events.code_block_revision();
    host.load_source(
        OTHER_PLUGIN,
        "maki.api.set_slot('ui.plan_form', function(prev, ...) return prev(...) end)",
    )
    .unwrap();
    host.unload(OTHER_PLUGIN).unwrap();
    assert_eq!(events.code_block_revision(), initial);

    layer(&host, "return { 'diagram' }");
    let layered = events.code_block_revision();
    assert_ne!(layered, initial);
    host.unload(RENDERER).unwrap();
    assert_ne!(events.code_block_revision(), layered);
}
