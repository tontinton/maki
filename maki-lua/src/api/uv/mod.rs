use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult};

use crate::plugin_permissions::PluginPermissions;

mod store;
mod tcp;
mod timer;

pub(crate) use store::{UvEvent, UvStore, deliver_uv_event, with_uv};
pub(crate) use tcp::TCP_DOCS;
pub(crate) use timer::TIMER_DOCS;

#[allow(unused_imports)]
use self::tcp::{new_tcp__doc, new_tcp__register};
#[allow(unused_imports)]
use self::timer::{new_timer__doc, new_timer__register};

/// Closes every handle owned by a plugin. Called on plugin unload/reload, the
/// uv-side mirror of the job kill in `LuaRuntime::drop_plugin_keys`.
pub(crate) fn close_plugin_handles(lua: &Lua, name: &str) {
    with_uv(lua, |store| store.close_owner(lua, name));
}

/// Return the current working directory as an absolute path. Like `vim.uv.cwd`.
///
/// @return (string?) Current working directory, or nil if it cannot be determined.
/// @example
/// local cwd = maki.uv.cwd()
/// if cwd then print("working in: " .. cwd) end
#[lua_fn(guard = FsRead)]
fn cwd(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from)))
}

/// Return the current user's home directory. Like `vim.uv.os_homedir`.
///
/// @return (string?) Home directory path, or nil if it cannot be determined.
/// @example
/// local home = maki.uv.os_homedir() -- e.g. "/home/user"
#[lua_fn(guard = FsRead)]
fn os_homedir(_lua: &Lua) -> LuaResult<Option<String>> {
    Ok(maki_storage::paths::home().and_then(|p| p.to_str().map(String::from)))
}

/// Look up the environment variable {name}. Like `vim.uv.os_getenv`.
/// Returns nil when the variable is not set.
///
/// @param name string Name of the environment variable.
/// @return (string?) Variable value, or nil if not set.
/// @example
/// local editor = maki.uv.os_getenv("EDITOR") or "vi"
#[lua_fn(guard = Env)]
fn os_getenv(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    Ok(std::env::var(&name).ok())
}

