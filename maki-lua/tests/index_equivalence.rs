use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::registry::DELEGATE_NATIVE;
use maki_code_index::{Language, index_source};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use tempfile::TempDir;
use test_case::test_case;

mod equivalence_support;

const RUST_FIXTURE: &str = r#"
//! Module doc

use std::collections::HashMap;
use std::io;

const MAX: usize = 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

enum Color {
    Red,
    Green,
    Blue,
}

pub trait Handler {
    fn handle(&self, req: Request) -> Response;
}

impl Display for Config {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Config")
    }
}

impl Config {
    pub fn new(name: String) -> Self {
        todo!()
    }
}

pub fn process(input: &str) -> Result<String, Error> {
    todo!()
}

pub mod utils;
mod internal;

macro_rules! my_macro {
    () => {};
}

/// Doc comment
pub fn documented() {}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
"#;

const PYTHON_FIXTURE: &str = r#"
"""Module docstring."""

import os
from typing import Optional, List

MAX_RETRIES = 3

class MyClass:
    def __init__(self, name: str):
        self.name = name

    @staticmethod
    def validate(token: str) -> bool:
        return True

def process(data: list) -> dict:
    return {}
"#;

const TS_FIXTURE: &str = r#"
import { Request, Response } from 'express';

export interface Config {
    port: number;
    host: string;
}

export type ID = string | number;

export class Service {
    process(input: string): string { return input; }
}

export function handler(req: Request): Response { return new Response(); }
"#;

const JS_FIXTURE: &str = r#"
import express from 'express';

const PORT = 3000;

class App {
    constructor() {}
    start() {}
}

function main() {
    const app = new App();
}
"#;

fn index_config() -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        builtins: vec!["index".to_string()],
        init_file: None,
    }
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: serde_json::Value) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await }).map(|out| match out {
        maki_agent::ToolOutput::Plain(s) => s,
        other => panic!("unexpected output: {other:?}"),
    })
}

/// Needs to be under cwd to land inside the Lua sandbox.
fn sandbox_tmp() -> TempDir {
    TempDir::new_in(".").unwrap()
}

struct Setup {
    reg: Arc<ToolRegistry>,
    _host: PluginHost,
    tmp: TempDir,
}

fn setup() -> Setup {
    let tmp = sandbox_tmp();
    let reg = Arc::new(ToolRegistry::with_natives());
    let _host = PluginHost::new(&index_config(), Arc::clone(&reg)).unwrap();
    Setup { reg, _host, tmp }
}

fn write_fixture(tmp: &std::path::Path, name: &str, source: &str) -> serde_json::Value {
    let file = tmp.join(name);
    std::fs::write(&file, source).unwrap();
    let abs = file.canonicalize().unwrap();
    serde_json::json!({"path": abs.to_str().unwrap()})
}

fn lua_index(reg: &ToolRegistry, source: &str, ext: &str, tmp: &std::path::Path) -> String {
    let input = write_fixture(tmp, &format!("test.{ext}"), source);
    exec_tool(reg, "index", input).unwrap()
}

fn native_index(source: &str, lang: Language) -> String {
    index_source(source.as_bytes(), lang).unwrap()
}

#[test_case(RUST_FIXTURE,   "rs", Language::Rust       ; "rust")]
#[test_case(PYTHON_FIXTURE, "py", Language::Python      ; "python")]
#[test_case(TS_FIXTURE,     "ts", Language::TypeScript   ; "typescript")]
#[test_case(JS_FIXTURE,     "js", Language::JavaScript   ; "javascript")]
fn lua_matches_native(source: &str, ext: &str, lang: Language) {
    let s = setup();
    let native = native_index(source, lang);
    let lua = lua_index(&s.reg, source, ext, s.tmp.path());
    equivalence_support::assert_equivalent(&native, &lua);
}

#[test]
fn summary_returns_normalized_path() {
    let s = setup();
    let input = write_fixture(s.tmp.path(), "test.rs", RUST_FIXTURE);
    let entry = s.reg.get("index").unwrap();
    let inv = entry.tool.parse(&input).expect("parse failed");
    let summary = smol::block_on(inv.start_summary());
    assert!(
        summary.contains("test.rs"),
        "summary '{summary}' should contain normalized path"
    );
}

#[test_case("test.xyz", "some content" ; "unsupported_extension")]
#[test_case("Makefile", "all:\n\techo hi"                ; "no_extension")]
fn delegates_to_native(filename: &str, source: &str) {
    let s = setup();
    let input = write_fixture(s.tmp.path(), filename, source);
    let result = exec_tool(&s.reg, "index", input).unwrap();
    assert_eq!(result, DELEGATE_NATIVE);
}

#[test]
fn delegate_native_via_dispatch_runs_native_tool() {
    use maki_agent::agent::tool_dispatch;

    let s = setup();
    assert!(s.reg.get_native_fallback("index").is_some());

    let input = write_fixture(
        s.tmp.path(),
        "test.go",
        "package main\n\nfunc main() {\n}\n",
    );
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);

    let done = smol::block_on(tool_dispatch::run(
        &s.reg,
        None,
        "t1".into(),
        "index",
        &input,
        &ctx,
    ));

    assert!(
        !done.is_error,
        "expected success, got: {}",
        done.output.as_text()
    );
    assert_ne!(done.output.as_text(), DELEGATE_NATIVE);
}

#[test]
fn lua_index_sends_snapshot_event() {
    let s = setup();
    let input = write_fixture(s.tmp.path(), "test.rs", RUST_FIXTURE);

    let (event_tx, event_rx) = flume::unbounded::<maki_agent::Envelope>();
    let sender = maki_agent::EventSender::new(event_tx, 0);
    let mut ctx = maki_agent::tools::test_support::stub_ctx_with(
        &maki_agent::AgentMode::Build,
        Some(&sender),
        Some("snap-test-id"),
    );
    ctx.tool_use_id = Some("snap-test-id".into());

    let entry = s.reg.get("index").unwrap();
    let inv = entry.tool.parse(&input).expect("parse failed");
    let result = smol::block_on(async { inv.execute(&ctx).await });
    assert!(result.is_ok(), "index should succeed");

    let mut found_snapshot = false;
    while let Ok(envelope) = event_rx.try_recv() {
        if let maki_agent::AgentEvent::ToolSnapshot { id, snapshot } = &envelope.event {
            assert_eq!(id, "snap-test-id");
            assert!(!snapshot.lines.is_empty(), "snapshot should have lines");
            found_snapshot = true;
        }
    }
    assert!(
        found_snapshot,
        "expected ToolSnapshot event for Lua-handled index"
    );
}
