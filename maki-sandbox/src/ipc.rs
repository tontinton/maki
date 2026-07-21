use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use tracing::debug;

use crate::error::SandboxError;

pub const HANDSHAKE_VERSION: u32 = 1;
pub const MAX_MSG_LEN: usize = 16 * 1024 * 1024;

/// Call id used for messages that carry no associated request
/// (e.g. a fatal child error reported before any request was sent).
pub const NO_CALL_ID: u32 = 0;

#[derive(Serialize, Deserialize, Debug)]
pub struct Handshake {
    pub name: String,
    pub version: u32,
}

pub fn send_handshake(sock: &mut UnixStream, name: &str) -> Result<(), SandboxError> {
    let msg = Handshake {
        name: name.to_string(),
        version: HANDSHAKE_VERSION,
    };
    let data = serde_json::to_vec(&msg)
        .map_err(|e| SandboxError::Ipc(format!("handshake serialize: {e}")))?;
    write_message(sock, &data)
}

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

pub fn send_sync(sock: &mut UnixStream, msg: &[u8]) -> Result<(), SandboxError> {
    sock.write_all(msg)
        .map_err(|e| SandboxError::Ipc(format!("sync send: {e}")))
}

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
    #[serde(rename = "run")]
    Run {
        call_id: u32,
        code: String,
        timeout_secs: u64,
        max_memory: usize,
        /// Serialized `AgentConfig` JSON applied to the child's Lua runtime.
        config: String,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        call_id: u32,
        name: String,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: u32,
        #[serde(flatten)]
        result: ToolResultPayload,
    },
    #[serde(rename = "cancel")]
    Cancel,
    #[serde(rename = "exit")]
    Exit,
    #[serde(rename = "ls")]
    Ls { call_id: u32, path: String },
    #[serde(rename = "pwd")]
    Pwd { call_id: u32 },
    #[serde(rename = "cd")]
    Cd { call_id: u32, path: String },
    #[serde(rename = "exec")]
    Exec { call_id: u32, command: String },
}

/// Messages sent by the sandbox child to the parent.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ChildMsg {
    #[serde(rename = "stdout")]
    Stdout { call_id: u32, text: String },
    /// A trusted tool the child wants executed in the parent process.
    #[serde(rename = "tool_call")]
    ToolCall {
        call_id: u32,
        name: String,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    #[serde(rename = "done")]
    Done {
        call_id: u32,
        output: Option<Value>,
        stdout: String,
        error: Option<String>,
    },
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
        #[serde(rename = "is_error")]
        is_error: bool,
    },
    /// Response to a parent-issued [`ParentMsg::ToolCall`]: the result of
    /// executing a tool inside the namespace.
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: u32,
        #[serde(flatten)]
        result: ToolResultPayload,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolResultPayload {
    pub output: Option<String>,
    pub error: Option<String>,
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
pub fn send_child_msg(sock: &mut UnixStream, msg: &ChildMsg) -> Result<(), SandboxError> {
    let label = child_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize child msg: {e}")))?;
    let r = write_message(sock, &data);
    let pid = std::process::id();
    if let ChildMsg::Done { error, .. } = msg {
        debug!(pid = %pid, msg = %label, ok = r.is_ok(), error = error.as_deref().unwrap_or(""), "ipc: child send");
    } else {
        debug!(pid = %pid, msg = %label, ok = r.is_ok(), "ipc: child send");
    }
    r
}

/// Receive a [`ChildMsg`] from the parent process over the IPC socket.
pub fn recv_child_msg(sock: &mut UnixStream) -> Result<ChildMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ChildMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize child msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: child recv");
    Ok(msg)
}

/// Send a [`ParentMsg`] to the child process over the IPC socket.
pub fn send_parent_msg(sock: &mut UnixStream, msg: &ParentMsg) -> Result<(), SandboxError> {
    let label = parent_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize parent msg: {e}")))?;
    let r = write_message(sock, &data);
    debug!(pid = %std::process::id(), msg = %label, ok = r.is_ok(), "ipc: parent send");
    r
}

/// Send an exit signal to the child process.
pub fn send_exit(sock: &mut UnixStream) -> Result<(), SandboxError> {
    send_parent_msg(sock, &ParentMsg::Exit)
}

/// Receive a [`ParentMsg`] from the child process over the IPC socket.
pub fn recv_parent_msg(sock: &mut UnixStream) -> Result<ParentMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ParentMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize parent msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: parent recv");
    Ok(msg)
}

fn child_msg_label(msg: &ChildMsg) -> &'static str {
    match msg {
        ChildMsg::Stdout { .. } => "stdout",
        ChildMsg::ToolCall { .. } => "tool_call",
        ChildMsg::Done { .. } => "done",
        ChildMsg::LsResult { .. } => "ls_result",
        ChildMsg::PwdResult { .. } => "pwd_result",
        ChildMsg::CdResult { .. } => "cd_result",
        ChildMsg::ExecResult { .. } => "exec_result",
        ChildMsg::ToolResult { .. } => "tool_result",
    }
}

