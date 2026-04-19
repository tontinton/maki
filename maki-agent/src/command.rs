use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use tracing::{debug, warn};

use crate::skill::{find_project_ancestor_dirs, parse_frontmatter};

const COMMAND_DIRS: &[&str] = &[".maki/commands", ".claude/commands"];
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandScope {
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub content: String,
    pub scope: CommandScope,
    pub accepts_args: bool,
}

impl CustomCommand {
    pub fn display_name(&self) -> String {
        let prefix = match self.scope {
            CommandScope::Project => "/project",
            CommandScope::User => "/user",
        };
        format!("{prefix}:{}", self.name)
    }

    pub fn has_args(&self) -> bool {
        self.accepts_args
    }

    pub fn render(&self, args: &str) -> String {
        self.content.replace(ARGUMENTS_PLACEHOLDER, args)
    }
}

pub fn discover_commands(cwd: &Path) -> Vec<CustomCommand> {
    let home = dirs::home_dir();
    discover_commands_inner(cwd, home.as_deref())
}

fn discover_commands_inner(cwd: &Path, home: Option<&Path>) -> Vec<CustomCommand> {
    let mut commands: HashMap<String, CustomCommand> = HashMap::new();

    if let Some(home) = home {
        for dir in COMMAND_DIRS {
            scan_command_dir(&home.join(dir), CommandScope::User, &mut commands);
        }
    }

    for dir in find_project_ancestor_dirs(cwd) {
        for cmd_dir in COMMAND_DIRS {
            scan_command_dir(&dir.join(cmd_dir), CommandScope::Project, &mut commands);
        }
    }

    let mut result: Vec<_> = commands.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    debug!(count = result.len(), "commands discovered");
    result
}

fn scan_command_dir(
    dir: &Path,
    scope: CommandScope,
    commands: &mut HashMap<String, CustomCommand>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension() else {
            continue;
        };
        if ext != "md" {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path)
            && let Some(cmd) = parse_command(&content, &path, scope.clone())
            && let Some(existing) = commands.insert(cmd.name.clone(), cmd)
        {
            debug!(
                command = existing.name,
                path = ?path,
                "command overridden by later priority"
            );
        }
    }
}

fn parse_command(content: &str, path: &Path, scope: CommandScope) -> Option<CustomCommand> {
    let name_from_file = path.file_stem()?.to_string_lossy().into_owned();
    let (fm, body) = parse_frontmatter(content);

    if body.is_empty() {
        let name = fm.name.as_deref().unwrap_or(&name_from_file);
        warn!(command = name, path = ?path, "command file has no content, skipping");
        return None;
    }

    let accepts_args = fm.argument_hint.is_some() || body.contains(ARGUMENTS_PLACEHOLDER);

    Some(CustomCommand {
        name: fm.name.unwrap_or(name_from_file),
        description: fm.description.unwrap_or_default(),
        content: body.to_string(),
        scope,
        accepts_args,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    #[test_case(
        "---\nname: review\ndescription: Code review\nargument-hint: <file>\n---\nReview $ARGUMENTS",
        "review", "Code review", true
        ; "full_frontmatter"
    )]
    #[test_case(
        "Just do things",
        "test-cmd", "", false
        ; "no_frontmatter_uses_filename"
    )]
    #[test_case(
        "---\ndescription: Quick fix\n---\nFix the code",
        "test-cmd", "Quick fix", false
        ; "no_args_placeholder"
    )]
    fn parse_command_fields(
        content: &str,
        expected_name: &str,
        expected_desc: &str,
        expected_has_args: bool,
    ) {
        let path = PathBuf::from("/fake/test-cmd.md");
        let cmd = parse_command(content, &path, CommandScope::Project).unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.description, expected_desc);
        assert_eq!(cmd.has_args(), expected_has_args);
    }

    #[test]
    fn parse_command_empty_body_returns_none() {
        let path = PathBuf::from("/fake/empty.md");
        assert!(parse_command("---\nname: empty\n---\n   \n", &path, CommandScope::User).is_none());
    }

    #[test_case(CommandScope::Project, "/project:review" ; "project_scope")]
    #[test_case(CommandScope::User, "/user:review" ; "user_scope")]
    fn display_name_prefix(scope: CommandScope, expected: &str) {
        let cmd = CustomCommand {
            name: "review".into(),
            description: String::new(),
            content: "body".into(),
            scope,
            accepts_args: false,
        };
        assert_eq!(cmd.display_name(), expected);
    }

    #[test]
    fn discover_project_overrides_global() {
        let project = TempDir::new().unwrap();
        let cmd_dir = project.path().join(".maki/commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("overlap.md"),
            "---\ndescription: Project version\n---\nProject content",
        )
        .unwrap();

        let global = TempDir::new().unwrap();
        let global_cmd_dir = global.path().join(".maki/commands");
        fs::create_dir_all(&global_cmd_dir).unwrap();
        fs::write(
            global_cmd_dir.join("overlap.md"),
            "---\ndescription: Global version\n---\nGlobal content",
        )
        .unwrap();

        let commands = discover_commands_inner(project.path(), Some(global.path()));
        let overlap: Vec<_> = commands.iter().filter(|c| c.name == "overlap").collect();
        assert_eq!(overlap.len(), 1);
        assert_eq!(overlap[0].description, "Project version");
        assert_eq!(overlap[0].scope, CommandScope::Project);
    }

    #[test]
    fn discover_supports_both_dir_sources() {
        let dir = TempDir::new().unwrap();

        for (cmd_dir, filename) in [
            (".maki/commands", "a-cmd.md"),
            (".claude/commands", "b-cmd.md"),
        ] {
            let path = dir.path().join(cmd_dir);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join(filename), "Content").unwrap();
        }

        let commands = discover_commands_inner(dir.path(), None);
        let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"a-cmd"));
        assert!(names.contains(&"b-cmd"));
    }

    #[test]
    fn discover_ignores_non_md_files() {
        let dir = TempDir::new().unwrap();
        let cmd_dir = dir.path().join(".maki/commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(cmd_dir.join("valid.md"), "Content").unwrap();
        fs::write(cmd_dir.join("invalid.txt"), "Content").unwrap();

        let commands = discover_commands_inner(dir.path(), None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "valid");
    }
}
