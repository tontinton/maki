//! Relays sessions of other project folders to one child `maki acp` per folder,
//! so plugins, `init.lua`, `.env`, the process cwd and `AGENTS.md` stay per
//! project.
//!
//! Contract with the server loop, which owns the [`Relay`] and calls it
//! sequentially:
//! - `initialize`: [`Relay::remember_initialize`], replayed to every child.
//! - `session/new` / `session/load` whose canonical cwd is another folder:
//!   [`Relay::open`]. The relay answers the client itself.
//! - every other client request or notification: [`Relay::forward`] first; `true`
//!   means a child owns its `sessionId` and the server must not handle it.
//! - every client response: [`Relay::on_client_response`] first; `true` means it
//!   answered a child's request.
//! - every [`RelayEvent`] the sink delivers: [`Relay::on_event`].
//! - shutdown: [`Relay::shutdown`].
//!
//! Children never touch server state: their output comes back through the sink
//! as events, and everything for the client goes out on `out_tx`.
//!
//! A child runs with [`RELAYED_ENV`] set and never relays further.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema::v1::{Error as AcpError, ErrorCode};
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child as Process, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::timeout;
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info, warn};

/// Set in every child's env. A process that sees it runs every session itself.
pub const RELAYED_ENV: &str = "MAKI_ACP_RELAYED";
const RELAYED_VALUE: &str = "1";
const JSONRPC_VERSION: &str = "2.0";
const INITIALIZE: &str = "initialize";
const SESSION_NEW: &str = "session/new";
const SESSION_LOAD: &str = "session/load";
const SESSION_CLOSE: &str = "session/close";
/// Ids of child requests as the client sees them. The server's own asks go out
/// as numbers and it only reads numeric strings back as its ids, so no answer
/// can be claimed by both.
const ASK_ID_PREFIX: &str = "relay-";
/// How long a child that closed its output gets to report its exit status.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// A child that died at startup closes its stderr with it. Anything it
/// spawned may hold the pipe longer, and the exit should not wait for that.
const STDERR_GRACE: Duration = Duration::from_secs(1);
/// Enough for an eyre report's headline and its first causes.
const STDERR_TAIL_LINES: usize = 8;
const ESC: char = '\x1b';
const CSI_START: char = '[';
/// A child closes its sessions on end of input, `SessionEnd` hooks included.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const AGENT_FOR: &str = "the maki agent for";
const CLOSED_OUTPUT: &str = "closed its output";
const READ_FAILED: &str = "could not be read";
const SPAWN_FAILED: &str = "could not start";
const INIT_FAILED: &str = "failed to initialize";
const PIPES_MISSING: &str = "child stdio was not piped";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildId(u64);

#[derive(Debug)]
pub enum RelayEvent {
    Line { child: ChildId, line: String },
    Exited { child: ChildId, reason: String },
}

/// Delivers [`RelayEvent`]s to the server loop. It should hold the loop's
/// sender weakly, or running children keep the loop alive after stdin closes.
pub type EventSink = Arc<dyn Fn(RelayEvent) + Send + Sync>;

/// Starts the agent for a folder. A field so tests run children in memory.
pub type Spawner = Box<dyn Fn(&Path) -> io::Result<ChildIo> + Send + Sync>;

pub struct ChildIo {
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    /// Copied to ours. Its last lines say why a child that never answered died.
    pub stderr: Box<dyn AsyncRead + Send + Unpin>,
    /// Waited on for the exit status, killed when dropped.
    pub process: Option<Process>,
}

enum InFlight {
    Initialize,
    Client { id: Value, on_success: OnSuccess },
}

enum OnSuccess {
    Nothing,
    BindNew,
    Bind(Box<str>),
    Unbind(Box<str>),
}

struct Child {
    folder: PathBuf,
    stdin: UnboundedSender<String>,
    /// Lines held until the child answers `initialize`; `None` once it did.
    queued: Option<Vec<String>>,
    /// Keyed by the id the child saw.
    in_flight: HashMap<u64, InFlight>,
    _writer: AbortOnDropHandle<()>,
    supervisor: AbortOnDropHandle<()>,
}

impl Child {
    fn write(&mut self, msg: &Value) {
        let line = encode(msg);
        match &mut self.queued {
            Some(queued) => queued.push(line),
            None => {
                let _ = self.stdin.send(line);
            }
        }
    }

