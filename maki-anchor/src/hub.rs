use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
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
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub final_chunk: bool,
}

/// Anchor-side messages bound for the instance over its tunnel.
pub enum TunnelCommand {
    Request {
        conn_id: u64,
        request: String,
    },
    /// The browser behind one stream is gone (tab closed, page reloaded,
    /// link revoked): the instance must retire its producer and subscriber.
    Cancel {
        conn_id: u64,
    },
    /// A JSON control frame to hand to the instance verbatim.
    Control(String),
}

/// Asynchronous instance -> anchor pushes that need no response. The owning
/// instance is the authenticated tunnel, never a field in the frame.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum TunnelPush {
    SessionIndex {
        sessions: Vec<SessionIndexEntry>,
    },
    /// `/rc down`: the instance asks the anchor to revoke the very link that
    /// fronts this tunnel, so shared URLs die with it rather than lingering
    /// for the rest of their TTL.
    LinkRevoke {
        link_revoke: String,
    },
    /// `/rc link`, `/rc link new`, `/rc link rm`: list this instance's
    /// anchor-side links, mint one, or revoke one. Each is answered with the
    /// fresh list as a control frame.
    LinkList {
        list_links: bool,
    },
    LinkMint {
        link_mint: MintSpec,
    },
    LinkRm {
        link_rm: String,
    },
}

#[derive(Debug, serde::Deserialize)]
pub struct MintSpec {
    #[serde(default)]
    pub rights: String,
    #[serde(default)]
    pub hours: Option<u64>,
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

/// A live tunnel for one instance. The epoch distinguishes a reconnect from
/// the connection it replaced, so the old connection's teardown cannot drop
/// the new one.
pub struct InstanceConnection {
    /// Sender consumed by the tunnel writer thread, which writes to the socket.
    commands: Sender<TunnelCommand>,
    epoch: u64,
    /// Hash of the control link minted for this tunnel, so proxying that link
    /// can slide its expiry forward.
    link_hash: String,
}

struct Slot {
    instance_id: i64,
    responses: Sender<TunnelResponse>,
}

struct Pending {
    responses: Mutex<HashMap<u64, Slot>>,
    next_conn_id: Mutex<u64>,
}

impl Pending {
    fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
            next_conn_id: Mutex::new(1),
        }
    }
    fn register(&self, instance_id: i64) -> (u64, Receiver<TunnelResponse>) {
        let (tx, rx) = channel();
        let mut next = self.next_conn_id.lock().unwrap();
        let conn_id = *next;
        *next = next.wrapping_add(1).max(1);
        drop(next);
        self.responses.lock().unwrap().insert(
            conn_id,
            Slot {
                instance_id,
                responses: tx,
            },
        );
        (conn_id, rx)
    }

    /// Deliver to the waiting request. Non-final chunks keep the slot alive
    /// (an SSE stream sends many); a final chunk or slot eviction consumes the
    /// sender, so a late frame after timeout cannot resurrect a dead slot.
    /// Frames only deliver to a slot belonging to the sending instance.
    fn deliver(&self, instance_id: i64, conn_id: u64, response: TunnelResponse) {
        let mut responses = self.responses.lock().unwrap();
        if !responses
            .get(&conn_id)
            .is_some_and(|s| s.instance_id == instance_id)
        {
            return;
        }
        let slot = responses.remove(&conn_id);
        if let Some(slot) = slot {
            let final_chunk = response.final_chunk;
            let delivered = slot.responses.send(response).is_ok();
            if delivered && !final_chunk {
                responses.insert(conn_id, slot);
            }
        }
    }

    fn drop_for_instance(&self, instance_id: i64) {
        self.responses
            .lock()
            .unwrap()
            .retain(|_, slot| slot.instance_id != instance_id);
    }

    fn remove(&self, conn_id: u64) {
        self.responses.lock().unwrap().remove(&conn_id);
    }
}