lua_table! {
    /// System and environment utilities, modelled after `vim.uv`.
    ///
    /// Beyond the location queries, this namespace carries libuv-style tcp and
    /// timer handles: `new_tcp` and `new_timer` return handles whose methods
    /// (`:connect`, `:read_start`, `:write`, `:start`, ...) keep `vim.uv`
    /// signatures, so plugin code can be copy-pasted between Neovim and maki.
    /// Handle operations return `0` on success and `(nil, err, name)` on
    /// failure, and their callbacks receive errors err-first, exactly like
    /// `vim.uv`. Connecting is guarded like `maki.net.request`: the plugin
    /// needs `net`, and loopback or private targets additionally need an entry
    /// in `net.allowed_private_hosts`.
    ///
    /// Filesystem location queries (`cwd`, `os_homedir`) need `fs_read`, while
    /// `os_getenv` reads the process environment, where secrets live, so it
    /// needs `env`.
    ///
    /// ```lua
    /// local home = maki.uv.os_homedir()
    /// ```
    "maki.uv" => pub(crate) fn create_uv_table(plugin: Arc<str>, perms: &PluginPermissions, net_allowed: bool), DOCS [
        cwd(perms), os_homedir(perms), os_getenv(perms),
        new_tcp(plugin, net_allowed), new_timer(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use mlua::Value;

    use super::*;

    const TEST_PLUGIN: &str = "test-plugin";
    /// The allowlist is a process global; uv tests run on threads under plain
    /// `cargo test`, so they serialize on this lock.
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// Every wait ends on an event, never on the clock, so only an already
    /// failing test pays this.
    const TEST_DEADLINE: Duration = Duration::from_secs(30);
    const PUMP_POLL: Duration = Duration::from_millis(1);
    const SETTLE_WINDOW: Duration = Duration::from_millis(120);
    const TICK_MS: u64 = 20;
    const ECHO_PAYLOAD: &str = "ping";
    const TIMER_STOPPED: &str = "timer stopped ticking";
    const CONNECT_EXPECTED_ERR: &str = "connect error never arrived";
    const TIMER_ONE_TICK: &str = "one-shot timer must fire exactly once";
    const REFUSED_ERR: &str = "connection refused";
    const GREETING: &str = "greetings";
    const PARKED_READ_ERR: &str = "parked read never delivered after the retry";

    fn fresh_lua(net_allowed: bool, allowlist: &[String]) -> Lua {
        crate::api::net::set_allowed_private_hosts(allowlist);
        let lua = Lua::new();
        let uv = create_uv_table(
            &lua,
            Arc::from(TEST_PLUGIN),
            &PluginPermissions::trusted(),
            net_allowed,
        )
        .unwrap();
        lua.globals().set("uv", uv).unwrap();
        lua
    }

    /// Drains and delivers queued uv events until {done} turns true.
    async fn pump_until(lua: &Lua, done: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + TEST_DEADLINE;
        while !done() {
            while let Some((id, event)) = with_uv(lua, |store| store.next_event()) {
                deliver_uv_event(lua, id, event).await.unwrap();
            }
            assert!(Instant::now() < deadline, "{what}");
            smol::Timer::after(PUMP_POLL).await;
        }
    }

    #[test]
    fn connect_write_read_echo_close_over_the_lua_surface() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf) {
                    let _ = stream.write_all(&buf[..n]);
                }
            }
        });

        let _guard = test_guard();
        smol::block_on(async {
            let lua = fresh_lua(true, &[format!("127.0.0.1:{}", addr.port())]);
            let script = format!(
                r#"
                local results = {{}}
                local tcp = uv.new_tcp()
                tcp:connect("{host}", {port}, function(err)
                    if err then
                        results.err = err
                        return
                    end
                    tcp:read_start(function(err, data)
                        if err then
                            results.err = err
                        elseif data then
                            results.data = (results.data or "") .. data
                        else
                            results.eof = true
                            tcp:read_start(function(err2, data2)
                                if err2 == nil and data2 == nil then
                                    results.replayed = true
                                end
                            end)
                        end
                    end)
                    tcp:write("{payload}")
                end)
                return results
                "#,
                host = addr.ip(),
                port = addr.port(),
                payload = ECHO_PAYLOAD,
            );
            let results: mlua::Table = lua.load(script).eval().unwrap();
            pump_until(
                &lua,
                || {
                    results.get::<bool>("eof").unwrap_or(false)
                        && results.get::<bool>("replayed").unwrap_or(false)
                },
                "echo never completed",
            )
            .await;
            assert_eq!(
                results.get::<String>("data").unwrap().as_str(),
                ECHO_PAYLOAD
            );
        });
    }

    #[test]
    fn one_shot_timer_fires_once() {
        let _guard = test_guard();
        smol::block_on(async {
            let lua = fresh_lua(false, &[]);
            lua.load(
                r#"
                results = { ticks = 0 }
                local timer = uv.new_timer()
                timer:start(10, 0, function() results.ticks = results.ticks + 1 end)
                "#,
            )
            .exec()
            .unwrap();
            pump_until(
                &lua,
                || {
                    lua.globals()
                        .get::<mlua::Table>("results")
                        .unwrap()
                        .get::<u64>("ticks")
                        .unwrap()
                        == 1
                },
                "one-shot never fired",
            )
            .await;
            smol::Timer::after(SETTLE_WINDOW).await;
            while let Some((id, event)) = with_uv(&lua, |store| store.next_event()) {
                deliver_uv_event(&lua, id, event).await.unwrap();
            }
            let results = lua.globals().get::<mlua::Table>("results").unwrap();
            assert_eq!(results.get::<u64>("ticks").unwrap(), 1, "{TIMER_ONE_TICK}");
        });
    }

    #[test]
    fn repeating_timer_stops() {
        let _guard = test_guard();
        smol::block_on(async {
            let lua = fresh_lua(false, &[]);
            lua.load(format!(
                r#"
                results = {{ ticks = 0 }}
                timer = uv.new_timer()
                timer:start({tick}, {tick}, function() results.ticks = results.ticks + 1 end)
                "#,
                tick = TICK_MS,
            ))
            .exec()
            .unwrap();
            pump_until(
                &lua,
                || {
                    lua.globals()
                        .get::<mlua::Table>("results")
                        .unwrap()
                        .get::<u64>("ticks")
                        .unwrap()
                        >= 3
                },
                "repeating timer never ticked",
            )
            .await;
            let results = lua.globals().get::<mlua::Table>("results").unwrap();
            lua.load("timer:stop()").exec().unwrap();
            let before = results.get::<u64>("ticks").unwrap();
            smol::Timer::after(SETTLE_WINDOW).await;
            while let Some((id, event)) = with_uv(&lua, |store| store.next_event()) {
                deliver_uv_event(&lua, id, event).await.unwrap();
            }
            assert_eq!(
                results.get::<u64>("ticks").unwrap(),
                before,
                "{TIMER_STOPPED}"
            );
        });
    }

    #[test]
    fn connect_failure_reaches_the_callback() {
        // Port 1 on loopback: nothing listens there, so the kernel refuses.
        let _guard = test_guard();
        smol::block_on(async {
            let lua = fresh_lua(true, &["127.0.0.1:1".to_owned()]);
            lua.load(
                r#"
                results = {}
                local tcp = uv.new_tcp()
                tcp:connect("127.0.0.1", 1, function(err) results.err = err end)
                "#,
            )
            .exec()
            .unwrap();
            pump_until(
                &lua,
                || {
                    lua.globals()
                        .get::<mlua::Table>("results")
                        .unwrap()
                        .get::<Value>("err")
                        .unwrap()
                        != Value::Nil
                },
                CONNECT_EXPECTED_ERR,
            )
            .await;
            let results = lua.globals().get::<mlua::Table>("results").unwrap();
            let err = results.get::<String>("err").unwrap();
            assert!(err.starts_with("ECONNREFUSED"), "{REFUSED_ERR}, got: {err}");
        });
    }

    /// A failed attempt costs the handle only its connect callback: options
    /// and reads armed before it stay armed, and a retry is accepted rather
    /// than refused as already connecting.
    #[test]
    fn a_failed_connect_leaves_the_handle_reusable_with_reads_still_parked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(GREETING.as_bytes());
            }
        });

        let _guard = test_guard();
        smol::block_on(async {
            // Port 1 on loopback: nothing listens there, so the kernel refuses.
            let lua = fresh_lua(
                true,
                &[
                    "127.0.0.1:1".to_owned(),
                    format!("127.0.0.1:{}", addr.port()),
                ],
            );
            let script = format!(
                r#"
                results = {{}}
                local tcp = uv.new_tcp()
                results.nodelay = tcp:nodelay(true)
                results.read = tcp:read_start(function(err, data)
                    if err then
                        results.read_err = err
                    elseif data then
                        results.data = (results.data or "") .. data
                    end
                end)
                tcp:connect("127.0.0.1", 1, function(err)
                    results.first_err = err
                    results.retry = tcp:connect("{host}", {port}, function(err2)
                        results.second_err = err2
                    end)
                end)
                "#,
                host = addr.ip(),
                port = addr.port(),
            );
            lua.load(script).exec().unwrap();
            let results = || lua.globals().get::<mlua::Table>("results").unwrap();
            pump_until(
                &lua,
                || results().get::<Value>("data").unwrap() != Value::Nil,
                PARKED_READ_ERR,
            )
            .await;
            let results = results();
            // Both pre-connect options were accepted with luv's `0`.
            assert_eq!(results.get::<i64>("nodelay").unwrap(), 0);
            assert_eq!(results.get::<i64>("read").unwrap(), 0);
            let first = results.get::<String>("first_err").unwrap();
            assert!(
                first.starts_with("ECONNREFUSED"),
                "{REFUSED_ERR}, got: {first}"
            );
            // The retry is booked, not refused EALREADY by a stale callback.
            assert_eq!(results.get::<i64>("retry").unwrap(), 0);
            assert_eq!(results.get::<Value>("second_err").unwrap(), Value::Nil);
            // The read armed before the failed attempt never had to be re-armed.
            assert_eq!(results.get::<String>("data").unwrap().as_str(), GREETING);
        });
    }

    #[test]
    fn closing_a_plugin_closes_its_handles() {
        let _guard = test_guard();
        smol::block_on(async {
            let lua = fresh_lua(false, &[]);
            lua.load(
                r#"
                closed = false
                ticked = false
                local timer = uv.new_timer()
                timer:start(5, 0, function() ticked = true end)
                timer:close(function() closed = true end)
                "#,
            )
            .exec()
            .unwrap();
            close_plugin_handles(&lua, TEST_PLUGIN);
            // Deliver anything queued: the close path drops callbacks, so
            // leftovers must not fire them.
            while let Some((id, event)) = with_uv(&lua, |store| store.next_event()) {
                deliver_uv_event(&lua, id, event).await.unwrap();
            }
            let closed: bool = lua.globals().get("closed").unwrap();
            assert!(!closed, "close callbacks must not fire after unload");
            let ticked: bool = lua.globals().get("ticked").unwrap();
            assert!(!ticked, "pending ticks must not fire after unload");
        });
    }
}