    fn start(&mut self) {
        for line in self.queued.take().unwrap_or_default() {
            let _ = self.stdin.send(line);
        }
    }
}

pub struct Relay {
    spawner: Spawner,
    sink: EventSink,
    out_tx: UnboundedSender<Value>,
    initialize_params: Value,
    children: HashMap<ChildId, Child>,
    sessions: HashMap<Box<str>, ChildId>,
    /// Client-facing id of a child request -> its child and original id.
    asks: HashMap<String, (ChildId, Value)>,
    /// Shared by child ids, relayed request ids and ask ids; each only needs
    /// to be unique within its own kind.
    next_id: u64,
}

impl Relay {
    /// Children run the current executable with this process's arguments and
    /// exactly `child_env`.
    pub fn new(
        child_env: Vec<(OsString, OsString)>,
        out_tx: UnboundedSender<Value>,
        sink: EventSink,
    ) -> Self {
        let spawner = Box::new(move |folder: &Path| spawn_process(folder, &child_env));
        Self::with_spawner(spawner, out_tx, sink)
    }

    pub fn with_spawner(spawner: Spawner, out_tx: UnboundedSender<Value>, sink: EventSink) -> Self {
        Self {
            spawner,
            sink,
            out_tx,
            initialize_params: Value::Null,
            children: HashMap::new(),
            sessions: HashMap::new(),
            asks: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn remember_initialize(&mut self, raw: &Value) {
        self.initialize_params = raw.get("params").cloned().unwrap_or(Value::Null);
    }

    /// The folder of the child that runs session `sid`, if one does.
    pub fn session_folder(&self, sid: &str) -> Option<&Path> {
        let child = self.sessions.get(sid)?;
        self.children.get(child).map(|state| state.folder.as_path())
    }

    /// Relays a `session/new` or `session/load` to the child of `folder`,
    /// starting one if there is none. `folder` must be canonical.
    pub fn open(&mut self, folder: &Path, raw: &Value) {
        let Some(client_id) = raw.get("id").cloned() else {
            return;
        };
        let on_success = match method(raw) {
            Some(SESSION_LOAD) => {
                session_id(raw).map_or(OnSuccess::Nothing, |sid| OnSuccess::Bind(sid.into()))
            }
            _ => OnSuccess::BindNew,
        };
        match self.child_for(folder) {
            Ok(child) => self.relay_request(child, raw, client_id, on_success),
            Err(e) => {
                warn!(folder = %folder.display(), error = %e, "could not start acp child");
                self.reply_error(
                    client_id,
                    &failure_message(folder, &format!("{SPAWN_FAILED}: {e}")),
                );
            }
        }
    }

    /// `true` when a child owns the message's `sessionId` and got the message.
    /// Opening methods are left to the server, which routes them by cwd.
    pub fn forward(&mut self, raw: &Value) -> bool {
        let method_name = method(raw);
        if matches!(method_name, Some(SESSION_NEW | SESSION_LOAD)) {
            return false;
        }
        let Some(sid) = session_id(raw) else {
            return false;
        };
        let Some(&child) = self.sessions.get(sid) else {
            return false;
        };
        match raw.get("id") {
            Some(id) => {
                let on_success = match method_name {
                    Some(SESSION_CLOSE) => OnSuccess::Unbind(sid.into()),
                    _ => OnSuccess::Nothing,
                };
                self.relay_request(child, raw, id.clone(), on_success);
            }
            None => {
                if let Some(state) = self.children.get_mut(&child) {
                    state.write(raw);
                }
            }
        }
        true
    }

    /// `true` when `raw` answers a child's request. An answer for a child that
    /// is gone is still the relay's, and is dropped.
    pub fn on_client_response(&mut self, raw: &Value) -> bool {
        let Some(id) = raw.get("id").and_then(Value::as_str) else {
            return false;
        };
        if !id.starts_with(ASK_ID_PREFIX) {
            return false;
        }
        let Some((child, original)) = self.asks.remove(id) else {
            debug!(id, "answer for a relayed request nobody waits on");
            return true;
        };
        if let Some(state) = self.children.get_mut(&child) {
            let mut msg = raw.clone();
            msg["id"] = original;
            state.write(&msg);
        }
        true
    }

    pub fn on_event(&mut self, event: RelayEvent) {
        match event {
            RelayEvent::Line { child, line } => self.on_child_line(child, &line),
            RelayEvent::Exited { child, reason } => self.forget_child(child, &reason),
        }
    }

    /// Closes every child's stdin, which ends `maki acp`, and kills the ones
    /// still running after the grace period.
    pub async fn shutdown(&mut self) {
        self.sessions.clear();
        self.asks.clear();
        let supervisors: Vec<_> = self
            .children
            .drain()
            .map(|(_, child)| child.supervisor)
            .collect();
        if timeout(SHUTDOWN_GRACE, join_all(supervisors))
            .await
            .is_err()
        {
            warn!(grace = ?SHUTDOWN_GRACE, "acp children outlived shutdown, killing them");
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn child_for(&mut self, folder: &Path) -> io::Result<ChildId> {
        if let Some((&child, _)) = self
            .children
            .iter()
            .find(|(_, state)| state.folder == folder)
        {
            return Ok(child);
        }
        let ChildIo {
            stdin,
            stdout,
            stderr,
            process,
        } = (self.spawner)(folder)?;
        let child = ChildId(self.next_id());
        let init_id = self.next_id();
        let (stdin_tx, stdin_rx) = unbounded_channel();
        let initialize = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": init_id,
            "method": INITIALIZE,
            "params": self.initialize_params,
        });
        let _ = stdin_tx.send(encode(&initialize));
        let supervisor = tokio::spawn(supervise(
            child,
            stdout,
            stderr,
            process,
            Arc::clone(&self.sink),
        ));
        self.children.insert(
            child,
            Child {
                folder: folder.to_path_buf(),
                stdin: stdin_tx,
                queued: Some(Vec::new()),
                in_flight: HashMap::from([(init_id, InFlight::Initialize)]),
                _writer: AbortOnDropHandle::new(tokio::spawn(write_lines(stdin, stdin_rx))),
                supervisor: AbortOnDropHandle::new(supervisor),
            },
        );
        info!(child = child.0, folder = %folder.display(), "started acp child");
        Ok(child)
    }

    fn relay_request(
        &mut self,
        child: ChildId,
        raw: &Value,
        client_id: Value,
        on_success: OnSuccess,
    ) {
        let relay_id = self.next_id();
        let Some(state) = self.children.get_mut(&child) else {
            return;
        };
        let mut msg = raw.clone();
        msg["id"] = relay_id.into();
        state.in_flight.insert(
            relay_id,
            InFlight::Client {
                id: client_id,
                on_success,
            },
        );
        state.write(&msg);
    }

    fn on_child_line(&mut self, child: ChildId, line: &str) {
        if !self.children.contains_key(&child) {
            return;
        }
        let mut msg: Value = match serde_json::from_str(line) {
            Ok(msg) => msg,
            Err(e) => {
                warn!(child = child.0, error = %e, "invalid JSON from acp child");
                return;
            }
        };
        if msg.get("method").is_none() {
            self.on_child_response(child, msg);
            return;
        }
        if let Some(original) = msg.get_mut("id").map(Value::take) {
            let ask = format!("{ASK_ID_PREFIX}{}", self.next_id());
            msg["id"] = Value::String(ask.clone());
            self.asks.insert(ask, (child, original));
        }
        let _ = self.out_tx.send(msg);
    }

    fn on_child_response(&mut self, child: ChildId, mut msg: Value) {
        let Some(state) = self.children.get_mut(&child) else {
            return;
        };
        let relay_id = msg.get("id").and_then(Value::as_u64);
        let Some(in_flight) = relay_id.and_then(|id| state.in_flight.remove(&id)) else {
            debug!(
                child = child.0,
                ?relay_id,
                "acp child answered an unknown request"
            );
            return;
        };
        let error = msg.get("error");
        match in_flight {
            InFlight::Initialize => match error {
                None => state.start(),
                Some(error) => {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.forget_child(child, &format!("{INIT_FAILED}: {message}"));
                }
            },
            InFlight::Client { id, on_success } => {
                if error.is_none() {
                    self.bind(child, on_success, &msg);
                }
                msg["id"] = id;
                let _ = self.out_tx.send(msg);
            }
        }
    }

    fn bind(&mut self, child: ChildId, on_success: OnSuccess, response: &Value) {
        match on_success {
            OnSuccess::Nothing => {}
            OnSuccess::BindNew => {
                if let Some(sid) = response
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                {
                    self.sessions.insert(sid.into(), child);
                }
            }
            OnSuccess::Bind(sid) => {
                self.sessions.insert(sid, child);
            }
            OnSuccess::Unbind(sid) => {
                self.sessions.remove(&sid);
            }
        }
    }

    /// Fails what the child still owes the client and forgets its sessions and
    /// requests. Dropping the child kills it if it still runs.
    fn forget_child(&mut self, child: ChildId, reason: &str) {
        let Some(state) = self.children.remove(&child) else {
            return;
        };
        warn!(
            child = child.0,
            folder = %state.folder.display(),
            reason,
            failed_requests = state.in_flight.len(),
            "acp child gone"
        );
        let message = failure_message(&state.folder, reason);
        for in_flight in state.in_flight.into_values() {
            if let InFlight::Client { id, .. } = in_flight {
                self.reply_error(id, &message);
            }
        }
        self.sessions.retain(|_, owner| *owner != child);
        self.asks.retain(|_, (owner, _)| *owner != child);
    }

    fn reply_error(&self, id: Value, message: &str) {
        let error = AcpError::new(ErrorCode::InternalError.into(), message);
        let _ = self
            .out_tx
            .send(json!({"jsonrpc": JSONRPC_VERSION, "id": id, "error": error}));
    }
}

fn spawn_process(folder: &Path, child_env: &[(OsString, OsString)]) -> io::Result<ChildIo> {
    let mut process = Command::new(env::current_exe()?)
        .args(env::args_os().skip(1))
        .current_dir(folder)
        .env_clear()
        .envs(child_env.iter().map(|(key, value)| (key, value)))
        .env(RELAYED_ENV, RELAYED_VALUE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (
        process.stdin.take(),
        process.stdout.take(),
        process.stderr.take(),
    ) else {
        return Err(io::Error::other(PIPES_MISSING));
    };
    Ok(ChildIo {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        process: Some(process),
    })
}

async fn write_lines(
    mut stdin: Box<dyn AsyncWrite + Send + Unpin>,
    mut lines: UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        if let Err(e) = write_line(&mut stdin, &line).await {
            debug!(error = %e, "acp child stdin closed");
            return;
        }
    }
}

async fn write_line(stdin: &mut (dyn AsyncWrite + Send + Unpin), line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

async fn supervise(
    child: ChildId,
    stdout: Box<dyn AsyncRead + Send + Unpin>,
    stderr: Box<dyn AsyncRead + Send + Unpin>,
    process: Option<Process>,
    sink: EventSink,
) {
    let stderr_tail = tokio::spawn(copy_stderr(stderr));
    let mut lines = BufReader::new(stdout).lines();
    let reason = loop {
        match lines.next_line().await {
            Ok(Some(line)) => sink(RelayEvent::Line { child, line }),
            Ok(None) => break exit_reason(process).await,
            Err(e) => break format!("{READ_FAILED}: {e}"),
        }
    };
    let tail = timeout(STDERR_GRACE, stderr_tail)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let reason = match tail.is_empty() {
        true => reason,
        false => format!("{reason}:\n{}", tail.join("\n")),
    };
    sink(RelayEvent::Exited { child, reason });
}

/// The child's logs stay on our stderr, as they were before the relay. The
/// tail keeps them without color, since it ends up in an editor's message.
async fn copy_stderr(stderr: Box<dyn AsyncRead + Send + Unpin>) -> Vec<String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut ours = tokio::io::stderr();
    let mut tail = VecDeque::with_capacity(STDERR_TAIL_LINES);
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = ours.write_all(line.as_bytes()).await;
        let _ = ours.write_all(b"\n").await;
        let plain = strip_ansi(&line);
        if plain.trim().is_empty() {
            continue;
        }
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(plain);
    }
    tail.into()
}

/// Drops CSI escape sequences, the only kind an eyre report or a log uses.
fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != ESC {
            plain.push(c);
            continue;
        }
        if chars.next() == Some(CSI_START) {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    plain
}

async fn exit_reason(process: Option<Process>) -> String {
    let Some(mut process) = process else {
        return CLOSED_OUTPUT.into();
    };
    match timeout(EXIT_GRACE, process.wait()).await {
        Ok(Ok(status)) => format!("exited ({status})"),
        Ok(Err(e)) => format!("{CLOSED_OUTPUT}: {e}"),
        Err(_) => CLOSED_OUTPUT.into(),
    }
}

fn encode(msg: &Value) -> String {
    let mut line = msg.to_string();
    line.push('\n');
    line
}

fn method(raw: &Value) -> Option<&str> {
    raw.get("method").and_then(Value::as_str)
}

fn session_id(raw: &Value) -> Option<&str> {
    raw.pointer("/params/sessionId").and_then(Value::as_str)
}

fn failure_message(folder: &Path, reason: &str) -> String {
    format!("{AGENT_FOR} {} {reason}", folder.display())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, duplex};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::{
        AcpError, ChildIo, ErrorCode, EventSink, JSONRPC_VERSION, RelayEvent, Spawner, encode,
    };

