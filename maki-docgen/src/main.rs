mod gen_commands;
mod gen_config;
mod gen_keybindings;
mod gen_lua_api;
mod gen_providers;
mod gen_tools;
mod lua_util;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const CONTENT_DIR: &str = "site/docs/content";

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

    let ((tools, providers), ((config, lua_api), (keybindings, commands))) =
        smol::block_on(async {
            smol::future::zip(
                smol::future::zip(
                    smol::unblock(gen_tools::generate),
                    smol::unblock(gen_providers::generate),
                ),
                smol::future::zip(
                    smol::future::zip(
                        smol::unblock(gen_config::generate),
                        smol::unblock(gen_lua_api::generate),
                    ),
                    smol::future::zip(
                        smol::unblock(gen_keybindings::generate),
                        smol::unblock(gen_commands::generate),
                    ),
                ),
            )
            .await
        });
    let outputs = [
        (page_path("tools"), tools),
        (page_path("providers"), providers),
        (page_path("configuration"), config),
        (page_path("lua-api"), lua_api),
        (page_path("keybindings"), keybindings),
        (page_path("commands"), commands),
    ];

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
