use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::mem;
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use flume::{Receiver, Sender};
use mlua::{Function, Lua, MultiValue, RegistryKey, Result as LuaResult, Value};
use smol::Async;

use super::tcp::{spawn_reader, spawn_shutdown, spawn_writer};
use crate::api::net::EACCES_NAME;

pub(crate) const READ_CHUNK_SIZE: usize = 64 * 1024;

pub(crate) const EBADF: &str = "EBADF";
pub(crate) const EALREADY: &str = "EALREADY";
pub(crate) const EISCONN: &str = "EISCONN";
pub(crate) const ENOTCONN: &str = "ENOTCONN";

pub(crate) const EPIPE: &str = "EPIPE";
pub(crate) const EOF: &str = "EOF";
const CLOSED_HANDLE_MSG: &str = "closed or unknown handle";
const CLOSING_MSG: &str = "handle is closing";
const NOT_CONNECTED_MSG: &str = "socket is not connected";
const SHUT_DOWN_MSG: &str = "write side is shut down";
const CONNECTING_MSG: &str = "connection already in progress";
const CONNECTED_MSG: &str = "socket is already connected";
const READING_MSG: &str = "stream is already being read";

/// luv's sync return shape: `(0)` on success, `(nil, err, name)` on failure,
/// where `err` is `"{name}: {message}"`.
pub(crate) type Status = (Value, Value, Value);

pub(crate) fn ok_status() -> Status {
    (Value::Integer(0), Value::Nil, Value::Nil)
}

/// A luv sync failure: the libuv errno name and the message behind it.
pub(crate) struct UvError {
    name: &'static str,
    msg: Cow<'static, str>,
}

impl UvError {
    fn new(name: &'static str, msg: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name,
            msg: msg.into(),
        }
    }

    pub(crate) fn status(self, lua: &Lua) -> Status {
        fail(lua, self.name, self.msg)
    }
}

/// A shutdown accepted for immediate spawning (`Some`), `None` when the FIN
/// waits for the write queue, or the sync failure.
pub(crate) type ShutdownOps = Result<Option<(Arc<Async<TcpStream>>, Sender<UvEvent>)>, UvError>;

pub(crate) type TimerOps = Result<(Sender<UvEvent>, u64, Receiver<Ctl>), UvError>;

/// A write request waiting for the in-flight writer to finish; the pump
/// starts queued writes in order, like libuv's write queue.
struct QueuedWrite {
    bytes: Vec<u8>,
    on_write: Option<RegistryKey>,
}

/// A write the store accepted for immediate spawning: the vetted socket, the
/// event channel, and the bytes.
pub(crate) struct WriteStart {
    pub(crate) socket: Arc<Async<TcpStream>>,
    pub(crate) tx: Sender<UvEvent>,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn fail(lua: &Lua, name: impl fmt::Display, msg: impl fmt::Display) -> Status {
    let err = format!("{name}: {msg}");
    (
        Value::Nil,
        Value::String(lua.create_string(err).expect("status string")),
        Value::String(lua.create_string(name.to_string()).expect("status name")),
    )
}

/// libuv errno name for an io error, so callback errors read like luv's.
pub(crate) fn io_err_name(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        io::ErrorKind::ConnectionReset => "ECONNRESET",
        io::ErrorKind::ConnectionAborted => "ECONNABORTED",
        io::ErrorKind::TimedOut => "ETIMEDOUT",
        io::ErrorKind::PermissionDenied => EACCES_NAME,
        io::ErrorKind::Interrupted => "EINTR",
        io::ErrorKind::WouldBlock => "EAGAIN",
        io::ErrorKind::AddrInUse => "EADDRINUSE",
        io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        io::ErrorKind::BrokenPipe => EPIPE,
        io::ErrorKind::NotConnected => ENOTCONN,
        io::ErrorKind::UnexpectedEof => EOF,
        _ => "EIO",
    }
}

pub(crate) fn io_err_message(e: &io::Error) -> String {
    format!("{}: {e}", io_err_name(e))
}

/// Tells a spawned task to finish its current iteration and exit.
pub(crate) enum Ctl {
    Stop,
}

pub(crate) enum UvEvent {
    /// A connect attempt finished: the socket, or why it did not.
    Connect(Result<Arc<Async<TcpStream>>, String>),
    /// One read off the socket: a chunk, the end of the stream, or why the
    /// read failed.
    Read(Result<Option<Vec<u8>>, String>),
    /// Queued by `read_start` to deliver what the reader produced while
    /// delivery was disarmed: buffered bytes first, then a buffered end.
    Replay,
    Write {
        err: Option<String>,
    },
    Shutdown {
        err: Option<String>,
    },
    Close,
    TimerTick {
        generation: u64,
    },
}