    const PIPE_CAPACITY: usize = 64 * 1024;
    const CHILD_GONE: &str = "the relay closed the fake child's input";

    /// The other end of a child's stdio, kept in memory.
    pub struct FakeChild {
        pub folder: PathBuf,
        pub input: Lines<BufReader<DuplexStream>>,
        pub output: DuplexStream,
        pub errors: DuplexStream,
    }

    impl FakeChild {
        pub async fn recv(&mut self) -> Value {
            let line = self.input.next_line().await.unwrap().expect(CHILD_GONE);
            serde_json::from_str(&line).unwrap()
        }

        pub async fn send(&mut self, msg: &Value) {
            self.output.write_all(encode(msg).as_bytes()).await.unwrap();
        }

        pub async fn answer(&mut self, request: &Value, result: Value) {
            self.send(&json!({"jsonrpc": JSONRPC_VERSION, "id": request["id"], "result": result}))
                .await;
        }

        pub async fn reject(&mut self, request: &Value, message: &str) {
            let error = AcpError::new(ErrorCode::InvalidParams.into(), message);
            self.send(&json!({"jsonrpc": JSONRPC_VERSION, "id": request["id"], "error": error}))
                .await;
        }
    }

    /// Every child it starts arrives on the receiver.
    pub fn fake_spawner() -> (Spawner, UnboundedReceiver<FakeChild>) {
        let (spawned_tx, spawned_rx) = unbounded_channel();
        let spawner: Spawner = Box::new(move |folder: &Path| {
            let (stdin, child_stdin) = duplex(PIPE_CAPACITY);
            let (child_stdout, stdout) = duplex(PIPE_CAPACITY);
            let (child_stderr, stderr) = duplex(PIPE_CAPACITY);
            let _ = spawned_tx.send(FakeChild {
                folder: folder.to_path_buf(),
                input: BufReader::new(child_stdin).lines(),
                output: child_stdout,
                errors: child_stderr,
            });
            Ok(ChildIo {
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
                stderr: Box::new(stderr),
                process: None,
            })
        });
        (spawner, spawned_rx)
    }

