use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;

use flume::Sender;
use maki_lua_macro::{lua_class, lua_fn};
use mlua::{Function, Lua, Result as LuaResult, Value};
use smol::Async;
use smol::io::{AsyncReadExt, AsyncWriteExt};

use super::store::{READ_CHUNK_SIZE, Status, UvEvent, fail, io_err_message, ok_status, with_uv};
use crate::api::net::{EACCES_NAME, Family, vet_connect_for, vet_literal};
use crate::plugin_permissions::{Permission, denied_message};

/// An outbound TCP stream, modelled after libuv's `uv_tcp_t` as exposed by
/// `vim.uv`. Create with `maki.uv.new_tcp()`, then `:connect()` and
/// `:read_start()` to receive, `:write()` to send.
///
/// Unlike `maki.fn.jobstart`, reads deliver raw chunks as they arrive, not
/// lines. The handle must be `:close()`d; unloading a plugin closes its
/// handles.
pub(crate) struct LuaTcp {
    id: u32,
    net_allowed: bool,
    family: Family,
}

/// Create a new TCP handle. Like `vim.uv.new_tcp`. The optional {flags}
/// restrict the handle to an address family; a wrong value throws, as a
/// programmer error.
///
/// @param flags string|integer? `"inet"`, `"inet6"`, or one of the `AF_*` integers (`0`, `2`, `10`).
/// @return (maki.uv.Tcp) The new handle.
/// @example
/// local tcp = maki.uv.new_tcp()
/// tcp:connect("127.0.0.1", 8080, function(err)
///   if err then print("connect failed: " .. err) end
/// end)
#[lua_fn]
fn new_tcp(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] net_allowed: bool,
    flags: Option<Value>,
) -> LuaResult<LuaTcp> {
    let family = parse_family(flags)?;
    let id = with_uv(lua, |store| store.alloc_tcp(plugin));
    Ok(LuaTcp {
        id,
        net_allowed,
        family,
    })
}

fn parse_family(flags: Option<Value>) -> LuaResult<Family> {
    const AF_UNSPEC: i64 = 0;
    const AF_INET: i64 = 2;
    const AF_INET6: i64 = 10;
    match flags {
        None | Some(Value::Nil) => Ok(Family::Any),
        Some(Value::String(s)) => match &*s.to_str()? {
            "inet" => Ok(Family::V4),
            "inet6" => Ok(Family::V6),
            other => Err(mlua::Error::runtime(format!(
                "unsupported address family {other:?}, expected \"inet\" or \"inet6\""
            ))),
        },
        Some(Value::Integer(n)) => match n {
            AF_UNSPEC => Ok(Family::Any),
            AF_INET => Ok(Family::V4),
            AF_INET6 => Ok(Family::V6),
            other => Err(mlua::Error::runtime(format!(
                "unsupported address family {other}, expected 0, 2, or 10"
            ))),
        },
        Some(_) => Err(mlua::Error::runtime(
            "address family must be a string or an integer",
        )),
    }
}