/// The reader task on a connected socket. It lives as long as the socket:
/// `read_stop` only disarms delivery, so a stop/start cycle neither loses
/// nor reorders bytes.
enum Reader {
    /// No reader task yet: reading was never armed on this socket.
    Idle,
    /// A reader task is draining the socket into the event channel.
    On,
    /// The reader saw the end of the stream: `error` records a failed read,
    /// or `None` a clean end-of-file. Either replays to every `read_start`,
    /// like libuv re-reading a stream that ended.
    Ended { error: Option<String> },
}

/// The writer task on a connected socket. One is out at a time, so pipelined
/// writes reach the wire in call order.
enum Writer {
    Idle,
    Busy { on_write: Option<RegistryKey> },
}

/// Half-close latch. Leaving [`Fin::Open`] is permanent for the handle's life:
/// further writes fail `EPIPE` and further shutdowns `ENOTCONN`, like libuv's.
enum Fin {
    Open,
    /// `shutdown` arrived with writes still pending; the FIN goes out when the
    /// queue drains.
    Queued(Option<RegistryKey>),
    /// The FIN is out; its callback fires when the Shutdown event lands.
    Sent(Option<RegistryKey>),
    /// The shutdown was reported. The stream stays shut for writes and
    /// further shutdowns, but the handle is no longer busy.
    Done,
}

impl Fin {
    fn drop_keys(self, lua: &Lua) {
        match self {
            Self::Open | Self::Done => {}
            Self::Queued(key) | Self::Sent(key) => drop_key(lua, key),
        }
    }
}

/// A tcp handle with a live socket: everything the read and write sides need
/// exists exactly here, and nowhere else in the handle's life.
struct Connected {
    socket: Arc<Async<TcpStream>>,
    on_read: Option<RegistryKey>,
    /// Chunks read off the wire while delivery was disarmed; replayed on the
    /// next `read_start`.
    unread: Vec<u8>,
    reader: Reader,
    writer: Writer,
    queue: VecDeque<QueuedWrite>,
    fin: Fin,
}

impl Connected {
    fn new(socket: Arc<Async<TcpStream>>, on_read: Option<RegistryKey>) -> Self {
        Self {
            socket,
            on_read,
            unread: Vec::new(),
            reader: Reader::Idle,
            writer: Writer::Idle,
            queue: VecDeque::new(),
            fin: Fin::Open,
        }
    }

    fn is_writing(&self) -> bool {
        matches!(self.writer, Writer::Busy { .. })
    }

    /// A pending or in-flight shutdown keeps the handle active too, like
    /// libuv counting its shutdown request; a reported one does not.
    fn is_busy(&self) -> bool {
        matches!(self.reader, Reader::On)
            || self.is_writing()
            || matches!(self.fin, Fin::Queued(_) | Fin::Sent(_))
    }

    /// Wakes a blocked reader by forcing the socket closed; the read returns
    /// the end of the stream and the task exits.
    fn stop_tasks(&self) {
        let _ = self.socket.get_ref().shutdown(Shutdown::Both);
    }

    fn drop_keys(self, lua: &Lua) {
        drop_key(lua, self.on_read);
        if let Writer::Busy { on_write } = self.writer {
            drop_key(lua, on_write);
        }
        for req in self.queue {
            drop_key(lua, req.on_write);
        }
        self.fin.drop_keys(lua);
    }
}

/// A tcp handle that still takes work. `close` is the one way out, and it
/// leaves [`Kind::Closing`], so no operation can reach these states again.
enum TcpLive {
    /// No socket: never connected, an attempt is in flight, or the last one
    /// failed. The read callback and `nodelay` park here until a connect
    /// completes, and `on_connect` armed is exactly "a connect task is out":
    /// it is set by [`UvStore::begin_connect`] and cleared by the Connect
    /// event.
    Unconnected {
        on_read: Option<RegistryKey>,
        nodelay: Option<bool>,
        on_connect: Option<RegistryKey>,
    },
    Connected(Connected),
}

impl TcpLive {
    fn stop_tasks(&self) {
        if let Self::Connected(connected) = self {
            connected.stop_tasks();
        }
    }

    fn drop_keys(self, lua: &Lua) {
        match self {
            Self::Unconnected {
                on_read,
                on_connect,
                ..
            } => {
                drop_key(lua, on_read);
                drop_key(lua, on_connect);
            }
            Self::Connected(connected) => connected.drop_keys(lua),
        }
    }
}

enum TimerState {
    Stopped,
    Armed {
        ctl: Sender<Ctl>,
        on_timer: RegistryKey,
        repeat_ms: u64,
    },
}

/// What a handle is. The two kinds share nothing but the event channel and the
/// close callback, so nothing tcp-shaped can be set on a timer or the reverse;
/// `Closing` is the state every handle ends in, reached only through `close`.
enum Kind {
    Tcp {
        live: TcpLive,
    },
    Timer {
        /// Bumped on every start, so ticks queued by an earlier run are
        /// dropped when they reach the pump.
        generation: u64,
        state: TimerState,
    },
    /// `close` was requested: the handle's tasks and keys are torn down and
    /// only the queued [`UvEvent::Close`] is left.
    Closing,
}