    /// A sink whose events the test feeds to the relay itself.
    pub fn event_channel() -> (EventSink, UnboundedReceiver<RelayEvent>) {
        let (events_tx, events_rx) = unbounded_channel();
        let sink: EventSink = Arc::new(move |event: RelayEvent| {
            let _ = events_tx.send(event);
        });
        (sink, events_rx)
    }
}

#[cfg(test)]
mod tests {
    use std::iter;

    use test_case::test_case;

    use super::test_support::{FakeChild, event_channel, fake_spawner};
    use super::*;

    const FOLDER_A: &str = "/relay-test/alpha";
    const FOLDER_B: &str = "/relay-test/beta";
    const SESSION: &str = "sess-relayed";
    const SESSION_PROMPT: &str = "session/prompt";
    const SESSION_CANCEL: &str = "session/cancel";
    const SESSION_UPDATE: &str = "session/update";
    const REQUEST_PERMISSION: &str = "session/request_permission";
    const CLIENT_NAME: &str = "relay-test-client";
    const INIT_REJECTED: &str = "unsupported protocol version";
    const LOAD_REJECTED: &str = "no such session";
    const CLIENT_ID: i64 = 7;
    const CLIENT_STRING_ID: &str = "client-request";
    const PROMPT_ID: i64 = 8;
    const CLOSE_ID: i64 = 9;
    const CHILD_ASK_ID: i64 = 1000;
    const UPDATE_COUNT: usize = 5;
    const NO_SPAWN: &str = "a child was spawned";
    const CLIENT_GOT_NOTHING: &str = "a message reached the client";
    const CRASH_REASON: &str = "invalid config: unknown key `modle`";
    const CRASH_COLOR: &str = "\x1b[31m";
    const CRASH_RESET: &str = "\x1b[0m";
    const NOT_BOUND_YET: &str = "a session is bound only once its open is answered";

