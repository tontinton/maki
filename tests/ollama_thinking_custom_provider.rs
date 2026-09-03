//! Black-box proof that a thinking level reaches Ollama through a *custom*
//! provider: the request `maki` sends for `ollama-local/<model>` (declared in
//! `providers.toml`, protocol `openai`) must carry `reasoning_effort` the same
//! way the built-in ollama slug does.
//!
//! The test builds its own config tree (HOME + XDG_* point at temp dirs) so it
//! never reads the developer's real `~/.config/maki`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const MODEL_SPEC: &str = "ollama-local/qwen3.8-coder-27b-mlx:latest";
const MODEL_ID: &str = "qwen3.8-coder-27b-mlx:latest";
/// Print mode starts with thinking off; a `requires_thinking` model clamps
/// that to the lowest supported effort level of the OLLAMA dialect.
const DIALECT_EXPECTED_EFFORT: &str = "low";
/// Per-model mapping: only `high` is declared (-> `xhigh`), which the OLLAMA
/// dialect could never express. `requires_thinking` clamps Off->Minimal, which
/// snaps up to the lowest declared level.
const FIELDS_EXPECTED_EFFORT: &str = "xhigh";
const BODY_POLL_TIMEOUT: Duration = Duration::from_secs(5);

const SSE_RESPONSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
data: [DONE]\n\n";

/// Records the JSON body of every POST and answers with a minimal SSE stream.
fn spawn_mock_ollama() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let thread_bodies = Arc::clone(&bodies);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = read_request_body(&mut stream);
            if let Ok(body) = serde_json::from_slice::<Value>(&body) {
                thread_bodies.lock().unwrap().push(body);
            }
            let _ = stream.write_all(SSE_RESPONSE.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), bodies)
}

fn read_request_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
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
    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let content_length = headers
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

fn providers_toml(base_url: &str) -> String {
    format!(
        r#"[ollama-local]
protocol = "openai"
base_url = "{base_url}"
api_key = "test-key"

[[ollama-local.models]]
id = "{MODEL_ID}"
requires_thinking = true
"#
    )
}

fn providers_toml_with_thinking_fields(base_url: &str) -> String {
    format!(
        r#"[ollama-local]
protocol = "openai"
base_url = "{base_url}"
api_key = "test-key"

[[ollama-local.models]]
id = "{MODEL_ID}"
requires_thinking = true

[ollama-local.models.thinking_fields]
high = {{ reasoning_effort = "xhigh" }}
"#
    )
}

fn run_maki(home: &std::path::Path, cwd: &std::path::Path, model: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_maki"))
        .args(["-p", "say hi", "--model", model, "--output-format", "json"])
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn maki")
}

#[test]
fn custom_ollama_provider_sends_reasoning_effort() {
    let (base_url, bodies) = spawn_mock_ollama();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config").join("maki");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("providers.toml"), providers_toml(&base_url)).unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let output = run_maki(home.path(), cwd.path(), MODEL_SPEC);

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "maki failed ({})\nstdout: {}\nstderr: {stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );

    let deadline = Instant::now() + BODY_POLL_TIMEOUT;
    let body = loop {
        let captured = bodies.lock().unwrap().clone();
        if let Some(body) = captured.into_iter().find(|b| b["model"] == MODEL_ID) {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "no chat request captured for {MODEL_ID}; captured: {:?}",
            bodies.lock().unwrap(),
        );
        thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(
        body["reasoning_effort"], DIALECT_EXPECTED_EFFORT,
        "thinking level must reach ollama through a custom provider; body: {body}"
    );
}

#[test]
fn custom_ollama_provider_uses_declared_thinking_fields() {
    let (base_url, bodies) = spawn_mock_ollama();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config").join("maki");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("providers.toml"),
        providers_toml_with_thinking_fields(&base_url),
    )
    .unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let output = run_maki(home.path(), cwd.path(), MODEL_SPEC);

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "maki failed ({})\nstdout: {}\nstderr: {stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );

    let deadline = Instant::now() + BODY_POLL_TIMEOUT;
    let body = loop {
        let captured = bodies.lock().unwrap().clone();
        if let Some(body) = captured.into_iter().find(|b| b["model"] == MODEL_ID) {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "no chat request captured for {MODEL_ID}; captured: {:?}",
            bodies.lock().unwrap(),
        );
        thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(
        body["reasoning_effort"], FIELDS_EXPECTED_EFFORT,
        "declared thinking_fields must replace the dialect fallback; body: {body}"
    );
}
