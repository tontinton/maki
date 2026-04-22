use maki_agent::tools::{
    BASH_TOOL_NAME, BATCH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME, EDIT_TOOL_NAME, GLOB_TOOL_NAME,
    GREP_TOOL_NAME, MULTIEDIT_TOOL_NAME, QUESTION_TOOL_NAME, READ_TOOL_NAME, TASK_TOOL_NAME,
    TODOWRITE_TOOL_NAME, WRITE_TOOL_NAME,
};
use maki_agent::{
    AgentEvent, BatchToolEntry, BatchToolStatus, Envelope, GrepFileEntry, GrepMatchGroup,
    QuestionInfo, QuestionOption, SubagentInfo, TodoItem, TodoPriority, TodoStatus, ToolDoneEvent,
    ToolInput, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use maki_providers::{Message, TokenUsage};

const TASK_TOOL_ID: &str = "t_task";
const TASK_TOOL_ID_2: &str = "t_task2";
const QUESTION_TOOL_ID: &str = "t_qform";

pub enum MockEvent {
    User(String),
    Error(String),
    Flush,
    Agent(Box<Envelope>),
}

fn user(text: &str) -> MockEvent {
    MockEvent::User(text.into())
}

fn evt(event: AgentEvent) -> MockEvent {
    MockEvent::Agent(Box::new(Envelope {
        event,
        subagent: None,
        run_id: 1,
    }))
}

fn sub_evt(event: AgentEvent, parent_id: &str, name: &str, prompt: Option<&str>) -> MockEvent {
    sub_evt_with(event, parent_id, name, prompt, None)
}

fn sub_evt_with(
    event: AgentEvent,
    parent_id: &str,
    name: &str,
    prompt: Option<&str>,
    model: Option<&str>,
) -> MockEvent {
    MockEvent::Agent(Box::new(Envelope {
        event,
        subagent: Some(SubagentInfo {
            parent_tool_use_id: parent_id.into(),
            name: name.into(),
            prompt: prompt.map(String::from),
            model: model.map(String::from),
            answer_tx: None,
        }),
        run_id: 1,
    }))
}

fn tool_start(id: &str, tool: &str, summary: &str, input: Option<ToolInput>) -> AgentEvent {
    tool_start_with(id, tool, summary, input, None)
}

fn tool_start_with(
    id: &str,
    tool: &str,
    summary: &str,
    input: Option<ToolInput>,
    annotation: Option<&str>,
) -> AgentEvent {
    AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: id.into(),
        tool: tool.into(),
        summary: summary.into(),
        annotation: annotation.map(Into::into),
        input,
        output: None,
        render_header: None,
    }))
}

fn tool_done(id: &str, tool: &str, output: ToolOutput, is_error: bool) -> AgentEvent {
    AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: tool.into(),
        output,
        is_error,
    }))
}

fn turn_complete() -> AgentEvent {
    AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: Message::user(String::new()),
        usage: TokenUsage::default(),
        model: "mock".into(),
        context_size: None,
    }))
}

pub fn mock_questions() -> Vec<QuestionInfo> {
    vec![
        QuestionInfo {
            question: "What language do you want to use?".into(),
            header: "Language".into(),
            options: vec![
                QuestionOption {
                    label: "TypeScript".into(),
                    description: "Popular for web".into(),
                },
                QuestionOption {
                    label: "Rust".into(),
                    description: "Fast and safe".into(),
                },
                QuestionOption {
                    label: "Go".into(),
                    description: "Simple concurrency".into(),
                },
            ],
            multiple: false,
        },
        QuestionInfo {
            question: "Which framework do you prefer?".into(),
            header: "Framework".into(),
            options: vec![
                QuestionOption {
                    label: "Next.js".into(),
                    description: "React SSR".into(),
                },
                QuestionOption {
                    label: "tRPC".into(),
                    description: "End-to-end typesafe".into(),
                },
                QuestionOption {
                    label: "SvelteKit".into(),
                    description: "Compiler-based".into(),
                },
            ],
            multiple: true,
        },
        QuestionInfo {
            question: "What database should we use?".into(),
            header: "Database".into(),
            options: vec![
                QuestionOption {
                    label: "PostgreSQL".into(),
                    description: "Relational".into(),
                },
                QuestionOption {
                    label: "SQLite".into(),
                    description: "Embedded".into(),
                },
            ],
            multiple: false,
        },
    ]
}