/// One `maki.uv` handle.
struct UvMeta {
    owner: Arc<str>,
    events: (Sender<UvEvent>, Receiver<UvEvent>),
    on_close: Option<RegistryKey>,
    /// Receiver of the previous delivery task still running for this handle:
    /// deliveries spawn as chained tasks so one handle's callbacks keep
    /// event order.
    chain: Option<Receiver<()>>,
    kind: Kind,
}

impl UvMeta {
    fn new(owner: Arc<str>, kind: Kind) -> Self {
        Self {
            owner,
            events: flume::unbounded(),
            on_close: None,
            chain: None,
            kind,
        }
    }

    fn has_pending(&self) -> bool {
        !self.events.1.is_empty()
    }

    fn is_closing(&self) -> bool {
        matches!(self.kind, Kind::Closing)
    }

    /// The live tcp state and the event sender. `None` for a timer id, which
    /// the Lua classes never hand to a tcp operation, for a closing handle,
    /// or for a handle that [`UvStore::live`] already rejected.
    fn tcp_live(&mut self) -> Option<(&Sender<UvEvent>, &mut TcpLive)> {
        match &mut self.kind {
            Kind::Tcp { live } => Some((&self.events.0, live)),
            _ => None,
        }
    }

    /// The timer generation and state, next to the event sender. `None` for a
    /// tcp id, mirroring [`Self::tcp_live`].
    fn timer_parts(&mut self) -> Option<(&Sender<UvEvent>, &mut u64, &mut TimerState)> {
        match &mut self.kind {
            Kind::Timer { generation, state } => Some((&self.events.0, generation, state)),
            _ => None,
        }
    }

    /// Stops whatever task the handle owns, so a blocked reader wakes up
    /// instead of holding the connection open.
    fn stop_tasks(&self) {
        match &self.kind {
            Kind::Tcp { live } => live.stop_tasks(),
            Kind::Timer { state, .. } => {
                if let TimerState::Armed { ctl, .. } = state {
                    let _ = ctl.send(Ctl::Stop);
                }
            }
            Kind::Closing => {}
        }
    }

    /// Tears the handle down for good: stops its tasks and releases every
    /// callback key parked on it, leaving the closing state. Idempotent.
    /// `on_close` is left alone: the close path still reports through it.
    fn tear_down(&mut self, lua: &Lua) {
        match mem::replace(&mut self.kind, Kind::Closing) {
            Kind::Tcp { live } => {
                live.stop_tasks();
                live.drop_keys(lua);
            }
            Kind::Timer { state, .. } => stop_armed(lua, state),
            Kind::Closing => {}
        }
    }
}

/// Store of live uv handles, mirroring [`crate::api::fn::JobStore`]: keyed by
/// id, drained by the host pump, per-handle Lua callbacks in the registry.
pub(crate) struct UvStore {
    handles: HashMap<u32, UvMeta>,
    next_id: u32,
    scan_cursor: u32,
}

impl UvStore {
    pub(crate) fn new() -> Self {
        Self {
            handles: HashMap::new(),
            next_id: 1,
            scan_cursor: 0,
        }
    }

