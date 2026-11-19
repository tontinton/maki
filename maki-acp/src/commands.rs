//! Slash commands over ACP. Clients only offer what the agent advertised, and
//! send a pick back as plain prompt text, so the names here are exactly what
//! the TUI palette accepts after the `/`.

use std::fmt;
use std::iter;

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, SessionUpdate,
    UnstructuredCommandInput,
};
use maki_agent::command::{
    COMMAND_PREFIX, COMPACT_DESCRIPTION, COMPACT_NAME, CustomCommand, PromptCommand,
};
use maki_agent::{McpPromptInfo, McpPromptRef};

const COMPACT_HINT: &str = "optional guidance";
const CUSTOM_ARGS_HINT: &str = "arguments";

#[derive(Debug, PartialEq)]
pub enum Invocation {
    Compact { instructions: Option<String> },
    Prompt { message: String },
    McpPrompt(McpPromptRef),
}

/// In the order lookups try them, so the first of two names that differ only
/// in case wins both in the advertised list and in [`parse`].
enum Command<'a> {
    Compact,
    Prompt(PromptCommand<'a>),
}

impl Command<'_> {
    fn is_named(&self, typed: &str) -> bool {
        match self {
            Self::Compact => COMPACT_NAME.eq_ignore_ascii_case(typed),
            Self::Prompt(command) => command.is_named(typed),
        }
    }

    fn takes_args(&self) -> bool {
        match self {
            Self::Compact => true,
            Self::Prompt(command) => command.takes_args(),
        }
    }

    fn description(&self) -> &str {
        match self {
            Self::Compact => COMPACT_DESCRIPTION,
            Self::Prompt(command) => command.description(),
        }
    }

    fn hint(&self) -> String {
        match self {
            Self::Compact => COMPACT_HINT.to_owned(),
            Self::Prompt(PromptCommand::Custom(cmd)) => cmd
                .argument_hint
                .as_deref()
                .unwrap_or(CUSTOM_ARGS_HINT)
                .to_owned(),
            Self::Prompt(PromptCommand::Mcp(prompt)) => mcp_hint(prompt),
        }
    }
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compact => f.write_str(COMPACT_NAME),
            Self::Prompt(command) => write!(f, "{command}"),
        }
    }
}

fn mcp_hint(prompt: &McpPromptInfo) -> String {
    let mut hint = String::new();
    for arg in &prompt.arguments {
        if !hint.is_empty() {
            hint.push(' ');
        }
        let (open, close) = if arg.required { ('<', '>') } else { ('[', ']') };
        hint.push(open);
        hint.push_str(&arg.name);
        hint.push(close);
    }
    hint
}

fn commands<'a>(
    custom: &'a [CustomCommand],
    prompts: &'a [McpPromptInfo],
) -> impl Iterator<Item = Command<'a>> {
    iter::once(Command::Compact)
        .chain(PromptCommand::catalog(custom, prompts).map(|(_, command)| Command::Prompt(command)))
}

/// The full list, since each update replaces whatever the client had. A name
/// that matches an earlier one case-blind is left out, as [`parse`] could
/// never reach it.
pub fn available_commands(custom: &[CustomCommand], prompts: &[McpPromptInfo]) -> SessionUpdate {
    let mut available: Vec<AvailableCommand> = Vec::new();
    for command in commands(custom, prompts) {
        let name = command.to_string();
        if available
            .iter()
            .any(|known| known.name.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        let input = command.takes_args().then(|| {
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(command.hint()))
        });
        available.push(AvailableCommand::new(name, command.description()).input(input));
    }
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(available))
}