pub fn question_tool_id() -> &'static str {
    QUESTION_TOOL_ID
}

#[allow(clippy::vec_init_then_push)]
pub fn mock_events() -> Vec<MockEvent> {
    let mut events = Vec::new();

    // === Main chat: config refactor conversation ===
    events.push(user(
        "Refactor the config module to use builder pattern and add validation.",
    ));

    events.push(evt(AgentEvent::ThinkingDelta {
        text: "Let me analyze the config module structure. I'll need to look at the existing implementation, understand the current API surface, and plan the refactor to use a builder pattern with proper validation.".into(),
    }));

    events.push(evt(AgentEvent::TextDelta {
        text: concat!(
            "I'll refactor the config module. Let me start by reading the current implementation.\n",
            "\n",
            "## Plan\n",
            "\n",
            "1. Read existing `Config` struct and *understand* the current API\n",
            "2. Create **`ConfigBuilder`** with a ***fluent interface***\n",
            "3. Add validation - ~~manual checks~~ replaced with `validate()` method\n",
            "4. Update tests\n",
            "   - Unit tests for _builder methods_\n",
            "   - Integration tests for **validation rules**\n",
            "\n",
            "### Current structure\n",
            "\n",
            "The `Config` struct in ``src/config/mod.rs`` is straightforward:\n",
            "\n",
            "```rust\n",
            "pub struct Config {\n",
            "    pub port: u16,\n",
            "    pub host: String,\n",
            "    pub workers: Option<usize>,\n",
            "}\n",
            "```\n",
            "\n",
            "I'll transform this into a *builder* with **compile-time** guarantees.",
        ).into(),
    }));

    events.push(evt(turn_complete()));

    // Bash - Success
    events.push(evt(tool_start_with(
        "t_bash",
        BASH_TOOL_NAME,
        "ls -la src/config/",
        Some(ToolInput::Code {
            language: "bash".into(),
            code: "ls -la src/config/".into(),
        }),
        Some("2m timeout"),
    )));
    events.push(evt(tool_done(
        "t_bash",
        BASH_TOOL_NAME,
        ToolOutput::Plain(
            "-rw-r--r-- 1 user staff  2048 Jan 15 10:30 mod.rs\n\
             -rw-r--r-- 1 user staff  1024 Jan 15 10:30 builder.rs\n\
             -rw-r--r-- 1 user staff   512 Jan 15 10:30 validation.rs"
                .into(),
        ),
        false,
    )));

    // Read - Success
    events.push(evt(tool_start(
        "t_read",
        READ_TOOL_NAME,
        "src/config/mod.rs",
        None,
    )));
    events.push(evt(tool_done(
        "t_read",
        READ_TOOL_NAME,
        ToolOutput::ReadCode {
            path: "src/config/mod.rs".into(),
            start_line: 1,
            lines: vec![
                "use std::path::PathBuf;".into(),
                "".into(),
                "pub struct Config {".into(),
                "    pub port: u16,".into(),
                "}".into(),
            ],
            total_lines: 5,
            instructions: None,
        },
        false,
    )));

    // Edit - Success
    events.push(evt(tool_start(
        "t_edit",
        EDIT_TOOL_NAME,
        "src/config/mod.rs",
        None,
    )));
    events.push(evt(tool_done(
        "t_edit",
        EDIT_TOOL_NAME,
        ToolOutput::Diff {
            path: "src/config/mod.rs".into(),
            before: "use std::net::IpAddr;\n\npub struct Config {\n    pub port: u16,\n}\n".into(),
            after: "use std::net::IpAddr;\n\npub struct ConfigBuilder {\n    pub port: u16,\n    pub host: String,\n}\n".into(),
            summary: "Renamed Config to ConfigBuilder, added host field".into(),
        },
        false,
    )));

    // Write - Success
    events.push(evt(tool_start(
        "t_write",
        WRITE_TOOL_NAME,
        "src/config/validation.rs (87 bytes)",
        None,
    )));
    events.push(evt(tool_done(
        "t_write",
        WRITE_TOOL_NAME,
        ToolOutput::WriteCode {
            path: "src/config/validation.rs".into(),
            byte_count: 87,
            lines: vec![
                "pub fn validate_port(port: u16) -> bool {".into(),
                "    port > 0 && port < 65535".into(),
                "}".into(),
            ],
        },
        false,
    )));

    // Glob - Success
    events.push(evt(tool_start("t_glob", GLOB_TOOL_NAME, "**/*.rs", None)));
    events.push(evt(tool_done(
        "t_glob",
        GLOB_TOOL_NAME,
        ToolOutput::GlobResult {
            files: vec![
                "src/config/mod.rs".into(),
                "src/config/builder.rs".into(),
                "src/config/validation.rs".into(),
            ],
        },
        false,
    )));

    // Grep - Success
    events.push(evt(tool_start(
        "t_grep",
        GREP_TOOL_NAME,
        "\\b(Config|Builder)\\b [*.rs] in src/config/",
        None,
    )));
    events.push(evt(tool_done(
        "t_grep",
        GREP_TOOL_NAME,
        ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/config/mod.rs".into(),
                    groups: vec![GrepMatchGroup::single(3, "pub struct ConfigBuilder {")],
                },
                GrepFileEntry {
                    path: "src/main.rs".into(),
                    groups: vec![GrepMatchGroup::single(12, "use config::ConfigBuilder;")],
                },
            ],
        },
        false,
    )));

    // TodoWrite - Success
    events.push(evt(tool_start(
        "t_todo",
        TODOWRITE_TOOL_NAME,
        "Updated todo list",
        None,
    )));
    events.push(evt(tool_done(
        "t_todo",
        TODOWRITE_TOOL_NAME,
        ToolOutput::TodoList(vec![
            TodoItem {
                content: "Read existing config".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::High,
            },
            TodoItem {
                content: "Create builder struct".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::High,
            },
            TodoItem {
                content: "Add validation".into(),
                status: TodoStatus::InProgress,
                priority: TodoPriority::Medium,
            },
            TodoItem {
                content: "Update tests".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
            },
        ]),
        false,
    )));

    // Index - Success
    events.push(evt(tool_start(
        "t_index",
        "index",
        "src/config/mod.rs",
        None,
    )));
    events.push(evt(tool_done(
        "t_index",
        "index",
        ToolOutput::Plain(
            concat!(
                "imports: [1-3]\n",
                "  std::collections::HashMap\n",
                "  serde::Deserialize\n",
                "\n",
                "types:\n",
                "  #[derive(Debug, Deserialize)]\n",
                "  pub struct Config [5-9]\n",
                "    pub port: u16\n",
                "    pub host: String\n",
                "    pub workers: Option<usize>\n",
                "\n",
                "impls:\n",
                "  Config [11-20]\n",
                "    pub fn from_env() -> Result<Self, ConfigError>\n",
                "    pub fn validate(&self) -> Result<(), ConfigError>",
            )
            .into(),
        ),
        false,
    )));

    // WebFetch - Success
    events.push(evt(tool_start(
        "t_web",
        "webfetch",
        "https://docs.rs/config",
        None,
    )));
    events.push(evt(tool_done(
        "t_web",
        "webfetch",
        ToolOutput::Plain("Configuration crate docs content...".into()),
        false,
    )));

    // Task - Success (main chat side; subagent events follow below)
    events.push(evt(tool_start(
        TASK_TOOL_ID,
        TASK_TOOL_NAME,
        "Explore config patterns",
        None,
    )));
    events.push(evt(tool_done(
        TASK_TOOL_ID,
        TASK_TOOL_NAME,
        ToolOutput::Plain(
            concat!(
                "Found 3 relevant patterns in the codebase.\n",
                "\n",
                "## Builder Pattern\n",
                "\n",
                "Used in `src/http/client.rs` with **fluent setters** and a `build()` method:\n",
                "\n",
                "```rust\n",
                "let client = ClientBuilder::new(\"https://api.example.com\")\n",
                "    .timeout(Duration::from_secs(30))\n",
                "    .retries(3)\n",
                "    .build()?;\n",
                "```\n",
                "\n",
                "## Validation\n",
                "\n",
                "In `src/auth/token.rs` — returns `Result<Claims>` with *descriptive* errors:\n",
                "\n",
                "| Check | Method | Returns |\n",
                "| --- | --- | --- |\n",
                "| Expiry | `validate_expiry()` | `TokenError::Expired` |\n",
                "| Signature | `validate_sig()` | `TokenError::InvalidSig` |\n",
                "| Audience | `validate_aud()` | `TokenError::WrongAudience` |\n",
                "\n",
                "## Default Impl\n",
                "\n",
                "`PoolBuilder` in `src/db/pool.rs` implements `Default`:\n",
                "- `max_connections`: **10**\n",
                "- `idle_timeout`: _30s_\n",
                "- ~~`min_connections`~~ removed in v2\n",
            )
            .into(),
        ),
        false,
    )));

    // Task - Success with model_tier weak (main chat side; subagent events follow below)
    events.push(evt(tool_start(
        TASK_TOOL_ID_2,
        TASK_TOOL_NAME,
        "Summarize test coverage",
        None,
    )));
    events.push(evt(tool_done(
        TASK_TOOL_ID_2,
        TASK_TOOL_NAME,
        ToolOutput::Plain(
            "Test coverage summary:\n- Unit tests: 82% (src/)\n- Integration: 64% (tests/)\n- Missing: src/config/validation.rs".into(),
        ),
        false,
    )));

    // Batch - Success
    events.push(evt(tool_start(
        "t_batch",
        BATCH_TOOL_NAME,
        "Batch (3 tools)",
        None,
    )));
    events.push(evt(tool_done(
        "t_batch",
        BATCH_TOOL_NAME,
        ToolOutput::Batch {
            entries: vec![
                BatchToolEntry {
                    tool: "read".into(),
                    summary: "src/config/mod.rs".into(),
                    status: BatchToolStatus::Success,
                    input: None,
                    output: None,
                    annotation: None,
                },
                BatchToolEntry {
                    tool: "read".into(),
                    summary: "src/config/builder.rs".into(),
                    status: BatchToolStatus::Success,
                    input: None,
                    output: None,
                    annotation: None,
                },
                BatchToolEntry {
                    tool: "read".into(),
                    summary: "src/config/validation.rs".into(),
                    status: BatchToolStatus::Success,
                    input: None,
                    output: None,
                    annotation: None,
                },
            ],
            text: String::new(),
        },
        false,
    )));

    // Question - Success
    events.push(evt(tool_start(
        "t_question",
        QUESTION_TOOL_NAME,
        "2 questions",
        None,
    )));
    events.push(evt(tool_done(
        "t_question",
        QUESTION_TOOL_NAME,
        ToolOutput::Plain(
            "What testing framework do you prefer?\nShould I add integration tests as well?".into(),
        ),
        false,
    )));

    // CodeExecution - Success
    events.push(evt(tool_start(
        "t_code_exec",
        CODE_EXECUTION_TOOL_NAME,
        "6 lines",
        Some(ToolInput::Script {
            language: "python".into(),
            code: "files = glob(pattern='**/*.rs', path='src/config')
total = 0
for f in files:
    content = read(path=f)
    total += len(content.split('\\n'))
print(f'Total lines across config: {total}')"
                .into(),
        }),
    )));
    events.push(evt(tool_done(
        "t_code_exec",
        CODE_EXECUTION_TOOL_NAME,
        ToolOutput::Plain("Total lines across config: 47".into()),
        false,
    )));

    // MultiEdit - Success
    events.push(evt(tool_start(
        "t_multiedit",
        MULTIEDIT_TOOL_NAME,
        "src/main.rs",
        None,
    )));
    events.push(evt(tool_done(
        "t_multiedit",
        MULTIEDIT_TOOL_NAME,
        ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "use config::Config;\n\nfn main() {}\n".into(),
            after: "use config::ConfigBuilder;\n\nfn main() {}\n".into(),
            summary: "Updated import to use ConfigBuilder".into(),
        },
        false,
    )));

    // Bash - Error
    events.push(evt(tool_start_with(
        "t_bash_err",
        BASH_TOOL_NAME,
        "cargo test",
        Some(ToolInput::Code {
            language: "bash".into(),
            code: "cargo test".into(),
        }),
        Some("2m timeout"),
    )));
    events.push(evt(tool_done(
        "t_bash_err",
        BASH_TOOL_NAME,
        ToolOutput::Plain(
            "error[E0433]: failed to resolve: use of undeclared type `Config`\n  --> src/main.rs:15:5".into(),
        ),
        true,
    )));

    // Bash - InProgress (spinner animates)
    events.push(evt(tool_start_with(
        "t_bash_ip",
        BASH_TOOL_NAME,
        "cargo build --release",
        Some(ToolInput::Code {
            language: "bash".into(),
            code: "cargo build --release".into(),
        }),
        Some("2m timeout"),
    )));

    events.push(evt(turn_complete()));

    // Error message (pushed directly, not via AgentEvent to avoid side effects)
    events.push(MockEvent::Error(
        "Connection timed out after 30s. Retrying...".into(),
    ));

    // New turn after error — assistant code block
    events.push(evt(AgentEvent::TextDelta {
        text: concat!(
            "Here's what the output looks like:\n",
            "\n",
            "```\n",
            "$ cargo test\n",
            "running 396 tests\n",
            "test result: ok. 396 passed; 0 failed\n",
            "```\n",
            "\n",
            "All good.",
        )
        .into(),
    }));

    events.push(evt(turn_complete()));
    events.push(MockEvent::Flush);

    // Assistant final summary
    events.push(evt(AgentEvent::TextDelta {
        text: concat!(
            "Done! The config module now uses a ***builder pattern*** with validation.\n",
            "\n",
            "## Summary\n",
            "\n",
            "**Changes:**\n",
            "- `ConfigBuilder` with `port()` and `host()` methods\n",
            "- ~~`Config::new()`~~ replaced with `ConfigBuilder::default().build()`\n",
            "- _Validation_ via `validate_port()` - rejects ports **outside** `1..=65534`\n",
            "  - Returns `Result<Config, ConfigError>` instead of *panicking*\n",
            "\n",
            "| File | Change | Lines |\n",
            "| --- | --- | --- |\n",
            "| `mod.rs` | Renamed struct | +2 / -1 |\n",
            "| `builder.rs` | New builder impl | +45 |\n",
            "| `validation.rs` | New validation | +12 |\n",
            "| `main.rs` | Updated imports | +1 / -1 |\n",
            "\n",
            "---\n",
            "\n",
            "### Before / After\n",
            "\n",
            "```rust\n",
            "// Before\n",
            "let cfg = Config { port: 8080, host: \"localhost\".into() };\n",
            "\n",
            "// After - this is an intentionally very long line to test horizontal wrapping behavior in the UI: ConfigBuilder::default().port(8080).host(\"localhost\").workers(num_cpus::get()).timeout(Duration::from_secs(30)).max_retries(3).backoff_strategy(ExponentialBackoff::new()).enable_tls(true).tls_cert_path(\"/etc/ssl/certs/server.pem\").build()\n",
            "// After\n",
            "let cfg = ConfigBuilder::default()\n",
            "    .port(8080)\n",
            "    .host(\"localhost\")\n",
            "    .build()?;\n",
            "```\n",
            "\n",
            "All **396** tests pass. Run `cargo test` to verify.\n",
            "\n",
            "### Go equivalent\n",
            "\n",
            "```go\n",
            "package main\n",
            "\n",
            "import \"fmt\"\n",
            "\n",
            "func main() {\n",
            "\tconfig := Config{\n",
            "\t\tPort: 8080,\n",
            "\t\tHost: \"localhost\",\n",
            "\t}\n",
            "\tfmt.Printf(\"listening on %s:%d\\n\", config.Host, config.Port)\n",
            "}\n",
            "```\n",
            "\n",
            "### Lua one-liner\n",
            "\n",
            "```lua\n",
            "print(\"hello from lua\")\n",
            "```\n",
            "\n",
            "Tabs render correctly above.",
        ).into(),
    }));

    // === Subagent: task tool ("Explore config patterns") ===
    let task_prompt = "Search the codebase for existing builder patterns and validation approaches. Return file paths and a summary of how they are implemented.";

    sub_evt(
        AgentEvent::ThinkingDelta {
            text: "The user wants me to explore config patterns in the codebase. Let me search for existing builder patterns and validation approaches.".into(),
        },
        TASK_TOOL_ID,
        "Explore config patterns",
        Some(task_prompt),
    );
    // ^ First subagent event creates the chat with prompt as user message

    events.push(sub_evt(
        AgentEvent::ThinkingDelta {
            text: "The user wants me to explore config patterns in the codebase. Let me search for existing builder patterns and validation approaches.".into(),
        },
        TASK_TOOL_ID,
        "Explore config patterns",
        Some(task_prompt),
    ));

    events.push(sub_evt(
        tool_start("s_grep1", GREP_TOOL_NAME, "\\bBuilder\\b [*.rs]", None),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));
    events.push(sub_evt(
        tool_done(
            "s_grep1",
            GREP_TOOL_NAME,
            ToolOutput::GrepResult {
                entries: vec![
                    GrepFileEntry {
                        path: "src/http/client.rs".into(),
                        groups: vec![
                            GrepMatchGroup::single(22, "pub struct ClientBuilder {"),
                            GrepMatchGroup::single(45, "impl ClientBuilder {"),
                        ],
                    },
                    GrepFileEntry {
                        path: "src/db/pool.rs".into(),
                        groups: vec![GrepMatchGroup::single(8, "pub struct PoolBuilder {")],
                    },
                ],
            },
            false,
        ),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));

    events.push(sub_evt(
        tool_start("s_read1", READ_TOOL_NAME, "src/http/client.rs", None),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));
    events.push(sub_evt(
        tool_done(
            "s_read1",
            READ_TOOL_NAME,
            ToolOutput::ReadCode {
                path: "src/http/client.rs".into(),
                start_line: 22,
                lines: vec![
                    "pub struct ClientBuilder {".into(),
                    "    timeout: Option<Duration>,".into(),
                    "    retries: u32,".into(),
                    "    base_url: String,".into(),
                    "}".into(),
                    "".into(),
                    "impl ClientBuilder {".into(),
                    "    pub fn new(base_url: impl Into<String>) -> Self {".into(),
                    "        Self { timeout: None, retries: 3, base_url: base_url.into() }".into(),
                    "    }".into(),
                    "".into(),
                    "    pub fn build(self) -> Result<Client, ConfigError> {".into(),
                ],
                total_lines: 50,
                instructions: None,
            },
            false,
        ),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));

    events.push(sub_evt(
        tool_start("s_grep2", GREP_TOOL_NAME, "validate [*.rs] in src/", None),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));
    events.push(sub_evt(
        tool_done(
            "s_grep2",
            GREP_TOOL_NAME,
            ToolOutput::GrepResult {
                entries: vec![GrepFileEntry {
                    path: "src/auth/token.rs".into(),
                    groups: vec![GrepMatchGroup::single(
                        31,
                        "fn validate_token(token: &str) -> Result<Claims> {",
                    )],
                }],
            },
            false,
        ),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));

    events.push(sub_evt(
        tool_start("s_read2", READ_TOOL_NAME, "src/db/pool.rs", None),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));
    events.push(sub_evt(
        tool_done(
            "s_read2",
            READ_TOOL_NAME,
            ToolOutput::ReadCode {
                path: "src/db/pool.rs".into(),
                start_line: 1,
                lines: vec![
                    "use std::time::Duration;".into(),
                    "".into(),
                    "pub struct PoolBuilder {".into(),
                    "    max_connections: u32,".into(),
                    "    idle_timeout: Duration,".into(),
                    "}".into(),
                    "".into(),
                    "impl Default for PoolBuilder {".into(),
                ],
                total_lines: 8,
                instructions: None,
            },
            false,
        ),
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));

    events.push(sub_evt(
        AgentEvent::TextDelta {
            text: concat!(
                "Found 3 relevant patterns in the codebase:\n",
                "\n",
                "- **Builder pattern** in `src/http/client.rs` — uses `ClientBuilder` with fluent setters and a `build()` that returns `Result<Client, ConfigError>`\n",
                "- **Validation** in `src/auth/token.rs` — `validate_token()` returns `Result<Claims>` with descriptive errors\n",
                "- **Default impl** in `src/db/pool.rs` — `PoolBuilder` implements `Default` for sensible defaults",
            ).into(),
        },
        TASK_TOOL_ID,
        "Explore config patterns",
        None,
    ));

    // === Subagent: task tool with weak model ("Summarize test coverage") ===
    let task2_prompt = "Gather test coverage stats across the codebase. Return a summary of unit and integration test coverage.";
    const WEAK_MODEL: &str = "anthropic/claude-3-5-haiku-20241022";

    events.push(sub_evt_with(
        AgentEvent::ThinkingDelta {
            text: "Let me check the test coverage across the project.".into(),
        },
        TASK_TOOL_ID_2,
        "Summarize test coverage",
        Some(task2_prompt),
        Some(WEAK_MODEL),
    ));

    events.push(sub_evt_with(
        tool_start("s2_grep1", GREP_TOOL_NAME, "#[test] [*.rs]", None),
        TASK_TOOL_ID_2,
        "Summarize test coverage",
        None,
        Some(WEAK_MODEL),
    ));
    events.push(sub_evt_with(
        tool_done(
            "s2_grep1",
            GREP_TOOL_NAME,
            ToolOutput::GrepResult {
                entries: vec![
                    GrepFileEntry {
                        path: "src/http/client.rs".into(),
                        groups: vec![
                            GrepMatchGroup::single(90, "#[test]"),
                            GrepMatchGroup::single(105, "#[test]"),
                        ],
                    },
                    GrepFileEntry {
                        path: "src/db/pool.rs".into(),
                        groups: vec![GrepMatchGroup::single(44, "#[test]")],
                    },
                ],
            },
            false,
        ),
        TASK_TOOL_ID_2,
        "Summarize test coverage",
        None,
        Some(WEAK_MODEL),
    ));

    events.push(sub_evt_with(
        AgentEvent::TextDelta {
            text: concat!(
                "Test coverage summary:\n",
                "\n",
                "- **Unit tests**: 82% coverage in `src/` — 2 test functions in `client.rs`, 1 in `pool.rs`\n",
                "- **Integration**: 64% coverage in `tests/`\n",
                "- **Missing**: `src/config/validation.rs` has no tests",
            ).into(),
        },
        TASK_TOOL_ID_2,
        "Summarize test coverage",
        None,
        Some(WEAK_MODEL),
    ));

    // === Subagent: question form ("Project setup") ===
    events.push(sub_evt(
        AgentEvent::ThinkingDelta {
            text: "I need to ask the user about their preferences before scaffolding the project."
                .into(),
        },
        QUESTION_TOOL_ID,
        "Project setup",
        Some("Help me set up a new web project."),
    ));

    events.push(sub_evt(
        tool_start(QUESTION_TOOL_ID, QUESTION_TOOL_NAME, "3 questions", None),
        QUESTION_TOOL_ID,
        "Project setup",
        None,
    ));

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn mock_events_no_duplicate_tool_ids() {
        let mut ids = HashSet::new();
        for event in mock_events() {
            let MockEvent::Agent(envelope) = event else {
                continue;
            };
            if let AgentEvent::ToolStart(ref e) = envelope.event {
                assert!(ids.insert(e.id.clone()), "duplicate tool id: {}", e.id);
            }
        }
    }

    #[test]
    fn mock_events_tool_starts_have_matching_dones() {
        let mut starts = HashSet::new();
        let mut dones = HashSet::new();
        let intentionally_in_progress = ["t_bash_ip", QUESTION_TOOL_ID];

        for event in mock_events() {
            let MockEvent::Agent(envelope) = event else {
                continue;
            };
            match envelope.event {
                AgentEvent::ToolStart(e) => {
                    starts.insert(e.id);
                }
                AgentEvent::ToolDone(e) => {
                    dones.insert(e.id);
                }
                _ => {}
            }
        }

        for id in &starts {
            if intentionally_in_progress.contains(&id.as_str()) {
                assert!(
                    !dones.contains(id),
                    "in-progress tool {id} has a done event"
                );
            } else {
                assert!(dones.contains(id), "tool {id} started but never finished");
            }
        }
    }

    #[test]
    fn mock_questions_non_empty() {
        assert!(!mock_questions().is_empty());
    }
}
