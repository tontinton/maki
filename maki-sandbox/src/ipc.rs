use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use tracing::debug;

use crate::error::SandboxError;

pub const HANDSHAKE_VERSION: u32 = 2;
pub const MAX_MSG_LEN: usize = 16 * 1024 * 1024;

/// Call id used for messages that carry no associated request
/// (e.g. a fatal child error reported before any request was sent).
pub const NO_CALL_ID: u32 = 0;

#[derive(Serialize, Deserialize, Debug)]
pub struct Handshake {
    pub name: String,
    pub version: u32,
}

/// Send a JSON-encoded [`Handshake`] over the socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if serialization or the socket write fails.
pub fn send_handshake(sock: &mut UnixStream, name: &str) -> Result<(), SandboxError> {
    let msg = Handshake {
        name: name.to_string(),
        version: HANDSHAKE_VERSION,
    };
    let data = serde_json::to_vec(&msg)
        .map_err(|e| SandboxError::Ipc(format!("handshake serialize: {e}")))?;
    write_message(sock, &data)
}

/// Receive and validate a [`Handshake`] from the socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the message cannot be read or
/// deserialized, or its version does not match [`HANDSHAKE_VERSION`].
pub fn recv_handshake(sock: &mut UnixStream) -> Result<String, SandboxError> {
    let data = read_message(sock)?;
    let hs: Handshake = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("handshake deserialize: {e}")))?;
    if hs.version != HANDSHAKE_VERSION {
        return Err(SandboxError::Ipc(format!(
            "handshake version mismatch: expected {}, got {}",
            HANDSHAKE_VERSION, hs.version
        )));
    }
    Ok(hs.name)
}

pub const SYNC_READY: &[u8] = b"ready";
pub const SYNC_GO: &[u8] = b"go";

/// Send a raw sync message (see [`SYNC_READY`] / [`SYNC_GO`]).
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the socket write fails.
pub fn send_sync(sock: &mut UnixStream, msg: &[u8]) -> Result<(), SandboxError> {
    sock.write_all(msg)
        .map_err(|e| SandboxError::Ipc(format!("sync send: {e}")))
}

/// Receive a raw sync message, expecting `expected`.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the read fails or the received bytes
/// differ from `expected`.
pub fn recv_sync(sock: &mut UnixStream, expected: &[u8]) -> Result<(), SandboxError> {
    let mut buf = vec![0u8; expected.len()];
    sock.read_exact(&mut buf)
        .map_err(|e| SandboxError::Ipc(format!("sync recv: {e}")))?;
    if buf[..] != *expected {
        return Err(SandboxError::Ipc(format!(
            "unexpected sync message: expected {:?}, got {:?}",
            std::str::from_utf8(expected).unwrap_or("?"),
            std::str::from_utf8(&buf).unwrap_or("?")
        )));
    }
    Ok(())
}

/// Messages sent by the parent to the persistent sandbox child.
///
/// Every request carries a `call_id`; the child echoes it in the matching
/// response so the parent routes results by id.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ParentMsg {
    #[serde(rename = "exit")]
    Exit,
    #[serde(rename = "ls")]
    Ls { call_id: u32, path: String },
    #[serde(rename = "pwd")]
    Pwd { call_id: u32 },
    #[serde(rename = "cd")]
    Cd { call_id: u32, path: String },
    #[serde(rename = "exec")]
    Exec {
        call_id: u32,
        command: String,
        /// Sandbox-side working directory for the command. Never touches
        /// host paths: the caller translates before sending.
        workdir: Option<String>,
        /// Kill the command after this many seconds. `None` runs without a
        /// hard timeout (interactive use).
        timeout_secs: Option<u64>,
    },
    /// A filesystem operation executed inside the namespace.
    #[serde(rename = "fs")]
    Fs { call_id: u32, op: FsOp },
}

/// Messages sent by the sandbox child to the parent.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ChildMsg {
    /// Outcome of the child's one-time namespace and mount setup, sent before
    /// it enters the request loop. The parent waits for it, so a sandbox that
    /// could not isolate fails at construction instead of looking usable.
    /// Carries the child's own error so the parent rebuilds the same variant,
    /// which is how "this host cannot sandbox" stays distinguishable from a
    /// sandbox that broke.
    ///
    /// Failures after this point need no message: the child just exits and the
    /// parent's read fails, which already fails every pending waiter.
    #[serde(rename = "setup")]
    Setup { error: Option<SandboxError> },
    #[serde(rename = "ls_result")]
    LsResult {
        call_id: u32,
        entries: Vec<DirEntry>,
    },
    #[serde(rename = "pwd_result")]
    PwdResult { call_id: u32, path: String },
    #[serde(rename = "cd_result")]
    CdResult { call_id: u32 },
    #[serde(rename = "exec_result")]
    ExecResult {
        call_id: u32,
        output: String,
        exit_code: i32,
    },
    #[serde(rename = "fs_result")]
    FsResult { call_id: u32, result: FsResult },
}

