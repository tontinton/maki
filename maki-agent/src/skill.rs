//! Skill discovery: YAML-fronted Markdown files that expose named prompts to the agent.
//!
//! Project skills (found by walking ancestors up to `.git`) override global (`$HOME`) skills by name.
//! Frontmatter is minimal: only `name:` and `description:` lines are parsed; full YAML is not required.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::ToolOutput;

const PROJECT_SKILL_DIRS: &[&str] = &[
    ".maki/skills",
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
];

const GLOBAL_SKILL_DIRS: &[&str] = &[
    ".maki/skills",
    ".claude/skills",
    ".config/opencode/skills",
    ".agents/skills",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub location: PathBuf,
}

pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let home = dirs::home_dir();
    discover_skills_inner(cwd, home.as_deref())
}

fn discover_skills_inner(cwd: &Path, home: Option<&Path>) -> Vec<Skill> {
    let mut skills: HashMap<String, Skill> = HashMap::new();

    if let Some(home) = home {
        for dir in GLOBAL_SKILL_DIRS {
            scan_skill_dir(&home.join(dir), &mut skills);
        }
    }

    for dir in find_project_ancestor_dirs(cwd) {
        for skill_dir in PROJECT_SKILL_DIRS {
            scan_skill_dir(&dir.join(skill_dir), &mut skills);
        }
    }

    let mut result: Vec<_> = skills.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    debug!(count = result.len(), "skills discovered");
    result
}

impl Skill {
    pub fn find<'a>(name: &str, skills: &'a [Skill]) -> Option<&'a Skill> {
        skills.iter().find(|s| s.name == name)
    }

    pub fn to_tool_output(&self) -> ToolOutput {
        let lines: Vec<String> = self.content.lines().map(String::from).collect();
        ToolOutput::ReadCode {
            path: self.location.display().to_string(),
            start_line: 1,
            total_lines: lines.len(),
            lines,
            instructions: None,
        }
    }
}

pub fn build_skill_list_description(skills: &[Skill]) -> String {
    let body = if skills.is_empty() {
        "No skills available.\n".to_string()
    } else {
        skills
            .iter()
            .map(|s| format!("- {}: {}\n", s.name, s.description))
            .collect()
    };

    format!("\n\n<available_skills>\n{body}</available_skills>")
}

pub(crate) fn find_project_ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.to_path_buf()];
    let mut current = cwd;

    while let Some(parent) = current.parent() {
        dirs.push(parent.to_path_buf());
        if parent.join(".git").exists() {
            break;
        }
        current = parent;
    }

    dirs
}

fn scan_skill_dir(dir: &Path, skills: &mut HashMap<String, Skill>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let skill_path = entry.path().join("SKILL.md");
        if let Ok(content) = fs::read_to_string(&skill_path)
            && let Some(skill) = parse_skill(&content, &skill_path)
            && let Some(existing) = skills.insert(skill.name.clone(), skill)
        {
            debug!(
                skill = existing.name,
                path = ?skill_path,
                "skill overridden by later priority"
            );
        }
    }
}

fn parse_skill(content: &str, path: &Path) -> Option<Skill> {
    let name_from_dir = path.parent()?.file_name()?.to_string_lossy().into_owned();

    let (frontmatter, body) = split_frontmatter(content);
    let (name, description) = parse_frontmatter(&frontmatter, &name_from_dir);

    let body = body.trim();
    if body.is_empty() {
        warn!(skill = name, path = ?path, "skill file has no content, skipping");
        return None;
    }

    Some(Skill {
        name,
        description,
        content: body.to_string(),
        location: path.to_path_buf(),
    })
}

pub(crate) fn split_frontmatter(content: &str) -> (String, String) {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return (String::new(), content.to_string());
    }

    let end = content[3..].find("\n---").map(|i| i + 3);
    let Some(end) = end else {
        return (String::new(), content.to_string());
    };

    let frontmatter = content[3..end].trim().to_string();
    let body = content[end + 4..].trim().to_string();
    (frontmatter, body)
}

