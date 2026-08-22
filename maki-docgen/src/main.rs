mod gen_commands;
mod gen_config;
mod gen_keybindings;
mod gen_lua_api;
mod gen_plugins;
mod gen_providers;
mod gen_tools;
mod lua_util;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

const CONTENT_DIR: &str = "site/docs/content";

type Page = (&'static str, fn() -> String);

/// One entry per generated page. Every generator is a slow, self-contained
/// string build (it boots a Lua host, walks the tool registry, and so on), so
/// they each get a thread.
const PAGES: [Page; 7] = [
    ("tools", gen_tools::generate),
    ("plugins", gen_plugins::generate),
    ("providers", gen_providers::generate),
    ("configuration", gen_config::generate),
    ("lua-api", gen_lua_api::generate),
    ("keybindings", gen_keybindings::generate),
    ("commands", gen_commands::generate),
];

fn page_path(section: &str) -> PathBuf {
    Path::new(CONTENT_DIR).join(section).join("_index.md")
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    println!("wrote {}", path.display());
}

fn check_file(path: &Path, expected: &str) -> bool {
    match fs::read_to_string(path) {
        Ok(existing) if existing == expected => {
            println!("ok {}", path.display());
            true
        }
        Ok(_) => {
            println!("mismatch {}", path.display());
            false
        }
        Err(_) => {
            println!("missing {}", path.display());
            false
        }
    }
}

fn main() -> ExitCode {
    let check = std::env::args().any(|a| a == "--check");

    let outputs = thread::scope(|scope| {
        let running = PAGES.map(|(section, generate)| (page_path(section), scope.spawn(generate)));
        running.map(|(path, page)| (path, page.join().unwrap()))
    });

    if check {
        let mismatches = outputs
            .iter()
            .filter(|(path, content)| !check_file(path, content))
            .count();
        if mismatches == 0 {
            ExitCode::SUCCESS
        } else {
            eprintln!("docs out of date, run `just gen-docs` to update");
            ExitCode::FAILURE
        }
    } else {
        for (path, content) in &outputs {
            write_file(path, content);
        }
        ExitCode::SUCCESS
    }
}
