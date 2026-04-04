use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::AgentMode;
use crate::template::Vars;

const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    "COPILOT.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
    "GEMINI.md",
    "CODING_AGENT.md",
];

const LOCAL_INSTRUCTION_FILE: &str = "AGENTS.local.md";
const SYSTEM_INSTRUCTIONS_FILE: &str = "SYSTEM.md";

#[derive(Clone, Default)]
pub struct LoadedInstructions(Arc<Mutex<HashSet<PathBuf>>>);

impl LoadedInstructions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains_or_insert(&self, path: PathBuf) -> bool {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !set.insert(path)
    }
}

#[derive(Default)]
pub struct Instructions {
    pub text: String,
    pub loaded: LoadedInstructions,
}

pub(crate) fn is_instruction_file(name: &str) -> bool {
    name == LOCAL_INSTRUCTION_FILE
        || INSTRUCTION_FILES
            .iter()
            .any(|f| *f == name || Path::new(f).file_name().is_some_and(|n| n == name))
}

pub fn build_system_prompt(vars: &Vars, mode: &AgentMode, instructions: &str) -> String {
    let mut out = crate::prompt::SYSTEM_PROMPT.to_string();

    out.push_str(&vars.apply(
        "\n\nEnvironment:\n- Working directory: {cwd}\n- Platform: {platform}\n- Date: {date}",
    ));

    out.push_str(instructions);

    if let Some(listing) = crate::tools::memory::list_memory_files() {
        out.push_str(&listing);
    }

    if let AgentMode::Plan(plan_path) = mode {
        let plan_vars = Vars::new().set("{plan_path}", plan_path.display().to_string());
        out.push_str(&plan_vars.apply(crate::prompt::PLAN_PROMPT));
    }

    out
}

fn append_instruction_files(out: &mut String, cwd: &str) {
    let root = Path::new(cwd);

    for filename in INSTRUCTION_FILES {
        if let Ok(content) = fs::read_to_string(root.join(filename)) {
            out.push_str(&format!(
                "\n\nProject instructions ({filename}):\n{content}"
            ));
            break;
        }
    }

    if let Ok(content) = fs::read_to_string(root.join(LOCAL_INSTRUCTION_FILE)) {
        out.push_str(&format!(
            "\n\nLocal instructions ({LOCAL_INSTRUCTION_FILE}):\n{content}"
        ));
    }

    if let Some(home) = dirs::home_dir()
        && let Ok(content) = fs::read_to_string(home.join(".maki").join(SYSTEM_INSTRUCTIONS_FILE))
    {
        out.push_str(&format!(
            "\n\nSystem instructions (~/.maki/{SYSTEM_INSTRUCTIONS_FILE}):\n{content}"
        ));
    }
}

pub fn load_instruction_text(cwd: &str) -> String {
    let mut text = String::new();
    append_instruction_files(&mut text, cwd);
    text
}

pub fn load_instructions(cwd: &str) -> Instructions {
    let root = Path::new(cwd);
    let mut instr = Instructions::default();
    append_instruction_files(&mut instr.text, cwd);

    for filename in INSTRUCTION_FILES {
        let path = root.join(filename);
        if let Ok(canonical) = path.canonicalize() {
            instr.loaded.contains_or_insert(canonical);
            break;
        }
    }

    instr
}