/// A filesystem operation the child executes natively inside the namespace.
///
/// Except where noted, `path` is an absolute path translated by the calling
/// process (host → sandbox) before this message is built.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op")]
pub enum FsOp {
    #[serde(rename = "read")]
    Read {
        path: String,
        /// Upper bound on bytes returned; larger files error out.
        max_bytes: u64,
    },
    #[serde(rename = "metadata")]
    Metadata { path: String },
    #[serde(rename = "dir")]
    Dir { path: String, depth: u32 },
    #[serde(rename = "exists")]
    Exists { path: String },
    /// Content is base64-encoded bytes.
    #[serde(rename = "write")]
    Write { path: String, content: String },
    /// Content is base64-encoded bytes.
    #[serde(rename = "append")]
    Append { path: String, content: String },
    /// Content is base64-encoded bytes.
    #[serde(rename = "atomic_write")]
    AtomicWrite { path: String, content: String },
    #[serde(rename = "remove")]
    Remove { path: String, recursive: bool },
    #[serde(rename = "mkdir")]
    Mkdir { path: String, parents: bool },
    /// Absolute search root plus one or more glob patterns.
    #[serde(rename = "glob")]
    Glob {
        path: String,
        patterns: Vec<String>,
        gitignore: bool,
        sort_mtime: bool,
        limit: Option<usize>,
    },
    #[serde(rename = "grep")]
    Grep {
        path: Option<String>,
        pattern: String,
        include: Option<String>,
        context_before: usize,
        context_after: usize,
        limit: usize,
        max_line_bytes: usize,
    },
}

/// Result of an [`FsOp`]. Byte payloads stay base64 so JSON messages with
/// binary content stay compact.
#[derive(Serialize, Deserialize, Debug)]
pub enum FsResult {
    #[serde(rename = "ok")]
    Ok(FsReply),
    #[serde(rename = "error")]
    Err(String),
}

/// Successful [`FsOp`] reply.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "reply")]
pub enum FsReply {
    /// Read result: base64-encoded bytes.
    #[serde(rename = "bytes")]
    Bytes { data: String },
    /// Structured payload (metadata table, dir entries, exists bool, glob
    /// paths, grep entries). Paths inside are sandbox-side absolute paths;
    /// the caller translates them back to the host view.
    #[serde(rename = "value")]
    Value { payload: Value },
    /// Write/append/atomic_write/remove/mkdir succeeded.
    #[serde(rename = "done")]
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

fn write_message(sock: &mut UnixStream, data: &[u8]) -> Result<(), SandboxError> {
    let len: u32 = data
        .len()
        .try_into()
        .map_err(|_| SandboxError::Ipc("message too large".into()))?;
    let header = len.to_be_bytes();
    sock.write_all(&header)
        .map_err(|e| SandboxError::Ipc(format!("write header: {e}")))?;
    sock.write_all(data)
        .map_err(|e| SandboxError::Ipc(format!("write payload: {e}")))?;
    Ok(())
}

fn read_message(sock: &mut UnixStream) -> Result<Vec<u8>, SandboxError> {
    let mut header = [0u8; 4];
    sock.read_exact(&mut header)
        .map_err(|e| SandboxError::Ipc(format!("read header: {e}")))?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_MSG_LEN {
        return Err(SandboxError::Ipc(format!(
            "message too large: {len} bytes (max {MAX_MSG_LEN})"
        )));
    }
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf)
        .map_err(|e| SandboxError::Ipc(format!("read payload: {e}")))?;
    Ok(buf)
}

/// Send a [`ChildMsg`] to the parent process over the IPC socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if serialization or the socket write fails.
pub fn send_child_msg(sock: &mut UnixStream, msg: &ChildMsg) -> Result<(), SandboxError> {
    let label = child_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize child msg: {e}")))?;
    let r = write_message(sock, &data);
    let pid = std::process::id();
    debug!(pid = %pid, msg = %label, ok = r.is_ok(), "ipc: child send");
    r
}

