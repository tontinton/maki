use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

use crate::McpPromptInfo;

const COMMANDS_DIR: &str = "commands";
const PROJECT_COMMAND_DIRS: &[&str] = &[".maki/commands", ".claude/commands"];
const GLOBAL_THIRD_PARTY_COMMAND_DIRS: &[&str] = &[".claude/commands"];
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
pub const APPENDED_ARGUMENTS_LABEL: &str = "ARGUMENTS:";
const PROJECT_SCOPE: &str = "project";
const USER_SCOPE: &str = "user";
const SCOPE_SEPARATOR: char = ':';
pub const COMMAND_PREFIX: char = '/';
pub const COMPACT_NAME: &str = "compact";
pub const COMPACT_DESCRIPTION: &str =
    "Summarize and compact conversation history (optional guidance)";

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "argument-hint")]
    argument_hint: Option<String>,
}

/// Walks up from `cwd` and stops at the first Git checkout, so a command in the
/// repository root is found from anywhere inside it without leaking into
/// whatever sits above the checkout.
pub(crate) fn project_ancestor_dirs(cwd: &Path) -> impl Iterator<Item = PathBuf> {
    let mut current = Some(cwd.to_path_buf());
    std::iter::from_fn(move || {
        let dir = current.take()?;
        if !dir.join(".git").exists() {
            current = dir.parent().map(Path::to_path_buf);
        }
        Some(dir)
    })
}

/// Every `remote.*.url` configured for the checkout at `root`, or empty when
/// git cannot answer (not a checkout, git missing). Only feeds `projects`
/// remote matching, so a failure to read one reads as "no match".
pub(crate) fn git_remote_urls(root: &Path) -> Vec<String> {
    let Ok(output) = std::process::Command::new("git")
        .args(["config", "--null", "--get-regexp", r"^remote\..+\.url$"])
        .current_dir(root)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter_map(|entry| entry.split_once('\n').map(|(_, url)| url.to_owned()))
        .collect()
}

