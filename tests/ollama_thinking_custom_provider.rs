//! A custom `openai`-protocol provider must forward thinking levels: plain
//! `requires_thinking` snaps to the ollama dialect, declared `thinking_fields`
//! replace it. Runs the real binary against a localhost mock with an isolated
//! HOME so developer config never leaks in.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;
use test_case::test_case;

const MODEL_SPEC: &str = "ollama-local/qwen3.8-coder-27b-mlx:latest";
const MODEL_ID: &str = "qwen3.8-coder-27b-mlx:latest";

const SSE_RESPONSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
data: [DONE]\n\n";

fn spawn_mock_ollama() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let thread_bodies = Arc::clone(&bodies);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            if let Ok(body) = serde_json::from_slice::<Value>(&read_body(&mut stream)) {
                thread_bodies.lock().unwrap().push(body);
            }
            let _ = stream.write_all(SSE_RESPONSE.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), bodies)
}

fn read_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "client closed before sending headers");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_length = String::from_utf8_lossy(&buf[..header_end])
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "client closed mid-body");
        body.extend_from_slice(&chunk[..n]);
    }
    body
}

fn providers_toml(base_url: &str, thinking_fields: &str) -> String {
    format!(
        "[ollama-local]\nprotocol = \"openai\"\nbase_url = \"{base_url}\"\napi_key = \"test-key\"\n\
        \n[[ollama-local.models]]\nid = \"{MODEL_ID}\"\nrequires_thinking = true\n{thinking_fields}"
    )
}

// Print mode starts Off; `requires_thinking` clamps that up to Minimal, which
// then snaps to the dialect floor ("low") for plain models. A declared `high`
// fragment snaps it up to "xhigh", which the dialect could never express.
#[test_case("", "low" ; "dialect_fallback")]
#[test_case("[ollama-local.models.thinking_fields]\nhigh = { reasoning_effort = \"xhigh\" }\n", "xhigh" ; "declared_fields_win")]
fn custom_ollama_provider_forwards_thinking(thinking_fields: &str, expected: &str) {
    let (base_url, bodies) = spawn_mock_ollama();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config").join("maki");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("providers.toml"),
        providers_toml(&base_url, thinking_fields),
    )
    .unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maki"))
        .args(["-p", "say hi", "--model", MODEL_SPEC, "--output-format", "json"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn maki");
    assert!(
        output.status.success(),
        "maki failed ({})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The mock pushes the body before replying, and maki exits only after the
    // reply, so by the time `output()` returns the body is already recorded.
    let bodies = bodies.lock().unwrap();
    let body = bodies
        .iter()
        .find(|b| b["model"] == MODEL_ID)
        .unwrap_or_else(|| panic!("no chat request captured; got: {bodies:?}"));
    assert_eq!(
        body["reasoning_effort"], expected,
        "wrong effort on the wire; body: {body}"
    );
}
