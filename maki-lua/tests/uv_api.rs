use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use maki_agent::tools::ToolRegistry;
use maki_lua::{Permission, PluginHost, PluginPermissions};

const PERMISSION_DENIED_SUBSTR: &str = "permission denied";
const CWD: &str = "maki.uv.cwd()";
const HOMEDIR: &str = "maki.uv.os_homedir()";
const GETENV: &str = r#"maki.uv.os_getenv("HOME")"#;
const ECHO_PAYLOAD: &str = "ping";
const ECHO_EXPECTED: &str = "ping|true|";
const ECHO_NEVER_DONE: &str = "the echo never completed";
const TICKS_NEVER: &str = "the timer never ticked";
const TICKS_FROZE: &str = "the timer kept ticking after stop";
const TICK_MS: u64 = 20;
const TICKS_NEEDED: u64 = 3;
const SETTLE: Duration = Duration::from_millis(120);
/// Every wait ends on an event, never on the clock, so only an already
/// failing test pays this.
const TEST_DEADLINE: Duration = Duration::from_secs(30);
const TEST_POLL: Duration = Duration::from_millis(10);
const PIPELINE_EXPECTED: &[u8] = b"ABBCCC";
const PIPELINE_REPORT: &str = "true|true|";
const PIPELINE_NEVER: &str = "the pipelined writes never completed";
const FLUSH_EXPECTED: &[u8] = b"bye\nlater";
const FLUSH_REPORT: &str = "true|EPIPE|";
const FLUSH_NEVER: &str = "the flushed shutdown never completed";
const SHUT_TWICE_REPORT: &str = "true|EPIPE|ENOTCONN|";
const SHUT_TWICE_NEVER: &str = "the second shutdown never ran";
const TRUE: &str = "true";
const FALSE: &str = "false";
const REPLAY_FIRST: &[u8] = b"AAA";
const REPLAY_SECOND: &[u8] = b"BBB";
const REPLAY_REPORT: &str = "AAABBB|true|";
const REPLAY_NEVER_STOPPED: &str = "the reader never stopped on the first chunk";
const REPLAY_NEVER_QUIET: &str = "the stream never ended while delivery was disarmed";
const REPLAY_NEVER_DONE: &str = "the buffered chunks never replayed";
const NAME_NEVER_ECHOED: &str = "the name connect never echoed";
const REFUSED_NEVER_REACHED: &str = "the refused name never reached the callback";
const CLOSE_REPORT: &str = "true|true|false";
const CLOSE_NEVER: &str = "the close callback never ran";
/// Long enough that the timer under test can only end by being closed.
const NEVER_MS: u64 = 10 * 60 * 1000;
const ALREADY_NAME: &str = "EALREADY";
const ALREADY_MSG: &str = "stream is already being read";
const BAD_HANDLE_NAME: &str = "EBADF";

/// `net.allowed_private_hosts` is a process global; tests that set or read it
/// serialize on this lock, because plain `cargo test` runs them on threads.
static ALLOWLIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn allowlist_guard() -> std::sync::MutexGuard<'static, ()> {
    ALLOWLIST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn setup() -> PluginHost {
    let reg = Arc::new(ToolRegistry::new());
    PluginHost::new(reg).unwrap()
}

#[test]
fn os_getenv_returns_nil_for_missing_var() {
    let host = setup();
    host.load_source(
        "getenv_missing",
        r#"
        local val = maki.uv.os_getenv("MAKI_TEST_VAR_DOES_NOT_EXIST_12345")
        assert(val == nil, "unset var should return nil, got: " .. tostring(val))
        "#,
    )
    .unwrap();
}

fn load_with(permission: Permission, chunk: &str) -> Result<(), maki_lua::PluginError> {
    let mut perms = PluginPermissions::denied();
    perms.set(permission, true);
    setup().load_source_with_permissions("uv_perm", chunk, perms)
}

#[test_case::test_case(Permission::FsRead, CWD ; "fs_read_cwd")]
#[test_case::test_case(Permission::FsRead, HOMEDIR ; "fs_read_homedir")]
#[test_case::test_case(Permission::Env, GETENV ; "env_getenv")]
fn the_permission_the_call_needs_is_enough(permission: Permission, call: &str) {
    let chunk = format!(
        r#"local value = {call}
        assert(type(value) == "string", "expected a string, got: " .. tostring(value))"#
    );
    load_with(permission, &chunk).unwrap();
}

