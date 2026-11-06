use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::{McpPromptInfo, McpPromptRef};
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::{BuiltinAction, PackCommand, PackPreparation, PluginReach};
use maki_storage::input_history::InputHistory;
use maki_storage::paths;

use super::{
    AGENT_BUSY_MSG, App, BTW_USAGE_MSG, COMMAND_DEPTH_MSG, EXIT_COMMAND, FAST_OFF_MSG, FAST_ON_MSG,
    FAST_PENDING_MSG, MAX_COMMAND_DEPTH, QueuedMessage, UNKNOWN_COMMAND_PREFIX, WORKFLOW_OFF_MSG,
    WORKFLOW_ON_MSG, YOLO_OFF_MSG, YOLO_ON_MSG,
};
use crate::components::command::ParsedCommand;
use crate::components::{Action, ExitRequest, Status};

impl App {
    /// Entry point for `maki.api.run_command`: splits a command line into the
    /// name and args the input bar would hand over, leading slash optional.
    /// `Err` means nothing ran at all, so the Lua caller can say why.
    pub(crate) fn run_cmdline(
        &mut self,
        cmdline: &str,
        depth: u8,
        caller: &PluginReach,
    ) -> Result<Vec<Action>, String> {
        if depth > MAX_COMMAND_DEPTH {
            return Err(COMMAND_DEPTH_MSG.to_string());
        }
        let trimmed = cmdline.trim();
        let (name, args) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let resolved = self
            .command_palette
            .resolve_for_plugin(&format!("/{}", name.trim_start_matches('/')), caller)?;
        Ok(self.execute_command(
            ParsedCommand {
                name: resolved,
                args: args.trim().to_string(),
                bang: false,
            },
            depth,
        ))
    }

