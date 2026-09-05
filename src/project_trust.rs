use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::bail;
use maki_config::ProjectConfig;
use maki_config::project::{self, confirm_trust};
use maki_lua::InitFiles;
use maki_storage::StateDir;
use maki_storage::trusted_folders::{
    CanonicalFolder, Change, TrustDecision, TrustStatus, TrustedFolders,
};

pub fn init_files(project_config: &ProjectConfig, no_plugins: bool) -> InitFiles {
    if no_plugins {
        InitFiles::Disabled
    } else if project_config.is_trusted() {
        InitFiles::GlobalAndProject
    } else {
        InitFiles::GlobalOnly
    }
}

pub fn add(storage: &StateDir, path: Option<&Path>, yes: bool) -> Result<()> {
    let path = resolve_argument(path)?;
    let project = ProjectConfig::discover(&path);
    let folder = CanonicalFolder::resolve(project.config_root())?;
    let trusted_folders = TrustedFolders::new(storage);
    let present = project::gated_files(project.config_root());
    // A folder that gained a kind of gated file since it was trusted still has
    // a question to answer, so it is not simply "already trusted".
    let added = match trusted_folders.decide(&folder, &present, &project::project_root)? {
        TrustDecision::Trusted | TrustDecision::Unrecorded => {
            println!("Already trusted: {}", folder.path().display());
            return Ok(());
        }
        TrustDecision::Widened { added } => added,
        // A folder that predates folder trust has no stored answer yet, so this
        // is the command that finally writes one down.
        TrustDecision::Grandfathered | TrustDecision::Rejected | TrustDecision::Unknown => {
            Vec::new()
        }
    };

    if !yes {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!("trust add needs a terminal confirmation or --yes");
        }
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let stderr = io::stderr();
        let mut output = stderr.lock();
        if !confirm_trust(&mut input, &mut output, &folder, &added)? {
            println!("Project was not trusted.");
            return Ok(());
        }
    }

    trusted_folders.add(&folder, &present)?;
    println!("Trusted: {}", folder.path().display());
    Ok(())
}

pub fn remove(storage: &StateDir, path: Option<&Path>) -> Result<()> {
    let path = resolve_argument(path)?;
    let target = if path.exists() {
        ProjectConfig::discover(&path).config_root().to_path_buf()
    } else {
        path
    };
    match TrustedFolders::new(storage).remove(&target)? {
        Change::Changed => println!("Removed project trust decision."),
        Change::Unchanged => println!("Project has no stored trust decision."),
    }
    Ok(())
}

pub fn list(storage: &StateDir) -> Result<Vec<String>> {
    Ok(TrustedFolders::new(storage)
        .list()?
        .into_iter()
        .map(|decision| {
            let status = match decision.status {
                TrustStatus::Trusted => "trusted",
                TrustStatus::Rejected => "rejected",
                TrustStatus::Unknown => "unknown",
            };
            format!("{status}\t{}", decision.path.display())
        })
        .collect())
}

fn resolve_argument(path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(env::current_dir()?),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;
    use test_case::test_case;

    use super::*;
    use crate::cli::{Cli, Command, TrustAction};

    const GIT_DIR: &str = ".git";
    const FILE_IN_PROJECT: &str = "README.md";

    /// `--yes` is a flag next to a positional path, so both orders have to land
    /// in the same place.
    #[test_case(&["trust", "add", "--yes", "/tmp"] ; "yes_before_path")]
    #[test_case(&["trust", "add", "/tmp", "--yes"] ; "yes_after_path")]
    fn trust_add_accepts_the_flag_on_either_side_of_the_path(args: &[&str]) {
        let cli = Cli::try_parse_from(std::iter::once("maki").chain(args.iter().copied())).unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Trust {
                action: TrustAction::Add { yes: true, .. }
            })
        ));
    }

    /// A path anywhere in the checkout trusts the checkout, so `maki trust add
    /// src/main.rs` does not quietly trust a subdirectory instead.
    #[test_case(false ; "the_checkout_itself")]
    #[test_case(true ; "a_file_inside_the_checkout")]
    fn add_and_remove_target_the_checkout_root(inside: bool) {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(GIT_DIR)).unwrap();
        let file = project.path().join(FILE_IN_PROJECT);
        fs::write(&file, "test\n").unwrap();
        let target = if inside {
            file
        } else {
            project.path().to_path_buf()
        };
        let storage = StateDir::from_path(state.path().to_path_buf());
        let folder = CanonicalFolder::resolve(project.path()).unwrap();

        add(&storage, Some(&target), true).unwrap();
        assert!(TrustedFolders::new(&storage).contains(&folder).unwrap());

        remove(&storage, Some(&target)).unwrap();
        assert!(!TrustedFolders::new(&storage).contains(&folder).unwrap());
    }

    #[test]
    fn trust_list_labels_trusted_and_rejected_decisions() {
        let state = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let trusted_path = projects.path().join("a-trusted");
        let rejected_path = projects.path().join("b-rejected");
        fs::create_dir(&trusted_path).unwrap();
        fs::create_dir(&rejected_path).unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        let trusted = CanonicalFolder::resolve(&trusted_path).unwrap();
        let rejected = CanonicalFolder::resolve(&rejected_path).unwrap();
        let store = TrustedFolders::new(&storage);
        store.add(&trusted, &[]).unwrap();
        store.reject(&rejected).unwrap();

        assert_eq!(
            list(&storage).unwrap(),
            vec![
                format!("trusted\t{}", trusted.path().display()),
                format!("rejected\t{}", rejected.path().display()),
            ]
        );
    }
}
