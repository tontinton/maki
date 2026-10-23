//! The committed transcript in `fixtures/` must load forever: whatever the
//! message shape becomes, sessions already on disk keep opening.

use std::fs;
use std::path::{Path, PathBuf};

use maki_providers::{ContentBlock, Message, TokenUsage};
use maki_storage::id::MakiId;
use maki_storage::sessions::Session;
use serde_json::{Value, json};
use tempfile::TempDir;

type FixtureSession = Session<Message, TokenUsage, Value>;

const FIXTURE: &str = include_str!("fixtures/session_v2.jsonl");
const FIXTURE_ID: &str = "01965087-4c71-7f00-8000-000000000000";
const FIXTURE_MESSAGES: usize = 4;
const MSG_RECORD_PREFIX: &str = r#"{"t":"msg""#;
const UPDATE_DATE: &str = "2026-10-02";
const PROMPT: &str = "fix the flaky login test";
const UNKNOWN_FIELD: &str = "cache_hint";
/// A media type a newer build may accept and this one cannot read.
const NEWER_MEDIA_TYPE: &str = "image/heic";

fn fixture_id() -> MakiId {
    FIXTURE_ID.parse().unwrap()
}

/// Where this build keeps the log, which is the id's display form, not the
/// spelling `FIXTURE_ID` happens to use.
fn fixture_path(dir: &Path) -> PathBuf {
    dir.join(format!("{}.jsonl", fixture_id()))
}

fn installed_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::write(fixture_path(tmp.path()), FIXTURE).unwrap();
    tmp
}

fn unknown_block() -> Value {
    json!({
        "type": "server_tool_use",
        "id": "srvtoolu_1",
        "name": "web_search",
        "input": { "query": "flaky login test" }
    })
}

fn newer_image_block() -> Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": NEWER_MEDIA_TYPE, "data": "aGk=" }
    })
}

fn msg_lines(dir: &Path) -> Vec<String> {
    fs::read_to_string(fixture_path(dir))
        .unwrap()
        .lines()
        .filter(|line| line.starts_with(MSG_RECORD_PREFIX))
        .map(str::to_owned)
        .collect()
}

fn resave(dir: &Path) -> FixtureSession {
    let (mut session, claim) = FixtureSession::claim_and_load_from(fixture_id(), dir).unwrap();
    session.save_to(&claim, dir).unwrap();
    session
}

#[test]
fn fixture_loads() {
    let tmp = installed_fixture();
    let session = FixtureSession::load_from(fixture_id(), tmp.path()).unwrap();
    let messages = session.messages();
    assert_eq!(messages.len(), FIXTURE_MESSAGES);
    assert_eq!(
        messages[0]
            .facts_update()
            .and_then(|facts| facts.date.as_deref()),
        Some(UPDATE_DATE)
    );
    assert!(matches!(&messages[1].content[..], [ContentBlock::Text { text }] if text == PROMPT));
    assert!(matches!(
        &messages[2].content[..],
        [ContentBlock::Unknown(raw), ContentBlock::Text { .. }] if *raw == unknown_block()
    ));
    assert!(matches!(
        &messages[3].content[..],
        [ContentBlock::Unknown(raw), ContentBlock::Text { .. }] if *raw == newer_image_block()
    ));
}

/// A block a newer build wrote, or a known block holding a value only a
/// newer build reads, must outlive this build's rewrites, or a session moved
/// between two builds loses it for good.
#[test]
fn unknown_block_survives_load_save_load_byte_identical() {
    let tmp = installed_fixture();
    let from_fixture = resave(tmp.path());
    let first_lines = msg_lines(tmp.path());
    let from_rewrite = resave(tmp.path());

    assert_eq!(from_rewrite.messages(), from_fixture.messages());
    assert_eq!(msg_lines(tmp.path()), first_lines);
}

#[test]
fn rewrite_writes_the_flat_kind_and_drops_unknown_fields() {
    let tmp = installed_fixture();
    resave(tmp.path());
    let lines = msg_lines(tmp.path());
    let update: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(update["d"]["kind"], "context_update");
    assert_eq!(update["d"]["facts"]["date"], UPDATE_DATE);
    assert!(!lines[1].contains(UNKNOWN_FIELD));
}