fn parse_frontmatter(content: &str) -> (Frontmatter, &str) {
    let content = content.trim_start();

    let Some(rest) = content.strip_prefix("---") else {
        return (Frontmatter::default(), content);
    };

    let Some(end) = rest.find("\n---") else {
        return (Frontmatter::default(), content);
    };

    let yaml = &rest[1..end + 1];
    let body = rest[end + 4..].trim();

    let fm = serde_saphyr::from_str(yaml).unwrap_or_default();
    (fm, body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandScope {
    Project,
    User,
}

impl CommandScope {
    fn label(&self) -> &'static str {
        match self {
            Self::Project => PROJECT_SCOPE,
            Self::User => USER_SCOPE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub content: String,
    pub scope: CommandScope,
    pub accepts_args: bool,
    pub argument_hint: Option<String>,
}

impl CustomCommand {
    pub fn display_name(&self) -> String {
        PromptCommand::Custom(self).slash_name()
    }

    /// A body without `$ARGUMENTS` still gets {args}, appended on a labeled
    /// line, so an `argument-hint` alone never swallows what the user typed.
    pub fn render(&self, args: &str) -> String {
        if self.content.contains(ARGUMENTS_PLACEHOLDER) {
            self.content.replace(ARGUMENTS_PLACEHOLDER, args)
        } else if args.is_empty() {
            self.content.clone()
        } else {
            format!("{}\n\n{APPENDED_ARGUMENTS_LABEL} {args}", self.content)
        }
    }

    fn is_named(&self, typed: &str) -> bool {
        typed
            .split_once(SCOPE_SEPARATOR)
            .is_some_and(|(scope, name)| {
                scope.eq_ignore_ascii_case(self.scope.label())
                    && name.eq_ignore_ascii_case(&self.name)
            })
    }

    fn summary(&self) -> &str {
        non_blank(&self.description)
            .or_else(|| self.content.lines().find_map(non_blank))
            .unwrap_or(&self.name)
    }
}

fn non_blank(text: &str) -> Option<&str> {
    Some(text.trim()).filter(|text| !text.is_empty())
}

/// A command that becomes a prompt, besides each front end's built-ins. The
/// TUI palette and ACP both read it, so a name typed in one runs the same
/// command in the other. `Display` is the name after the `/`.
#[derive(Clone, Copy)]
pub enum PromptCommand<'a> {
    Custom(&'a CustomCommand),
    Mcp(&'a McpPromptInfo),
}

impl<'a> PromptCommand<'a> {
    /// Custom commands, then MCP prompts, each with its index in its own
    /// slice. A prompt whose name holds whitespace is left out: the argument
    /// split would cut it, so nobody can type it.
    pub fn catalog(
        custom: &'a [CustomCommand],
        prompts: &'a [McpPromptInfo],
    ) -> impl Iterator<Item = (usize, Self)> + 'a {
        let custom = custom.iter().map(Self::Custom).enumerate();
        let prompts = prompts
            .iter()
            .enumerate()
            .filter(|(_, prompt)| !prompt.display_name.contains(char::is_whitespace))
            .map(|(i, prompt)| (i, Self::Mcp(prompt)));
        custom.chain(prompts)
    }

    pub fn slash_name(&self) -> String {
        format!("{COMMAND_PREFIX}{self}")
    }

    /// {typed} is the name without its `/`, compared case-blind.
    pub fn is_named(&self, typed: &str) -> bool {
        match self {
            Self::Custom(cmd) => cmd.is_named(typed),
            Self::Mcp(prompt) => prompt.display_name.eq_ignore_ascii_case(typed),
        }
    }

    pub fn takes_args(&self) -> bool {
        match self {
            Self::Custom(cmd) => cmd.accepts_args,
            Self::Mcp(prompt) => !prompt.arguments.is_empty(),
        }
    }

    /// Never blank: a custom command falls back to the first line of its
    /// body, an MCP prompt to its name.
    pub fn description(&self) -> &'a str {
        match *self {
            Self::Custom(cmd) => cmd.summary(),
            Self::Mcp(prompt) => non_blank(&prompt.description).unwrap_or(&prompt.display_name),
        }
    }
}

impl fmt::Display for PromptCommand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Custom(cmd) => write!(f, "{}{SCOPE_SEPARATOR}{}", cmd.scope.label(), cmd.name),
            Self::Mcp(prompt) => f.write_str(&prompt.display_name),
        }
    }
}

pub fn discover_commands(cwd: &Path) -> Vec<CustomCommand> {
    discover_commands_inner(
        cwd,
        maki_storage::paths::home().as_deref(),
        maki_storage::paths::config_dir().ok().as_deref(),
    )
}