/// Asking where a file lives must not cost a plugin the key to every secret in
/// the environment, so the two guards do not stand in for each other.
#[test_case::test_case(Permission::Env, CWD, Permission::FsRead ; "env_alone_misses_cwd")]
#[test_case::test_case(Permission::Env, HOMEDIR, Permission::FsRead ; "env_alone_misses_homedir")]
#[test_case::test_case(Permission::FsRead, GETENV, Permission::Env ; "fs_read_alone_misses_getenv")]
fn a_neighbouring_permission_does_not_carry_over(held: Permission, call: &str, needed: Permission) {
    let err = load_with(held, call)
        .expect_err("the guarded call must fail")
        .to_string();
    assert!(err.contains(PERMISSION_DENIED_SUBSTR), "got: {err}");
    assert!(err.contains(&format!("'{needed}'")), "got: {err}");
}

fn poll_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + TEST_DEADLINE;
    loop {
        if let Some(got) = check() {
            return got;
        }
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(TEST_POLL);
    }
}

fn exec_tool(reg: &ToolRegistry, name: &str) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry
        .tool
        .parse(&serde_json::json!({}))
        .expect("parse failed");
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    match smol::block_on(async { inv.execute(&ctx).await }).output? {
        maki_agent::ToolOutput::Plain(out) => Ok(out.text),
        other => Err(format!("unexpected output: {other:?}")),
    }
}