    fn alloc(&mut self, owner: Arc<str>, kind: Kind) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(id, UvMeta::new(owner, kind));
        id
    }

    pub(crate) fn alloc_tcp(&mut self, owner: Arc<str>) -> u32 {
        self.alloc(
            owner,
            Kind::Tcp {
                live: TcpLive::Unconnected {
                    on_read: None,
                    nodelay: None,
                    on_connect: None,
                },
            },
        )
    }

    pub(crate) fn alloc_timer(&mut self, owner: Arc<str>) -> u32 {
        self.alloc(
            owner,
            Kind::Timer {
                generation: 0,
                state: TimerState::Stopped,
            },
        )
    }

    /// The one lifecycle gate: every operation starts here, so a handle that
    /// is gone or closing rejects the same way everywhere.
    fn live(&mut self, id: u32) -> Result<&mut UvMeta, UvError> {
        match self.handles.get_mut(&id) {
            Some(meta) if !meta.is_closing() => Ok(meta),
            Some(_) => Err(UvError::new(EBADF, CLOSING_MSG)),
            None => Err(UvError::new(EBADF, CLOSED_HANDLE_MSG)),
        }
    }

    fn tcp(&mut self, id: u32) -> Result<(&Sender<UvEvent>, &mut TcpLive), UvError> {
        self.live(id)?
            .tcp_live()
            .ok_or_else(|| UvError::new(EBADF, CLOSED_HANDLE_MSG))
    }

    fn timer(&mut self, id: u32) -> Result<(&Sender<UvEvent>, &mut u64, &mut TimerState), UvError> {
        self.live(id)?
            .timer_parts()
            .ok_or_else(|| UvError::new(EBADF, CLOSED_HANDLE_MSG))
    }

    /// Books a connect attempt: state check, connect callback. The caller
    /// spawns the task outside the store borrow. A refused attempt releases
    /// the callback key it was handed.
    pub(crate) fn begin_connect(
        &mut self,
        lua: &Lua,
        id: u32,
        on_connect: RegistryKey,
    ) -> Result<Sender<UvEvent>, UvError> {
        let (tx, live) = match self.tcp(id) {
            Ok(parts) => parts,
            Err(err) => {
                drop_key(lua, Some(on_connect));
                return Err(err);
            }
        };
        let tx = tx.clone();
        match live {
            TcpLive::Unconnected {
                on_connect: slot, ..
            } if slot.is_none() => {
                *slot = Some(on_connect);
                Ok(tx)
            }
            TcpLive::Connected(_) => {
                drop_key(lua, Some(on_connect));
                Err(UvError::new(EISCONN, CONNECTED_MSG))
            }
            TcpLive::Unconnected { .. } => {
                drop_key(lua, Some(on_connect));
                Err(UvError::new(EALREADY, CONNECTING_MSG))
            }
        }
    }

    /// Arms reading. Chunks buffered while delivery was disarmed replay before
    /// anything else, followed by a buffered end of the stream. Arming before
    /// the socket is up parks the callback until the connect completes.
    pub(crate) fn begin_read(&mut self, id: u32, on_read: RegistryKey) -> Result<(), UvError> {
        let (tx, live) = self.tcp(id)?;
        let arm = match live {
            TcpLive::Unconnected { on_read: slot, .. } => {
                if slot.is_some() {
                    return Err(UvError::new(EALREADY, READING_MSG));
                }
                *slot = Some(on_read);
                false
            }
            TcpLive::Connected(connected) => {
                if connected.on_read.is_some() {
                    return Err(UvError::new(EALREADY, READING_MSG));
                }
                connected.on_read = Some(on_read);
                let parked = !connected.unread.is_empty()
                    || matches!(connected.reader, Reader::Ended { .. });
                if parked {
                    let _ = tx.send(UvEvent::Replay);
                }
                matches!(connected.reader, Reader::Idle)
            }
        };
        if arm {
            self.start_reading(id);
        }
        Ok(())
    }

    /// Disarms delivery. The reader task keeps draining so no bytes are lost;
    /// chunks that arrive meanwhile buffer and replay on the next `read_start`.
    pub(crate) fn end_read(&mut self, lua: &Lua, id: u32) {
        let Ok((_, live)) = self.tcp(id) else {
            return;
        };
        let slot = match live {
            TcpLive::Unconnected { on_read, .. } => on_read,
            TcpLive::Connected(connected) => &mut connected.on_read,
        };
        drop_key(lua, slot.take());
    }

    /// Books a write. While a writer task is out on the socket the request
    /// joins the queue and the pump starts it on the next Write event, so
    /// pipelined writes reach the wire in call order like libuv's write
    /// queue. `Ok(None)` means queued; `Ok(Some(...))` hands the write back
    /// to spawn now.
    pub(crate) fn queue_write(
        &mut self,
        id: u32,
        bytes: Vec<u8>,
        on_write: Option<RegistryKey>,
    ) -> Result<Option<WriteStart>, UvError> {
        let (tx, live) = self.tcp(id)?;
        let TcpLive::Connected(connected) = live else {
            return Err(UvError::new(ENOTCONN, NOT_CONNECTED_MSG));
        };
        if !matches!(connected.fin, Fin::Open) {
            return Err(UvError::new(EPIPE, SHUT_DOWN_MSG));
        }
        if connected.is_writing() {
            connected.queue.push_back(QueuedWrite { bytes, on_write });
            return Ok(None);
        }
        connected.writer = Writer::Busy { on_write };
        Ok(Some(WriteStart {
            socket: Arc::clone(&connected.socket),
            tx: tx.clone(),
            bytes,
        }))
    }

    /// Books a half-close. With writes still queued or in flight the FIN waits
    /// for them and the pump sends it once the queue drains (`Ok(None)`), like
    /// libuv's shutdown; `Ok(Some(...))` hands the socket back to shut down
    /// now. A further shutdown fails `ENOTCONN`, like libuv's.
    pub(crate) fn queue_shutdown(
        &mut self,
        id: u32,
        on_shutdown: Option<RegistryKey>,
    ) -> ShutdownOps {
        let (tx, live) = self.tcp(id)?;
        let TcpLive::Connected(connected) = live else {
            return Err(UvError::new(ENOTCONN, NOT_CONNECTED_MSG));
        };
        if !matches!(connected.fin, Fin::Open) {
            return Err(UvError::new(ENOTCONN, SHUT_DOWN_MSG));
        }
        // The writer only idles once the queue is drained, so a busy writer
        // covers queued bytes too.
        if connected.is_writing() {
            connected.fin = Fin::Queued(on_shutdown);
            return Ok(None);
        }
        connected.fin = Fin::Sent(on_shutdown);
        Ok(Some((Arc::clone(&connected.socket), tx.clone())))
    }

    pub(crate) fn set_nodelay(&mut self, id: u32, enable: bool) -> Result<(), UvError> {
        let (_, live) = self.tcp(id)?;
        match live {
            TcpLive::Unconnected { nodelay, .. } => {
                *nodelay = Some(enable);
                Ok(())
            }
            TcpLive::Connected(connected) => connected
                .socket
                .get_ref()
                .set_nodelay(enable)
                .map_err(|e| UvError::new(io_err_name(&e), e.to_string())),
        }
    }

    /// `timer:start`. Stops a previous run, bumps the generation, books the
    /// callback, and returns the sender, generation, and control receiver for
    /// the spawned task.
    pub(crate) fn begin_timer(
        &mut self,
        lua: &Lua,
        id: u32,
        on_timer: RegistryKey,
        repeat_ms: u64,
    ) -> TimerOps {
        let (tx, generation, state) = self.timer(id)?;
        let tx = tx.clone();
        *generation += 1;
        let (ctl_tx, ctl_rx) = flume::unbounded();
        let armed = TimerState::Armed {
            ctl: ctl_tx,
            on_timer,
            repeat_ms,
        };
        stop_armed(lua, mem::replace(state, armed));
        Ok((tx, *generation, ctl_rx))
    }

    /// `timer:stop`. A tick already queued by the previous run is dropped by
    /// the pump: the state is no longer armed, and a re-started timer carries
    /// a new generation.
    pub(crate) fn stop_timer(&mut self, lua: &Lua, id: u32) {
        let Ok((_, _, state)) = self.timer(id) else {
            return;
        };
        stop_armed(lua, mem::replace(state, TimerState::Stopped));
    }

    pub(crate) fn is_active(&self, id: u32) -> bool {
        let Some(meta) = self.handles.get(&id) else {
            return false;
        };
        match &meta.kind {
            Kind::Tcp { live } => match live {
                TcpLive::Unconnected { on_connect, .. } => on_connect.is_some(),
                TcpLive::Connected(connected) => connected.is_busy(),
            },
            Kind::Timer { state, .. } => matches!(state, TimerState::Armed { .. }),
            Kind::Closing => false,
        }
    }

    pub(crate) fn is_closing(&self, id: u32) -> bool {
        self.handles.get(&id).is_none_or(UvMeta::is_closing)
    }

    /// Requests handle close: idempotent, libuv-style. The close callback fires
    /// when the queued [`UvEvent::Close`] reaches the pump, which is also when
    /// the handle leaves the store.
    pub(crate) fn close(&mut self, lua: &Lua, id: u32, on_close: Option<RegistryKey>) {
        let Ok(meta) = self.live(id) else {
            drop_key(lua, on_close);
            return;
        };
        meta.tear_down(lua);
        meta.on_close = on_close;
        let _ = meta.events.0.send(UvEvent::Close);
    }

    /// Plugin unload/reload: handles of that owner are torn down for good, no
    /// close callbacks survive.
    pub(crate) fn close_owner(&mut self, lua: &Lua, owner: &str) {
        let ids: Vec<u32> = self
            .handles
            .iter()
            .filter(|(_, meta)| meta.owner.as_ref() == owner)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let Some(mut meta) = self.handles.remove(&id) else {
                continue;
            };
            meta.tear_down(lua);
            drop_key(lua, meta.on_close.take());
        }
    }

    /// Pops one queued event, round-robin over handles so a chatty timer or a
    /// fast socket cannot starve the rest.
    pub(crate) fn next_event(&mut self) -> Option<(u32, UvEvent)> {
        let mut past_cursor = None;
        let mut lowest = None;
        for (&id, meta) in &self.handles {
            if !meta.has_pending() {
                continue;
            }
            if id > self.scan_cursor {
                past_cursor = Some(past_cursor.map_or(id, |seen: u32| seen.min(id)));
            }
            lowest = Some(lowest.map_or(id, |seen: u32| seen.min(id)));
        }
        let id = past_cursor.or(lowest)?;
        self.scan_cursor = id;
        let meta = self.handles.get_mut(&id)?;
        let (_, rx) = &meta.events;
        Some((id, rx.try_recv().ok()?))
    }

    /// Whether a delivery task for `id` is still alive: its token receiver
    /// sits on the handle, connected from the moment the token is minted
    /// until the task's own sender drops.
    fn delivery_in_flight(&self, id: u32) -> bool {
        self.handles
            .get(&id)
            .and_then(|meta| meta.chain.as_ref())
            .is_some_and(|rx| !rx.is_disconnected())
    }

    /// Pops the next deliverable event. Timer ticks of a handle whose
    /// previous delivery is still running are dropped: a repeating timer
    /// queues a tick per interval, and replaying all of them when the
    /// callback finishes is a catch-up burst libuv does not have. Ticks are
    /// droppable; read and write events are not, bytes and writer state ride
    /// on them.
    pub(crate) fn next_delivery(&mut self) -> Option<(u32, UvEvent)> {
        while let Some((id, event)) = self.next_event() {
            if matches!(event, UvEvent::TimerTick { .. }) && self.delivery_in_flight(id) {
                continue;
            }
            return Some((id, event));
        }
        None
    }

    /// State transitions for one delivered event, plus the callback (if any)
    /// converted out of the registry. Runs on the runtime thread with exclusive
    /// store access, so it is the only place handles change shape.
    fn absorb(
        &mut self,
        lua: &Lua,
        id: u32,
        event: UvEvent,
    ) -> LuaResult<Option<(Function, MultiValue)>> {
        // `close` marks the handle closing before it queues the Close event,
        // so a closing handle has only that event left to run; whatever else
        // its tasks queued behind the close goes quiet.
        if self.is_closing(id) {
            let UvEvent::Close = event else {
                return Ok(None);
            };
            let Some(meta) = self.handles.remove(&id) else {
                return Ok(None);
            };
            return callback_from_key(lua, meta.on_close, MultiValue::new());
        }
        let Some(meta) = self.handles.get_mut(&id) else {
            return Ok(None);
        };
        match event {
            UvEvent::Connect(result) => {
                let Some((_, live)) = meta.tcp_live() else {
                    return Ok(None);
                };
                let TcpLive::Unconnected {
                    on_read,
                    nodelay,
                    on_connect,
                } = live
                else {
                    return Ok(None);
                };
                let on_connect = on_connect.take();
                let (socket, err) = match result {
                    Ok(socket) => (Some(socket), None),
                    Err(err) => (None, Some(err)),
                };
                let args = one_err_arg(lua, err.as_deref())?;
                // A failed attempt leaves the handle where it is, minus the
                // connect callback: the parked read and nodelay stay armed for
                // a retry.
                let arm = match socket {
                    Some(socket) => {
                        let nodelay = nodelay.take();
                        let on_read = on_read.take();
                        if let Some(enable) = nodelay {
                            let _ = socket.get_ref().set_nodelay(enable);
                        }
                        let arm = on_read.is_some();
                        *live = TcpLive::Connected(Connected::new(socket, on_read));
                        arm
                    }
                    None => false,
                };
                if arm {
                    self.start_reading(id);
                }
                callback_from_key(lua, on_connect, args)
            }
            UvEvent::Read(result) => {
                let Some((tx, TcpLive::Connected(connected))) = meta.tcp_live() else {
                    return Ok(None);
                };
                // While delivery is disarmed, chunks buffer on the handle so a
                // read_stop/read_start cycle neither loses nor reorders bytes;
                // the end of the stream is recorded for the next read_start.
                if connected.on_read.is_none() {
                    match result {
                        Ok(Some(bytes)) => connected.unread.extend_from_slice(&bytes),
                        Ok(None) => connected.reader = Reader::Ended { error: None },
                        Err(error) => connected.reader = Reader::Ended { error: Some(error) },
                    }
                    return Ok(None);
                }
                match result {
                    Ok(Some(bytes)) => {
                        // Buffered bytes are a prefix of the stream: they ride
                        // along so a replay never lands behind fresh bytes.
                        let mut chunk = mem::take(&mut connected.unread);
                        chunk.extend_from_slice(&bytes);
                        let args = read_args(lua, &chunk)?;
                        let callback = keep_callback(lua, &connected.on_read)?;
                        Ok(callback.map(|callback| (callback, args)))
                    }
                    result => {
                        // End-of-file and errors end the stream but still
                        // reach the callback, as `cb(err, nil)`: a websocket
                        // client reconnects on them.
                        let error = result.err();
                        let args = one_err_arg(lua, error.as_deref())?;
                        connected.reader = Reader::Ended { error };
                        if connected.unread.is_empty() {
                            callback_from_key(lua, connected.on_read.take(), args)
                        } else {
                            // Buffered bytes go first; the end follows them.
                            let _ = tx.send(UvEvent::Replay);
                            Ok(None)
                        }
                    }
                }
            }
            // Queued by `read_start` over a disarmed stretch: deliver the
            // buffered bytes, then the buffered end, if any.
            UvEvent::Replay => {
                let Some((tx, TcpLive::Connected(connected))) = meta.tcp_live() else {
                    return Ok(None);
                };
                if connected.on_read.is_none() {
                    // Disarmed again before this ran; the bytes stay parked
                    // for the next `read_start`.
                    return Ok(None);
                }
                if !connected.unread.is_empty() {
                    let data = mem::take(&mut connected.unread);
                    if matches!(connected.reader, Reader::Ended { .. }) {
                        let _ = tx.send(UvEvent::Replay);
                    }
                    let args = read_args(lua, &data)?;
                    let callback = keep_callback(lua, &connected.on_read)?;
                    return Ok(callback.map(|callback| (callback, args)));
                }
                let Reader::Ended { error } = &connected.reader else {
                    return Ok(None);
                };
                let args = one_err_arg(lua, error.as_deref())?;
                callback_from_key(lua, connected.on_read.take(), args)
            }
            UvEvent::Write { err } => {
                let args = one_err_arg(lua, err.as_deref())?;
                let Some((_, TcpLive::Connected(connected))) = meta.tcp_live() else {
                    return Ok(None);
                };
                let on_write = match mem::replace(&mut connected.writer, Writer::Idle) {
                    Writer::Busy { on_write } => on_write,
                    Writer::Idle => None,
                };
                self.advance_writes(id);
                callback_from_key(lua, on_write, args)
            }
            UvEvent::Shutdown { err } => {
                let args = one_err_arg(lua, err.as_deref())?;
                let Some((_, TcpLive::Connected(connected))) = meta.tcp_live() else {
                    return Ok(None);
                };
                let on_shutdown = match mem::replace(&mut connected.fin, Fin::Done) {
                    Fin::Sent(key) => key,
                    other => {
                        connected.fin = other;
                        None
                    }
                };
                callback_from_key(lua, on_shutdown, args)
            }
            UvEvent::TimerTick { generation } => {
                let Some((_, current, state)) = meta.timer_parts() else {
                    return Ok(None);
                };
                if generation != *current {
                    return Ok(None);
                }
                let TimerState::Armed {
                    on_timer,
                    repeat_ms,
                    ..
                } = state
                else {
                    return Ok(None);
                };
                let one_shot = *repeat_ms == 0;
                let callback = lua.registry_value::<Function>(on_timer)?;
                if one_shot {
                    stop_armed(lua, mem::replace(state, TimerState::Stopped));
                }
                Ok(Some((callback, MultiValue::new())))
            }
            // Queued only by `close`, which marks the handle closing first, so
            // it is always taken by the branch above.
            UvEvent::Close => Ok(None),
        }
    }

    /// Mints the ordering token for the next delivery task of `id`: the task
    /// waits on the previous one, and its own token releases the one after it.
    pub(crate) fn chain(&mut self, id: u32) -> CallbackChain {
        let (done, rx) = flume::bounded(0);
        let prev = self
            .handles
            .get_mut(&id)
            .and_then(|meta| meta.chain.replace(rx));
        CallbackChain { prev, _done: done }
    }

    /// Starts the next queued write, or sends a shutdown that waited for the
    /// queue. Runs from the pump after each Write event, so one writer task
    /// is out at a time and pipelined writes keep their order. A failed
    /// write does not stop the queue: the rest still run and each reports
    /// its own error, and a parked shutdown still flushes.
    fn advance_writes(&mut self, id: u32) {
        let Ok((tx, TcpLive::Connected(connected))) = self.tcp(id) else {
            return;
        };
        if let Some(req) = connected.queue.pop_front() {
            connected.writer = Writer::Busy {
                on_write: req.on_write,
            };
            spawn_writer(Arc::clone(&connected.socket), req.bytes, tx.clone());
            return;
        }
        if let Fin::Queued(on_shutdown) = &mut connected.fin {
            connected.fin = Fin::Sent(on_shutdown.take());
            spawn_shutdown(Arc::clone(&connected.socket), tx.clone());
        }
    }

    /// Spawns the reader task for a connected socket. Shared by `read_start`
    /// and the parked-read path of a completing connect.
    fn start_reading(&mut self, id: u32) {
        let Ok((tx, TcpLive::Connected(connected))) = self.tcp(id) else {
            return;
        };
        connected.reader = Reader::On;
        spawn_reader(Arc::clone(&connected.socket), tx.clone());
    }
}