/// Connect the handle to `host:port`. Like `vim.uv.tcp_connect`. `host` may
/// be an IP address or a name: names resolve on the blocking pool through the
/// same guard as `maki.net.request`, and their verdict reaches `callback`.
/// Literal addresses and immediate failures report at the call site: the
/// plugin's `net` permission, the `net.allowed_private_hosts` allowlist for
/// private targets, a closed handle, or one already connecting or connected.
///
/// @param host string Host or IP to connect to.
/// @param port integer Port to connect to.
/// @param callback function Called with `err` (nil on success).
/// @return (0|nil, string?, string?) `0` on success, or luv's `(nil, err, name)` fail triple for immediate failures (closed handle, already connecting or connected, guard refusal).
/// @example
/// tcp:connect("127.0.0.1", 8080, function(err)
///   if not err then tcp:read_start(function(err, chunk)
///     if chunk then print(chunk) end
///   end) end
/// end)
#[lua_fn]
fn connect(
    lua: &Lua,
    this: &LuaTcp,
    host: String,
    port: u16,
    callback: Function,
) -> LuaResult<Status> {
    if !this.net_allowed {
        return Ok(fail(lua, EACCES_NAME, denied_message(Permission::Net)));
    }
    let addr = match vet_literal(&host, port, this.family) {
        Ok(Some(addr)) => Some(addr),
        Ok(None) => None,
        Err((name, msg)) => return Ok(fail(lua, name, msg)),
    };
    let key = lua.create_registry_value(callback)?;
    let tx = match with_uv(lua, |store| store.begin_connect(lua, this.id, key)) {
        Ok(tx) => tx,
        Err(err) => return Ok(err.status(lua)),
    };
    match addr {
        Some(addr) => spawn_connect(addr, tx),
        None => spawn_connect_name(host, port, this.family, tx),
    }
    Ok(ok_status())
}

/// Start reading chunks as they arrive. Like `vim.uv.read_start`, except
/// chunks are raw and arbitrary: unlike `maki.fn.jobstart` there is no line
/// buffering. Reading may be armed before `:connect` finishes; it then starts
/// as soon as the socket is up. End-of-file and errors end the stream; the
/// callback still runs once with `data` nil, and re-arming after the stream
/// ended delivers that end once more.
///
/// @param callback function Called err-first as `function(err, data)`. `data` is nil at end-of-file or on error.
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// tcp:read_start(function(err, data)
///   if err then print("read error: " .. err)
///   elseif data then print("got " .. #data .. " bytes")
///   else print("closed by peer") end
/// end)
#[lua_fn]
fn read_start(lua: &Lua, this: &LuaTcp, callback: Function) -> LuaResult<Status> {
    let key = lua.create_registry_value(callback)?;
    match with_uv(lua, |store| store.begin_read(this.id, key)) {
        Ok(()) => Ok(ok_status()),
        Err(err) => Ok(err.status(lua)),
    }
}

/// Stop delivering chunks to the read callback. Like `vim.uv.read_stop`.
/// Idempotent, and safe on a stopped or closed stream. The socket keeps being
/// read: chunks that arrive while stopped buffer on the handle and replay in
/// order on the next `read_start`.
///
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// tcp:read_stop()
#[lua_fn]
fn read_stop(lua: &Lua, this: &LuaTcp) -> LuaResult<Status> {
    with_uv(lua, |store| store.end_read(lua, this.id));
    Ok(ok_status())
}

/// Write `data` to the stream. Like `vim.uv.write`. `data` may be a string or
/// a table of strings, sent in order. Writes queue up and go out one writer
/// task at a time, so pipelined writes reach the wire in call order and each
/// callback reports its own write, like libuv's write queue. Writing before
/// the socket is up fails with `ENOTCONN` where libuv would queue it.
/// Writing after `:shutdown` fails with `EPIPE`.
///
/// @param data string|table Bytes to write.
/// @param callback function? Called with `err` (nil on success) once the bytes have been handed to the socket.
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// tcp:write("hello\n")
/// tcp:write({"line1\n", "line2\n"}, function(err)
///   if err then print("write failed: " .. err) end
/// end)
#[lua_fn]
fn write(lua: &Lua, this: &LuaTcp, data: Value, callback: Option<Function>) -> LuaResult<Status> {
    let bytes = buffer_to_bytes(&data).map_err(mlua::Error::runtime)?;
    let key = callback
        .map(|cb| lua.create_registry_value(cb))
        .transpose()?;
    let start = match with_uv(lua, |store| store.queue_write(this.id, bytes, key)) {
        Ok(start) => start,
        Err(err) => return Ok(err.status(lua)),
    };
    if let Some(start) = start {
        spawn_writer(start.socket, start.bytes, start.tx);
    }
    Ok(ok_status())
}