/// {text} is the prompt's first block when that block is text: a command has
/// to lead the prompt. The server attaches every later block (resource links,
/// embedded resources, images, more text) to the resulting message as it does
/// for a plain prompt. [`Invocation::Compact`] has no message, so it drops them.
///
/// `Ok(None)` hands the prompt to the model unchanged: no leading `/`, a name
/// nothing registered (`/usr/bin ...`), or arguments for a command that takes
/// none (like the TUI, which then sends the line as text). Names match
/// case-blind and arguments arrive trimmed. Zed only sends advertised names in
/// their exact case; the case-blind match serves other clients.
///
/// `Err` is a known command it cannot run, such as an MCP prompt missing a
/// required argument. The server answers with the message as an
/// `agent_message_chunk`, ends the prompt with `end_turn`, and adds nothing to
/// the history.
pub fn parse(
    text: &str,
    custom: &[CustomCommand],
    prompts: &[McpPromptInfo],
) -> Result<Option<Invocation>, String> {
    let Some(line) = text.trim().strip_prefix(COMMAND_PREFIX) else {
        return Ok(None);
    };
    let (name, args) = line
        .split_once(char::is_whitespace)
        .map_or((line, ""), |(name, args)| (name, args.trim()));
    let Some(command) = commands(custom, prompts).find(|command| command.is_named(name)) else {
        return Ok(None);
    };
    if !args.is_empty() && !command.takes_args() {
        return Ok(None);
    }
    Ok(Some(match command {
        Command::Compact => Invocation::Compact {
            instructions: (!args.is_empty()).then(|| args.to_owned()),
        },
        Command::Prompt(PromptCommand::Custom(cmd)) => Invocation::Prompt {
            message: cmd.render(args),
        },
        Command::Prompt(PromptCommand::Mcp(prompt)) => Invocation::McpPrompt(prompt.resolve(args)?),
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use maki_agent::command::{APPENDED_ARGUMENTS_LABEL, CommandScope};
    use maki_agent::mcp::USAGE_PREFIX;
    use test_case::test_case;

    use super::*;

    const REVIEW: &str = "review";
    const REVIEW_HINT: &str = "<file>";
    const REVIEW_CONTENT: &str = "Review $ARGUMENTS";
    const REVIEW_DESCRIPTION: &str = "Code review";
    const FIX: &str = "fix";
    const FIX_CONTENT: &str = "Fix the code";
    const NOTES: &str = "notes";
    const NOTES_CONTENT: &str = "Notes on $ARGUMENTS";
    const TODO: &str = "todo";
    const TODO_HINT: &str = "<topic>";
    const TODO_CONTENT: &str = "List the open todos";
    const GREET: &str = "srv:greet";
    const GREET_QUALIFIED: &str = "srv.greet";
    const GREET_HINT: &str = "<name> [tone]";
    const GREET_DESCRIPTION: &str = "Say hello";
    const PING: &str = "srv:ping";
    const PING_QUALIFIED: &str = "srv.ping";
    const SPACED: &str = "srv:say hi";
    const SPACED_QUALIFIED: &str = "srv.say hi";
    const COLLIDING_PROMPT: &str = "USER:fix";
    const COLLIDING_QUALIFIED: &str = "user.fix";
    const NAME_ARG: &str = "name";
    const TONE_ARG: &str = "tone";
    const PROJECT_REVIEW: &str = "project:review";
    const USER_FIX: &str = "user:fix";
    const USER_NOTES: &str = "user:notes";
    const USER_TODO: &str = "user:todo";
    const SRC_MAIN: &str = "src/main.rs";
    const REVIEW_SRC_MAIN: &str = "Review src/main.rs";
    const GUIDANCE: &str = "keep the test plan";

    fn custom_command(
        name: &str,
        scope: CommandScope,
        content: &str,
        hint: Option<&str>,
        accepts_args: bool,
    ) -> CustomCommand {
        CustomCommand {
            name: name.into(),
            description: String::new(),
            content: content.into(),
            scope,
            accepts_args,
            argument_hint: hint.map(Into::into),
        }
    }

    fn sample_custom() -> Vec<CustomCommand> {
        let review = CustomCommand {
            description: REVIEW_DESCRIPTION.into(),
            ..custom_command(
                REVIEW,
                CommandScope::Project,
                REVIEW_CONTENT,
                Some(REVIEW_HINT),
                true,
            )
        };
        vec![
            review,
            custom_command(FIX, CommandScope::User, FIX_CONTENT, None, false),
            custom_command(NOTES, CommandScope::User, NOTES_CONTENT, None, true),
            custom_command(
                TODO,
                CommandScope::User,
                TODO_CONTENT,
                Some(TODO_HINT),
                true,
            ),
        ]
    }

    fn sample_prompts() -> Vec<McpPromptInfo> {
        let greet = McpPromptInfo {
            description: GREET_DESCRIPTION.into(),
            ..McpPromptInfo::new(
                GREET,
                GREET_QUALIFIED,
                &[(NAME_ARG, true), (TONE_ARG, false)],
            )
        };
        vec![
            greet,
            McpPromptInfo::new(PING, PING_QUALIFIED, &[]),
            McpPromptInfo::new(SPACED, SPACED_QUALIFIED, &[]),
        ]
    }

    fn advertised_in(custom: &[CustomCommand], prompts: &[McpPromptInfo]) -> Vec<AvailableCommand> {
        let SessionUpdate::AvailableCommandsUpdate(update) = available_commands(custom, prompts)
        else {
            panic!("expected an available commands update");
        };
        update.available_commands
    }

    fn advertised(name: &str) -> AvailableCommand {
        advertised_in(&sample_custom(), &sample_prompts())
            .into_iter()
            .find(|c| c.name == name)
            .unwrap()
    }

    fn run(text: &str) -> Result<Option<Invocation>, String> {
        parse(text, &sample_custom(), &sample_prompts())
    }

    fn prompt_ref(qualified_name: &str, arguments: &[(&str, &str)]) -> McpPromptRef {
        McpPromptRef {
            qualified_name: qualified_name.into(),
            arguments: arguments
                .iter()
                .map(|&(k, v)| (k.into(), v.into()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn advertises_every_typable_command_without_slash() {
        let names: Vec<_> = advertised_in(&sample_custom(), &sample_prompts())
            .into_iter()
            .map(|c| c.name)
            .collect();
        let expected = [
            COMPACT_NAME,
            PROJECT_REVIEW,
            USER_FIX,
            USER_NOTES,
            USER_TODO,
            GREET,
            PING,
        ];
        assert_eq!(names, expected);
    }

    #[test]
    fn case_blind_collision_keeps_the_first() {
        let custom = [custom_command(
            FIX,
            CommandScope::User,
            FIX_CONTENT,
            None,
            false,
        )];
        let prompts = [McpPromptInfo::new(
            COLLIDING_PROMPT,
            COLLIDING_QUALIFIED,
            &[],
        )];
        let names: Vec<_> = advertised_in(&custom, &prompts)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, [COMPACT_NAME, USER_FIX]);
        assert_eq!(
            parse(&format!("/{COLLIDING_PROMPT}"), &custom, &prompts),
            Ok(Some(Invocation::Prompt {
                message: FIX_CONTENT.into()
            }))
        );
    }

    #[test_case(COMPACT_NAME, Some(COMPACT_HINT) ; "compact_takes_guidance")]
    #[test_case(PROJECT_REVIEW, Some(REVIEW_HINT) ; "custom_hint_from_frontmatter")]
    #[test_case(USER_NOTES, Some(CUSTOM_ARGS_HINT) ; "custom_placeholder_without_hint")]
    #[test_case(USER_FIX, None ; "custom_without_args")]
    #[test_case(GREET, Some(GREET_HINT) ; "mcp_prompt_args")]
    #[test_case(PING, None ; "mcp_prompt_without_args")]
    fn advertised_hint(name: &str, expected: Option<&str>) {
        let hint = advertised(name).input.map(|input| match input {
            AvailableCommandInput::Unstructured(unstructured) => unstructured.hint,
            _ => panic!("expected unstructured input"),
        });
        assert_eq!(hint.as_deref(), expected);
    }

    #[test_case(PROJECT_REVIEW, REVIEW_DESCRIPTION ; "custom_own_description")]
    #[test_case(USER_FIX, FIX_CONTENT ; "custom_falls_back_to_body")]
    #[test_case(GREET, GREET_DESCRIPTION ; "mcp_own_description")]
    #[test_case(PING, PING ; "mcp_falls_back_to_name")]
    fn advertised_description(name: &str, expected: &str) {
        assert_eq!(advertised(name).description, expected);
    }

    #[test_case("/compact", None ; "no_guidance")]
    #[test_case("  /COMPACT \n", None ; "case_and_whitespace")]
    #[test_case("/compact   keep the test plan  ", Some(GUIDANCE) ; "guidance_trimmed")]
    #[test_case("/compact\tkeep the test plan", Some(GUIDANCE) ; "tab_separator")]
    #[test_case("/compact\u{3000}keep the test plan", Some(GUIDANCE) ; "unicode_separator")]
    #[test_case("/compact\nkeep it\nshort", Some("keep it\nshort") ; "multiline_guidance")]
    fn parses_compact(text: &str, expected: Option<&str>) {
        let instructions = expected.map(Into::into);
        assert_eq!(run(text), Ok(Some(Invocation::Compact { instructions })));
    }

    #[test_case("/project:review src/main.rs", REVIEW_SRC_MAIN ; "renders_args")]
    #[test_case(" /Project:REVIEW \t src/main.rs ", REVIEW_SRC_MAIN ; "case_and_whitespace")]
    #[test_case("/project:review", "Review " ; "args_optional")]
    #[test_case("/user:fix", FIX_CONTENT ; "no_args")]
    #[test_case("/user:todo", TODO_CONTENT ; "hint_without_placeholder_no_args")]
    fn parses_custom_command(text: &str, expected: &str) {
        let message = expected.into();
        assert_eq!(run(text), Ok(Some(Invocation::Prompt { message })));
    }

    #[test]
    fn custom_command_without_placeholder_appends_args() {
        let message = format!("{TODO_CONTENT}\n\n{APPENDED_ARGUMENTS_LABEL} {SRC_MAIN}");
        assert_eq!(
            run(&format!("/{USER_TODO} {SRC_MAIN}")),
            Ok(Some(Invocation::Prompt { message }))
        );
    }

    #[test_case("/srv:greet bob", &[(NAME_ARG, "bob")] ; "required_only")]
    #[test_case("/SRV:Greet  bob  very formal ", &[(NAME_ARG, "bob"), (TONE_ARG, "very formal")] ; "last_takes_rest")]
    fn parses_mcp_prompt(text: &str, arguments: &[(&str, &str)]) {
        let expected = Invocation::McpPrompt(prompt_ref(GREET_QUALIFIED, arguments));
        assert_eq!(run(text), Ok(Some(expected)));
    }

    #[test]
    fn mcp_prompt_missing_required_arg_is_rejected() {
        assert_eq!(
            run(&format!("/{GREET}")).unwrap_err(),
            format!("{USAGE_PREFIX} /{GREET} <{NAME_ARG}>")
        );
    }

    #[test_case("/usr/bin is broken" ; "absolute_path")]
    #[test_case("/unknowncmd" ; "unknown_command")]
    #[test_case("/comp" ; "prefix_is_not_fuzzy")]
    #[test_case("/compact!" ; "bang")]
    #[test_case("/ compact" ; "space_after_slash")]
    #[test_case("/" ; "bare_slash")]
    #[test_case("" ; "empty")]
    #[test_case("compact" ; "no_slash")]
    #[test_case("please /compact" ; "slash_mid_text")]
    #[test_case("/user:fix now" ; "args_for_custom_without_args")]
    #[test_case("/user:fix\nand the docs too" ; "body_after_custom_without_args")]
    #[test_case("/srv:ping now" ; "args_for_prompt_without_args")]
    #[test_case("/srv:say hi" ; "untypable_prompt")]
    fn passes_through(text: &str) {
        assert_eq!(run(text), Ok(None));
    }
}
