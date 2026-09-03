use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::Duration,
};

const REQ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("instance offline")]
    Offline,
    #[error("request timed out")]
    Timeout,
    #[error("instance dropped the tunnel")]
    Disconnected,
}

/// One HTTP request/response exchange routed through an instance tunnel.
pub struct TunnelResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub final_chunk: bool,
}

/// Anchor-side messages bound for the instance over its tunnel.
pub enum TunnelCommand {
    Request { conn_id: u64, request: String },
}

/// Asynchronous instance -> anchor pushes that need no response.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum TunnelPush {
    SessionIndex {
        instance_name: String,
        sessions: Vec<SessionIndexEntry>,
    },
}

#[derive(Debug, serde::Deserialize)]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub title: String,
    pub model: String,
    pub cwd: String,
    pub status: String,
    #[serde(default)]
    pub cost_cents: i64,
    #[serde(default)]
    pub tokens_in: i64,
    #[serde(default)]
    pub tokens_out: i64,
    #[serde(default)]
    pub context_window: i64,
}

pub struct InstanceConnection {
    /// Sender consumed by the tunnel writer thread, which writes to the socket.
    pub commands: Sender<TunnelCommand>,
}

struct Pending {
    responses: Mutex<HashMap<u64, Sender<TunnelResponse>>>,
    next_conn_id: Mutex<u64>,
}

impl Pending {
    fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
            next_conn_id: Mutex::new(1),
        }
    }
    fn register(&self) -> (u64, Receiver<TunnelResponse>) {
        let (tx, rx) = channel();
        let mut next = self.next_conn_id.lock().unwrap();
        let conn_id = *next;
        *next = next.wrapping_add(1).max(1);
        drop(next);
        self.responses.lock().unwrap().insert(conn_id, tx);
        (conn_id, rx)
    }

    /// Deliver to the waiting request. Non-final chunks keep the slot alive
    /// (an SSE stream sends many); a final chunk or slot eviction consumes the
    /// sender, so a late frame after timeout cannot resurrect a dead slot.
    fn deliver(&self, conn_id: u64, response: TunnelResponse) {
        let mut responses = self.responses.lock().unwrap();
        let sender = responses.remove(&conn_id);
        if let Some(sender) = sender {
            let final_chunk = response.final_chunk;
            let delivered = sender.send(response).is_ok();
            if delivered && !final_chunk {
                responses.insert(conn_id, sender);
            }
        }
    }

    fn drop_all(&self) {
        self.responses.lock().unwrap().clear();
    }
}

pub struct Hub {
    connections: Mutex<HashMap<i64, InstanceConnection>>,
    pending: Mutex<Pending>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(HashMap::new()),
            pending: Mutex::new(Pending::new()),
        })
    }

    pub fn attach(&self, instance_id: i64, commands: Sender<TunnelCommand>) {
        self.connections
            .lock()
            .unwrap()
            .insert(instance_id, InstanceConnection { commands });
    }

    pub fn detach(&self, instance_id: i64) {
        self.connections.lock().unwrap().remove(&instance_id);
        self.pending.lock().unwrap().drop_all();
    }

    pub fn is_online(&self, instance_id: i64) -> bool {
        self.connections.lock().unwrap().contains_key(&instance_id)
    }

    /// Send one serialized HTTP request over the tunnel and wait for its first
    /// response chunk. `deliver_response` feeds later chunks back as they arrive.
    pub fn request(
        &self,
        instance_id: i64,
        request: String,
    ) -> Result<(u64, Receiver<TunnelResponse>), HubError> {
        let commands = {
            let conns = self.connections.lock().unwrap();
            conns
                .get(&instance_id)
                .map(|c| c.commands.clone())
                .ok_or(HubError::Offline)?
        };
        let (conn_id, rx) = self.pending.lock().unwrap().register();
        commands
            .send(TunnelCommand::Request { conn_id, request })
            .map_err(|_| HubError::Offline)?;
        Ok((conn_id, rx))
    }

    pub fn wait_first(&self, rx: &Receiver<TunnelResponse>) -> Result<TunnelResponse, HubError> {
        match rx.recv_timeout(REQ_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(RecvTimeoutError::Timeout) => Err(HubError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(HubError::Timeout),
        }
    }

    /// Blocks until the next chunk. A chunk with `final_chunk` ends the stream.
    pub fn wait_chunk(&self, rx: &Receiver<TunnelResponse>) -> Result<TunnelResponse, HubError> {
        match rx.recv_timeout(REQ_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(RecvTimeoutError::Timeout) => Err(HubError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(HubError::Disconnected),
        }
    }
}

impl Hub {
    pub fn deliver_response(&self, conn_id: u64, response: TunnelResponse) {
        self.pending.lock().unwrap().deliver(conn_id, response);
    }
}