/// Receive a [`ChildMsg`] from the parent process over the IPC socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the message cannot be read or
/// deserialized.
pub fn recv_child_msg(sock: &mut UnixStream) -> Result<ChildMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ChildMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize child msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: child recv");
    Ok(msg)
}

/// Send a [`ParentMsg`] to the child process over the IPC socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if serialization or the socket write fails.
pub fn send_parent_msg(sock: &mut UnixStream, msg: &ParentMsg) -> Result<(), SandboxError> {
    let label = parent_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize parent msg: {e}")))?;
    let r = write_message(sock, &data);
    debug!(pid = %std::process::id(), msg = %label, ok = r.is_ok(), "ipc: parent send");
    r
}

/// Send an exit signal to the child process.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the socket write fails.
pub fn send_exit(sock: &mut UnixStream) -> Result<(), SandboxError> {
    send_parent_msg(sock, &ParentMsg::Exit)
}

/// Receive a [`ParentMsg`] from the child process over the IPC socket.
///
/// # Errors
///
/// Returns [`SandboxError::Ipc`] if the message cannot be read or
/// deserialized.
pub fn recv_parent_msg(sock: &mut UnixStream) -> Result<ParentMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ParentMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize parent msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: parent recv");
    Ok(msg)
}

fn child_msg_label(msg: &ChildMsg) -> &'static str {
    match msg {
        ChildMsg::Setup { .. } => "setup",
        ChildMsg::LsResult { .. } => "ls_result",
        ChildMsg::PwdResult { .. } => "pwd_result",
        ChildMsg::CdResult { .. } => "cd_result",
        ChildMsg::ExecResult { .. } => "exec_result",
        ChildMsg::FsResult { .. } => "fs_result",
    }
}