    enum Startup {
        Rejected,
        Exited,
        Crashed,
    }

    struct Harness {
        relay: Relay,
        out_rx: UnboundedReceiver<Value>,
        events_rx: UnboundedReceiver<RelayEvent>,
        spawned_rx: UnboundedReceiver<FakeChild>,
    }

    impl Harness {
        fn new() -> Self {
            let (out_tx, out_rx) = unbounded_channel();
            let (sink, events_rx) = event_channel();
            let (spawner, spawned_rx) = fake_spawner();
            let mut relay = Relay::with_spawner(spawner, out_tx, sink);
            relay.remember_initialize(&request(json!(0), INITIALIZE, initialize_params()));
            Self {
                relay,
                out_rx,
                events_rx,
                spawned_rx,
            }
        }

        async fn pump(&mut self) {
            let event = self.events_rx.recv().await.unwrap();
            self.relay.on_event(event);
        }

        fn next_to_client(&mut self) -> Value {
            self.out_rx.try_recv().expect(CLIENT_GOT_NOTHING)
        }

        fn spawned(&mut self) -> FakeChild {
            self.spawned_rx.try_recv().expect(NO_SPAWN)
        }

        async fn ready_child(&mut self, request: &Value) -> FakeChild {
            self.relay.open(Path::new(FOLDER_A), request);
            let mut child = self.spawned();
            let initialize = child.recv().await;
            child.answer(&initialize, json!({})).await;
            self.pump().await;
            child
        }