pub(crate) fn parse_frontmatter(frontmatter: &str, default_name: &str) -> (String, String) {
    let mut name = default_name.to_string();
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("description:") {
            description = value.trim().to_string();
        }
    }

    (name, description)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    #[test_case(
        "---\nname: test-skill\ndescription: A test\n---\nBody content",
        "name: test-skill\ndescription: A test",
        "Body content"
        ; "with_frontmatter"
    )]
    #[test_case(
        "Just body\nno frontmatter",
        "",
        "Just body\nno frontmatter"
        ; "no_frontmatter"
    )]
    #[test_case(
        "---\nname: test\nBody without closing",
        "",
        "---\nname: test\nBody without closing"
        ; "unclosed_frontmatter"
    )]
    fn split_frontmatter_cases(input: &str, expected_fm: &str, expected_body: &str) {
        let (fm, body) = split_frontmatter(input);
        assert_eq!(fm, expected_fm);
        assert_eq!(body, expected_body);
    }

    #[test_case(
        "---\nname: git-release\ndescription: Create releases\n---\n## Instructions\nDo stuff",
        "git-release",
        "git-release",
        "Create releases"
        ; "with_frontmatter"
    )]
    #[test_case(
        "Just content without frontmatter",
        "my-awesome-skill",
        "my-awesome-skill",
        ""
        ; "defaults_to_dir_name"
    )]
    fn parse_skill_extracts_fields(
        content: &str,
        dir: &str,
        expected_name: &str,
        expected_desc: &str,
    ) {
        let path = PathBuf::from(format!("/fake/{dir}/SKILL.md"));
        let skill = parse_skill(content, &path).unwrap();
        assert_eq!(skill.name, expected_name);
        assert_eq!(skill.description, expected_desc);
    }

    #[test]
    fn parse_skill_empty_content_returns_none() {
        let path = PathBuf::from("/fake/empty/SKILL.md");
        assert!(parse_skill("---\nname: empty\n---\n   \n", &path).is_none());
    }

    #[test]
    fn discover_project_overrides_global() {
        let project = TempDir::new().unwrap();
        let project_skill_dir = project.path().join(".maki/skills/overlap");
        fs::create_dir_all(&project_skill_dir).unwrap();
        fs::write(
            project_skill_dir.join("SKILL.md"),
            "---\nname: overlap\ndescription: Project version\n---\nProject content",
        )
        .unwrap();

        let global_dir = TempDir::new().unwrap();
        let global_skill_dir = global_dir.path().join(".maki/skills/overlap");
        fs::create_dir_all(&global_skill_dir).unwrap();
        fs::write(
            global_skill_dir.join("SKILL.md"),
            "---\nname: overlap\ndescription: Global version\n---\nGlobal content",
        )
        .unwrap();

        let skills = discover_skills_inner(project.path(), Some(global_dir.path()));

        let overlap: Vec<_> = skills.iter().filter(|s| s.name == "overlap").collect();
        assert_eq!(overlap.len(), 1);
        assert_eq!(overlap[0].description, "Project version");
    }

    #[test]
    fn discover_supports_all_dir_sources() {
        let dir = TempDir::new().unwrap();

        for (skill_dir, name) in [
            (".maki/skills/a-skill", "a-skill"),
            (".claude/skills/b-skill", "b-skill"),
            (".opencode/skills/c-skill", "c-skill"),
            (".agents/skills/d-skill", "d-skill"),
        ] {
            let path = dir.path().join(skill_dir);
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\n---\nContent"),
            )
            .unwrap();
        }

        let skills = discover_skills_inner(dir.path(), None);
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"a-skill"));
        assert!(names.contains(&"b-skill"));
        assert!(names.contains(&"c-skill"));
        assert!(names.contains(&"d-skill"));
    }

    #[test]
    fn build_skill_list_description_empty() {
        assert_eq!(
            build_skill_list_description(&[]),
            "\n\n<available_skills>\nNo skills available.\n</available_skills>"
        );
    }

    #[test]
    fn build_skill_list_description_with_skills() {
        let skills = vec![Skill {
            name: "git-release".into(),
            description: "Create releases".into(),
            content: "".into(),
            location: PathBuf::new(),
        }];
        let desc = build_skill_list_description(&skills);
        assert!(desc.contains("<available_skills>"));
        assert!(desc.contains("git-release"));
        assert!(desc.contains("Create releases"));
    }
}