    /// {depth} is the `maki.api.run_command` hop count, forwarded to a Lua
    /// handler so an alias cycle keeps counting. 0 when the user typed it.
    pub(super) fn execute_command(&mut self, cmd: ParsedCommand, depth: u8) -> Vec<Action> {
        match cmd.name.as_str() {
            "/compact" => {
                let instructions = (!cmd.args.is_empty()).then(|| cmd.args.clone());
                if self.status == Status::Streaming {
                    self.queue_compact(instructions);
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact(instructions)]
            }
            "/help" => self.run_builtin(BuiltinAction::Help),
            "/usage" => {
                self.toggle_prompt(self.usage_modal.is_open(), |app| app.usage_modal.toggle());
                if self.usage_modal.is_open() {
                    vec![Action::RefreshUsage]
                } else {
                    vec![]
                }
            }
            "/btw" => {
                let question = cmd.args.trim().to_string();
                if question.is_empty() {
                    self.flash(BTW_USAGE_MSG.into());
                    vec![]
                } else {
                    vec![Action::Btw(question)]
                }
            }
            "/new" => self.start_new_session(cmd.bang),
            "/fork" => self.fork_session(),
            "/queue" => {
                self.queue.set_focus();
                vec![]
            }
            "/model" => self.run_builtin(BuiltinAction::ModelPicker),
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/login" => {
                self.login_picker.open(self.storage.clone());
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
            "/yolo" => {
                let enabled = self.permissions.toggle_yolo();
                let msg = if enabled { YOLO_ON_MSG } else { YOLO_OFF_MSG };
                self.flash(msg.into());
                vec![]
            }
            "/fast" => {
                match self.set_fast(!self.state.fast_intent()) {
                    Ok(()) => self.flash(
                        if self.state.pending_fast {
                            FAST_PENDING_MSG
                        } else if self.state.fast {
                            FAST_ON_MSG
                        } else {
                            FAST_OFF_MSG
                        }
                        .into(),
                    ),
                    Err(msg) => self.flash(msg),
                }
                vec![]
            }
            "/workflow" => {
                self.state.workflow = !self.state.workflow;
                self.flash(
                    if self.state.workflow {
                        WORKFLOW_ON_MSG
                    } else {
                        WORKFLOW_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            EXIT_COMMAND => self.quit(),
            "/reload" => self.reload(cmd.bang),
            // Typing `/trust` is the consent, exactly like `maki trust add
            // --yes`, so there is no second question to ask here.
            "/trust" => self.trust_folder(),
            name @ ("/packupdate" | "/packdel") => {
                match PackCommand::parse(name, &cmd.args, cmd.bang) {
                    Ok(command) => vec![Action::PreparePack(command)],
                    Err(message) => {
                        self.flash(message);
                        vec![]
                    }
                }
            }
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args, depth);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String, depth: u8) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        self.lua_event_handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
            depth,
        );
    }

    pub(crate) fn handle_pack_preparation(&mut self, preparation: PackPreparation) -> Vec<Action> {
        match preparation {
            PackPreparation::Complete(report) => {
                self.flash(report.message());
                Vec::new()
            }
            PackPreparation::Ready(plan) => self.quit_with(ExitRequest::Pack(plan)),
            PackPreparation::Review { prompt, plan } => {
                self.open_prompt(|app| app.pack_review.open(prompt, plan));
                Vec::new()
            }
        }
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let prompt = self.command_palette.find_mcp_prompt(name).unwrap().clone();

        let arguments = Self::parse_prompt_args(&prompt, args);
        let missing: Vec<_> = prompt
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.contains_key(&a.name))
            .map(|a| format!("<{}>", a.name))
            .collect();
        if !missing.is_empty() {
            self.flash(format!("Usage: {} {}", name, missing.join(" ")));
            return vec![];
        }

        let prompt_ref = McpPromptRef {
            qualified_name: prompt.qualified_name.clone(),
            arguments,
        };
        let display_text = if args.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: display_text.clone(),
            images: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash(AGENT_BUSY_MSG.into());
            vec![]
        } else {
            self.start_run(input, display_text)
        }
    }

    fn parse_prompt_args(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut remaining = args.trim();
        if remaining.is_empty() || prompt.arguments.is_empty() {
            return result;
        }
        let last_idx = prompt.arguments.len() - 1;
        for (i, arg) in prompt.arguments.iter().enumerate() {
            if remaining.is_empty() {
                break;
            }
            if i == last_idx {
                result.insert(arg.name.clone(), remaining.to_string());
            } else if let Some((word, rest)) = remaining.split_once(char::is_whitespace) {
                result.insert(arg.name.clone(), word.to_string());
                remaining = rest.trim_start();
            } else {
                result.insert(arg.name.clone(), remaining.to_string());
                break;
            }
        }
        result
    }

    fn execute_custom_command(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(cmd) = self.command_palette.find_custom_command(name) else {
            self.flash(format!("{UNKNOWN_COMMAND_PREFIX} {name}"));
            return vec![];
        };
        self.submit_or_queue(QueuedMessage {
            text: cmd.render(args),
            images: Vec::new(),
        })
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        let path = if args.is_empty() {
            paths::home().unwrap_or_default()
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = paths::home().unwrap_or_default();
                    if rest.is_empty() {
                        home
                    } else {
                        home.join(rest.trim_start_matches('/'))
                    }
                }
                None => PathBuf::from(args),
            }
        };
        self.save_input_history();
        match env::set_current_dir(&path) {
            Ok(()) => {
                if let Ok(canonical) = env::current_dir() {
                    let max_entries = self.input_box.history().max_entries();
                    self.input_box.set_history(InputHistory::load(
                        &self.storage,
                        &canonical,
                        max_entries,
                    ));
                    self.state
                        .session_mut()
                        .set_cwd(canonical.to_string_lossy().into_owned());
                    self.fire_session_event(HostEvent::SessionCwdChanged {
                        cwd: &canonical.to_string_lossy(),
                    });
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()))
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }
}