pub fn find_subdirectory_instructions(
    dir: &Path,
    cwd: &Path,
    loaded: &LoadedInstructions,
) -> Vec<(String, String)> {
    let Ok(cwd) = cwd.canonicalize() else {
        return Vec::new();
    };
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };

    if !dir.starts_with(&cwd) || dir == cwd {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut current = dir.as_path();
    while current != cwd {
        for filename in INSTRUCTION_FILES {
            let Ok(canonical) = current.join(filename).canonicalize() else {
                continue;
            };
            if loaded.contains_or_insert(canonical.clone()) {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&canonical) {
                let display = canonical.display().to_string();
                results.push((display, content));
                break;
            }
        }
        current = match current.parent() {
            Some(p) => p,
            None => break,
        };
    }
    results
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use test_case::test_case;

    use super::*;

    const PLAN_PATH: &str = ".maki/plans/123.md";

    #[test_case(&AgentMode::Build, false ; "build_excludes_plan")]
    #[test_case(&AgentMode::Plan(PathBuf::from(PLAN_PATH)), true ; "plan_includes_plan")]
    fn plan_section_presence(mode: &AgentMode, expect_plan: bool) {
        let vars = Vars::new().set("{cwd}", "/tmp").set("{platform}", "linux");
        let prompt = build_system_prompt(&vars, mode, "");
        assert_eq!(prompt.contains("Plan Mode"), expect_plan);
        if expect_plan {
            assert!(prompt.contains(PLAN_PATH));
        }
    }

    #[test_case("AGENTS.md",                true  ; "direct_match")]
    #[test_case("CLAUDE.md",                true  ; "claude_md")]
    #[test_case("copilot-instructions.md",  true  ; "nested_path_filename")]
    #[test_case(".cursorrules",             true  ; "dotfile")]
    #[test_case("AGENTS.local.md",          true  ; "local_instruction_file")]
    #[test_case("random.md",                false ; "unrelated_file")]
    #[test_case("not-AGENTS.md",            false ; "partial_match")]
    fn is_instruction_file_cases(name: &str, expected: bool) {
        assert_eq!(is_instruction_file(name), expected);
    }

    #[test]
    fn load_instructions_merges_project_and_local() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "team rules").unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "my preferences").unwrap();

        let text = &load_instructions(dir.path().to_str().unwrap()).text;
        assert!(text.contains("team rules"));
        assert!(text.contains("my preferences"));
        assert!(
            text.find("team rules").unwrap() < text.find("my preferences").unwrap(),
            "project instructions should come before local instructions"
        );
    }

    #[test]
    fn load_instructions_local_without_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.local.md"), "solo preferences").unwrap();
        assert!(
            load_instructions(dir.path().to_str().unwrap())
                .text
                .contains("solo preferences")
        );
    }

    #[test]
    fn load_instructions_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_instructions(dir.path().to_str().unwrap())
                .text
                .is_empty()
        );
    }

    #[test]
    fn find_subdirectory_instructions_discovers_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src").join("api");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("src").join("AGENTS.md"), "api rules").unwrap();

        let loaded = LoadedInstructions::new();
        let results = find_subdirectory_instructions(&sub, dir.path(), &loaded);

        assert_eq!(results.len(), 1);
        assert!(results[0].0.ends_with("AGENTS.md"));
        assert_eq!(results[0].1, "api rules");
    }

    #[test]
    fn find_subdirectory_instructions_skips_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

        let loaded = LoadedInstructions::new();
        let from_root = find_subdirectory_instructions(dir.path(), dir.path(), &loaded);
        assert!(from_root.is_empty(), "should skip root-level directory");
    }

    #[test]
    fn find_subdirectory_instructions_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        fs::write(&agents_path, "rules").unwrap();

        let canonical = agents_path.canonicalize().unwrap();
        let loaded = LoadedInstructions::new();
        loaded.contains_or_insert(canonical);
        let pre_loaded = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert!(pre_loaded.is_empty(), "should skip already-loaded files");

        let loaded = LoadedInstructions::new();
        let first = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        let second = find_subdirectory_instructions(&sub, dir.path(), &loaded);
        assert_eq!(first.len(), 1);
        assert!(
            second.is_empty(),
            "should not return same file twice across calls"
        );
    }

    #[test]
    fn load_instructions_populates_loaded_set() {
        let dir = tempfile::tempdir().unwrap();
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&agents_path, "content").unwrap();

        let instr = load_instructions(dir.path().to_str().unwrap());
        assert!(
            instr
                .loaded
                .contains_or_insert(agents_path.canonicalize().unwrap())
        );
    }
}