#[test]
fn tcp_echo_flows_through_the_real_pump() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            if let Ok(n) = stream.read(&mut buf) {
                let _ = stream.write_all(&buf[..n]);
            }
        }
    });
    maki_lua::set_allowed_private_hosts(&[format!("127.0.0.1:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "echo_client",
        &format!(
            r#"
            local state = {{ data = "", eof = false, err = "" }}
            maki.api.register_tool({{
                name = "echo_connect",
                description = "connects, writes, reads the echo",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    local tcp = maki.uv.new_tcp()
                    tcp:connect("127.0.0.1", {port}, function(err)
                        if err then
                            state.err = err
                            return
                        end
                        tcp:read_start(function(err, data)
                            if err then
                                state.err = err
                            elseif data then
                                state.data = state.data .. data
                            else
                                state.eof = true
                            end
                        end)
                        tcp:write("{payload}")
                    end)
                    return "started"
                end,
            }})
            maki.api.register_tool({{
                name = "echo_report",
                description = "what the echo callbacks have seen",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return state.data .. "|" .. tostring(state.eof) .. "|" .. state.err
                end,
            }})
            "#,
            port = addr.port(),
            payload = ECHO_PAYLOAD,
        ),
    )
    .unwrap();
    assert_eq!(exec_tool(&reg, "echo_connect").unwrap(), "started");
    poll_until(ECHO_NEVER_DONE, || {
        (exec_tool(&reg, "echo_report").unwrap() == ECHO_EXPECTED).then_some(())
    });
}

#[test]
fn pipelined_writes_reach_the_wire_in_order_with_their_callbacks() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (wire_tx, wire_rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut received = vec![0u8; PIPELINE_EXPECTED.len()];
            if stream.read_exact(&mut received).is_ok() {
                let _ = wire_tx.send(received);
            }
        }
    });
    maki_lua::set_allowed_private_hosts(&[format!("127.0.0.1:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "pipeline_client",
        &format!(
            r#"
            local state = {{ wrote2 = false, wrote3 = false, err = "" }}
            maki.api.register_tool({{
                name = "pipeline_connect",
                description = "connects and pipelines three writes",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    local tcp = maki.uv.new_tcp()
                    tcp:connect("127.0.0.1", {port}, function(err)
                        if err then
                            state.err = err
                            return
                        end
                        tcp:write("A")
                        tcp:write("BB", function(err2)
                            state.wrote2 = err2 == nil
                        end)
                        tcp:write("CCC", function(err3)
                            state.wrote3 = err3 == nil
                        end)
                    end)
                    return "started"
                end,
            }})
            maki.api.register_tool({{
                name = "pipeline_report",
                description = "what the write callbacks have seen",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return tostring(state.wrote2) .. "|" .. tostring(state.wrote3) .. "|" .. state.err
                end,
            }})
            "#,
            port = addr.port(),
        ),
    )
    .unwrap();
    assert_eq!(exec_tool(&reg, "pipeline_connect").unwrap(), "started");
    poll_until(PIPELINE_NEVER, || {
        (exec_tool(&reg, "pipeline_report").unwrap() == PIPELINE_REPORT).then_some(())
    });
    let received = wire_rx.recv_timeout(TEST_DEADLINE).expect(PIPELINE_NEVER);
    assert_eq!(received, PIPELINE_EXPECTED);
}

#[test]
fn shutdown_flushes_queued_writes_before_fin() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (wire_tx, wire_rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut received = Vec::new();
            if stream.read_to_end(&mut received).is_ok() {
                let _ = wire_tx.send(received);
            }
        }
    });
    maki_lua::set_allowed_private_hosts(&[format!("127.0.0.1:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "flush_client",
        &format!(
            r#"
            local state = {{ done = false, late = "pending", err = "" }}
            maki.api.register_tool({{
                name = "flush_connect",
                description = "writes two chunks, then half-closes",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    local tcp = maki.uv.new_tcp()
                    tcp:connect("127.0.0.1", {port}, function(err)
                        if err then
                            state.err = err
                            return
                        end
                        tcp:write("bye\n")
                        tcp:write("later")
                        tcp:shutdown(function(serr)
                            state.done = serr == nil
                            local _, _, wname = tcp:write("late")
                            state.late = tostring(wname or "none")
                            tcp:close()
                        end)
                    end)
                    return "started"
                end,
            }})
            maki.api.register_tool({{
                name = "flush_report",
                description = "what the shutdown callback has seen",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return tostring(state.done) .. "|" .. state.late .. "|" .. state.err
                end,
            }})
            "#,
            port = addr.port(),
        ),
    )
    .unwrap();
    assert_eq!(exec_tool(&reg, "flush_connect").unwrap(), "started");
    poll_until(FLUSH_NEVER, || {
        (exec_tool(&reg, "flush_report").unwrap() == FLUSH_REPORT).then_some(())
    });
    let received = wire_rx.recv_timeout(TEST_DEADLINE).expect(FLUSH_NEVER);
    assert_eq!(received, FLUSH_EXPECTED);
}

#[test]
fn a_second_shutdown_fails_with_enotconn() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    maki_lua::set_allowed_private_hosts(&[format!("127.0.0.1:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "shut_twice_client",
        &format!(
            r#"
            local state = {{ first = false, write = "pending", second = "", err = "" }}
            maki.api.register_tool({{
                name = "shut_twice_connect",
                description = "shuts down twice back to back",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    local tcp = maki.uv.new_tcp()
                    tcp:connect("127.0.0.1", {port}, function(err)
                        if err then
                            state.err = err
                            return
                        end
                        state.first = tcp:shutdown(function() end) == 0
                        local okw, _, namew = tcp:write("late")
                        state.write = tostring(namew or okw)
                        local _, _, name = tcp:shutdown(function() end)
                        state.second = tostring(name)
                        tcp:close()
                    end)
                    return "started"
                end,
            }})
            maki.api.register_tool({{
                name = "shut_twice_report",
                description = "what the double shutdown produced",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return tostring(state.first) .. "|" .. state.write .. "|" .. state.second .. "|" .. state.err
                end,
            }})
            "#,
            port = addr.port(),
        ),
    )
    .unwrap();
    assert_eq!(exec_tool(&reg, "shut_twice_connect").unwrap(), "started");
    poll_until(SHUT_TWICE_NEVER, || {
        (exec_tool(&reg, "shut_twice_report").unwrap() == SHUT_TWICE_REPORT).then_some(())
    });
}

#[test]
fn timer_ticks_flow_through_the_real_pump_and_stop() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "ticking",
        &format!(
            r#"
            local ticks = 0
            local timer = maki.uv.new_timer()
            timer:start({tick}, {tick}, function() ticks = ticks + 1 end)
            maki.api.register_tool({{
                name = "tick_report",
                description = "how many ticks",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function() return tostring(ticks) end,
            }})
            maki.api.register_tool({{
                name = "tick_stop",
                description = "stops the timer",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    assert(timer:stop() == 0, "stop must succeed")
                    return "stopped"
                end,
            }})
            "#,
            tick = TICK_MS,
        ),
    )
    .unwrap();
    poll_until(TICKS_NEVER, || {
        let ticks: u64 = exec_tool(&reg, "tick_report").unwrap().parse().unwrap();
        (ticks >= TICKS_NEEDED).then_some(())
    });
    assert_eq!(exec_tool(&reg, "tick_stop").unwrap(), "stopped");
    let frozen = exec_tool(&reg, "tick_report").unwrap();
    std::thread::sleep(SETTLE);
    assert_eq!(
        exec_tool(&reg, "tick_report").unwrap(),
        frozen,
        "{TICKS_FROZE}"
    );
}

