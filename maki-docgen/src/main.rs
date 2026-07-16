mod gen_commands;
mod gen_config;
mod gen_keybindings;
mod gen_lua_api;
mod gen_providers;
mod gen_tools;
mod lua_util;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

const CONTENT_DIR: &str = "site/docs/content";

fn page_path(section: &str) -> std::path::PathBuf {
    Path::new(CONTENT_DIR).join(section).join("_index.md")
}

fn write_page(section: &str, content: &str) {
    let dir = Path::new(CONTENT_DIR).join(section);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("_index.md");
    fs::write(&path, content).unwrap();
    println!("wrote {}", path.display());
}

fn check_page(section: &str, expected: &str) -> bool {
    let path = page_path(section);
    match fs::read_to_string(&path) {
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

    let pages = [
        ("tools", tools),
        ("providers", providers),
        ("configuration", config),
        ("lua-api", lua_api),
        ("keybindings", keybindings),
        ("commands", commands),
    ];

    if check {
        let all_ok = pages
            .iter()
            .all(|(section, content)| check_page(section, content));
        if all_ok {
            ExitCode::SUCCESS
        } else {
            eprintln!("docs out of date, run `just gen-docs` to update");
            ExitCode::FAILURE
        }
    } else {
        for (section, content) in pages {
            write_page(section, &content);
        }
        ExitCode::SUCCESS
    }
}