impl Drop for UvStore {
    fn drop(&mut self) {
        for (_, meta) in self.handles.drain() {
            meta.stop_tasks();
        }
    }
}

/// Silences a timer run that is being replaced: stops its task and releases
/// its callback. Anything but [`TimerState::Armed`] has neither.
fn stop_armed(lua: &Lua, state: TimerState) {
    if let TimerState::Armed { ctl, on_timer, .. } = state {
        let _ = ctl.send(Ctl::Stop);
        drop_key(lua, Some(on_timer));
    }
}

pub(crate) fn with_uv<R>(lua: &Lua, f: impl FnOnce(&mut UvStore) -> R) -> R {
    if lua.app_data_ref::<UvStore>().is_none() {
        lua.set_app_data(UvStore::new());
    }
    let mut store = lua
        .app_data_mut::<UvStore>()
        .expect("uv store was just installed");
    f(&mut store)
}

/// Orders one handle's deliveries: a task waits on the previous one and, held
/// to its end, the token's drop releases the next, so a panicked predecessor
/// still unwedges the chain. Flume over a per-handle async mutex: drop must
/// release, and a mutex hands the lock to its waiters in no fixed order,
/// while a linked token has one waiter.
pub(crate) struct CallbackChain {
    prev: Option<Receiver<()>>,
    _done: Sender<()>,
}