fn discover_commands_inner(
    cwd: &Path,
    home: Option<&Path>,
    global_config: Option<&Path>,
) -> Vec<CustomCommand> {
    let mut commands: HashMap<String, CustomCommand> = HashMap::new();

    if let Some(dir) = global_config {
        scan_command_dir(&dir.join(COMMANDS_DIR), CommandScope::User, &mut commands);
    }
    if let Some(home) = home {
        for dir in GLOBAL_THIRD_PARTY_COMMAND_DIRS {
            scan_command_dir(&home.join(dir), CommandScope::User, &mut commands);
        }
    }

    for dir in project_ancestor_dirs(cwd) {
        for cmd_dir in PROJECT_COMMAND_DIRS {
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

    let argument_hint = fm.argument_hint.filter(|hint| !hint.trim().is_empty());
    let accepts_args = argument_hint.is_some() || body.contains(ARGUMENTS_PLACEHOLDER);

    Some(CustomCommand {
        name: fm.name.unwrap_or(name_from_file),
        description: fm.description.unwrap_or_default(),
        content: body.to_string(),
        scope,
        accepts_args,
        argument_hint,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const FIX: &str = "fix";
    const FIX_BODY: &str = "Fix the code";
    const FIX_DESCRIPTION: &str = "Quick fix";
    const REVIEW_BODY: &str = "Review $ARGUMENTS";
    const ARGS: &str = "src/main.rs";
    const REVIEW_RENDERED: &str = "Review src/main.rs";
    const MULTILINE_BODY: &str = "\n  First line  \nSecond line";
    const MULTILINE_FIRST_LINE: &str = "First line";
    const PROMPT_NAME: &str = "srv:greet";
    const PROMPT_QUALIFIED: &str = "srv.greet";
    const PROMPT_DESCRIPTION: &str = "Say hello";
    const SPACED_PROMPT_NAME: &str = "srv:say hello";

    fn custom(scope: CommandScope, content: &str, description: &str) -> CustomCommand {
        CustomCommand {
            name: FIX.into(),
            description: description.into(),
            content: content.into(),
            scope,
            accepts_args: true,
            argument_hint: None,
        }
    }

    #[test_case(REVIEW_BODY, ARGS, REVIEW_RENDERED ; "fills_placeholder")]
    #[test_case(FIX_BODY, "", FIX_BODY ; "no_args_keeps_body")]
    fn render_custom_command(content: &str, args: &str, expected: &str) {
        let cmd = custom(CommandScope::User, content, "");
        assert_eq!(cmd.render(args), expected);
    }

    #[test]
    fn render_appends_args_without_placeholder() {
        let cmd = custom(CommandScope::User, FIX_BODY, "");
        assert_eq!(
            cmd.render(ARGS),
            format!("{FIX_BODY}\n\n{APPENDED_ARGUMENTS_LABEL} {ARGS}")
        );
    }

    #[test_case(FIX_DESCRIPTION, FIX_BODY, FIX_DESCRIPTION ; "own_description")]
    #[test_case("", MULTILINE_BODY, MULTILINE_FIRST_LINE ; "first_body_line")]
    #[test_case("  ", FIX_BODY, FIX_BODY ; "blank_description")]
    #[test_case("", "", FIX ; "falls_back_to_name")]
    fn custom_description(description: &str, content: &str, expected: &str) {
        let cmd = custom(CommandScope::User, content, description);
        assert_eq!(PromptCommand::Custom(&cmd).description(), expected);
    }

    #[test_case(PROMPT_DESCRIPTION, PROMPT_DESCRIPTION ; "own_description")]
    #[test_case("", PROMPT_NAME ; "falls_back_to_name")]
    fn mcp_description(description: &str, expected: &str) {
        let prompt = McpPromptInfo {
            description: description.into(),
            ..McpPromptInfo::new(PROMPT_NAME, PROMPT_QUALIFIED, &[])
        };
        assert_eq!(PromptCommand::Mcp(&prompt).description(), expected);
    }

    #[test_case(CommandScope::User, "user:fix", true ; "exact")]
    #[test_case(CommandScope::User, "USER:Fix", true ; "case_blind")]
    #[test_case(CommandScope::Project, "user:fix", false ; "other_scope")]
    #[test_case(CommandScope::User, "fix", false ; "no_scope")]
    #[test_case(CommandScope::User, "user:fi", false ; "prefix")]
    fn custom_is_named(scope: CommandScope, typed: &str, expected: bool) {
        let cmd = custom(scope, FIX_BODY, "");
        assert_eq!(PromptCommand::Custom(&cmd).is_named(typed), expected);
    }

    #[test]
    fn catalog_skips_prompts_nobody_can_type() {
        let commands = [custom(CommandScope::User, FIX_BODY, "")];
        let prompts = [
            McpPromptInfo::new(SPACED_PROMPT_NAME, PROMPT_QUALIFIED, &[]),
            McpPromptInfo::new(PROMPT_NAME, PROMPT_QUALIFIED, &[]),
        ];
        let names: Vec<_> = PromptCommand::catalog(&commands, &prompts)
            .map(|(i, command)| (i, command.slash_name()))
            .collect();
        let expected = [
            (0, commands[0].display_name()),
            (1, format!("{COMMAND_PREFIX}{PROMPT_NAME}")),
        ];
        assert_eq!(names, expected);
    }

    #[test_case(
        "---\nname: review\ndescription: Code review\nargument-hint: <file>\n---\nReview $ARGUMENTS",
        "review", "Code review", true, Some("<file>")
        ; "full_frontmatter"
    )]
    #[test_case(
        "Review $ARGUMENTS",
        "test-cmd", "", true, None
        ; "body_placeholder_without_hint"
    )]
    #[test_case(
        "Just do things",
        "test-cmd", "", false, None
        ; "no_frontmatter_uses_filename"
    )]
    #[test_case(
        "---\ndescription: Quick fix\n---\nFix the code",
        "test-cmd", "Quick fix", false, None
        ; "no_args_placeholder"
    )]
    #[test_case(
        "---\nargument-hint: \"  \"\n---\nFix the code",
        "test-cmd", "", false, None
        ; "blank_hint_is_no_hint"
    )]
    fn parse_command_fields(
        content: &str,
        expected_name: &str,
        expected_desc: &str,
        expected_has_args: bool,
        expected_hint: Option<&str>,
    ) {
        let path = PathBuf::from("/fake/test-cmd.md");
        let cmd = parse_command(content, &path, CommandScope::Project).unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.description, expected_desc);
        assert_eq!(cmd.accepts_args, expected_has_args);
        assert_eq!(cmd.argument_hint.as_deref(), expected_hint);
    }

    #[test]
    fn parse_command_keeps_argument_hint() {
        let path = PathBuf::from("/fake/review.md");
        let cmd = parse_command(
            "---\nargument-hint: <file>\n---\nReview $ARGUMENTS",
            &path,
            CommandScope::Project,
        )
        .unwrap();
        assert_eq!(cmd.argument_hint.as_deref(), Some("<file>"));
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
            argument_hint: None,
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

        let commands = discover_commands_inner(project.path(), Some(global.path()), None);
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

        let commands = discover_commands_inner(dir.path(), None, None);
        let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"a-cmd"));
        assert!(names.contains(&"b-cmd"));
    }

    #[test]
    fn discover_loads_commands_from_each_project_owner() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let child = root.path().join("child");
        for (owner, name) in [(root.path(), "root"), (child.as_path(), "child")] {
            let command_dir = owner.join(".maki/commands");
            fs::create_dir_all(&command_dir).unwrap();
            fs::write(command_dir.join(format!("{name}.md")), name).unwrap();
        }

        let commands = discover_commands_inner(&child, None, None);
        let names = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["child", "root"]);
    }

    #[test]
    fn discover_ignores_non_md_files() {
        let dir = TempDir::new().unwrap();
        let cmd_dir = dir.path().join(".maki/commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(cmd_dir.join("valid.md"), "Content").unwrap();
        fs::write(cmd_dir.join("invalid.txt"), "Content").unwrap();

        let commands = discover_commands_inner(dir.path(), None, None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "valid");
    }

    #[test_case(
        "---\n: invalid: yaml: [[\n---\nBody",
        None, "Body"
        ; "invalid_yaml_falls_back"
    )]
    #[test_case(
        "---\nname: oops\nThis never closes",
        None, "---\nname: oops\nThis never closes"
        ; "no_closing_delimiter"
    )]
    #[test_case(
        "  \n---\nname: trimmed\n---\nBody",
        Some("trimmed"), "Body"
        ; "leading_whitespace"
    )]
    fn parse_frontmatter_edge_cases(input: &str, expected_name: Option<&str>, expected_body: &str) {
        let (fm, body) = parse_frontmatter(input);
        assert_eq!(fm.name.as_deref(), expected_name);
        assert_eq!(body, expected_body);
    }

    #[test]
    fn project_ancestor_dirs_stop_at_git() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(tmp.path().join("a/.git")).unwrap();

        let dirs: Vec<_> = project_ancestor_dirs(&deep).collect();
        let dir_strs: Vec<_> = dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();

        assert!(dir_strs.contains(&deep.to_string_lossy().into_owned()));
        assert!(dir_strs.contains(&tmp.path().join("a/b").to_string_lossy().into_owned()));
        assert!(dir_strs.contains(&tmp.path().join("a").to_string_lossy().into_owned()));
        assert!(
            !dir_strs.contains(&tmp.path().to_string_lossy().into_owned()),
            "should not traverse past .git"
        );
    }
}