#[test]
fn connect_denied_without_net_permission() {
    // Trusted minus net isolates the one gate under test.
    let mut perms = PluginPermissions::trusted();
    perms.set(Permission::Net, false);
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source_with_permissions(
        "uv_perm",
        r#"
        local tcp = maki.uv.new_tcp()
        local ok, err, name = tcp:connect("127.0.0.1", 9, function() end)
        assert(ok == nil, "connect must fail without the net permission")
        assert(name == "EACCES", "expected EACCES, got: " .. tostring(name))
        assert(tostring(err):find("permission denied", 1, true), "got: " .. tostring(err))
        "#,
        perms,
    )
    .unwrap();
}

#[test]
fn connect_denied_without_allowlist_entry() {
    let _allowlist = allowlist_guard();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    maki_lua::set_allowed_private_hosts(&[]);
    host.load_source(
        "uv_allowlist",
        r#"
        local tcp = maki.uv.new_tcp()
        local ok, err, name = tcp:connect("127.0.0.1", 9, function() end)
        assert(ok == nil, "loopback must be blocked without an allowlist entry")
        assert(name == "EACCES", "expected EACCES, got: " .. tostring(name))
        "#,
    )
    .unwrap();
}

/// A name resolves and gets vetted inside the connect task, like luv's
/// hostname support: an allowlisted name dials, a refused one reports through
/// the callback instead of at the call site.
#[test]
fn name_connect_reports_the_guard_verdict_in_the_callback() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4];
            if let Ok(n) = stream.read(&mut buf) {
                let _ = stream.write_all(&buf[..n]);
            }
        }
    });
    maki_lua::set_allowed_private_hosts(&[format!("localhost:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "name_client",
        &format!(
            r#"
            local state = {{ err = "unset", echoed = false }}
            local tcp = maki.uv.new_tcp("inet")
            tcp:connect("localhost", {port}, function(err)
                state.err = err or "nil"
                if err then
                    return
                end
                tcp:write("ping", function(werr)
                    if werr then
                        state.err = werr
                        return
                    end
                    tcp:read_start(function(rerr, chunk)
                        if rerr then
                            state.err = rerr
                        elseif chunk == "ping" then
                            state.echoed = true
                        end
                    end)
                end)
            end)
            maki.api.register_tool({{
                name = "name_report",
                description = "what the name connect has seen",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return state.err .. "|" .. tostring(state.echoed)
                end,
            }})
            "#,
            port = addr.port(),
        ),
    )
    .unwrap();
    poll_until(NAME_NEVER_ECHOED, || {
        (exec_tool(&reg, "name_report").unwrap() == "nil|true").then_some(())
    });

    maki_lua::set_allowed_private_hosts(&[]);
    host.load_source(
        "refused_name_client",
        r#"
        local state = { err = "unset" }
        local tcp = maki.uv.new_tcp()
        tcp:connect("localhost", 1, function(err)
            state.err = err or "nil"
        end)
        maki.api.register_tool({
            name = "refused_report",
            description = "what the refused name connect has seen",
            schema = { type = "object", properties = {}, additionalProperties = false },
            audiences = { "main" },
            handler = function() return state.err end,
        })
        "#,
    )
    .unwrap();
    poll_until(REFUSED_NEVER_REACHED, || {
        (exec_tool(&reg, "refused_report")
            .unwrap()
            .starts_with("EACCES"))
        .then_some(())
    });
}

/// A `read_stop` only disarms delivery: the reader keeps draining, so chunks
/// and the end of the stream that arrive meanwhile replay in order on the next
/// `read_start`.
#[test]
fn chunks_and_eof_buffered_while_disarmed_replay_on_the_next_read_start() {
    let _allowlist = allowlist_guard();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (go_tx, go_rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(REPLAY_FIRST);
            if go_rx.recv().is_ok() {
                let _ = stream.write_all(REPLAY_SECOND);
            }
        }
    });
    maki_lua::set_allowed_private_hosts(&[format!("127.0.0.1:{}", addr.port())]);
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "replay_client",
        &format!(
            r#"
            local state = {{ data = "", eof = false, stopped = false, err = "" }}
            local tcp = maki.uv.new_tcp()
            local function on_chunk(err, data)
                if err then
                    state.err = err
                elseif data then
                    state.data = state.data .. data
                    if not state.stopped then
                        state.stopped = true
                        tcp:read_stop()
                    end
                else
                    state.eof = true
                end
            end
            tcp:connect("127.0.0.1", {port}, function(err)
                if err then
                    state.err = err
                    return
                end
                tcp:read_start(on_chunk)
            end)
            maki.api.register_tool({{
                name = "replay_stopped",
                description = "whether the first chunk disarmed delivery",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function() return tostring(state.stopped) end,
            }})
            maki.api.register_tool({{
                name = "replay_active",
                description = "whether the handle is still busy",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function() return tostring(tcp:is_active()) end,
            }})
            maki.api.register_tool({{
                name = "replay_resume",
                description = "re-arms delivery",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    assert(tcp:read_start(on_chunk) == 0, "re-arming must succeed")
                    return "resumed"
                end,
            }})
            maki.api.register_tool({{
                name = "replay_report",
                description = "what the read callbacks have seen",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return state.data .. "|" .. tostring(state.eof) .. "|" .. state.err
                end,
            }})
            "#,
            port = addr.port(),
        ),
    )
    .unwrap();
    poll_until(REPLAY_NEVER_STOPPED, || {
        (exec_tool(&reg, "replay_stopped").unwrap() == TRUE).then_some(())
    });
    go_tx.send(()).unwrap();
    // The reader task outlives the disarm, so the handle only goes quiet once
    // it has taken the rest of the stream and the end of it off the wire.
    poll_until(REPLAY_NEVER_QUIET, || {
        (exec_tool(&reg, "replay_active").unwrap() == FALSE).then_some(())
    });
    assert_eq!(exec_tool(&reg, "replay_resume").unwrap(), "resumed");
    poll_until(REPLAY_NEVER_DONE, || {
        (exec_tool(&reg, "replay_report").unwrap() == REPLAY_REPORT).then_some(())
    });
}