impl CallbackChain {
    pub(crate) async fn wait(&self) {
        if let Some(prev) = &self.prev {
            let _ = prev.recv_async().await;
        }
    }
}

/// Delivers one event: state transitions first, then the handle's callback in
/// a fresh coroutine so it may suspend, mirroring [`crate::api::fn::deliver_job_event`].
pub(crate) async fn deliver_uv_event(lua: &Lua, id: u32, event: UvEvent) -> LuaResult<()> {
    let Some((callback, args)) = with_uv(lua, |store| store.absorb(lua, id, event))? else {
        return Ok(());
    };
    lua.create_thread(callback)?.into_async::<()>(args)?.await?;
    Ok(())
}

fn drop_key(lua: &Lua, key: Option<RegistryKey>) {
    if let Some(key) = key {
        let _ = lua.remove_registry_value(key);
    }
}

fn callback_from_key(
    lua: &Lua,
    key: Option<RegistryKey>,
    args: MultiValue,
) -> LuaResult<Option<(Function, MultiValue)>> {
    let Some(key) = key else {
        return Ok(None);
    };
    let f = lua.registry_value::<Function>(&key);
    drop_key(lua, Some(key));
    f.map(|f| Some((f, args)))
}

fn keep_callback(lua: &Lua, slot: &Option<RegistryKey>) -> LuaResult<Option<Function>> {
    slot.as_ref()
        .map(|key| lua.registry_value::<Function>(key))
        .transpose()
}