        async fn open_session(&mut self) -> FakeChild {
            let mut child = self.ready_child(&new_session(json!(CLIENT_ID))).await;
            let relayed = child.recv().await;
            child.answer(&relayed, json!({"sessionId": SESSION})).await;
            self.pump().await;
            self.next_to_client();
            child
        }
    }

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({"jsonrpc": JSONRPC_VERSION, "id": id, "method": method, "params": params})
    }

    fn new_session(id: Value) -> Value {
        request(id, SESSION_NEW, json!({"cwd": FOLDER_A, "mcpServers": []}))
    }

    fn session_request(id: i64, method: &str) -> Value {
        request(json!(id), method, json!({"sessionId": SESSION}))
    }

    fn initialize_params() -> Value {
        json!({"protocolVersion": 1, "clientInfo": {"name": CLIENT_NAME}})
    }

    fn error_message(response: &Value) -> &str {
        response["error"]["message"].as_str().unwrap()
    }

    #[tokio::test]
    async fn initialize_is_replayed_before_queued_requests() {
        let mut h = Harness::new();
        let open = new_session(json!(CLIENT_ID));
        h.relay.open(Path::new(FOLDER_A), &open);
        let mut child = h.spawned();

        let initialize = child.recv().await;
        assert_eq!(initialize["method"], INITIALIZE);
        assert_eq!(initialize["params"], initialize_params());
        child.answer(&initialize, json!({})).await;
        h.pump().await;

        let relayed = child.recv().await;
        assert_eq!(relayed["method"], SESSION_NEW);
        assert_eq!(relayed["params"], open["params"]);
        assert!(
            h.out_rx.try_recv().is_err(),
            "the relay's own initialize stays with it"
        );
    }

    #[test_case(Startup::Rejected, INIT_REJECTED ; "rejected_initialize")]
    #[test_case(Startup::Exited, CLOSED_OUTPUT ; "exit_before_ready")]
    #[test_case(Startup::Crashed, CRASH_REASON ; "crash_names_its_error")]
    #[tokio::test]
    async fn failed_startup_fails_queued_requests(startup: Startup, reason: &str) {
        let mut h = Harness::new();
        h.relay
            .open(Path::new(FOLDER_A), &new_session(json!(CLIENT_ID)));
        let mut child = h.spawned();
        let initialize = child.recv().await;
        match startup {
            Startup::Rejected => child.reject(&initialize, INIT_REJECTED).await,
            Startup::Exited => {
                drop(child.output);
                drop(child.errors);
            }
            Startup::Crashed => {
                let report = format!("{CRASH_COLOR}{CRASH_REASON}{CRASH_RESET}\n");
                child.errors.write_all(report.as_bytes()).await.unwrap();
                drop(child.output);
                drop(child.errors);
            }
        }
        h.pump().await;

        let response = h.next_to_client();
        assert_eq!(response["id"], CLIENT_ID);
        let message = error_message(&response);
        assert!(
            message.contains(FOLDER_A) && message.contains(reason),
            "{message}"
        );
        assert_eq!(
            child.input.next_line().await.unwrap(),
            None,
            "a queued request never reaches a child that failed to start"
        );
    }

    #[test_case(json!(CLIENT_ID) ; "numeric_id")]
    #[test_case(json!(CLIENT_STRING_ID) ; "string_id")]
    #[tokio::test]
    async fn request_ids_are_rewritten_both_ways(client_id: Value) {
        let mut h = Harness::new();
        let mut child = h.ready_child(&new_session(client_id.clone())).await;
        let relayed = child.recv().await;
        assert_ne!(relayed["id"], client_id);

        child.answer(&relayed, json!({"sessionId": SESSION})).await;
        h.pump().await;

        let response = h.next_to_client();
        assert_eq!(response["id"], client_id);
        assert_eq!(response["result"]["sessionId"], SESSION);
    }

    #[test_case(SESSION_NEW, json!({"cwd": FOLDER_A}), json!({"sessionId": SESSION}) ; "new_binds_the_returned_id")]
    #[test_case(SESSION_LOAD, json!({"cwd": FOLDER_A, "sessionId": SESSION}), json!({}) ; "load_binds_the_requested_id")]
    #[tokio::test]
    async fn opened_session_routes_to_its_child(method: &str, params: Value, result: Value) {
        let mut h = Harness::new();
        let mut child = h
            .ready_child(&request(json!(CLIENT_ID), method, params))
            .await;
        let relayed = child.recv().await;
        assert!(h.relay.session_folder(SESSION).is_none(), "{NOT_BOUND_YET}");
        child.answer(&relayed, result).await;
        h.pump().await;
        h.next_to_client();

        assert_eq!(h.relay.session_folder(SESSION), Some(Path::new(FOLDER_A)));
        assert!(h.relay.forward(&session_request(PROMPT_ID, SESSION_PROMPT)));
        let relayed = child.recv().await;
        assert_eq!(relayed["method"], SESSION_PROMPT);
        child
            .answer(&relayed, json!({"stopReason": "end_turn"}))
            .await;
        h.pump().await;
        assert_eq!(h.next_to_client()["id"], PROMPT_ID);
    }

    #[tokio::test]
    async fn failed_load_binds_nothing() {
        let mut h = Harness::new();
        let load = request(
            json!(CLIENT_ID),
            SESSION_LOAD,
            json!({"cwd": FOLDER_A, "sessionId": SESSION}),
        );
        let mut child = h.ready_child(&load).await;
        let relayed = child.recv().await;
        child.reject(&relayed, LOAD_REJECTED).await;
        h.pump().await;

        let response = h.next_to_client();
        assert_eq!(response["id"], CLIENT_ID);
        assert_eq!(error_message(&response), LOAD_REJECTED);
        assert!(h.relay.session_folder(SESSION).is_none());
        assert!(!h.relay.forward(&session_request(PROMPT_ID, SESSION_PROMPT)));
    }

    #[tokio::test]
    async fn close_unbinds_once_answered() {
        let mut h = Harness::new();
        let mut child = h.open_session().await;

        let cancel = json!({"jsonrpc": JSONRPC_VERSION, "method": SESSION_CANCEL, "params": {"sessionId": SESSION}});
        assert!(h.relay.forward(&cancel));
        assert_eq!(child.recv().await, cancel);

        assert!(h.relay.forward(&session_request(CLOSE_ID, SESSION_CLOSE)));
        let relayed = child.recv().await;
        assert!(h.relay.session_folder(SESSION).is_some(), "{NOT_BOUND_YET}");
        child.answer(&relayed, json!({})).await;
        h.pump().await;

        assert_eq!(h.next_to_client()["id"], CLOSE_ID);
        assert!(h.relay.session_folder(SESSION).is_none());
        assert!(!h.relay.forward(&session_request(PROMPT_ID, SESSION_PROMPT)));
    }

    #[tokio::test]
    async fn child_requests_get_relay_ids_and_answers_route_back() {
        let mut h = Harness::new();
        let mut child = h.open_session().await;
        let ask = request(
            json!(CHILD_ASK_ID),
            REQUEST_PERMISSION,
            json!({"sessionId": SESSION}),
        );
        child.send(&ask).await;
        h.pump().await;

        let relayed = h.next_to_client();
        assert_eq!(relayed["params"], ask["params"]);
        let ask_id = relayed["id"].as_str().unwrap();
        assert!(ask_id.starts_with(ASK_ID_PREFIX), "{ask_id}");

        let answer = json!({"jsonrpc": JSONRPC_VERSION, "id": ask_id, "result": {"outcome": {"outcome": "cancelled"}}});
        assert!(h.relay.on_client_response(&answer));
        let routed = child.recv().await;
        assert_eq!(routed["id"], CHILD_ASK_ID);
        assert_eq!(routed["result"], answer["result"]);

        let own = json!({"jsonrpc": JSONRPC_VERSION, "id": CHILD_ASK_ID, "result": {}});
        assert!(
            !h.relay.on_client_response(&own),
            "the server's own asks stay with it"
        );
    }

    #[tokio::test]
    async fn crash_fails_in_flight_requests_and_forgets_sessions() {
        let mut h = Harness::new();
        let mut child = h.open_session().await;
        assert!(h.relay.forward(&session_request(PROMPT_ID, SESSION_PROMPT)));
        child.recv().await;
        child
            .send(&request(
                json!(CHILD_ASK_ID),
                REQUEST_PERMISSION,
                json!({"sessionId": SESSION}),
            ))
            .await;
        h.pump().await;
        let ask_id = h.next_to_client()["id"].clone();

        drop(child);
        h.pump().await;

        let response = h.next_to_client();
        assert_eq!(response["id"], PROMPT_ID);
        let message = error_message(&response);
        assert!(
            message.contains(FOLDER_A) && message.contains(CLOSED_OUTPUT),
            "{message}"
        );
        assert!(h.relay.session_folder(SESSION).is_none());
        assert!(!h.relay.forward(&session_request(PROMPT_ID, SESSION_PROMPT)));
        assert!(
            h.relay.on_client_response(
                &json!({"jsonrpc": JSONRPC_VERSION, "id": ask_id, "result": {}})
            )
        );

        h.relay
            .open(Path::new(FOLDER_A), &new_session(json!(CLIENT_ID)));
        assert_eq!(
            h.spawned().folder,
            Path::new(FOLDER_A),
            "the next session respawns"
        );
    }

    #[tokio::test]
    async fn notifications_pass_through_in_order() {
        let mut h = Harness::new();
        let mut child = h.open_session().await;
        let updates: Vec<Value> = (0..UPDATE_COUNT)
            .map(|n| {
                json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "method": SESSION_UPDATE,
                    "params": {"sessionId": SESSION, "update": {"text": n}},
                })
            })
            .collect();
        for update in &updates {
            child.send(update).await;
        }
        for _ in &updates {
            h.pump().await;
        }

        let received: Vec<Value> = (0..updates.len()).map(|_| h.next_to_client()).collect();
        assert_eq!(received, updates);
    }

    #[tokio::test]
    async fn one_child_per_folder() {
        let mut h = Harness::new();
        for (n, folder) in [FOLDER_A, FOLDER_B, FOLDER_A].into_iter().enumerate() {
            h.relay.open(Path::new(folder), &new_session(json!(n)));
        }

        let folders: Vec<PathBuf> = iter::from_fn(|| h.spawned_rx.try_recv().ok())
            .map(|child| child.folder)
            .collect();
        assert_eq!(folders, [PathBuf::from(FOLDER_A), PathBuf::from(FOLDER_B)]);
    }

    #[tokio::test]
    async fn shutdown_closes_child_stdin() {
        let mut h = Harness::new();
        let FakeChild {
            mut input, output, ..
        } = h.open_session().await;
        let child_exits = async move {
            while input.next_line().await.unwrap().is_some() {}
            drop(output);
        };

        tokio::join!(h.relay.shutdown(), child_exits);
    }

    #[test_case("plain", "plain" ; "no_escapes")]
    #[test_case("\x1b[1;31merror\x1b[0m: bad", "error: bad" ; "colors")]
    #[test_case("\x1b[2Kcleared", "cleared" ; "erase_line")]
    fn strip_ansi_keeps_only_text(line: &str, expected: &str) {
        assert_eq!(strip_ansi(line), expected);
    }
}