fn parent_msg_label(msg: &ParentMsg) -> &'static str {
    match msg {
        ParentMsg::Exit => "exit",
        ParentMsg::Ls { .. } => "ls",
        ParentMsg::Pwd { .. } => "pwd",
        ParentMsg::Cd { .. } => "cd",
        ParentMsg::Exec { .. } => "exec",
        ParentMsg::Fs { .. } => "fs",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn write_read_message_roundtrip() {
        let (mut tx, mut rx) = pair();
        let data = b"hello world";
        write_message(&mut tx, data).unwrap();
        let got = read_message(&mut rx).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn write_read_empty_message() {
        let (mut tx, mut rx) = pair();
        write_message(&mut tx, b"").unwrap();
        let got = read_message(&mut rx).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_message_rejects_oversized() {
        let (mut tx, mut rx) = pair();
        let len = (u32::try_from(MAX_MSG_LEN).unwrap() + 1).to_be_bytes();
        tx.write_all(&len).unwrap();
        let err = read_message(&mut rx).unwrap_err();
        assert!(err.to_string().contains("message too large"));
    }

    #[test]
    fn handshake_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_handshake(&mut tx, "maki-server").unwrap();
        let name = recv_handshake(&mut rx).unwrap();
        assert_eq!(name, "maki-server");
    }

    #[test]
    fn handshake_version_mismatch() {
        let (mut tx, mut rx) = pair();
        let hs = Handshake {
            name: "bad".into(),
            version: 999,
        };
        let data = serde_json::to_vec(&hs).unwrap();
        write_message(&mut tx, &data).unwrap();
        let err = recv_handshake(&mut rx).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn sync_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_sync(&mut tx, SYNC_READY).unwrap();
        recv_sync(&mut rx, SYNC_READY).unwrap();
    }

    #[test]
    fn sync_wrong_message() {
        let (mut tx, mut rx) = pair();
        tx.write_all(SYNC_READY).unwrap();
        let err = recv_sync(&mut rx, SYNC_GO).unwrap_err();
        assert!(err.to_string().contains("unexpected sync"));
    }

    #[test]
    fn parent_msg_exit_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_parent_msg(&mut tx, &ParentMsg::Exit).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        assert!(matches!(got, ParentMsg::Exit));
    }

    #[test]
    fn parent_msg_ls_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Ls {
            call_id: 8,
            path: "/tmp".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Ls { call_id, path } => {
                assert_eq!(call_id, 8);
                assert_eq!(path, "/tmp");
            }
            other => panic!("expected Ls, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_pwd_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_parent_msg(&mut tx, &ParentMsg::Pwd { call_id: 10 }).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        assert!(matches!(got, ParentMsg::Pwd { call_id: 10 }));
    }

    #[test]
    fn parent_msg_cd_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Cd {
            call_id: 12,
            path: "/home".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Cd { call_id, path } => {
                assert_eq!(call_id, 12);
                assert_eq!(path, "/home");
            }
            other => panic!("expected Cd, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_exec_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Exec {
            call_id: 13,
            command: "ls -la".into(),
            workdir: Some("/home/maki/workspace/maki".into()),
            timeout_secs: Some(30),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Exec {
                call_id,
                command,
                workdir,
                timeout_secs,
            } => {
                assert_eq!(call_id, 13);
                assert_eq!(command, "ls -la");
                assert_eq!(workdir.as_deref(), Some("/home/maki/workspace/maki"));
                assert_eq!(timeout_secs, Some(30));
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_fs_read_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Fs {
            call_id: 20,
            op: FsOp::Read {
                path: "/home/maki/workspace/a.txt".into(),
                max_bytes: 1024,
            },
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Fs { call_id, op } => {
                assert_eq!(call_id, 20);
                match op {
                    FsOp::Read { path, max_bytes } => {
                        assert_eq!(path, "/home/maki/workspace/a.txt");
                        assert_eq!(max_bytes, 1024);
                    }
                    other => panic!("expected FsOp::Read, got {other:?}"),
                }
            }
            other => panic!("expected Fs, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_fs_write_roundtrip() {
        let (mut tx, mut rx) = pair();
        let content = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hi");
        let msg = ParentMsg::Fs {
            call_id: 21,
            op: FsOp::Write {
                path: "/tmp/x".into(),
                content,
            },
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Fs {
                call_id,
                op: FsOp::Write { path, content },
            } => {
                let got =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, content)
                        .unwrap();
                assert_eq!(call_id, 21);
                assert_eq!(path, "/tmp/x");
                assert_eq!(got, b"hi");
            }
            other => panic!("expected Fs::Write, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_fs_grep_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Fs {
            call_id: 22,
            op: FsOp::Grep {
                path: Some("/home/maki/workspace/src".into()),
                pattern: "TODO".into(),
                include: Some("*.rs".into()),
                context_before: 1,
                context_after: 2,
                limit: 5,
                max_line_bytes: 100,
            },
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Fs {
                call_id,
                op:
                    FsOp::Grep {
                        path,
                        pattern,
                        include,
                        context_before,
                        context_after,
                        limit,
                        max_line_bytes,
                    },
            } => {
                assert_eq!(call_id, 22);
                assert_eq!(path.as_deref(), Some("/home/maki/workspace/src"));
                assert_eq!(pattern, "TODO");
                assert_eq!(pattern, "TODO");
                assert_eq!(include.as_deref(), Some("*.rs"));
                assert_eq!(context_before, 1);
                assert_eq!(context_after, 2);
                assert_eq!(limit, 5);
                assert_eq!(max_line_bytes, 100);
            }
            other => panic!("expected Fs::Grep, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_ls_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::LsResult {
            call_id: 1,
            entries: vec![DirEntry {
                name: "src".into(),
                is_dir: true,
            }],
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::LsResult { call_id, entries } => {
                assert_eq!(call_id, 1);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "src");
                assert!(entries[0].is_dir);
            }
            other => panic!("expected LsResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_pwd_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::PwdResult {
            call_id: 2,
            path: "/home/maki/workspace".into(),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::PwdResult { call_id, path } => {
                assert_eq!(call_id, 2);
                assert_eq!(path, "/home/maki/workspace");
            }
            other => panic!("expected PwdResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_cd_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_child_msg(&mut tx, &ChildMsg::CdResult { call_id: 4 }).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        assert!(matches!(got, ChildMsg::CdResult { call_id: 4 }));
    }

    #[test]
    fn child_msg_exec_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::ExecResult {
            call_id: 6,
            output: "result".into(),
            exit_code: 3,
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ExecResult {
                call_id,
                output,
                exit_code,
            } => {
                assert_eq!(call_id, 6);
                assert_eq!(output, "result");
                assert_eq!(exit_code, 3);
            }
            other => panic!("expected ExecResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_fs_result_bytes_roundtrip() {
        let (mut tx, mut rx) = pair();
        let data =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"\x00\xffhi");
        let msg = ChildMsg::FsResult {
            call_id: 30,
            result: FsResult::Ok(FsReply::Bytes { data }),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::FsResult { call_id, result } => match result {
                FsResult::Ok(FsReply::Bytes { data }) => {
                    let bytes =
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
                            .unwrap();
                    assert_eq!(call_id, 30);
                    assert_eq!(bytes, b"\x00\xffhi");
                }
                other => panic!("expected FsResult::Ok(Bytes), got {other:?}"),
            },
            other => panic!("expected FsResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_fs_result_error_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::FsResult {
            call_id: 31,
            result: FsResult::Err("permission denied".into()),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::FsResult { call_id, result } => {
                assert_eq!(call_id, 31);
                assert!(matches!(result, FsResult::Err(ref e) if e == "permission denied"));
            }
            other => panic!("expected FsResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_fs_result_done_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::FsResult {
            call_id: 32,
            result: FsResult::Ok(FsReply::Done),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::FsResult { call_id, result } => {
                assert_eq!(call_id, 32);
                assert!(matches!(result, FsResult::Ok(FsReply::Done)));
            }
            other => panic!("expected FsResult, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_label_all_variants() {
        assert_eq!(child_msg_label(&ChildMsg::Setup { error: None }), "setup");
        assert_eq!(
            child_msg_label(&ChildMsg::LsResult {
                call_id: 0,
                entries: vec![]
            }),
            "ls_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::PwdResult {
                call_id: 0,
                path: String::new()
            }),
            "pwd_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::CdResult { call_id: 0 }),
            "cd_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::ExecResult {
                call_id: 0,
                output: String::new(),
                exit_code: 0
            }),
            "exec_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::FsResult {
                call_id: 0,
                result: FsResult::Ok(FsReply::Done)
            }),
            "fs_result"
        );
    }

    #[test]
    fn setup_msg_roundtrip_ready() {
        let (mut tx, mut rx) = pair();
        send_child_msg(&mut tx, &ChildMsg::Setup { error: None }).unwrap();
        assert!(matches!(
            recv_child_msg(&mut rx).unwrap(),
            ChildMsg::Setup { error: None }
        ));
    }

    /// The parent rebuilds the child's error to decide whether the host simply
    /// cannot sandbox, so the variant has to survive the socket.
    #[test]
    fn setup_msg_preserves_isolation_unavailable() {
        let (mut tx, mut rx) = pair();
        send_child_msg(
            &mut tx,
            &ChildMsg::Setup {
                error: Some(SandboxError::IsolationUnavailable("denied".into())),
            },
        )
        .unwrap();
        let ChildMsg::Setup { error: Some(e) } = recv_child_msg(&mut rx).unwrap() else {
            panic!("expected a failed setup message");
        };
        assert!(
            e.is_isolation_unavailable(),
            "variant must survive the wire"
        );
        assert_eq!(
            e.to_string(),
            "filesystem isolation unavailable: unshare(CLONE_NEWNS) failed: denied"
        );
    }

    #[test]
    fn setup_msg_keeps_other_failures_distinguishable() {
        let (mut tx, mut rx) = pair();
        send_child_msg(
            &mut tx,
            &ChildMsg::Setup {
                error: Some(SandboxError::Mount("bad bind".into())),
            },
        )
        .unwrap();
        let ChildMsg::Setup { error: Some(e) } = recv_child_msg(&mut rx).unwrap() else {
            panic!("expected a failed setup message");
        };
        assert!(
            !e.is_isolation_unavailable(),
            "a broken mount is not host unavailability and must not be skipped"
        );
        assert!(matches!(e, SandboxError::Mount(_)), "got {e}");
    }

    #[test]
    fn parent_msg_label_all_variants() {
        assert_eq!(parent_msg_label(&ParentMsg::Exit), "exit");
        assert_eq!(
            parent_msg_label(&ParentMsg::Ls {
                call_id: 0,
                path: String::new()
            }),
            "ls"
        );
        assert_eq!(parent_msg_label(&ParentMsg::Pwd { call_id: 0 }), "pwd");
        assert_eq!(
            parent_msg_label(&ParentMsg::Cd {
                call_id: 0,
                path: String::new()
            }),
            "cd"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::Exec {
                call_id: 0,
                command: String::new(),
                workdir: None,
                timeout_secs: None
            }),
            "exec"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::Fs {
                call_id: 0,
                op: FsOp::Exists {
                    path: String::new()
                }
            }),
            "fs"
        );
    }
}