pub struct Hub {
    connections: Mutex<HashMap<i64, InstanceConnection>>,
    pending: Mutex<Pending>,
    next_epoch: AtomicU64,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(HashMap::new()),
            pending: Mutex::new(Pending::new()),
            next_epoch: AtomicU64::new(1),
        })
    }

    /// Attach a tunnel, returning its epoch. Re-attaching takes over routing;
    /// the replaced connection's writer exits when its command channel drops.
    pub fn attach(
        &self,
        instance_id: i64,
        commands: Sender<TunnelCommand>,
        link_hash: String,
    ) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        self.connections.lock().unwrap().insert(
            instance_id,
            InstanceConnection {
                commands,
                epoch,
                link_hash,
            },
        );
        epoch
    }

    /// The control link minted for the live tunnel of an instance, if any.
    pub fn link_hash(&self, instance_id: i64) -> Option<String> {
        self.connections
            .lock()
            .unwrap()
            .get(&instance_id)
            .map(|c| c.link_hash.clone())
    }

    /// Detach only if `epoch` is still the live connection. Returns whether
    /// this connection was current; pending requests are dropped only then,
    /// so a reconnect's teardown cannot kill the new tunnel's traffic.
    pub fn detach(&self, instance_id: i64, epoch: u64) -> bool {
        let current = {
            let mut conns = self.connections.lock().unwrap();
            match conns.get(&instance_id) {
                Some(c) if c.epoch == epoch => {
                    conns.remove(&instance_id);
                    true
                }
                _ => false,
            }
        };
        if current {
            self.pending.lock().unwrap().drop_for_instance(instance_id);
        }
        current
    }

    /// Forgets and kills the tunnel for an instance: dropping the command
    /// sender ends its writer thread, which closes the socket. Used when a
    /// link is closed from the dashboard; the instance notices the drop and
    /// re-registers for a fresh link.
    pub fn disconnect(&self, instance_id: i64) {
        let mut connections = self.connections.lock().unwrap();
        connections.remove(&instance_id);
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
        let (conn_id, rx) = self.pending.lock().unwrap().register(instance_id);
        if commands
            .send(TunnelCommand::Request { conn_id, request })
            .is_err()
        {
            self.pending.lock().unwrap().remove(conn_id);
            return Err(HubError::Offline);
        }
        Ok((conn_id, rx))
    }

    pub fn wait_first(&self, rx: &Receiver<TunnelResponse>) -> Result<TunnelResponse, HubError> {
        match rx.recv_timeout(REQ_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(RecvTimeoutError::Timeout) => Err(HubError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(HubError::Disconnected),
        }
    }

    /// Blocks until the next chunk. A chunk with `final_chunk` ends the stream.
    /// Fire-and-forget stream cancellation; an offline instance needs no
    /// notice, its next registration clears every old producer itself.
    pub fn cancel(&self, instance_id: i64, conn_id: u64) {
        let conns = self.connections.lock().unwrap();
        if let Some(conn) = conns.get(&instance_id) {
            let _ = conn.commands.send(TunnelCommand::Cancel { conn_id });
        }
    }

    /// Ask the instance side to send a control frame back (link lists,
    /// mint results). Reuses the response bus with a synthetic conn id the
    /// browser never sees.
    pub fn control(&self, instance_id: i64, frame: String) {
        let conns = self.connections.lock().unwrap();
        if let Some(conn) = conns.get(&instance_id) {
            let _ = conn.commands.send(TunnelCommand::Control(frame));
        }
    }

    pub fn wait_chunk(&self, rx: &Receiver<TunnelResponse>) -> Result<TunnelResponse, HubError> {
        match rx.recv_timeout(REQ_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(RecvTimeoutError::Timeout) => Err(HubError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(HubError::Disconnected),
        }
    }

    pub fn deliver_response(&self, instance_id: i64, conn_id: u64, response: TunnelResponse) {
        self.pending
            .lock()
            .unwrap()
            .deliver(instance_id, conn_id, response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_frame_is_not_a_push() {
        const RESPONSE: &str =
            r#"{"conn_id":1,"status":200,"content_type":"text/plain","body":"aGk=","final":true}"#;
        let push: Result<TunnelPush, _> = serde_json::from_str(RESPONSE);
        assert!(push.is_err(), "response must not parse as a push: {push:?}");
        let mint_like: Result<TunnelPush, _> = serde_json::from_str(r#"{"list_links":true}"#);
        assert!(matches!(mint_like, Ok(TunnelPush::LinkList { .. })));
    }

    #[test]
    fn stale_detach_leaves_the_live_tunnel_attached() {
        let hub = Hub::new();
        let (tx, _rx) = channel();
        let old_epoch = hub.attach(1, tx, "h1".into());
        let (tx2, _rx2) = channel();
        let new_epoch = hub.attach(1, tx2, "h2".into());
        assert!(!hub.detach(1, old_epoch), "old epoch must not detach");
        assert!(hub.is_online(1));
        assert!(hub.detach(1, new_epoch));
        assert!(!hub.is_online(1));
    }

    #[test]
    fn one_instances_disconnect_only_drops_its_own_pending() {
        let hub = Hub::new();
        let (tx_a, _rx_a) = channel();
        let (tx_b, _rx_b) = channel();
        let epoch_a = hub.attach(1, tx_a, "h".into());
        hub.attach(2, tx_b, "h".into());
        let (_conn_a, _ra) = hub.request(1, "{}".into()).unwrap();
        let (_conn_b, rb) = hub.request(2, "{}".into()).unwrap();

        hub.detach(1, epoch_a);
        assert!(
            matches!(rb.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "instance 2's in-flight request must survive instance 1's teardown"
        );
    }

    #[test]
    fn wrong_instance_cannot_deliver_into_anothers_slot() {
        let hub = Hub::new();
        let (tx, _rx) = channel();
        hub.attach(1, tx, "h".into());
        let (conn, rx) = hub.request(1, "{}".into()).unwrap();
        hub.deliver_response(
            999,
            conn,
            TunnelResponse {
                status: 200,
                content_type: None,
                body: vec![],
                final_chunk: true,
            },
        );
        assert!(rx.try_recv().is_err(), "slot untouched by foreign delivery");
    }
}