fn one_err_arg(lua: &Lua, err: Option<&str>) -> LuaResult<MultiValue> {
    Ok(MultiValue::from_vec(vec![opt_string_value(lua, err)?]))
}

fn read_args(lua: &Lua, data: &[u8]) -> LuaResult<MultiValue> {
    Ok(MultiValue::from_vec(vec![
        Value::Nil,
        Value::String(lua.create_string(data)?),
    ]))
}

fn opt_string_value(lua: &Lua, value: Option<&str>) -> LuaResult<Value> {
    match value {
        Some(s) => Ok(Value::String(lua.create_string(s)?)),
        None => Ok(Value::Nil),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_callback(lua: &Lua) -> RegistryKey {
        let f: Function = lua.load("return function() end").eval().unwrap();
        lua.create_registry_value(f).unwrap()
    }

    /// A connected handle wired straight into the store, no connect task:
    /// {unread} rides on it and {on_read} decides whether delivery is armed.
    fn connected_handle(
        store: &mut UvStore,
        on_read: Option<RegistryKey>,
        unread: &[u8],
        reader: Reader,
    ) -> u32 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut connected = Connected::new(Arc::new(smol::Async::new(stream).unwrap()), on_read);
        connected.unread = unread.to_vec();
        connected.reader = reader;
        let id = store.next_id;
        store.next_id += 1;
        store.handles.insert(
            id,
            UvMeta::new(
                Arc::from("uv-test"),
                Kind::Tcp {
                    live: TcpLive::Connected(connected),
                },
            ),
        );
        id
    }

    /// Chunks buffered while delivery was disarmed are a prefix of the
    /// stream: replaying them must not land behind bytes the reader has
    /// produced since.
    #[test]
    fn buffered_bytes_stay_ahead_of_fresh_ones() {
        let lua = Lua::new();
        let mut store = UvStore::new();
        let id = connected_handle(&mut store, Some(read_callback(&lua)), b"AB", Reader::On);
        let (_, args) = store
            .absorb(&lua, id, UvEvent::Read(Ok(Some(b"CD".to_vec()))))
            .unwrap()
            .unwrap();
        let args: Vec<Value> = args.into_iter().collect();
        assert!(matches!(args[0], Value::Nil), "no error on a fresh chunk");
        let Value::String(data) = &args[1] else {
            panic!("expected the chunk as second argument");
        };
        assert_eq!(data.to_str().unwrap(), "ABCD");
    }

    /// A read error that lands while delivery is disarmed is buffered like
    /// any chunk: the next `read_start` replays it as `cb(err, nil)` instead
    /// of spawning a reader on an already-broken stream.
    #[test]
    fn read_error_while_disarmed_replays_on_the_next_read_start() {
        let lua = Lua::new();
        let mut store = UvStore::new();
        let id = connected_handle(&mut store, None, b"", Reader::On);
        store
            .absorb(
                &lua,
                id,
                UvEvent::Read(Err("connection reset by peer".to_string())),
            )
            .unwrap();
        store.begin_read(id, read_callback(&lua)).ok().unwrap();
        assert!(!store.is_active(id), "an ended stream spawns no reader");
        let (_, args) = store.absorb(&lua, id, UvEvent::Replay).unwrap().unwrap();
        let args: Vec<Value> = args.into_iter().collect();
        let Value::String(err) = &args[0] else {
            panic!("expected the buffered error as first argument");
        };
        assert_eq!(err.to_str().unwrap(), "connection reset by peer");
        assert!(matches!(args.get(1), None | Some(Value::Nil)));
    }
}
