//! Unix domain sockets for `maki.net`, built on Unix only.
//!
//! They need the `run` permission: a plugin that can start a process already
//! reaches every socket its user can, through `socat` or `nc -U`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::future::poll_fn;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::task::Poll;
use std::time::Duration;

use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table, UserDataRef};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::timeout;

use super::{Conn, Stream, connect_timeout_secs};
use crate::api::fs::expand_tilde;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const ACCEPT_IN_PROGRESS: &str = "accept already in progress";

/// Every listener in this process, by path. `/reload` loads the new host's
/// plugins before the old host is gone, so a plugin listening on its usual
/// path would find it still taken by its own old copy: `listen_unix` takes
/// over a path the same plugin holds, and leaves anyone else's alone.
static LISTENING: LazyLock<Mutex<HashMap<PathBuf, Weak<ListenerState>>>> =
    LazyLock::new(Mutex::default);
/// Held from looking a path up to registering its new listener, so two hosts
/// listening on one path at once cannot both find it free and race to bind.
static TAKEOVER: Mutex<()> = Mutex::new(());

thread_local! {
    /// Like `PLUGIN_CONNS`: unload closes a listener a plugin keeps in a plain
    /// variable, and the refs are weak so the GC can still close it first.
    static PLUGIN_LISTENERS: RefCell<HashMap<Arc<str>, Vec<Weak<ListenerState>>>> = RefCell::default();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn absolute(path: &str) -> Result<PathBuf, String> {
    let path = expand_tilde(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{} is not an absolute path", path.display()))
    }
}

/// Connect to the Unix socket at {path}.
///
/// {opts} fields:
///   `timeout` (integer) Connect timeout in seconds, max 60 (default 10).
///     0 means the default.
///
/// @param path string Absolute path of the socket, or one starting with `~/`.
/// @param opts table? Options (see above).
/// @return (maki.net.Conn?, string?) The connection, or nil plus an error string.
/// @example
/// maki.async.spawn(function()
///   local conn, err = maki.net.connect_unix("/run/user/1000/app/control.sock")
///   if not conn then return maki.log.error(err) end
///   conn:write("status\n")
///   print(conn:read())
///   conn:close()
/// end)
#[lua_fn(guard = Run)]
async fn connect_unix(
    _lua: Lua,
    #[ctx] plugin: Arc<str>,
    path: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Conn>> {
    let secs = connect_timeout_secs(opts.as_ref())?;
    let path = try_pair!(absolute(&path));
    let stream = match timeout(Duration::from_secs(secs), UnixStream::connect(&path)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            return Ok(err_pair(format!(
                "cannot connect to {}: {e}",
                path.display()
            )));
        }
        Err(_) => {
            return Ok(err_pair(format!(
                "connect to {} timed out after {secs}s",
                path.display()
            )));
        }
    };
    Ok((Some(Conn::track(plugin, Stream::Unix(stream))), None))
}

/// Listen on the Unix socket at {path}.
///
/// Fails if a file exists at {path}, so remove a stale socket first. The one
/// exception is a path this plugin already listens on, which it takes over,
/// so it can listen again after `/reload`. Keep the socket in a directory
/// only your user can write, so no other user can swap the file.
///
/// @param path string Absolute path of the socket, or one starting with `~/`.
/// @return (maki.net.Listener?, string?) The listener, or nil plus an error string.
/// @example
/// maki.async.spawn(function()
///   local listener = assert(maki.net.listen_unix("/run/user/1000/app/inbox.sock"))
///   while true do
///     local conn = listener:accept()
///     if not conn then break end
///     print(conn:read())
///     conn:close()
///   end
/// end)
#[lua_fn(guard = Run)]
fn listen_unix(_lua: &Lua, #[ctx] plugin: Arc<str>, path: String) -> LuaResult<Pair<Listener>> {
    let path = try_pair!(absolute(&path));
    let _takeover = lock(&TAKEOVER);
    let previous = lock(&LISTENING).get(&path).and_then(Weak::upgrade);
    if let Some(previous) = previous.filter(|old| old.plugin == plugin) {
        previous.close();
    }
    let listener = try_pair!(
        UnixListener::bind(&path).map_err(|e| format!("cannot listen on {}: {e}", path.display()))
    );
    let identity = try_pair!(
        fs::symlink_metadata(&path)
            .map(|meta| (meta.dev(), meta.ino()))
            .map_err(|e| format!("cannot listen on {}: {e}", path.display()))
    );
    let state = Arc::new(ListenerState {
        socket: Mutex::new(Some(listener)),
        path,
        identity,
        plugin: Arc::clone(&plugin),
        wake: Notify::new(),
        accepting: AsyncMutex::new(()),
    });
    lock(&LISTENING).insert(state.path.clone(), Arc::downgrade(&state));
    PLUGIN_LISTENERS.with_borrow_mut(|listeners| {
        let open = listeners.entry(plugin).or_default();
        open.retain(|listener| listener.strong_count() > 0);
        open.push(Arc::downgrade(&state));
    });
    Ok((Some(Listener(state)), None))
}

pub(super) fn close_plugin_listeners(plugin: &str) {
    let listeners = PLUGIN_LISTENERS.with_borrow_mut(|listeners| listeners.remove(plugin));
    for listener in listeners.iter().flatten().filter_map(Weak::upgrade) {
        listener.close();
    }
}

struct ListenerState {
    /// `None` once closed. Held for the whole close: a new host taking the
    /// path over waits here until the old one's file is gone.
    socket: Mutex<Option<UnixListener>>,
    path: PathBuf,
    identity: (u64, u64),
    plugin: Arc<str>,
    wake: Notify,
    accepting: AsyncMutex<()>,
}

impl ListenerState {
    fn close(&self) {
        let mut socket = lock(&self.socket);
        if socket.take().is_none() {
            return;
        }
        self.wake.notify_waiters();
        remove_if_still_ours(&self.path, self.identity);
        let mut listening = lock(&LISTENING);
        if listening
            .get(&self.path)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            listening.remove(&self.path);
        }
    }

    async fn accept(&self) -> Result<Option<UnixStream>, String> {
        let closed = || lock(&self.socket).is_none();
        if closed() {
            return Ok(None);
        }
        let Ok(_turn) = self.accepting.try_lock() else {
            return if closed() {
                Ok(None)
            } else {
                Err(ACCEPT_IN_PROGRESS.to_owned())
            };
        };
        let woken = self.wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        if closed() {
            return Ok(None);
        }
        let accepted = poll_fn(|cx| match lock(&self.socket).as_ref() {
            Some(socket) => socket.poll_accept(cx).map(Some),
            None => Poll::Ready(None),
        });
        tokio::select! {
            () = &mut woken => Ok(None),
            accepted = accepted => match accepted {
                Some(Ok((stream, _))) => Ok(Some(stream)),
                Some(Err(_)) | None if closed() => Ok(None),
                Some(Err(e)) => Err(format!("accept failed: {e}")),
                None => Ok(None),
            },
        }
    }
}

