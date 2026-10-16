use std::env;
use std::fmt::Write;
use std::io::{self, IsTerminal};
use std::path::Path;

use color_eyre::Result;
use maki_storage::paths::tilde;

use crate::provider_scripts::{self, MIGRATE_COMMAND, Reference, Script};

const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";

/// The prompt goes to stdout and everything addressed to the user to stderr,
/// so `maki "$(maki migrate providers)"` and a pipe into a clipboard tool
/// carry the prompt and nothing else.
pub fn providers() -> Result<()> {
    let Some(dir) = provider_scripts::providers_dir() else {
        eprintln!("No provider scripts found, so there is nothing to migrate.");
        return Ok(());
    };
    let unported = provider_scripts::unported(&dir);
    if unported.is_empty() {
        eprintln!("{}", nothing_to_port(&dir));
        return Ok(());
    }
    let reference = Reference::find();
    if io::stdout().is_terminal() {
        eprint!(
            "{}",
            providers_banner(&unported, reference.old_maki.as_deref())
        );
    }
    print!("{}", provider_scripts::prompt(&dir, &unported, &reference));
    Ok(())
}

fn nothing_to_port(dir: &Path) -> String {
    if provider_scripts::scripts_in(dir).is_empty() {
        return format!(
            "No provider scripts in {}, so there is nothing to migrate.",
            tilde(dir)
        );
    }
    format!(
        "Every provider script in {0} has a Lua plugin now. Maki ignores the scripts, so you can delete {0} once your plugins no longer read credentials from there.",
        tilde(dir)
    )
}

fn providers_banner(scripts: &[Script], old_maki: Option<&Path>) -> String {
    let width = scripts.iter().map(|s| s.slug.len()).max().unwrap_or(0);
    let mut out =
        String::from("Maki no longer runs provider scripts. These need a Lua plugin:\n\n");
    for script in scripts {
        let _ = writeln!(out, "  {:<width$}  {}", script.slug, tilde(&script.path));
    }
    out.push_str("\nThe prompt below asks a coding agent to port them for you.\n");
    if !cfg!(windows) {
        let run = match old_maki {
            Some(path) => {
                let _ = write!(
                    out,
                    "\nRun it with your previous maki, which `maki update` kept and which still runs the scripts:\n\n  {} \"$({MIGRATE_COMMAND})\"\n",
                    tilde(path)
                );
                "Or run"
            }
            None => "Run",
        };
        let _ = write!(
            out,
            "\n{run} it in maki, with any model you have set up:\n\n  maki \"$({MIGRATE_COMMAND})\"\n"
        );
    }
    let copy = if cfg!(windows) { "Copy" } else { "Or copy" };
    let _ = write!(
        out,
        "\n{copy} it into another agent:\n\n  {MIGRATE_COMMAND} | {}\n\n",
        clipboard_command()
    );
    out
}

fn clipboard_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(windows) {
        "clip"
    } else if env::var_os(WAYLAND_DISPLAY_ENV).is_some() {
        "wl-copy"
    } else {
        "xclip -selection clipboard"
    }
}