/// `close` reports through its callback once, and the handle counts as closing
/// from the call itself, not from when the pump gets there.
#[test]
fn close_silences_an_armed_timer_and_reports_once() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "closing_timer",
        &format!(
            r#"
            local closes = 0
            local timer = maki.uv.new_timer()
            timer:start({never}, 0, function() end)
            assert(timer:is_active(), "an armed timer is active")
            maki.api.register_tool({{
                name = "close_timer",
                description = "closes the timer",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    timer:close(function() closes = closes + 1 end)
                    timer:close(function() closes = closes + 1 end)
                    return tostring(timer:is_closing())
                end,
            }})
            maki.api.register_tool({{
                name = "close_report",
                description = "what the close produced",
                schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
                audiences = {{ "main" }},
                handler = function()
                    return tostring(closes == 1) .. "|" .. tostring(timer:is_closing())
                        .. "|" .. tostring(timer:is_active())
                end,
            }})
            "#,
            never = NEVER_MS,
        ),
    )
    .unwrap();
    assert_eq!(exec_tool(&reg, "close_timer").unwrap(), TRUE);
    poll_until(CLOSE_NEVER, || {
        (exec_tool(&reg, "close_report").unwrap() == CLOSE_REPORT).then_some(())
    });
    std::thread::sleep(SETTLE);
    assert_eq!(exec_tool(&reg, "close_report").unwrap(), CLOSE_REPORT);
}

/// Reading is armed before the socket exists, so the guards that reject a
/// second `read_start` and a closed handle apply there too.
#[test]
fn read_start_guards_hold_before_the_socket_is_up() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source(
        "read_guards",
        &format!(
            r#"
            local tcp = maki.uv.new_tcp()
            assert(tcp:nodelay(true) == 0, "nodelay is allowed before connect")
            assert(tcp:read_start(function() end) == 0, "arming before connect must succeed")
            local ok, err, name = tcp:read_start(function() end)
            assert(ok == nil, "a second read_start must fail")
            assert(name == "{already}", "expected {already}, got: " .. tostring(name))
            assert(tostring(err):find("{already_msg}", 1, true), "got: " .. tostring(err))
            assert(not tcp:is_active(), "a handle with no socket is not active")
            tcp:close()
            local ok2, _, name2 = tcp:read_start(function() end)
            assert(ok2 == nil, "read_start on a closing handle must fail")
            assert(name2 == "{bad}", "expected {bad}, got: " .. tostring(name2))
            "#,
            already = ALREADY_NAME,
            already_msg = ALREADY_MSG,
            bad = BAD_HANDLE_NAME,
        ),
    )
    .unwrap();
}