/// Best effort: a process that swaps the path between this check and the
/// removal wins, which is why a listening plugin keeps its socket in a
/// directory only its user can write.
fn remove_if_still_ours(path: &Path, identity: (u64, u64)) {
    let ours = fs::symlink_metadata(path).is_ok_and(|meta| (meta.dev(), meta.ino()) == identity);
    if ours && let Err(error) = fs::remove_file(path) {
        tracing::warn!(path = %path.display(), %error, "cannot remove a closed socket file");
    }
}

pub(crate) struct Listener(Arc<ListenerState>);

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Wait for the next client and return its connection. Returns `nil, nil`
/// once the listener is closed, also when it closes during the wait.
/// Only one `accept` at a time: a second one while one waits returns an
/// error.
///
/// @return (maki.net.Conn?, string?) The connection, nil plus an error string, or nil, nil once closed.
/// @example
/// local conn, err = listener:accept()
/// if err then return maki.log.error(err) end
/// if not conn then print("listener closed") end
#[lua_fn]
async fn accept(_lua: Lua, this: UserDataRef<Listener>) -> LuaResult<Pair<Conn>> {
    let state = Arc::clone(&this.0);
    drop(this);
    match state.accept().await {
        Ok(Some(stream)) => {
            let conn = Conn::track(Arc::clone(&state.plugin), Stream::Unix(stream));
            Ok((Some(conn), None))
        }
        Ok(None) => Ok((None, None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// Stop listening and remove the socket file. A pending `accept` returns
/// `nil, nil`. Extra calls do nothing.
///
/// @return
#[lua_fn(name = "close")]
fn listener_close(_lua: &Lua, this: &Listener) -> LuaResult<()> {
    this.0.close();
    Ok(())
}

lua_class! {
    /// A Unix socket listener opened by `maki.net.listen_unix`.
    ///
    /// It closes and removes its socket file on `:close()`, when the handle
    /// is garbage collected, and when the plugin unloads.
    "maki.net.Listener" => Listener, LISTENER_DOCS [accept, listener_close]
}

lua_table! {
    extend "maki.net" => pub(crate) fn add_unix_methods(perms: &PluginPermissions, plugin: Arc<str>), DOCS [
        connect_unix(perms, plugin),
        listen_unix(perms, plugin),
    ]
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};

    use futures::poll;
    use mlua::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        ACCEPT_IN_PROGRESS, Arc, Lua, Path, PathBuf, PluginPermissions, add_unix_methods, fs,
    };
    use crate::api::net::close_plugin_conns;
    use crate::plugin_permissions::Permission;

    const TEST_PLUGIN: &str = "unix_test";
    const OTHER_PLUGIN: &str = "unix_other";
    const SOCKET_NAME: &str = "app.sock";
    const PING: &str = "ping\n";
    const PONG: &str = "pong\n";
    const PRIVATE: u32 = 0o700;
    const TMP: &str = "/tmp";
    const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";
    /// `sun_path` holds 108 bytes on Linux and 104 elsewhere, one the NUL.
    const SUN_PATH_MAX: usize = if cfg!(target_os = "linux") { 107 } else { 103 };
    const PERMISSION_DENIED: &str = "permission denied";
    const NOT_ABSOLUTE: &str = "is not an absolute path";
    const CANNOT_LISTEN: &str = "cannot listen on";
    const CONNECT: &str = "net.connect_unix";
    const LISTEN: &str = "net.listen_unix";

    type LuaPair = (Option<String>, Option<String>);

    /// A private temporary directory with room in `sun_path` for a socket
    /// name: under `$XDG_RUNTIME_DIR` when that is short enough, else `/tmp`.
    /// `None` skips the calling test, with a note.
    fn short_dir() -> Option<TempDir> {
        [
            std::env::var_os(RUNTIME_DIR_VAR).map(PathBuf::from),
            Some(PathBuf::from(TMP)),
        ]
        .into_iter()
        .flatten()
        .filter(|base| base.is_dir())
        .find_map(|base| {
            let dir = tempfile::Builder::new().prefix("u").tempdir_in(base).ok()?;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(PRIVATE)).ok()?;
            let room = dir.path().join(SOCKET_NAME).as_os_str().len();
            (room <= SUN_PATH_MAX).then_some(dir)
        })
        .or_else(|| {
            eprintln!(
                "skipped: neither $XDG_RUNTIME_DIR nor /tmp is short enough for a socket path"
            );
            None
        })
    }

    fn lua_for(plugin: &str, permissions: &PluginPermissions) -> Lua {
        let lua = Lua::new();
        let net = lua.create_table().unwrap();
        add_unix_methods(&net, &lua, permissions, Arc::from(plugin)).unwrap();
        lua.globals().set("net", net).unwrap();
        lua
    }

    fn lua_with(permissions: &PluginPermissions) -> Lua {
        lua_for(TEST_PLUGIN, permissions)
    }

    fn lua() -> Lua {
        lua_with(&PluginPermissions::trusted())
    }

    fn set_path(lua: &Lua, path: &Path) {
        lua.globals().set("path", path.to_str().unwrap()).unwrap();
    }

    async fn listen(lua: &Lua, path: &Path) {
        set_path(lua, path);
        lua.load("listener = assert(net.listen_unix(path))")
            .exec_async()
            .await
            .unwrap();
    }

    async fn eval_pair(lua: &Lua, code: &str) -> LuaPair {
        lua.load(code).call_async(()).await.unwrap()
    }

    #[tokio::test]
    async fn a_unix_conn_carries_bytes_both_ways() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let server = StdUnixListener::bind(&path).unwrap();
        let lua = lua();
        set_path(&lua, &path);
        lua.load(format!(
            "conn = assert(net.connect_unix(path)) assert(conn:write({PING:?}))"
        ))
        .exec_async()
        .await
        .unwrap();
        let (mut peer, _) = server.accept().unwrap();
        let mut got = [0u8; PING.len()];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(got, PING.as_bytes());
        peer.write_all(PONG.as_bytes()).unwrap();
        drop(peer);
        assert_eq!(
            eval_pair(&lua, "return conn:read()").await,
            (Some(PONG.to_owned()), None)
        );
        assert_eq!(eval_pair(&lua, "return conn:read()").await, (None, None));
    }

    #[tokio::test]
    async fn listening_on_an_existing_path_fails_and_leaves_it() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        fs::write(&path, PING).unwrap();
        let lua = lua();
        set_path(&lua, &path);
        let (listener, err): (Option<Value>, Option<String>) = lua
            .load("return net.listen_unix(path)")
            .call_async(())
            .await
            .unwrap();
        assert!(listener.is_none());
        assert!(err.unwrap().contains(CANNOT_LISTEN));
        assert_eq!(fs::read_to_string(&path).unwrap(), PING);
    }

    #[tokio::test]
    async fn close_removes_the_file_and_ends_every_accept() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let lua = lua();
        listen(&lua, &path).await;
        let parked = lua
            .load("return listener:accept()")
            .call_async::<LuaPair>(());
        let then = async {
            let second = eval_pair(&lua, "return listener:accept()").await;
            lua.load("listener:close() listener:close()")
                .exec()
                .unwrap();
            let after = eval_pair(&lua, "return listener:accept()").await;
            (second, after)
        };
        let (parked, (second, after)) = tokio::join!(parked, then);
        assert_eq!(second, (None, Some(ACCEPT_IN_PROGRESS.to_owned())));
        assert_eq!(parked.unwrap(), (None, None));
        assert_eq!(after, (None, None));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn close_keeps_a_file_that_replaced_the_socket() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let lua = lua();
        listen(&lua, &path).await;
        fs::remove_file(&path).unwrap();
        fs::write(&path, PING).unwrap();
        lua.load("listener:close()").exec().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), PING);
    }

    #[tokio::test]
    async fn unload_closes_the_plugins_listeners() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let lua = lua();
        listen(&lua, &path).await;
        close_plugin_conns(TEST_PLUGIN);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn collecting_a_listener_removes_its_file() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let lua = lua();
        listen(&lua, &path).await;
        lua.load("listener = nil").exec().unwrap();
        lua.gc_collect().unwrap();
        lua.gc_collect().unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn dropping_the_lua_state_removes_the_file() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let lua = lua();
        listen(&lua, &path).await;
        drop(lua);
        assert!(!path.exists());
    }

    /// Runs {work} on its own thread and runtime, the way an old plugin host
    /// keeps its own Lua thread while `/reload` builds the new one.
    fn on_own_thread<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move { work() })
        })
    }

    #[test]
    fn a_takeover_from_another_thread_ends_a_parked_accept() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let old_path = path.clone();
        let old_host = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let old = lua();
                listen(&old, &old_path).await;
                let accept = eval_pair(&old, "return listener:accept()");
                tokio::pin!(accept);
                assert!(poll!(accept.as_mut()).is_pending());
                parked_tx.send(()).unwrap();
                accept.await
            })
        });
        parked_rx.recv().unwrap();
        let new_path = path.clone();
        let new_host = on_own_thread(move || {
            let new = lua();
            set_path(&new, &new_path);
            new.load("listener = assert(net.listen_unix(path))")
                .exec()
                .unwrap();
            new
        });
        let new = new_host.join().unwrap();
        assert_eq!(old_host.join().unwrap(), (None, None));
        assert!(StdUnixStream::connect(&path).is_ok());
        drop(new);
    }

    /// What `/reload` does: a second host listens on the path while the
    /// first one still holds it.
    #[tokio::test]
    async fn listening_again_takes_the_path_over() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let old = lua();
        listen(&old, &path).await;
        let new = lua();
        listen(&new, &path).await;
        assert_eq!(
            eval_pair(&old, "return listener:accept()").await,
            (None, None)
        );
        drop(old);
        let mut client = StdUnixStream::connect(&path).unwrap();
        client.write_all(PING.as_bytes()).unwrap();
        let got = eval_pair(
            &new,
            "local conn = assert(listener:accept()) return conn:read()",
        )
        .await;
        assert_eq!(got, (Some(PING.to_owned()), None));
    }

    #[tokio::test]
    async fn another_plugins_path_is_left_alone() {
        let Some(dir) = short_dir() else { return };
        let path = dir.path().join(SOCKET_NAME);
        let owner = lua();
        listen(&owner, &path).await;
        let other = lua_for(OTHER_PLUGIN, &PluginPermissions::trusted());
        set_path(&other, &path);
        let (listener, err): (Option<Value>, Option<String>) = other
            .load("return net.listen_unix(path)")
            .call_async(())
            .await
            .unwrap();
        assert!(listener.is_none());
        assert!(err.unwrap().contains(CANNOT_LISTEN));
        let mut client = StdUnixStream::connect(&path).unwrap();
        client.write_all(PING.as_bytes()).unwrap();
        let got = eval_pair(
            &owner,
            "local conn = assert(listener:accept()) return conn:read()",
        )
        .await;
        assert_eq!(got, (Some(PING.to_owned()), None));
    }

    #[test_case(CONNECT ; "connect")]
    #[test_case(LISTEN ; "listen")]
    #[tokio::test]
    async fn a_relative_path_is_refused(call: &str) {
        let (_, err): (Option<Value>, Option<String>) = lua()
            .load(format!("return {call}('relative/{SOCKET_NAME}')"))
            .call_async(())
            .await
            .unwrap();
        assert!(err.unwrap().contains(NOT_ABSOLUTE));
    }

    #[test_case(CONNECT ; "connect")]
    #[test_case(LISTEN ; "listen")]
    #[tokio::test]
    async fn the_run_permission_is_required(call: &str) {
        let mut permissions = PluginPermissions::trusted();
        permissions.set(Permission::Run, false);
        let err = lua_with(&permissions)
            .load(format!("return {call}('/{SOCKET_NAME}')"))
            .call_async::<Value>(())
            .await
            .unwrap_err();
        assert!(err.to_string().contains(PERMISSION_DENIED), "{err}");
    }
}