fn parent_msg_label(msg: &ParentMsg) -> &'static str {
    match msg {
        ParentMsg::Run { .. } => "run",
        ParentMsg::ToolCall { .. } => "tool_call",
        ParentMsg::ToolResult { .. } => "tool_result",
        ParentMsg::Cancel => "cancel",
        ParentMsg::Exit => "exit",
        ParentMsg::Ls { .. } => "ls",
        ParentMsg::Pwd { .. } => "pwd",
        ParentMsg::Cd { .. } => "cd",
        ParentMsg::Exec { .. } => "exec",
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
        let len = (MAX_MSG_LEN as u32 + 1).to_be_bytes();
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
    fn parent_msg_run_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Run {
            call_id: 9,
            code: "print('hello')".into(),
            timeout_secs: 30,
            max_memory: 1024,
            config: "{}".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Run {
                call_id,
                code,
                timeout_secs,
                max_memory,
                config,
            } => {
                assert_eq!(call_id, 9);
                assert_eq!(code, "print('hello')");
                assert_eq!(timeout_secs, 30);
                assert_eq!(max_memory, 1024);
                assert_eq!(config, "{}");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_tool_call_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::ToolCall {
            call_id: 3,
            name: "read".into(),
            args: vec![Value::String("/foo".into())],
            kwargs: vec![("offset".into(), Value::from(1))],
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::ToolCall {
                call_id,
                name,
                args,
                kwargs,
            } => {
                assert_eq!(call_id, 3);
                assert_eq!(name, "read");
                assert_eq!(args, vec![Value::String("/foo".into())]);
                assert_eq!(kwargs, vec![("offset".into(), Value::from(1))]);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_stdout_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::Stdout {
            call_id: 5,
            text: "line1\n".into(),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::Stdout { call_id, text } => {
                assert_eq!(call_id, 5);
                assert_eq!(text, "line1\n");
            }
            other => panic!("expected Stdout, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_tool_call_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::ToolCall {
            call_id: 42,
            name: "webfetch".into(),
            args: vec![],
            kwargs: vec![("url".into(), Value::String("https://example.com".into()))],
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, 42);
                assert_eq!(name, "webfetch");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_done_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::Done {
            call_id: 7,
            output: Some(Value::Bool(true)),
            stdout: "out".into(),
            error: Some("err".into()),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::Done {
                call_id,
                output,
                stdout,
                error,
            } => {
                assert_eq!(call_id, 7);
                assert_eq!(output, Some(Value::Bool(true)));
                assert_eq!(stdout, "out");
                assert_eq!(error, Some("err".into()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_tool_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::ToolResult {
            call_id: 11,
            result: ToolResultPayload {
                output: Some("file contents".into()),
                error: None,
            },
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ToolResult { call_id, result } => {
                assert_eq!(call_id, 11);
                assert_eq!(result.output, Some("file contents".into()));
                assert!(result.error.is_none());
            }
            other => panic!("expected ToolResult, got {other:?}"),
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
            is_error: false,
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ExecResult {
                call_id,
                output,
                is_error,
            } => {
                assert_eq!(call_id, 6);
                assert_eq!(output, "result");
                assert!(!is_error);
            }
            other => panic!("expected ExecResult, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_tool_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::ToolResult {
            call_id: 7,
            result: ToolResultPayload {
                output: Some("ok".into()),
                error: None,
            },
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::ToolResult { call_id, result } => {
                assert_eq!(call_id, 7);
                assert_eq!(result.output, Some("ok".into()));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_cancel_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_parent_msg(&mut tx, &ParentMsg::Cancel).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        assert!(matches!(got, ParentMsg::Cancel));
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
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Exec { call_id, command } => {
                assert_eq!(call_id, 13);
                assert_eq!(command, "ls -la");
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_label_all_variants() {
        assert_eq!(
            child_msg_label(&ChildMsg::Stdout {
                call_id: 0,
                text: "".into()
            }),
            "stdout"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::ToolCall {
                call_id: 0,
                name: "".into(),
                args: vec![],
                kwargs: vec![]
            }),
            "tool_call"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::Done {
                call_id: 0,
                output: None,
                stdout: "".into(),
                error: None
            }),
            "done"
        );
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
                path: "".into()
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
                output: "".into(),
                is_error: false
            }),
            "exec_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::ToolResult {
                call_id: 0,
                result: ToolResultPayload {
                    output: None,
                    error: None
                }
            }),
            "tool_result"
        );
    }

    #[test]
    fn parent_msg_label_all_variants() {
        assert_eq!(
            parent_msg_label(&ParentMsg::Run {
                call_id: 0,
                code: "".into(),
                timeout_secs: 0,
                max_memory: 0,
                config: "".into()
            }),
            "run"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::ToolCall {
                call_id: 0,
                name: "".into(),
                args: vec![],
                kwargs: vec![]
            }),
            "tool_call"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::ToolResult {
                call_id: 0,
                result: ToolResultPayload {
                    output: None,
                    error: None
                }
            }),
            "tool_result"
        );
        assert_eq!(parent_msg_label(&ParentMsg::Cancel), "cancel");
        assert_eq!(parent_msg_label(&ParentMsg::Exit), "exit");
        assert_eq!(
            parent_msg_label(&ParentMsg::Ls {
                call_id: 0,
                path: "".into()
            }),
            "ls"
        );
        assert_eq!(parent_msg_label(&ParentMsg::Pwd { call_id: 0 }), "pwd");
        assert_eq!(
            parent_msg_label(&ParentMsg::Cd {
                call_id: 0,
                path: "".into()
            }),
            "cd"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::Exec {
                call_id: 0,
                command: "".into()
            }),
            "exec"
        );
    }
}