/// Half-close the stream: send FIN while the read side stays open. Like
/// `vim.uv.shutdown`, queued writes are flushed before the FIN goes out.
/// Once shut down, further writes fail with `EPIPE` and further shutdowns
/// with `ENOTCONN`, like libuv's.
///
/// @param callback function? Called with `err` (nil on success) once the shutdown completed.
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// tcp:write("bye\n")
/// tcp:shutdown(function(err)
///   if not err then print("peer may still reply") end
/// end)
#[lua_fn]
fn shutdown(lua: &Lua, this: &LuaTcp, callback: Option<Function>) -> LuaResult<Status> {
    let key = callback
        .map(|cb| lua.create_registry_value(cb))
        .transpose()?;
    let now = match with_uv(lua, |store| store.queue_shutdown(this.id, key)) {
        Ok(now) => now,
        Err(err) => return Ok(err.status(lua)),
    };
    if let Some((socket, tx)) = now {
        spawn_shutdown(socket, tx);
    }
    Ok(ok_status())
}

/// Close the handle and release its resources. Like `vim.uv.close`. Must be
/// called on every handle; in-flight operations go quiet instead of delivering
/// an `ECANCELED` callback.
///
/// @param callback function? Called with no arguments once the handle is closed.
/// @example
/// tcp:close(function() print("closed") end)
#[lua_fn]
fn close(lua: &Lua, this: &LuaTcp, callback: Option<Function>) -> LuaResult<()> {
    let on_close = callback
        .map(|cb| lua.create_registry_value(cb))
        .transpose()?;
    with_uv(lua, |store| store.close(lua, this.id, on_close));
    Ok(())
}

/// Whether the handle is busy: connecting, reading, or writing. Like
/// `vim.uv.is_active`.
///
/// @return (boolean)
/// @example
/// if not tcp:is_active() then tcp:read_stop() end
#[lua_fn]
fn is_active(lua: &Lua, this: &LuaTcp) -> LuaResult<bool> {
    Ok(with_uv(lua, |store| store.is_active(this.id)))
}

/// Whether the handle is closing or closed. Like `vim.uv.is_closing`.
///
/// @return (boolean)
/// @example
/// if not tcp:is_closing() then tcp:close() end
#[lua_fn]
fn is_closing(lua: &Lua, this: &LuaTcp) -> LuaResult<bool> {
    Ok(with_uv(lua, |store| store.is_closing(this.id)))
}

/// Enable or disable Nagle's algorithm. Like `vim.uv.tcp_nodelay`. Allowed
/// before `:connect`; the setting is applied as soon as the socket exists.
///
/// @param enable boolean True to send chunks without waiting to coalesce them.
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// tcp:nodelay(true)
#[lua_fn]
fn nodelay(lua: &Lua, this: &LuaTcp, enable: bool) -> LuaResult<Status> {
    match with_uv(lua, |store| store.set_nodelay(this.id, enable)) {
        Ok(()) => Ok(ok_status()),
        Err(err) => Ok(err.status(lua)),
    }
}

lua_class! {
    /// An outbound TCP client handle, mirroring libuv's `uv_tcp_t`. Created by
    /// `maki.uv.new_tcp()`, connected with `:connect()`, read with
    /// `:read_start()`, written with `:write()`, released with `:close()`.
    "maki.uv.Tcp" => LuaTcp, TCP_DOCS [
        connect, read_start, read_stop, write, shutdown, close, is_active, is_closing, nodelay,
    ]
}

fn buffer_to_bytes(data: &Value) -> Result<Vec<u8>, String> {
    match data {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Table(t) => {
            let mut bytes = Vec::new();
            for part in t.sequence_values::<Value>() {
                match part.map_err(|e| e.to_string())? {
                    Value::String(s) => bytes.extend_from_slice(&s.as_bytes()),
                    other => {
                        return Err(format!(
                            "buffer entries must be strings, got {}",
                            other.type_name()
                        ));
                    }
                }
            }
            Ok(bytes)
        }
        other => Err(format!(
            "expected string or table of strings, got {}",
            other.type_name()
        )),
    }
}

/// Dials one vetted address; the SSRF guard already approved it, so no second
/// lookup can redirect the connection.
pub(crate) fn spawn_connect(addr: SocketAddr, tx: Sender<UvEvent>) {
    smol::spawn(async move {
        let _ = tx.send(UvEvent::Connect(dial(addr).await));
    })
    .detach();
}

/// Resolves a name on the blocking pool through net's shared `resolve` (its
/// retries included), vets it, and dials the address that passed. The verdict
/// reaches the connect callback: only literal addresses can fail at call
/// time, where there is no lookup to wait for.
pub(crate) fn spawn_connect_name(host: String, port: u16, family: Family, tx: Sender<UvEvent>) {
    smol::spawn(async move {
        let result = match vet_connect_for(&host, port, family).await {
            Ok(addr) => dial(addr).await,
            Err((name, msg)) => Err(format!("{name}: {msg}")),
        };
        let _ = tx.send(UvEvent::Connect(result));
    })
    .detach();
}

async fn dial(addr: SocketAddr) -> Result<Arc<Async<TcpStream>>, String> {
    Async::<TcpStream>::connect(addr)
        .await
        .map(Arc::new)
        .map_err(|e| io_err_message(&e))
}

/// Drains the socket into the event channel until the stream ends. It is not
/// stopped by `read_stop`: delivery disarms instead, and chunks buffer on the
/// handle, so a stop/start cycle neither loses nor reorders bytes.
pub(crate) fn spawn_reader(socket: Arc<Async<TcpStream>>, tx: Sender<UvEvent>) {
    smol::spawn(async move {
        loop {
            let mut buf = vec![0u8; READ_CHUNK_SIZE];
            match (&*socket).read(&mut buf).await {
                Ok(0) => {
                    let _ = tx.send(UvEvent::Read(Ok(None)));
                    break;
                }
                Ok(n) => {
                    let event = UvEvent::Read(Ok(Some(buf[..n].to_vec())));
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(UvEvent::Read(Err(io_err_message(&e))));
                    break;
                }
            }
        }
    })
    .detach();
}

pub(crate) fn spawn_writer(socket: Arc<Async<TcpStream>>, data: Vec<u8>, tx: Sender<UvEvent>) {
    smol::spawn(async move {
        // The event always fires: the pump uses it to start the next queued
        // write, callback or no callback.
        let err = (&*socket)
            .write_all(&data)
            .await
            .err()
            .map(|e| io_err_message(&e));
        let _ = tx.send(UvEvent::Write { err });
    })
    .detach();
}

/// Sends FIN. Spawned only once the write queue has drained (see
/// `queue_shutdown` and `advance_writes`), so the peer sees every queued byte
/// before the FIN, like libuv's shutdown.
pub(crate) fn spawn_shutdown(socket: Arc<Async<TcpStream>>, tx: Sender<UvEvent>) {
    smol::spawn(async move {
        let err = socket
            .get_ref()
            .shutdown(Shutdown::Write)
            .err()
            .map(|e| io_err_message(&e));
        let _ = tx.send(UvEvent::Shutdown { err });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_accepts_strings_and_tables() {
        let lua = Lua::new();
        let value: Value = lua.load("\"hello\"").eval().unwrap();
        assert_eq!(buffer_to_bytes(&value).unwrap(), b"hello");
        let value: Value = lua.load("{\"a\", \"b\\0c\"}").eval().unwrap();
        assert_eq!(buffer_to_bytes(&value).unwrap(), b"ab\0c");
        let value: Value = lua.load("42").eval().unwrap();
        assert!(buffer_to_bytes(&value).is_err());
    }
}
